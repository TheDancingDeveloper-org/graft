//! Each read tool, called over the MCP protocol against the fake API, with
//! golden JSON output. The cap test runs the same path with oversized data.

mod support;

use graft_mcp::{MAX_RESULT_BYTES, PATCH_MAX_BYTES};
use serde_json::{Value, json};
use support::{Fake, assert_golden, call, cid, connect};

fn structured(result: &rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("a structured result")
}

#[tokio::test]
async fn log_pages_from_the_newest_changeset() {
    let client = connect(Fake::standard()).await;
    let first = call(
        &client,
        "graft_log",
        json!({"repo": "r", "from": cid(3).to_string(), "limit": 2}),
    )
    .await;
    assert_eq!(first.is_error, Some(false));
    assert_golden("log_page1", &structured(&first));

    let next = structured(&first)["next"].as_str().unwrap().to_owned();
    let second = call(
        &client,
        "graft_log",
        json!({"repo": "r", "from": cid(3).to_string(), "limit": 2, "cursor": next}),
    )
    .await;
    assert_golden("log_page2", &structured(&second));
    assert!(
        structured(&second)["next"].is_null(),
        "the last page has no next"
    );
}

#[tokio::test]
async fn log_accepts_a_ref_name() {
    let client = connect(Fake::standard()).await;
    let result = call(
        &client,
        "graft_log",
        json!({"repo": "r", "from": "main", "limit": 1}),
    )
    .await;
    assert_eq!(
        structured(&result)["changesets"][0]["id"],
        cid(3).to_string()
    );
}

#[tokio::test]
async fn show_a_root_lists_no_files() {
    let client = connect(Fake::standard()).await;
    let result = call(
        &client,
        "graft_show",
        json!({"repo": "r", "changeset": cid(1).to_string()}),
    )
    .await;
    assert_golden("show_root", &structured(&result));
}

#[tokio::test]
async fn show_a_child_diffs_against_its_first_parent() {
    let client = connect(Fake::with_files()).await;
    let result = call(
        &client,
        "graft_show",
        json!({"repo": "r", "changeset": "main"}),
    )
    .await;
    assert_golden("show_child", &structured(&result));

    // `main` is c3: a Put, a Delete, a binary Put with no patch, and a text
    // Put whose patch is clipped and flagged.
    let files = &structured(&result)["files"];
    assert_eq!(files.as_array().unwrap().len(), 5);
    let delete = files
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["op"] == "delete");
    assert_eq!(delete.unwrap()["path"], "src/old.rs");
    let blob = files
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "bin/blob.dat");
    assert!(
        blob.unwrap()["patch"].is_null(),
        "binary files have no patch"
    );
    let big = files
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "docs/big.md")
        .unwrap();
    assert_eq!(big["truncated"], true);
    assert_eq!(big["patch"].as_str().unwrap().len(), PATCH_MAX_BYTES);
}

#[tokio::test]
async fn diff_between_snapshots() {
    let client = connect(Fake::with_files()).await;
    let result = call(
        &client,
        "graft_diff",
        json!({"repo": "r", "from": cid(1).to_string(), "to": "main"}),
    )
    .await;
    assert_golden("diff_range", &structured(&result));

    // c2 and c3 together: each path once, sorted, with the Delete of src/old.rs
    // and the clipped patch of docs/big.md.
    let files = structured(&result)["files"].clone();
    let paths: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "README.md",
            "bin/blob.dat",
            "docs/big.md",
            "src/lib.rs",
            "src/old.rs",
            "src/util/mod.rs"
        ]
    );
    assert_eq!(files[4]["op"], "delete");
    assert_eq!(files[2]["truncated"], true);
    assert_eq!(files[3]["truncated"], false);
}

#[tokio::test]
async fn diff_with_patches_off_omits_them() {
    let client = connect(Fake::with_files()).await;
    let result = call(
        &client,
        "graft_diff",
        json!({"repo": "r", "from": cid(2).to_string(), "to": "main", "patch": false}),
    )
    .await;
    let files = &structured(&result)["files"];
    assert_eq!(files.as_array().unwrap().len(), 5);
    assert!(
        files
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["patch"].is_null())
    );
}

