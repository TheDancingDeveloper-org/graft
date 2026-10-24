//! The pre-land check (`docs/design.md` §8.8, decision D27): what `graft
//! land` and `graft_land` read before they send `Land`. Read-only and
//! advisory: the server's `Land` still decides, and nothing here builds or
//! tests anything (D22).
//!
//! 1. **Conflict preview.** The files the stack (`Diff` base..head) and the
//!    line (`Diff` base..tip, restricted to the stack's paths) both changed,
//!    and one `Rebase { dry_run: true }` of the stack onto the tip under the
//!    land's merge mode. A conflict it reports is a real conflict and stops
//!    the land; files both sides changed that merge cleanly are reported.
//! 2. **Draft overlap** (`OverlappingDrafts`, D24): other live workspaces
//!    with open drafts on the stack's paths. Reported, never a stop; without
//!    the `repo:<r>:drafts` scope it is skipped with a note.
//!
//! Every read is paged and capped; a capped read says so and does not stop
//! the land on what it did not see. The CLI and the MCP tool share this
//! code, so both return the same `preflight` JSON.

use std::collections::{BTreeMap, BTreeSet};

use graft_model::{ChangesetId, RepoPath};
use graft_proto::{
    ApiError,
    api::{RepoApi, WorkspaceApi},
    context::CallContext,
    types::{
        Diff, DraftWorkspace, MAX_OVERLAP_PAGE, MAX_OVERLAP_PATHS, MergeMode, OverlappingDrafts,
        Page, Rebase, ResolveRef,
    },
};
use serde_json::{Value, json};

/// The most of the stack's paths checked.
const MAX_OWN_PATHS: usize = 1024;
/// Paths per restricted `Diff` request (the server's default `max_diff_paths`).
const DIFF_PATHS: usize = 64;
/// Entries per page when listing the stack's changes.
const OWN_PAGE: u32 = 256;
/// The most paths listed in either result list before it says
/// `truncated`, and the most workspaces and paths per workspace in the
/// overlap, so the result stays well under the MCP result cap.
const MAX_LISTED: usize = 64;
const MAX_WORKSPACES: usize = 16;
const MAX_WORKSPACE_PATHS: usize = 8;
/// The most overlap pages read.
const MAX_DRAFT_PAGES: usize = 64;

/// What the check is asked about.
#[derive(Clone, Debug)]
pub struct PreflightRequest {
    /// Repository.
    pub repo: String,
    /// The line the land targets.
    pub line: String,
    /// The stack's base.
    pub base: ChangesetId,
    /// The stack head that lands.
    pub head: ChangesetId,
    /// The land's merge mode.
    pub merge: MergeMode,
}

/// One other workspace with drafts on some of the stack's paths.
#[derive(Clone, Debug)]
pub struct DraftOverlapRow {
    /// The workspace.
    pub workspace: DraftWorkspace,
    /// The shared paths.
    pub paths: BTreeSet<RepoPath>,
}

/// What the draft-overlap read found.
#[derive(Clone, Debug, Default)]
pub struct DraftCheck {
    /// The workspaces, by name.
    pub overlaps: Vec<DraftOverlapRow>,
    /// A bound cut the read.
    pub truncated: bool,
    /// Why the read was skipped: the scope the caller lacks, or a server
    /// that does not answer it.
    pub skipped: Option<String>,
    /// The short reason code for `skipped`: `scope` or `unserved`.
    pub skip_code: Option<&'static str>,
}

/// What the pre-land check found.
#[derive(Clone, Debug)]
pub struct Preflight {
    /// The line tip the check compared with.
    pub tip: ChangesetId,
    /// Files the stack and the line both changed since the base.
    pub both_changed: Vec<RepoPath>,
    /// A bound cut the stack's paths or `both_changed`.
    pub both_changed_truncated: bool,
    /// The dry-run rebase's conflict, when it found one.
    pub conflict: Option<ApiError>,
    /// Why the dry-run rebase was not run, when a server does not answer it.
    pub preview_skipped: Option<String>,
    /// The dry-run rebase was sent (false when the stack is on the tip, or
    /// it was skipped).
    pub preview_ran: bool,
    /// The draft overlap.
    pub drafts: DraftCheck,
}

