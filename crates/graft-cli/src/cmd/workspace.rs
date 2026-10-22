//! `graft workspace`: shows, lists, opens, renews or closes workspaces
//! (`docs/design.md` §3, §7.2). Inside a working copy the default workspace
//! is the working copy's; `open` there opens it at the working copy's base,
//! and `close` detaches the working copy (its drafts become abandoned on the
//! server, and the next write reopens a workspace and adopts them).

use std::time::Duration;

use graft_client::workspace::DEFAULT_LEASE_MS;
use graft_proto::api::WorkspaceApi;
use graft_proto::types::{OpenWorkspace, Page, WorkspaceRef, WorkspaceView};
use serde_json::json;

use crate::cli::{Global, WorkspaceAction, WorkspaceArgs};
use crate::conn::{call_context, connect, runtime, working_copy};
use crate::error::Failure;
use crate::render::{print_json, short, workspace_view};

/// Runs a workspace action.
pub(crate) fn run(g: &Global, args: &WorkspaceArgs) -> Result<(), Failure> {
    let action = args
        .action
        .as_ref()
        .unwrap_or(&WorkspaceAction::Show { name: None });
    runtime()?.block_on(async {
        let mut wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let ours = wc.as_ref().and_then(|w| w.config().workspace.clone());
        let named = |name: &Option<String>| -> Result<WorkspaceRef, Failure> {
            let name = name.clone().or_else(|| ours.clone()).ok_or_else(|| {
                Failure::usage("no workspace: name one, or run inside a working copy that has one")
            })?;
            Ok(WorkspaceRef {
                repo: conn.repo.clone(),
                name,
            })
        };
        match action {
            WorkspaceAction::Show { name } => {
                let view = conn.remote.get_workspace(&cx, named(name)?).await?;
                print_view(g.json, &view, None)
            }
            WorkspaceAction::List { limit, cursor } => {
                let page = Page {
                    limit: *limit,
                    token: cursor.clone(),
                };
                let views = conn
                    .remote
                    .list_workspaces(&cx, conn.repo.clone(), page)
                    .await?;
                // A full page may have more after it; the last name resumes.
                let next = (views.len() as u32 >= *limit && *limit > 0)
                    .then(|| views.last().map(|v| v.name.clone()))
                    .flatten();
                if g.json {
                    return print_json(&json!({
                        "workspaces": views.iter().map(workspace_view).collect::<Vec<_>>(),
                        "next": next,
                    }));
                }
                for v in &views {
                    let head = v.head.map_or_else(|| "-".to_owned(), short);
                    outln!(
                        "{:<24} {:<12} base {} head {}",
                        v.name,
                        v.line,
                        short(v.base),
                        head
                    );
                }
                if let Some(next) = next {
                    outln!("more: --cursor {next}");
                }
                Ok(())
            }
            WorkspaceAction::Open { name, line } => match wc.as_mut() {
                Some(wc) => {
                    match (&ours, name) {
                        (Some(have), Some(want)) if have != want => {
                            return Err(Failure::usage(format!(
                                "the working copy already has workspace {have}"
                            )));
                        }
                        (None, Some(want)) => wc.set_workspace(Some(want.clone()))?,
                        _ => {}
                    }
                    let begun = wc.begin(&conn.remote, &cx, DEFAULT_LEASE_MS).await;
                    let begun = match begun {
                        Ok(b) => b,
                        Err(e) => {
                            if ours.is_none() {
                                wc.set_workspace(None)?;
                            }
                            return Err(e.into());
                        }
                    };
                    print_view(g.json, &begun.workspace, Some(begun.opened))
                }
                None => {
                    let view = conn
                        .remote
                        .open_workspace(
                            &cx,
                            OpenWorkspace {
                                repo: conn.repo.clone(),
                                name: name.clone(),
                                line: line.clone(),
                                at: None,
                                lease_ms: DEFAULT_LEASE_MS,
                            },
                        )
                        .await?;
                    print_view(g.json, &view, Some(true))
                }
            },
            WorkspaceAction::Renew { name, lease } => {
                let lease_ms = lease.map_or(DEFAULT_LEASE_MS, |s| {
                    Duration::from_secs(s).as_millis() as u64
                });
                let view = conn.remote.renew_lease(&cx, named(name)?, lease_ms).await?;
                print_view(g.json, &view, None)
            }
            WorkspaceAction::Close { name, keep_drafts } => {
                let target = named(name)?;
                conn.remote
                    .close_workspace(&cx, target.clone(), *keep_drafts)
                    .await?;
                if let Some(wc) = wc.as_mut()
                    && ours.as_deref() == Some(target.name.as_str())
                {
                    wc.set_workspace(None)?;
                }
                if g.json {
                    return print_json(&json!({"name": target.name, "closed": true}));
                }
                outln!("closed {}", target.name);
                Ok(())
            }
        }
    })
}

/// A workspace as JSON (with `opened` for `open`) or text.
fn print_view(json: bool, view: &WorkspaceView, opened: Option<bool>) -> Result<(), Failure> {
    if json {
        let mut value = workspace_view(view);
        if let Some(opened) = opened {
            value["opened"] = json!(opened);
        }
        return print_json(&value);
    }
    if opened == Some(true) {
        outln!("opened {}", view.name);
    }
    outln!("workspace {}", view.name);
    outln!("owner {}", view.owner);
    outln!("line {}", view.line);
    outln!("base {}", short(view.base));
    match view.head {
        Some(head) => outln!("head {}", short(head)),
        None => outln!("head (none)"),
    }
    outln!("lease until {} ms", view.lease_until_ms);
    Ok(())
}
