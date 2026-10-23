//! `graft stack`: the workspace's drafts between its base and head, oldest
//! first, as the server holds them (`docs/design.md` §8.5), and the
//! operations that rewrite them: `squash`, `reorder`, `split` and `drop`.
//! The server makes the new drafts and moves the head; the working copy,
//! which must be clean, follows. Drafts are named by id or by a unique
//! prefix of one (as `stack list` prints them). Every form prints the stack
//! as it is afterwards.

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::DEFAULT_CACHE_BYTES;
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, stack, with_heartbeat};
use graft_model::{ChangesetId, RepoPath};
use graft_proto::api::WorkspaceApi;
use graft_proto::types::{StackOp, WorkspaceRef};
use serde_json::json;

use crate::cli::{Global, StackAction, StackArgs};
use crate::conn::{begin, call_context, connect, repo_path, require_working_copy, runtime};
use crate::error::Failure;
use crate::render::{self, print_json, short, state_name};

/// Lists or rewrites the stack.
pub(crate) fn run(g: &Global, args: &StackArgs) -> Result<(), Failure> {
    match &args.action {
        None | Some(StackAction::List) => list(g),
        Some(action) => rewrite(g, action),
    }
}

fn list(g: &Global) -> Result<(), Failure> {
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
        print_stack(g, &conn.remote, &cx, &conn.repo, name, base, head, None).await
    })
}

fn rewrite(g: &Global, action: &StackAction) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let mut s = begin(g).await?;
        s.note_recovery();
        s.finish_unfinished().await?;
        let (base, head) = (s.wc.state().base, s.wc.state().head);
        let drafts: Vec<ChangesetId> = stack(&s.conn.remote, &s.cx, &s.conn.repo, base, head)
            .await?
            .iter()
            .map(|v| v.changeset.id())
            .collect();
        if drafts.is_empty() {
            return Err(Failure::new(
                crate::Exit::Failure,
                "no_draft",
                "the workspace has no draft: commit first",
            ));
        }
        let find = |rev: &str| pick(&drafts, rev);
        let (op, verb) = match action {
            StackAction::List => unreachable!("listed above"),
            StackAction::Squash { drafts: revs } if revs.is_empty() => {
                (StackOp::Squash(drafts.clone()), "squashed")
            }
            StackAction::Squash { drafts: revs } => (
                StackOp::Squash(revs.iter().map(|r| find(r)).collect::<Result<_, _>>()?),
                "squashed",
            ),
            StackAction::Reorder { drafts: revs } => (
                StackOp::Reorder(revs.iter().map(|r| find(r)).collect::<Result<_, _>>()?),
                "reordered",
            ),
            StackAction::Split { draft, parts } => {
                let groups = parts
                    .iter()
                    .map(|part| {
                        part.split(',')
                            .filter(|p| !p.trim().is_empty())
                            .map(|p| repo_path(p.trim()))
                            .collect::<Result<Vec<RepoPath>, _>>()
                    })
                    .collect::<Result<_, _>>()?;
                (
                    StackOp::Split {
                        draft: find(draft)?,
                        groups,
                    },
                    "split",
                )
            }
            StackAction::Drop { draft } => (StackOp::Drop(find(draft)?), "dropped"),
        };
        let cache = s.wc.open_cache(DEFAULT_CACHE_BYTES)?;
        let ws = s.workspace_ref();
        let done = with_heartbeat(
            &s.conn.remote,
            &s.cx,
            ws,
            DEFAULT_LEASE_MS,
            HEARTBEAT_EVERY,
            s.wc.rewrite_stack(
                &s.conn.remote,
                &cache,
                &s.cx,
                op,
                DEFAULT_LEASE_MS,
                &FetchOptions::default(),
            ),
        )
        .await?;
        let name = s.wc.config().workspace.clone();
        let note = (!g.json).then(|| format!("{verb}: head {}", head_text(done.head)));
        print_stack(
            g,
            &s.conn.remote,
            &s.cx,
            &s.conn.repo,
            name,
            s.wc.state().base,
            done.head,
            note,
        )
        .await
    })
}

fn head_text(head: Option<ChangesetId>) -> String {
    head.map_or_else(|| "(none)".to_owned(), short)
}

/// The draft `rev` names: a full id, or a unique prefix of one (with or
/// without the kind letter), at least four characters long.
fn pick(drafts: &[ChangesetId], rev: &str) -> Result<ChangesetId, Failure> {
    if let Ok(id) = ChangesetId::parse(rev) {
        return drafts
            .iter()
            .copied()
            .find(|d| *d == id)
            .ok_or_else(|| Failure::usage(format!("{} is not a draft of this stack", short(id))));
    }
    let rev = rev.trim();
    if rev.len() < 4 {
        return Err(Failure::usage(format!(
            "draft {rev:?}: give at least four characters of its id"
        )));
    }
    let found: Vec<ChangesetId> = drafts
        .iter()
        .copied()
        .filter(|d| {
            let text = d.to_string();
            text.starts_with(rev) || text[1..].starts_with(rev)
        })
        .collect();
    match found.as_slice() {
        [one] => Ok(*one),
        [] => Err(Failure::usage(format!(
            "no draft of this stack matches {rev:?}"
        ))),
        _ => Err(Failure::usage(format!(
            "{rev:?} matches {} drafts: give more of the id",
            found.len()
        ))),
    }
}

/// Prints a stack, as `stack list` does.
#[allow(clippy::too_many_arguments)]
async fn print_stack(
    g: &Global,
    remote: &graft_client::Remote,
    cx: &graft_proto::CallContext,
    repo: &str,
    name: Option<String>,
    base: ChangesetId,
    head: Option<ChangesetId>,
    note: Option<String>,
) -> Result<(), Failure> {
    let drafts = stack(remote, cx, repo, base, head).await?;
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
    if let Some(note) = note {
        outln!("{note}");
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_are_picked_by_unique_prefix() {
        let a = ChangesetId::hash(b"a");
        let b = ChangesetId::hash(b"b");
        let drafts = [a, b];
        assert_eq!(pick(&drafts, &a.to_string()).unwrap(), a);
        assert_eq!(pick(&drafts, &short(b)).unwrap(), b);
        assert_eq!(pick(&drafts, &b.to_string()[1..9]).unwrap(), b);
        assert!(pick(&drafts, "c").is_err());
        assert!(pick(&drafts, "zzzzzz").is_err());
        let other = ChangesetId::hash(b"other");
        assert!(pick(&drafts, &other.to_string()).is_err());
    }
}
