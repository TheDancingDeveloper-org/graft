//! `graft status`: the working copy's changes against its base, the
//! conflicts a sync left to resolve, and whether an update is unfinished
//! (`docs/design.md` §8.2: the base, head and state are always visible).
//! Stat-based and local, so it never contacts the server, unless
//! `--upstream` asks it to compare with the line tip (WI-2309): that reads
//! from the server and writes nothing, here or there. `--drafts` asks the
//! server which other live workspaces have open drafts on the same paths
//! (WI-2310, decision D24); it reads only, too.

use std::collections::{BTreeMap, BTreeSet};

use graft_client::Remote;
use graft_client::workcopy::{FileStatus, Ignored, Outcome, Upstream, WorkingCopy};
use graft_model::RepoPath;
use graft_proto::ApiError;
use graft_proto::api::{RepoApi, WorkspaceApi};
use graft_proto::context::CallContext;
use graft_proto::types::{
    Diff, DraftWorkspace, MAX_OVERLAP_PAGE, MAX_OVERLAP_PATHS, OverlappingDrafts, Page,
};
use serde_json::{Value, json};

use crate::cli::{Global, StatusArgs};
use crate::conn::{call_context, connect, require_working_copy, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// The most ignored entries `--ignored` lists before the answer is cut.
const MAX_IGNORED: usize = 1000;
/// The most of the stack's changed paths asked about.
const MAX_DRAFT_PATHS: usize = 1024;
/// The most overlap hits gathered before the answer is cut.
const MAX_DRAFT_HITS: usize = 2000;
/// The most pages either loop reads before the answer is cut (a page can
/// be short, so a page count bounds the calls where a hit count can't).
const MAX_DRAFT_PAGES: usize = 64;

/// One other workspace with drafts on some of the paths asked about.
struct DraftOverlapRow {
    workspace: DraftWorkspace,
    paths: BTreeSet<RepoPath>,
}

/// What `--drafts` found.
struct Drafts {
    rows: Vec<DraftOverlapRow>,
    /// Paths asked about.
    asked: usize,
    /// Whether a bound cut the paths asked about or the hits gathered.
    truncated: bool,
    /// The scope the caller lacks, when the server refused and `--upstream`
    /// was asked too (so its answer still prints).
    denied: Option<String>,
}

impl Drafts {
    fn json(&self) -> Value {
        json!({
            "asked": self.asked,
            "overlaps": self.rows.iter().map(|r| {
                let w = &r.workspace;
                json!({
                    "workspace": w.workspace,
                    "owner": w.owner,
                    "session": w.session,
                    "work_item": w.work_item,
                    "draft_age_ms": w.draft_age_ms,
                    "lease_until_ms": w.lease_until_ms,
                    "paths": r.paths.iter().map(ToString::to_string).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
            "truncated": self.truncated,
            "denied": self.denied,
        })
    }
}

/// The paths `--drafts` asks about: the arguments, else the uncommitted
/// changes plus the paths the stack (`base..head`) changes, bounded.
async fn draft_paths(
    remote: &Remote,
    cx: &CallContext,
    repo: &str,
    wc: &WorkingCopy,
    changes: &[FileStatus],
    args: &StatusArgs,
) -> Result<(Vec<RepoPath>, bool), Failure> {
    if !args.paths.is_empty() {
        let paths = args
            .paths
            .iter()
            .map(|p| {
                RepoPath::new(p.trim_end_matches('/'))
                    .map_err(|e| Failure::usage(format!("path `{p}`: {e:?}")))
            })
            .collect::<Result<_, _>>()?;
        return Ok((paths, false));
    }
    let mut paths: BTreeSet<RepoPath> = changes.iter().map(|c| c.path().clone()).collect();
    let state = wc.state();
    let mut truncated = false;
    if let Some(head) = state.head {
        let mut token = None;
        for page_no in 1.. {
            let req = Diff {
                repo: repo.to_owned(),
                from: state.base,
                to: head,
                paths: Vec::new(),
                patch: None,
                page: Page { limit: 500, token },
            };
            let page = remote.diff(cx, req).await?;
            paths.extend(page.files.into_iter().map(|f| f.change.path().clone()));
            token = page.next;
            if paths.len() >= MAX_DRAFT_PATHS || (page_no >= MAX_DRAFT_PAGES && token.is_some()) {
                truncated = true;
                break;
            }
            if token.is_none() {
                break;
            }
        }
    }
    let mut paths: Vec<RepoPath> = paths.into_iter().collect();
    if paths.len() > MAX_DRAFT_PATHS {
        paths.truncate(MAX_DRAFT_PATHS);
        truncated = true;
    }
    Ok((paths, truncated))
}

/// Asks `OverlappingDrafts` about `paths`, 64 at a time, paging each batch
/// to its end, and groups the hits by workspace. With `soft`, a refusal
/// for want of a scope is reported in the answer rather than as an error.
async fn drafts(
    remote: &Remote,
    cx: &CallContext,
    repo: &str,
    line: &str,
    paths: Vec<RepoPath>,
    mut truncated: bool,
    soft: bool,
) -> Result<Drafts, Failure> {
    let asked = paths.len();
    let mut rows: BTreeMap<String, DraftOverlapRow> = BTreeMap::new();
    let mut hits = 0;
    let mut pages = 0;
    'batches: for batch in paths.chunks(MAX_OVERLAP_PATHS) {
        let mut token = None;
        loop {
            let req = OverlappingDrafts {
                repo: repo.to_owned(),
                line: line.to_owned(),
                paths: batch.to_vec(),
                include_own: false,
                page: Page {
                    limit: MAX_OVERLAP_PAGE,
                    token,
                },
            };
            let page = match remote.overlapping_drafts(cx, req).await {
                Ok(page) => page,
                Err(ApiError::PermissionDenied { scope, .. }) if soft => {
                    return Ok(Drafts {
                        rows: Vec::new(),
                        asked,
                        truncated: false,
                        denied: Some(scope),
                    });
                }
                Err(e) => return Err(e.into()),
            };
            pages += 1;
            for w in page.workspaces {
                rows.entry(w.workspace.clone()).or_insert(DraftOverlapRow {
                    workspace: w,
                    paths: BTreeSet::new(),
                });
            }
            for h in page.hits {
                hits += 1;
                if let Some(row) = rows.get_mut(&h.workspace) {
                    row.paths.insert(h.path);
                }
            }
            token = page.next;
            if hits >= MAX_DRAFT_HITS || pages >= MAX_DRAFT_PAGES {
                truncated |= token.is_some();
                break 'batches;
            }
            if token.is_none() {
                break;
            }
        }
    }
    Ok(Drafts {
        rows: rows.into_values().collect(),
        asked,
        truncated,
        denied: None,
    })
}

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
    let conflicts = wc.open_conflicts()?;
    let ignored = if args.ignored {
        let mut list = wc.ignored()?;
        let cut = list.len() > MAX_IGNORED;
        list.truncate(MAX_IGNORED);
        Some((list, cut))
    } else {
        None
    };
    let (upstream, drafts) = if args.upstream || args.drafts {
        runtime()?.block_on(async {
            let conn = connect(g, Some(&wc)).await?;
            let cx = call_context();
            let up = if args.upstream {
                Some(
                    wc.upstream(&conn.remote, &cx, args.limit, args.cursor.clone())
                        .await?,
                )
            } else {
                None
            };
            let found = if args.drafts {
                let (paths, cut) =
                    draft_paths(&conn.remote, &cx, &conn.repo, &wc, &changes, args).await?;
                let line = wc.state().line.clone();
                Some(
                    drafts(
                        &conn.remote,
                        &cx,
                        &conn.repo,
                        &line,
                        paths,
                        cut,
                        args.upstream,
                    )
                    .await?,
                )
            } else {
                None
            };
            Ok::<_, Failure>((up, found))
        })?
    } else {
        (None, None)
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
        if let Some((list, cut)) = &ignored {
            value["ignored"] = json!({
                "entries": list.iter().map(|i| json!({
                    "path": i.path.to_string(),
                    "dir": i.dir,
                })).collect::<Vec<_>>(),
                "truncated": cut,
            });
        }
        if let Some(up) = &upstream {
            value["upstream"] = upstream_json(up, wc.config().workspace.as_deref());
        }
        if let Some(d) = &drafts {
            value["drafts"] = d.json();
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
    if let Some((list, cut)) = &ignored {
        print_ignored(list, *cut)?;
    }
    if let Some(up) = &upstream {
        print_upstream(up)?;
    }
    if let Some(d) = &drafts {
        print_drafts(d)?;
    }
    Ok(())
}

fn print_ignored(list: &[Ignored], cut: bool) -> Result<(), Failure> {
    for i in list {
        let slash = if i.dir { "/" } else { "" };
        outln!("ignored   {}{slash}", i.path);
    }
    if cut {
        outln!("(ignored cut at {MAX_IGNORED})");
    }
    Ok(())
}

fn print_drafts(d: &Drafts) -> Result<(), Failure> {
    if let Some(scope) = &d.denied {
        outln!("drafts: not shown (needs the {scope} scope)");
        return Ok(());
    }
    if d.asked == 0 {
        outln!("drafts: nothing changed to ask about (name PATHS)");
        return Ok(());
    }
    if d.rows.is_empty() {
        outln!("drafts: no other live workspace has drafts on these paths");
    }
    for r in &d.rows {
        let w = &r.workspace;
        let who = match (&w.work_item, &w.session) {
            (Some(item), _) => format!(" ({item})"),
            (None, Some(session)) => format!(" ({session})"),
            (None, None) => String::new(),
        };
        outln!(
            "drafts: {} by {}{who}, {}s old:",
            w.workspace,
            w.owner,
            w.draft_age_ms / 1000
        );
        for p in &r.paths {
            outln!("  {p}");
        }
    }
    if d.truncated {
        outln!("(drafts cut at the limit)");
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
