//! `graft stack list`: the workspace's drafts between its base and head,
//! oldest first, as the server holds them (`docs/design.md` §8.5). The
//! other stack operations come in phase 3.

use graft_client::workspace::stack;
use graft_proto::api::WorkspaceApi;
use graft_proto::types::WorkspaceRef;
use serde_json::json;

use crate::cli::{Global, StackAction, StackArgs};
use crate::conn::{call_context, connect, require_working_copy, runtime};
use crate::error::Failure;
use crate::render::{self, print_json, short, state_name};

/// Lists the stack.
pub(crate) fn run(g: &Global, args: &StackArgs) -> Result<(), Failure> {
    match args.action {
        None | Some(StackAction::List) => {}
    }
    runtime()?.block_on(async {
        let wc = require_working_copy()?;
        let conn = connect(g, Some(&wc)).await?;
        let cx = call_context();
        // The server's view when there is a workspace, else the local one.
        let (name, base, head) = match &wc.config().workspace {
            Some(name) => {
                let view = conn
                    .remote
                    .get_workspace(
                        &cx,
                        WorkspaceRef {
                            repo: conn.repo.clone(),
                            name: name.clone(),
                        },
                    )
                    .await?;
                (Some(name.clone()), view.base, view.head)
            }
            None => (None, wc.state().base, wc.state().head),
        };
        let drafts = stack(&conn.remote, &cx, &conn.repo, base, head).await?;
        if g.json {
            return print_json(&json!({
                "workspace": name,
                "base": base.to_string(),
                "head": head.map(|h| h.to_string()),
                "drafts": drafts.iter().map(|v| json!({
                    "id": v.changeset.id().to_string(),
                    "state": state_name(v.state),
                    "author": render::author(&v.changeset.author),
                    "authored_at": render::timestamp(v.changeset.authored_at),
                    "subject": v.changeset.message.lines().next().unwrap_or(""),
                    "changes": v.changeset.changes.len(),
                })).collect::<Vec<_>>(),
            }));
        }
        outln!("base {}", short(base));
        if drafts.is_empty() {
            outln!("no drafts");
        }
        for v in &drafts {
            outln!(
                "{} {:<10} {}",
                short(v.changeset.id()),
                state_name(v.state),
                v.changeset.message.lines().next().unwrap_or("")
            );
        }
        Ok(())
    })
}
