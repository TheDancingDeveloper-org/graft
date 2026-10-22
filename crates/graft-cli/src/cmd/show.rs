//! `graft show`: one changeset, its record, and its patch against the first
//! parent (the changes themselves, for a root).

use graft_model::Change;
use graft_proto::ApiError;
use graft_proto::api::RepoApi;
use graft_proto::types::{ChangesetView, DiffResult, FileDiff};
use graft_proto::types::{Diff, GetChangesets, Page, PatchOptions};
use serde_json::json;

use crate::cli::{Global, ShowArgs};
use crate::conn::{call_context, connect, runtime, working_copy};
use crate::error::Failure;
use crate::render::{changeset_view, file_diff, kind_name, print_json, state_name};

/// Context lines and the per-file patch cap, as the MCP `graft_show` tool uses.
const PATCH_CONTEXT: u32 = 3;
const PATCH_MAX_BYTES: u64 = 64 * 1024;

/// Prints a changeset with its files and patches.
pub(crate) fn run(g: &Global, args: &ShowArgs) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let id = conn.resolve(&cx, &args.rev).await?;
        let got = conn
            .remote
            .get_changesets(
                &cx,
                GetChangesets {
                    repo: conn.repo.clone(),
                    ids: vec![id],
                },
            )
            .await?;
        let view = match got.found.into_iter().next() {
            Some(view) => view,
            None if got.collected.contains(&id) => return Err(ApiError::Collected { id }.into()),
            None => {
                return Err(ApiError::NotFound {
                    what: format!("changeset {id}"),
                }
                .into());
            }
        };
        let page = Page {
            limit: args.limit,
            token: args.cursor.clone(),
        };
        let files: DiffResult = match view.changeset.parents.first().copied() {
            Some(parent) => {
                conn.remote
                    .diff(
                        &cx,
                        Diff {
                            repo: conn.repo.clone(),
                            from: parent,
                            to: id,
                            paths: Vec::new(),
                            patch: Some(PatchOptions {
                                context: PATCH_CONTEXT,
                                max_bytes_per_file: PATCH_MAX_BYTES,
                            }),
                            page,
                        },
                    )
                    .await?
            }
            None => DiffResult {
                files: view
                    .changeset
                    .changes
                    .iter()
                    .map(|change| FileDiff {
                        change: change.clone(),
                        patch: None,
                        truncated: false,
                    })
                    .collect(),
                next: None,
            },
        };
        let mut value = changeset_view(&view);
        if let Some(obj) = value.as_object_mut() {
            obj.remove("changes");
        }
        value["files"] = json!(files.files.iter().map(file_diff).collect::<Vec<_>>());
        value["files_next"] = json!(files.next);
        if g.json {
            return print_json(&value);
        }
        print_text(&view, &files)?;
        Ok(())
    })
}

/// The human form: the header, the message, then each file and its patch.
fn print_text(view: &ChangesetView, files: &DiffResult) -> Result<(), Failure> {
    let c = &view.changeset;
    outln!("changeset {}", c.id());
    for parent in &c.parents {
        outln!("parent {parent}");
    }
    outln!("author {} <{}>", c.author.name, c.author.email);
    outln!("authored {} ms since the epoch", c.authored_at.ms);
    outln!("state {}", state_name(view.state));
    outln!();
    outln!("{}", c.message.trim_end());
    outln!();
    for f in &files.files {
        let line = match &f.change {
            Change::Put { path, kind, .. } => format!("put {} {path}", kind_name(*kind)),
            Change::Delete { path } => format!("delete {path}"),
        };
        outln!("{line}");
        if let Some(patch) = &f.patch {
            out!("{patch}");
        }
    }
    if let Some(next) = &files.next {
        outln!("more files: --cursor {next}");
    }
    Ok(())
}