#[tokio::test]
async fn ls_the_root_of_a_snapshot() {
    let client = connect(Fake::standard()).await;
    let result = call(&client, "graft_ls", json!({"repo": "r", "at": "main"})).await;
    assert_golden("ls_root", &structured(&result));
}

#[tokio::test]
async fn status_needs_a_working_copy() {
    let client = connect(Fake::standard()).await;
    let result = call(&client, "graft_status", json!({"repo": "r"})).await;
    assert_eq!(result.is_error, Some(true));
    assert_golden("status_error", &structured(&result));
}

#[tokio::test]
async fn unknown_refs_are_typed_errors_not_protocol_errors() {
    let client = connect(Fake::standard()).await;
    let result = call(&client, "graft_log", json!({"repo": "r", "from": "nope"})).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["kind"], "not_found");
    assert_eq!(structured(&result)["retryable"], false);
}

#[tokio::test]
async fn out_of_range_limits_are_invalid_arguments() {
    let client = connect(Fake::standard()).await;
    let result = call(
        &client,
        "graft_log",
        json!({"repo": "r", "from": "main", "limit": 0}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["kind"], "invalid_argument");
}

#[tokio::test]
async fn results_stay_within_the_byte_cap_and_page_smaller() {
    // Long author names and subjects: each clipped to 256 bytes, so a page
    // of 100 would overflow 64 KiB and must shrink.
    let long = "n".repeat(1000);
    let fake = Fake::with_authors(100, |_| long.clone(), |_| long.clone());
    let client = connect(fake).await;
    let result = call(
        &client,
        "graft_log",
        json!({"repo": "r", "from": "main", "limit": 100}),
    )
    .await;
    let size = serde_json::to_vec(&result).unwrap().len();
    assert!(size <= MAX_RESULT_BYTES, "result is {size} bytes");
    let page = structured(&result);
    let returned = page["changesets"].as_array().unwrap().len();
    assert!(
        returned < 100 && returned > 0,
        "a smaller page was returned: {returned}"
    );
    assert!(
        page["next"].is_string(),
        "the shrunk page still has a cursor"
    );
}

#[tokio::test]
async fn messages_and_subjects_are_clipped_and_flagged() {
    let long = "m".repeat(20_000);
    let fake = Fake::with_authors(3, |_| "a".into(), |_| long.clone());
    let client = connect(fake).await;
    let result = call(
        &client,
        "graft_show",
        json!({"repo": "r", "changeset": cid(1).to_string()}),
    )
    .await;
    let message = &structured(&result)["changeset"]["message"];
    assert_eq!(message["truncated"], true);
    assert!(message["text"].as_str().unwrap().len() <= 8 * 1024);
    let size = serde_json::to_vec(&result).unwrap().len();
    assert!(size <= MAX_RESULT_BYTES);
}

// ---- path-bearing tools (graft-model G1.1 `RepoPath`) --------------------

#[tokio::test]
async fn ls_a_directory_pages_entries() {
    let client = connect(Fake::with_files()).await;
    let result = call(
        &client,
        "graft_ls",
        json!({"repo": "r", "at": "main", "path": "src"}),
    )
    .await;
    assert_golden("ls_src", &structured(&result));
}

#[tokio::test]
async fn read_a_file_in_chunks() {
    let client = connect(Fake::with_files()).await;
    let result = call(
        &client,
        "graft_read",
        json!({"repo": "r", "at": "main", "path": "README.md", "limit": 4}),
    )
    .await;
    assert_golden("read_chunk", &structured(&result));
}

#[tokio::test]
async fn blame_a_file_by_line_window() {
    let client = connect(Fake::with_files()).await;
    let result = call(
        &client,
        "graft_blame",
        json!({"repo": "r", "at": "main", "path": "README.md", "start_line": 0, "end_line": 10}),
    )
    .await;
    assert_golden("blame_window", &structured(&result));
}

#[tokio::test]
async fn conflict_markers_are_clipped_and_pages_shrink_to_the_cap() {
    use graft_mcp::CONFLICT_MARKERS_MAX_BYTES;
    let client = connect(Fake::with_conflicts(1, CONFLICT_MARKERS_MAX_BYTES + 100)).await;
    let one = call(
        &client,
        "graft_conflicts",
        json!({"repo": "r", "workspace": "w"}),
    )
    .await;
    let body = structured(&one);
    assert_eq!(body["paths"][0]["markers_truncated"], true);
    assert!(body["paths"][0]["markers"].as_str().unwrap().len() <= CONFLICT_MARKERS_MAX_BYTES);

    let client = connect(Fake::with_conflicts(80, 4 * 1024)).await;
    let page = call(
        &client,
        "graft_conflicts",
        json!({"repo": "r", "workspace": "w", "limit": 64}),
    )
    .await;
    let size = serde_json::to_vec(&page).unwrap().len();
    assert!(size <= MAX_RESULT_BYTES, "conflicts result is {size} bytes");
    let body = structured(&page);
    let returned = body["paths"].as_array().unwrap().len();
    assert!(returned < 64 && returned > 0, "page shrank to {returned}");
    assert!(body["next"].is_string(), "the shrunk page has a cursor");
}

#[tokio::test]
async fn sync_changed_paths_are_capped_and_flagged() {
    let client = connect(Fake::with_synced_paths(250)).await;
    let result = call(
        &client,
        "graft_sync",
        json!({"repo": "r", "workspace": "w"}),
    )
    .await;
    let body = structured(&result);
    assert_eq!(body["changed_paths_truncated"], true);
    assert_eq!(body["changed_paths_total"], 250);
    assert_eq!(body["changed_paths"].as_array().unwrap().len(), 200);
    let size = serde_json::to_vec(&result).unwrap().len();
    assert!(size <= MAX_RESULT_BYTES);
}

#[tokio::test]
async fn draft_overlap_pages_shrink_to_the_byte_cap() {
    let client = connect(Fake::with_overlaps(200, 2_000)).await;
    let result = call(
        &client,
        "graft_draft_overlap",
        json!({"repo": "r", "paths": ["src"], "limit": 200}),
    )
    .await;
    let size = serde_json::to_vec(&result).unwrap().len();
    assert!(size <= MAX_RESULT_BYTES, "overlap result is {size} bytes");
    let body = structured(&result);
    let returned = body["overlaps"].as_array().unwrap().len();
    assert!(returned < 200 && returned > 0, "page shrank to {returned}");
    assert!(body["next"].is_string(), "the shrunk page has a cursor");
}

#[tokio::test]
async fn upstream_changed_paths_shrink_to_the_byte_cap() {
    let client = connect(Fake::with_upstream_paths(100, 200)).await;
    let result = call(
        &client,
        "graft_upstream",
        json!({"repo": "r", "workspace": "w", "limit": 100}),
    )
    .await;
    let size = serde_json::to_vec(&result).unwrap().len();
    assert!(size <= MAX_RESULT_BYTES, "upstream result is {size} bytes");
    let body = structured(&result);
    let returned = body["changed"].as_array().unwrap().len();
    assert!(returned < 100 && returned > 0, "page shrank to {returned}");
    assert!(body["next"].is_string(), "the shrunk page has a cursor");
}

/// A working copy whose status payload is deliberately larger than the cap.
struct OversizedStatus;

impl graft_mcp::WorkingCopyTools for OversizedStatus {
    fn status<'a>(&'a self, _repo: &'a str) -> graft_mcp::WorkingCopyFuture<'a, Value> {
        Box::pin(async { Ok(json!({"blob": "x".repeat(MAX_RESULT_BYTES)})) })
    }

    fn commit<'a>(
        &'a self,
        _cx: &'a graft_proto::CallContext,
        _req: graft_mcp::WorkingCopyCommit,
    ) -> graft_mcp::WorkingCopyFuture<'a, Value> {
        Box::pin(async { Ok(json!({})) })
    }

    fn land<'a>(
        &'a self,
        _cx: &'a graft_proto::CallContext,
        _req: graft_mcp::WorkingCopyLand,
    ) -> graft_mcp::WorkingCopyFuture<'a, Option<Value>> {
        Box::pin(async { Ok(None) })
    }

    fn sync<'a>(
        &'a self,
        _cx: &'a graft_proto::CallContext,
        _req: graft_mcp::WorkingCopySync,
    ) -> graft_mcp::WorkingCopyFuture<'a, Option<Value>> {
        Box::pin(async { Ok(None) })
    }
}

