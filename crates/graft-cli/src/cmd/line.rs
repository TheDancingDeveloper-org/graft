//! `graft line`: creates or lists lines (`docs/design.md` §4.6). `create`
//! forks a line by compare-and-swap from absent, so it never overwrites an
//! existing line; it needs `repo:<r>:land:<name>` or `repo:<r>:admin`.

use graft_proto::api::RepoApi;
use graft_proto::types::{CreateLine, ListRefs, Page, RefKind};
use serde_json::json;

use crate::cli::{Global, LineAction, LineArgs};
use crate::conn::{call_context, connect, runtime, working_copy};
use crate::error::Failure;
use crate::render::{print_json, ref_view, short};

/// Runs a line action.
pub(crate) fn run(g: &Global, args: &LineArgs) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        match &args.action {
            LineAction::Create { name, from } => {
                let from = conn.resolve(&cx, from).await?;
                let view = conn
                    .remote
                    .create_line(
                        &cx,
                        CreateLine {
                            repo: conn.repo.clone(),
                            name: name.clone(),
                            from,
                        },
                    )
                    .await?;
                if g.json {
                    return print_json(&ref_view(&view));
                }
                outln!("created line {} at {}", view.name, short(view.target));
                Ok(())
            }
            LineAction::List {
                prefix,
                limit,
                cursor,
            } => {
                let refs = conn
                    .remote
                    .list_refs(
                        &cx,
                        ListRefs {
                            repo: conn.repo.clone(),
                            prefix: prefix.clone(),
                            page: Page {
                                limit: *limit,
                                token: cursor.clone(),
                            },
                        },
                    )
                    .await?;
                // A full page may have more after it; the last ref name
                // resumes, whatever its kind.
                let next = (refs.len() as u32 >= *limit && *limit > 0)
                    .then(|| refs.last().map(|r| r.name.clone()))
                    .flatten();
                let lines: Vec<_> = refs.iter().filter(|r| r.kind == RefKind::Line).collect();
                if g.json {
                    return print_json(&json!({
                        "lines": lines.iter().map(|r| ref_view(r)).collect::<Vec<_>>(),
                        "next": next,
                    }));
                }
                for r in &lines {
                    outln!("{:<24} {}  v{}", r.name, short(r.target), r.version);
                }
                if let Some(next) = next {
                    outln!("more: --cursor {next}");
                }
                Ok(())
            }
        }
    })
}
