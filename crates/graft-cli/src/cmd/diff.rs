//! `graft diff`: two changesets (server-side, with patches), or the working
//! copy against its base (paths and statuses, no patch text).

use graft_model::Change;
use graft_proto::api::RepoApi;
use graft_proto::types::{Diff, Page, PatchOptions};
use serde_json::json;

use crate::cli::{DiffArgs, Global};
use crate::cmd::status::{change_json, word};
use crate::conn::{call_context, connect, repo_path, require_working_copy, runtime, working_copy};
use crate::error::Failure;
use crate::render::{diff, kind_name, print_json, short};

/// Context lines and the per-file patch cap, as the MCP `graft_diff` tool uses.
const PATCH_CONTEXT: u32 = 3;
const PATCH_MAX_BYTES: u64 = 64 * 1024;

/// Diffs two changesets, or the working copy.
pub(crate) fn run(g: &Global, args: &DiffArgs) -> Result<(), Failure> {
    match (&args.from, &args.to) {
        (None, None) => working_copy_diff(g),
        (Some(from), Some(to)) => changeset_diff(g, args, from, to),
        _ => Err(Failure::usage(
            "give two changesets (FROM TO), or none to diff the working copy",
        )),
    }
}

/// The working copy against its base: the changed paths and how each changed.
fn working_copy_diff(g: &Global) -> Result<(), Failure> {
    let wc = require_working_copy()?;
    let changes = wc.status()?;
    if g.json {
        return print_json(&json!({
            "from": wc.state().base.to_string(),
            "to": null,
            "files": changes.iter().map(change_json).collect::<Vec<_>>(),
        }));
    }
    for change in &changes {
        outln!("{:<9} {}", word(change), change.path());
    }
    if changes.is_empty() {
        outln!("no changes against {}", short(wc.state().base));
    } else {
        outln!("(paths only; diff two changesets for patches)");
    }
    Ok(())
}

/// Two changesets, server-side, one page of files.
fn changeset_diff(g: &Global, args: &DiffArgs, from: &str, to: &str) -> Result<(), Failure> {
    let paths = args
        .paths
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let from = conn.resolve(&cx, from).await?;
        let to = conn.resolve(&cx, to).await?;
        let result = conn
            .remote
            .diff(
                &cx,
                Diff {
                    repo: conn.repo.clone(),
                    from,
                    to,
                    paths,
                    patch: Some(PatchOptions {
                        context: PATCH_CONTEXT,
                        max_bytes_per_file: PATCH_MAX_BYTES,
                    }),
                    page: Page {
                        limit: args.limit,
                        token: args.cursor.clone(),
                    },
                },
            )
            .await?;
        if g.json {
            return print_json(&diff(from, to, &result));
        }
        for f in &result.files {
            match &f.change {
                Change::Put { path, kind, .. } => {
                    outln!("put {} {path}", kind_name(*kind));
                }
                Change::Delete { path } => outln!("delete {path}"),
            }
            if let Some(patch) = &f.patch {
                out!("{patch}");
            }
        }
        if let Some(next) = &result.next {
            outln!("more: --cursor {next}");
        }
        Ok(())
    })
}
