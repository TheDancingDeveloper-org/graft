//! Oversized data through every paged tool: each page stays within the 64 KiB
//! cap, shrinks to fit, and still carries the cursor that continues it. A
//! walk over all pages reassembles the whole list or file.

mod support;

use std::collections::BTreeSet;

use graft_mcp::{MAX_RESULT_BYTES, PATCH_MAX_BYTES};
use graft_model::ChangesetId;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use support::{Fake, FileSpec, blob_data, call, cid, connect, delete, put, text_file};

fn structured(result: &rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("a structured result")
}

/// The size of a whole result as a client receives it.
fn wire_size(result: &rmcp::model::CallToolResult) -> usize {
    serde_json::to_vec(result).unwrap().len()
}

/// Pages through a tool by its `cursor` and `next` fields, checking every
/// page against the cap. Returns the items of `items` from all pages, in
/// order, and the number of pages.
async fn walk(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    mut args: Value,
    items: &str,
) -> (Vec<Value>, usize) {
    let mut all = Vec::new();
    let mut pages = 0;
    loop {
        let result = call(client, tool, args.clone()).await;
        assert_eq!(result.is_error, Some(false), "{tool} failed");
        let size = wire_size(&result);
        assert!(size <= MAX_RESULT_BYTES, "{tool} page is {size} bytes");
        let page = structured(&result);
        all.extend(page[items].as_array().unwrap().iter().cloned());
        pages += 1;
        match page["next"].as_str() {
            Some(next) => args["cursor"] = json!(next),
            None => break,
        }
    }
    (all, pages)
}

/// Twelve text files of about 12 KiB each, all changed in `c3`. Each patch is
/// over the 4 KiB cap, so a page of twelve files overflows the result cap.
fn twelve_big_files() -> std::sync::Arc<Fake> {
    let lines: Vec<(String, ChangesetId)> = (0..300)
        .map(|i| (format!("line {i:03} {}", "y".repeat(30)), cid(3)))
        .collect();
    let specs: Vec<(String, FileSpec)> = (0..12)
        .map(|i| (format!("many/{i:02}.txt"), text_file(&lines)))
        .collect();
    let changes = specs
        .iter()
        .map(|(path, spec)| (3, put(path, spec)))
        .collect();
    Fake::with_tree(specs, changes)
}

