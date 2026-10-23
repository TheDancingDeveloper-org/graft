//! `graft status`: the working copy's changes against its base, the
//! conflicts a sync left to resolve, and whether an update is unfinished
//! (`docs/design.md` §8.2: the base, head and state are always visible).
//! Stat-based and local, so it never contacts the server, unless
//! `--upstream` asks it to compare with the line tip (WI-2309): that reads
//! from the server and writes nothing, here or there.

use graft_client::workcopy::{FileStatus, Outcome, Upstream};
use serde_json::{Value, json};

use crate::cli::{Global, StatusArgs};
use crate::conn::{call_context, connect, require_working_copy, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// The status word for a change in JSON and in text.
pub(crate) fn word(change: &FileStatus) -> &'static str {
    match change {
        FileStatus::Added(_) => "added",
        FileStatus::Modified(_) => "modified",
        FileStatus::Deleted(_) => "deleted",
    }
}

/// One change as JSON.
pub(crate) fn change_json(change: &FileStatus) -> Value {
    json!({"path": change.path().to_string(), "status": word(change)})
}

/// `Upstream` as JSON, the shape `graft_upstream` returns too.
pub(crate) fn upstream_json(up: &Upstream, workspace: Option<&str>) -> Value {
    json!({
        "line": up.line,
        "workspace": workspace,
        "base": up.base.to_string(),
        "head": up.head.map(|h| h.to_string()),
        "tip": up.tip.to_string(),
        "up_to_date": up.tip == up.base,
        "behind": up.behind,
        "behind_more": up.behind_more,
        "changed": up.changed.iter().map(|c| json!({
            "path": c.path.to_string(),
            "status": if c.deleted { "deleted" } else { "changed" },
        })).collect::<Vec<_>>(),
        "next": up.next,
        "overlaps": up.overlaps.iter().map(|o| {
            let (outcome, hunks, reason) = match &o.outcome {
                Outcome::Clean => ("clean", &[][..], None),
                Outcome::Conflict { hunks, reason } => ("conflict", &hunks[..], *reason),
            };
            json!({
                "path": o.path.to_string(),
                "uncommitted": o.uncommitted,
                "outcome": outcome,
                "hunks": hunks.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>(),
                "reason": reason,
            })
        }).collect::<Vec<_>>(),
        "overlaps_truncated": up.overlaps_truncated,
        "stack_conflict": up.stack_conflict.map(|d| d.to_string()),
        "uncommitted_checked": true,
        "clean": up.clean(),
    })
}

/// Prints the line, base, head and the changed paths, and with
/// `--upstream` the comparison with the line tip.
pub(crate) fn run(g: &Global, args: &StatusArgs) -> Result<(), Failure> {
    let wc = require_working_copy()?;
    let changes = wc.status()?;
    let conflicts = wc.conflicts()?.map(|c| c.files).unwrap_or_default();
    let upstream = if args.upstream {
        Some(runtime()?.block_on(async {
            let conn = connect(g, Some(&wc)).await?;
            let up = wc
                .upstream(
                    &conn.remote,
                    &call_context(),
                    args.limit,
                    args.cursor.clone(),
                )
                .await?;
            Ok::<_, Failure>(up)
        })?)
    } else {
        None
    };
    let state = wc.state();
    if g.json {
        let mut value = json!({
            "line": state.line,
            "base": state.base.to_string(),
            "head": state.head.map(|h| h.to_string()),
            "clean": changes.is_empty(),
            "changes": changes.iter().map(change_json).collect::<Vec<_>>(),
            "conflicts": conflicts.iter().map(|f| json!({
                "path": f.path.to_string(),
                "mode": f.mode.name(),
            })).collect::<Vec<_>>(),
            "unfinished": state.moving.is_some(),
        });
        if let Some(up) = &upstream {
            value["upstream"] = upstream_json(up, wc.config().workspace.as_deref());
        }
        return print_json(&value);
    }
    outln!("line {}", state.line);
    outln!("base {}", short(state.base));
    match state.head {
        Some(head) => outln!("head {}", short(head)),
        None => outln!("head (none)"),
    }
    if state.moving.is_some() {
        outln!("an update is unfinished: run `graft sync`");
    }
    if changes.is_empty() {
        outln!("clean");
    }
    for change in &changes {
        outln!("{:<9} {}", word(change), change.path());
    }
    for f in &conflicts {
        outln!("conflict {:<8} {}", f.mode.name(), f.path);
    }
    if let Some(up) = &upstream {
        print_upstream(up)?;
    }
    Ok(())
}

fn print_upstream(up: &Upstream) -> Result<(), Failure> {
    if up.tip == up.base {
        outln!("upstream: {} is at your base", up.line);
        return Ok(());
    }
    let more = if up.behind_more { "+" } else { "" };
    outln!(
        "upstream: {} is at {}, {}{more} changeset(s) ahead of your base",
        up.line,
        short(up.tip),
        up.behind
    );
    for c in &up.changed {
        let word = if c.deleted { "deleted" } else { "changed" };
        outln!("  {word:<9} {}", c.path);
    }
    if let Some(next) = &up.next {
        outln!("  (more: --cursor {next})");
    }
    if up.overlaps.is_empty() {
        outln!("none of your changes overlap");
    }
    for o in &up.overlaps {
        let source = if o.uncommitted {
            "uncommitted"
        } else {
            "stack"
        };
        match &o.outcome {
            Outcome::Clean => outln!("merges   {} ({source})", o.path),
            Outcome::Conflict { hunks, reason } => {
                let lines = hunks
                    .iter()
                    .map(|(a, b)| {
                        if a == b {
                            a.to_string()
                        } else {
                            format!("{a}-{b}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let why = match (reason, lines.is_empty()) {
                    (Some(r), _) => format!(" [{r}]"),
                    (None, false) => format!(" lines {lines}"),
                    (None, true) => String::new(),
                };
                outln!("conflict {} ({source}){why}", o.path);
            }
        }
    }
    if let Some(d) = up.stack_conflict {
        outln!("the stack conflicts at draft {}", short(d));
    }
    if up.overlaps_truncated {
        outln!("(overlaps cut at the limit)");
    }
    Ok(())
}