impl Preflight {
    /// True when a real conflict should stop the land.
    pub fn stops(&self) -> bool {
        self.conflict.is_some()
    }

    /// The paths the dry run reports conflicting.
    pub fn conflict_paths(&self) -> Vec<&str> {
        match &self.conflict {
            Some(ApiError::Conflict { paths, .. }) => {
                paths.iter().map(|p| p.path.as_str()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// What ran, as `{rebase, overlap}`: `ran`, `not_needed` or
    /// `skipped(<code>)`. The rebase needs only what a land needs; the
    /// overlap needs `repo:<r>:drafts` (WI-2417).
    pub fn summary(&self) -> Value {
        let rebase = match (&self.preview_skipped, self.preview_ran) {
            (Some(_), _) => "skipped(unserved)".to_owned(),
            (None, true) => "ran".to_owned(),
            (None, false) => "not_needed".to_owned(),
        };
        let overlap = match self.drafts.skip_code {
            Some(code) => format!("skipped({code})"),
            None => "ran".to_owned(),
        };
        json!({"rebase": rebase, "overlap": overlap})
    }

    /// Notes for parts of the check that did not run, for stderr in every
    /// output mode, so a skipped part is never silent.
    pub fn skip_notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(why) = &self.preview_skipped {
            out.push(format!("pre-land: conflict preview not run ({why})"));
        }
        if let Some(why) = &self.drafts.skipped {
            let still = if self.preview_ran {
                "; the conflict preview still ran"
            } else {
                ""
            };
            out.push(format!(
                "pre-land: draft overlap not checked ({why}){still}"
            ));
        }
        out
    }

    /// The `preflight` object every client returns.
    pub fn json(&self) -> Value {
        let conflicts = self.conflict_paths();
        let conflicted: BTreeSet<&str> = conflicts.iter().copied().collect();
        let merges: Vec<&str> = self
            .both_changed
            .iter()
            .map(RepoPath::as_str)
            .filter(|p| !conflicted.contains(p))
            .collect();
        let d = &self.drafts;
        json!({
            "checked": true,
            "pre_land_check": self.summary(),
            "tip": self.tip.to_string(),
            "stop": self.stops(),
            "preview_skipped": self.preview_skipped,
            "conflicts": conflicts.iter().take(MAX_LISTED).collect::<Vec<_>>(),
            "conflicts_truncated": conflicts.len() > MAX_LISTED,
            "merges_cleanly": merges.iter().take(MAX_LISTED).collect::<Vec<_>>(),
            "both_changed_truncated": self.both_changed_truncated || merges.len() > MAX_LISTED,
            "drafts": {
                "checked": d.skipped.is_none(),
                "skipped": d.skipped,
                "overlaps": d.overlaps.iter().take(MAX_WORKSPACES).map(|r| {
                    let w = &r.workspace;
                    json!({
                        "workspace": w.workspace,
                        "owner": w.owner,
                        "session": w.session,
                        "work_item": w.work_item,
                        "draft_age_ms": w.draft_age_ms,
                        "lease_until_ms": w.lease_until_ms,
                        "paths": r.paths.iter().take(MAX_WORKSPACE_PATHS).map(RepoPath::as_str).collect::<Vec<_>>(),
                        "paths_truncated": r.paths.len() > MAX_WORKSPACE_PATHS,
                    })
                }).collect::<Vec<_>>(),
                "truncated": d.truncated || d.overlaps.len() > MAX_WORKSPACES,
            },
        })
    }

    /// One line per finding, for text output. Skipped parts are in
    /// [`Preflight::skip_notes`].
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let conflicts = self.conflict_paths();
        for p in &conflicts {
            out.push(format!("pre-land: conflict {p}"));
        }
        let conflicted: BTreeSet<&str> = conflicts.iter().copied().collect();
        for p in &self.both_changed {
            if !conflicted.contains(p.as_str()) {
                out.push(format!("pre-land: merges cleanly {p}"));
            }
        }
        if self.both_changed_truncated {
            out.push("pre-land: (both-changed files cut at the limit)".to_owned());
        }
        let d = &self.drafts;
        for r in &d.overlaps {
            let w = &r.workspace;
            let who = match (&w.work_item, &w.session) {
                (Some(item), _) => format!(" ({item})"),
                (None, Some(session)) => format!(" ({session})"),
                (None, None) => String::new(),
            };
            let paths = r
                .paths
                .iter()
                .map(RepoPath::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!(
                "pre-land: drafts in {} by {}{who}, {}s old: {paths}",
                w.workspace,
                w.owner,
                w.draft_age_ms / 1000
            ));
        }
        if d.truncated {
            out.push("pre-land: (draft overlap cut at the limit)".to_owned());
        }
        out
    }
}

/// The `preflight` object when `--no-check` (or `check: false`) skipped it.
pub fn skipped_json() -> Value {
    json!({"checked": false})
}

/// Runs the pre-land check. Reads only; a dry-run conflict is returned in
/// the answer, any other error is returned as is.
pub async fn preflight<A: RepoApi + WorkspaceApi>(
    api: &A,
    cx: &CallContext,
    req: &PreflightRequest,
) -> Result<Preflight, ApiError> {
    let tip = api
        .resolve_ref(
            cx,
            ResolveRef {
                repo: req.repo.clone(),
                name: req.line.clone(),
            },
        )
        .await?
        .target;
    let (own, mut truncated) = stack_paths(api, cx, req).await?;
    let mut out = Preflight {
        tip,
        both_changed: Vec::new(),
        both_changed_truncated: false,
        conflict: None,
        preview_skipped: None,
        preview_ran: false,
        drafts: DraftCheck::default(),
    };
    if tip != req.base {
        let (both, cut) = both_changed(api, cx, req, tip, &own).await?;
        out.both_changed = both;
        truncated |= cut;
        let dry = api
            .rebase(
                cx,
                Rebase {
                    repo: req.repo.clone(),
                    changeset: req.head,
                    onto: tip,
                    merge: req.merge,
                    dry_run: true,
                },
            )
            .await;
        match dry {
            Ok(_) => out.preview_ran = true,
            Err(e @ ApiError::Conflict { .. }) => {
                out.preview_ran = true;
                out.conflict = Some(e);
            }
            Err(e) if unserved(&e) => out.preview_skipped = Some(UNSERVED.to_owned()),
            Err(e) => return Err(e),
        }
    }
    out.both_changed_truncated = truncated;
    out.drafts = drafts(api, cx, req, &own).await?;
    Ok(out)
}

fn diff_req(
    repo: &str,
    from: ChangesetId,
    to: ChangesetId,
    paths: Vec<RepoPath>,
    limit: u32,
    token: Option<String>,
) -> Diff {
    Diff {
        repo: repo.to_owned(),
        from,
        to,
        paths,
        patch: None,
        page: Page { limit, token },
    }
}

/// The stack's changed paths, base..head, up to [`MAX_OWN_PATHS`].
async fn stack_paths<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    req: &PreflightRequest,
) -> Result<(Vec<RepoPath>, bool), ApiError> {
    let mut own = BTreeSet::new();
    let mut token = None;
    loop {
        let page = api
            .diff(
                cx,
                diff_req(&req.repo, req.base, req.head, Vec::new(), OWN_PAGE, token),
            )
            .await?;
        own.extend(page.files.into_iter().map(|f| f.change.path().clone()));
        match page.next {
            Some(next) if own.len() < MAX_OWN_PATHS => token = Some(next),
            Some(_) => break,
            None => return Ok((own.into_iter().collect(), false)),
        }
    }
    Ok((own.into_iter().take(MAX_OWN_PATHS).collect(), true))
}

/// The stack's paths the line also changed, base..tip, `DIFF_PATHS` at a
/// time.
async fn both_changed<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    req: &PreflightRequest,
    tip: ChangesetId,
    own: &[RepoPath],
) -> Result<(Vec<RepoPath>, bool), ApiError> {
    let mut found = Vec::new();
    for chunk in own.chunks(DIFF_PATHS) {
        let wanted: BTreeSet<&RepoPath> = chunk.iter().collect();
        let mut token = None;
        loop {
            let page = api
                .diff(
                    cx,
                    diff_req(
                        &req.repo,
                        req.base,
                        tip,
                        chunk.to_vec(),
                        DIFF_PATHS as u32,
                        token,
                    ),
                )
                .await?;
            for f in page.files {
                // A path filter is a prefix: keep only the exact paths.
                if wanted.contains(f.change.path()) {
                    if found.len() == MAX_LISTED {
                        return Ok((found, true));
                    }
                    found.push(f.change.path().clone());
                }
            }
            match page.next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
    }
    Ok((found, false))
}

/// Other live workspaces with drafts on `paths`, grouped by workspace. A
/// refusal for want of the drafts scope, or from a server that does not
/// serve the read, is reported, not returned.
async fn drafts<A: WorkspaceApi>(
    api: &A,
    cx: &CallContext,
    req: &PreflightRequest,
    paths: &[RepoPath],
) -> Result<DraftCheck, ApiError> {
    let mut rows: BTreeMap<String, DraftOverlapRow> = BTreeMap::new();
    let mut pages = 0;
    let mut truncated = false;
    'batches: for batch in paths.chunks(MAX_OVERLAP_PATHS) {
        let mut token = None;
        loop {
            let ask = OverlappingDrafts {
                repo: req.repo.clone(),
                line: req.line.clone(),
                paths: batch.to_vec(),
                include_own: false,
                page: Page {
                    limit: MAX_OVERLAP_PAGE,
                    token,
                },
            };
            let page = match api.overlapping_drafts(cx, ask).await {
                Ok(page) => page,
                Err(ApiError::PermissionDenied { scope, .. }) => {
                    return Ok(skipped("scope", format!("needs the {scope} scope")));
                }
                // The check is advisory: a server that does not serve the
                // read lets the land go on without it.
                Err(e) if unserved(&e) => return Ok(skipped("unserved", UNSERVED.to_owned())),
                Err(e) => return Err(e),
            };
            pages += 1;
            for w in page.workspaces {
                rows.entry(w.workspace.clone()).or_insert(DraftOverlapRow {
                    workspace: w,
                    paths: BTreeSet::new(),
                });
            }
            for h in page.hits {
                if let Some(row) = rows.get_mut(&h.workspace) {
                    row.paths.insert(h.path);
                }
            }
            token = page.next;
            if pages >= MAX_DRAFT_PAGES {
                truncated = token.is_some();
                break 'batches;
            }
            if token.is_none() {
                break;
            }
        }
    }
    Ok(DraftCheck {
        overlaps: rows.into_values().collect(),
        truncated,
        skipped: None,
        skip_code: None,
    })
}

/// The note for a read the server does not serve.
const UNSERVED: &str = "the server does not answer it";

/// True for a server that does not serve a read (an older or partial one,
/// which answers `unimplemented`). Neither read has a policy of its own, so
/// any refusal by policy means that.
fn unserved(e: &ApiError) -> bool {
    matches!(e, ApiError::Unavailable { .. } | ApiError::Rejected { .. })
}

fn skipped(code: &'static str, why: String) -> DraftCheck {
    DraftCheck {
        skipped: Some(why),
        skip_code: Some(code),
        ..DraftCheck::default()
    }
}