#[tokio::test]
async fn read_chunks_of_binary_data_fit_and_reassemble() {
    // Binary bytes are hex-encoded, and the result carries the content twice,
    // so a 64 KiB chunk cannot fit. Every chunk shrinks, and following
    // `next_offset` returns the whole file in order.
    let client = connect(Fake::with_files()).await;
    let mut offset = 0;
    let mut bytes = Vec::new();
    let mut first = true;
    loop {
        let result = call(
            &client,
            "graft_read",
            json!({"repo": "r", "at": "main", "path": "bin/blob.dat", "limit": 65536, "offset": offset}),
        )
        .await;
        assert_eq!(result.is_error, Some(false));
        let size = wire_size(&result);
        assert!(size <= MAX_RESULT_BYTES, "read page is {size} bytes");
        let page = structured(&result);
        assert_eq!(page["encoding"], "hex");
        let returned = page["returned_bytes"].as_u64().unwrap();
        if first {
            assert!(returned < 65536, "the first chunk shrank to {returned}");
            assert_eq!(page["next_offset"], returned, "the cursor is the chunk end");
            first = false;
        }
        let text = page["content"].as_str().unwrap();
        assert_eq!(text.len() as u64, returned * 2, "hex is two digits a byte");
        bytes.extend(
            (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digits")),
        );
        match page["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(bytes, blob_data());
}

#[tokio::test]
async fn ls_pages_long_entries_under_the_cap() {
    // 300 entries with 200-byte names: a page of 500 entries would be far
    // over the cap, so the page shrinks and the cursor carries on.
    let tree = (0..300)
        .map(|i| {
            (
                format!("wide/{i:03}-{}", "n".repeat(200)),
                text_file(&[("x", cid(1))]),
            )
        })
        .collect();
    let client = connect(Fake::with_tree(tree, vec![])).await;
    let mut paths = BTreeSet::new();
    let mut args = json!({"repo": "r", "at": "main", "path": "wide", "limit": 500});
    let mut pages = 0;
    loop {
        let result = call(&client, "graft_ls", args.clone()).await;
        let size = wire_size(&result);
        assert!(size <= MAX_RESULT_BYTES, "ls page is {size} bytes");
        let page = structured(&result);
        let entries = page["entries"].as_array().unwrap();
        assert!(entries.len() < 500, "the page shrank to {}", entries.len());
        paths.extend(
            entries
                .iter()
                .map(|e| e["path"].as_str().unwrap().to_owned()),
        );
        pages += 1;
        match page["next"].as_str() {
            Some(next) => args["cursor"] = json!(next),
            None => break,
        }
    }
    assert_eq!(paths.len(), 300, "every entry is listed once");
    assert!(pages > 1, "the listing took {pages} page");
}

#[tokio::test]
async fn diff_patches_page_under_the_cap() {
    // Twelve files of default patches are over the cap as one page of twenty.
    let client = connect(twelve_big_files()).await;
    let (files, pages) = walk(
        &client,
        "graft_diff",
        json!({"repo": "r", "from": cid(2).to_string(), "to": "main"}),
        "files",
    )
    .await;
    assert!(pages > 1, "the diff took {pages} page");
    assert_eq!(files.len(), 12);
    for file in &files {
        assert_eq!(file["truncated"], true, "{}", file["path"]);
        assert!(file["patch"].as_str().unwrap().len() <= PATCH_MAX_BYTES);
    }
}

#[tokio::test]
async fn show_patches_page_under_the_cap() {
    let client = connect(twelve_big_files()).await;
    let (files, pages) = walk(
        &client,
        "graft_show",
        json!({"repo": "r", "changeset": "main"}),
        "files",
    )
    .await;
    assert!(pages > 1, "the show took {pages} page");
    assert_eq!(files.len(), 12);
}

#[tokio::test]
async fn show_with_deletes_and_clipped_patches_stays_under_the_cap() {
    // A changeset with a patch over the cap and a delete, shown in full.
    let big: Vec<(String, ChangesetId)> = (0..200)
        .map(|i| (format!("big line {i:03} {}", "x".repeat(60)), cid(3)))
        .collect();
    let spec = text_file(&big);
    let changes = vec![(3, put("big.md", &spec)), (3, delete("gone.md"))];
    let client = connect(Fake::with_tree(vec![("big.md".into(), spec)], changes)).await;
    let (files, pages) = walk(
        &client,
        "graft_show",
        json!({"repo": "r", "changeset": "main"}),
        "files",
    )
    .await;
    assert_eq!(pages, 1);
    assert_eq!(files.len(), 2);
    assert_eq!(files[1]["op"], "delete");
}

#[tokio::test]
async fn blame_windows_shrink_under_the_cap_and_cover_the_file() {
    // A thousand lines, each from a different origin in turn: every line is a
    // range of its own, so the default window of 1,000 lines overflows.
    let lines: Vec<(String, ChangesetId)> = (0..1_000)
        .map(|i| (format!("line {i}"), cid(1 + (i % 3) as u8)))
        .collect();
    let spec = text_file(&lines);
    let client = connect(Fake::with_tree(vec![("long.txt".into(), spec)], vec![])).await;

    let mut start = 0_u64;
    let mut covered = 0_u64;
    let mut pages = 0;
    loop {
        let result = call(
            &client,
            "graft_blame",
            json!({"repo": "r", "at": "main", "path": "long.txt", "start_line": start}),
        )
        .await;
        let size = wire_size(&result);
        assert!(size <= MAX_RESULT_BYTES, "blame page is {size} bytes");
        let page = structured(&result);
        for range in page["ranges"].as_array().unwrap() {
            assert_eq!(range["start"], covered, "ranges are contiguous");
            covered = range["end"].as_u64().unwrap();
        }
        pages += 1;
        match page["next_start_line"].as_u64() {
            Some(next) => start = next,
            None => break,
        }
    }
    assert_eq!(covered, 1_000, "every line is blamed");
    assert!(pages > 1, "the blame took {pages} window");
}
