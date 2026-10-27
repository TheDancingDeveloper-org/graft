//! `graft review-queue <line>`: bounded review work from the journal.

use graft_model::AuthorKind;
use graft_model::RepoPath;
use graft_proto::api::RepoApi;
use graft_proto::types::{Page, ReviewQueue};
use serde_json::json;

use crate::cli::{Global, ReviewQueueArgs};
use crate::conn::{call_context, connect, runtime, working_copy_info};
use crate::error::Failure;
use crate::render::{print_json, short};

pub(crate) fn run(g: &Global, args: &ReviewQueueArgs) -> Result<(), Failure> {
    let path_prefix = args
        .path
        .as_deref()
        .map(RepoPath::new)
        .transpose()
        .map_err(|error| Failure::usage(format!("invalid --path: {error}")))?;
    runtime()?.block_on(async {
        let wc = working_copy_info()?;
        let conn = connect(g, wc.as_ref()).await?;
        let page = conn
            .remote
            .review_queue(
                &call_context(),
                ReviewQueue {
                    repo: conn.repo.clone(),
                    line: args.line.clone(),
                    since_lsn: args.since,
                    path_prefix,
                    max_lines: args.max_lines,
                    page: Page {
                        limit: args.limit,
                        token: None,
                    },
                },
            )
            .await?;
        if g.json {
            return print_json(&json!({
                "line": args.line,
                "items": page.items.iter().map(|item| json!({
                    "changeset": item.changeset.to_string(),
                    "pos": item.pos,
                    "lsn": item.lsn,
                    "author": {
                        "name": item.author.name,
                        "email": item.author.email,
                        "kind": author_kind_name(item.author.kind),
                    },
                    "paths_touched": item.paths_touched,
                    "lines_changed": item.lines_changed,
                })).collect::<Vec<_>>(),
                "next_lsn": page.next_lsn,
            }));
        }
        for item in page.items {
            outln!(
                "{} {} {} {} paths {} lines {}",
                item.lsn,
                item.pos,
                short(item.changeset),
                item.author.name,
                item.paths_touched,
                item.lines_changed
            );
        }
        outln!("next_lsn {}", page.next_lsn.unwrap_or(args.since));
        Ok(())
    })
}

fn author_kind_name(kind: AuthorKind) -> &'static str {
    match kind {
        AuthorKind::Human => "human",
        AuthorKind::Agent => "agent",
        AuthorKind::Bot => "bot",
        AuthorKind::Ci => "ci",
    }
}