#[tokio::test]
async fn every_registered_tool_result_fits_and_an_oversized_status_is_typed() {
    use std::sync::Arc;
    let client = support::connect_with(Fake::standard(), Some(Arc::new(OversizedStatus))).await;
    let result = call(&client, "graft_status", json!({"repo": "r"})).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["kind"], "result_too_large");
    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_RESULT_BYTES);

    // One ordinary call per registered tool. Typed errors still go through
    // `call_tool`, so a handler that forgot the cap fails this too.
    let calls: &[(&str, Value)] = &[
        (
            "graft_log",
            json!({"repo": "r", "from": "main", "limit": 1}),
        ),
        (
            "graft_show",
            json!({"repo": "r", "changeset": cid(1).to_string()}),
        ),
        (
            "graft_diff",
            json!({"repo": "r", "from": cid(1).to_string(), "to": "main", "patch": false}),
        ),
        (
            "graft_blame",
            json!({"repo": "r", "path": "src/lib.rs", "at": "main"}),
        ),
        (
            "graft_read",
            json!({"repo": "r", "path": "src/lib.rs", "at": "main"}),
        ),
        ("graft_ls", json!({"repo": "r", "at": "main"})),
        ("graft_upstream", json!({"repo": "r"})),
        (
            "graft_draft_overlap",
            json!({"repo": "r", "paths": ["src/lib.rs"]}),
        ),
        (
            "graft_commit",
            json!({"repo": "r", "workspace": "w", "message": "m", "files": []}),
        ),
        ("graft_land", json!({"repo": "r", "workspace": "w"})),
        ("graft_sync", json!({"repo": "r", "workspace": "w"})),
        (
            "graft_revert",
            json!({"repo": "r", "workspace": "w", "changeset": cid(1).to_string()}),
        ),
        (
            "graft_pick",
            json!({"repo": "r", "workspace": "w", "changeset": cid(1).to_string()}),
        ),
        ("graft_conflicts", json!({"repo": "r", "workspace": "w"})),
        (
            "graft_resolve",
            json!({"repo": "r", "workspace": "w", "resolutions": []}),
        ),
        (
            "graft_line_create",
            json!({"repo": "r", "name": "side", "at": "main"}),
        ),
        (
            "graft_merge",
            json!({"repo": "r", "from": "side", "into": "main"}),
        ),
        (
            "graft_tag",
            json!({"repo": "r", "name": "v0", "at": "main"}),
        ),
        (
            "graft_workspace",
            json!({"repo": "r", "action": "get", "name": "w"}),
        ),
    ];
    let registered: std::collections::BTreeSet<&str> =
        graft_mcp::TOOLS.iter().map(|t| t.name).collect();
    let called: std::collections::BTreeSet<&str> = calls
        .iter()
        .map(|(n, _)| *n)
        .chain(["graft_status"])
        .collect();
    assert_eq!(
        called, registered,
        "the sweep must name every registered tool"
    );
    for (tool, args) in calls {
        let result = call(&client, tool, args.clone()).await;
        let size = serde_json::to_vec(&result).unwrap().len();
        assert!(size <= MAX_RESULT_BYTES, "{tool} returned {size} bytes");
    }
}
