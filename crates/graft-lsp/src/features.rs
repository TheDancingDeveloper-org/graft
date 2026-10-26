//! Hover blame, code lenses and diagnostics.

use std::collections::BTreeSet;

use graft_client::workcopy::{Stat, needs_rehash};
use graft_model::{BlobId, Change, ChangesetId, RepoPath};
use graft_proto::ApiError;
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::attribution;
use graft_proto::types::{
    Blame, Diff, DraftState, GetChangesets, Log, Page, PatchOptions, ResolveRef,
};
use serde_json::{Value, json};

use crate::render::{kind_name, old_ranges, short, time};
use crate::rpc::RpcError;
use crate::{LspServer, Target, doc_uri, file_uri};

/// History entries a code lens counts before it says "N+".
pub(crate) const LOG_PAGE: u32 = 100;

/// The most diagnostics for one file; the last one says how many more.
pub(crate) const MAX_REGIONS: usize = 50;

/// The patch bytes read per file to place diagnostics.
pub(crate) const PATCH_BYTES: u64 = 64 << 10;

/// The most conflicting paths published.
const MAX_CONFLICT_URIS: usize = 200;

/// The longest subject or name shown.
const MAX_TEXT: usize = 200;

const ERROR: u8 = 1;
const WARNING: u8 = 2;
const INFORMATION: u8 = 3;

/// How a file changed between two snapshots.
enum Changed {
    No,
    Deleted,
    /// These 0-based old-side line ranges.
    Regions(Vec<(u32, u32)>),
    /// Somewhere the patch doesn't place (binary, or over the cap).
    Whole,
}

impl<A: ObjectApi + RepoApi + WorkspaceApi> LspServer<A> {
    /// The snapshot and path a document shows, or `None` when graft has
    /// nothing to say about it. The flag is whether it is a working-copy
    /// file.
    pub(crate) async fn locate(
        &self,
        uri: &str,
    ) -> Result<Option<(ChangesetId, RepoPath, bool)>, RpcError> {
        match self.target(uri) {
            Target::Local(path) => {
                let wc = self.workcopy.as_ref().expect("local targets need one");
                Ok(Some((wc.snapshot(), path, true)))
            }
            Target::Virtual(g) => {
                if g.repo != self.repo {
                    return Err(RpcError::invalid_params(format!(
                        "this server serves repository {:?}, not {:?}",
                        self.repo, g.repo
                    )));
                }
                let at = self.resolve(&g.rev).await?;
                Ok(Some((at, g.path, false)))
            }
            Target::Other => Ok(None),
        }
    }

    /// A changeset id, or the target of a ref.
    pub(crate) async fn resolve(&self, rev: &str) -> Result<ChangesetId, RpcError> {
        if let Ok(id) = ChangesetId::parse(rev) {
            return Ok(id);
        }
        let view = self
            .api
            .resolve_ref(
                &self.cx(),
                ResolveRef {
                    repo: self.repo.clone(),
                    name: rev.to_owned(),
                },
            )
            .await
            .map_err(api_error)?;
        Ok(view.target)
    }

    pub(crate) async fn hover(&mut self, params: &Value) -> Result<Value, RpcError> {
        self.refresh_workcopy();
        let uri = doc_uri(params).ok_or_else(|| RpcError::invalid_params("no textDocument"))?;
        let line = params
            .pointer("/position/line")
            .and_then(Value::as_u64)
            .and_then(|l| u32::try_from(l).ok())
            .filter(|l| *l < u32::MAX)
            .ok_or_else(|| RpcError::invalid_params("no position"))?;
        let Some((at, path, local)) = self.locate(&uri).await? else {
            return Ok(Value::Null);
        };
        let cx = self.cx();
        let blame = self
            .api
            .blame(
                &cx,
                Blame {
                    repo: self.repo.clone(),
                    at,
                    path: path.clone(),
                    lines: Some((line, line + 1)),
                },
            )
            .await;
        let blame = match blame {
            Ok(b) => b,
            // A file the snapshot doesn't have: nothing to blame.
            Err(ApiError::NotFound { .. }) => return Ok(Value::Null),
            Err(e) => return Err(api_error(e)),
        };
        let Some(run) = blame
            .ranges
            .into_iter()
            .find(|r| r.start <= line && line < r.end)
        else {
            return Ok(Value::Null);
        };
        let origin = &run.origin;
        let view = self
            .api
            .get_changesets(
                &cx,
                GetChangesets {
                    repo: self.repo.clone(),
                    ids: vec![origin.id],
                },
            )
            .await
            .ok()
            .and_then(|c| c.found.into_iter().find(|v| v.changeset.id() == origin.id));

        let mut text = format!(
            "**`{}`** {}\n\n{} ({}) · {}",
            short(origin.id),
            escape(&clip(&origin.subject)),
            escape(&clip(&origin.author.name)),
            kind_name(origin.author.kind),
            time(origin.authored_at),
        );
        if let Some(view) = &view {
            let state = match view.state {
                DraftState::Landed => None,
                DraftState::Draft => Some("draft"),
                DraftState::Superseded => Some("superseded"),
                DraftState::Abandoned => Some("abandoned"),
            };
            if let Some(state) = state {
                text.push_str(&format!(" · {state}"));
            }
            let meta = &view.changeset.metadata;
            // New keys win; legacy names are read as aliases (D39).
            if let Some(item) = attribution::get(meta, attribution::Key::WorkItem) {
                text.push_str(&format!(" · work item {}", escape(&clip(item))));
            }
            if let Some(session) = attribution::get(meta, attribution::Key::Session) {
                text.push_str(&format!(" · session {}", escape(&clip(session))));
            }
            if let Some(model) = attribution::get(meta, attribution::Key::Model) {
                text.push_str(&format!(" · model {}", escape(&clip(model))));
            }
            if let Some(authors) = attribution::get(meta, attribution::Key::Coauthors) {
                let names = authors
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !names.is_empty() {
                    text.push_str(&format!(" · co-authors {}", escape(&clip(&names))));
                }
            }
        }
        if local && self.has_local_changes(&path) {
            text.push_str(&format!(
                "\n\n_This file has local changes: the lines are as of `{}`._",
                short(at)
            ));
        }
        Ok(json!({
            "contents": {"kind": "markdown", "value": text},
            "range": range(run.start, run.end),
        }))
    }

    /// Whether the working-copy file differs from the snapshot's.
    fn has_local_changes(&self, path: &RepoPath) -> bool {
        let Some(wc) = &self.workcopy else {
            return false;
        };
        let Some(entry) = wc.index().entries.get(path) else {
            return true;
        };
        let file = wc.root().join(path.as_str());
        let Ok(meta) = std::fs::symlink_metadata(&file) else {
            return true;
        };
        if !needs_rehash(&entry.stat, &Stat::of(&meta), wc.index().written_ns) {
            return false;
        }
        match std::fs::read(&file) {
            Ok(bytes) => BlobId::hash(&bytes) != entry.id,
            Err(_) => true,
        }
    }

    pub(crate) async fn code_lens(&mut self, params: &Value) -> Result<Value, RpcError> {
        self.refresh_workcopy();
        let uri = doc_uri(params).ok_or_else(|| RpcError::invalid_params("no textDocument"))?;
        let Some((at, path, local)) = self.locate(&uri).await? else {
            return Ok(json!([]));
        };
        let history = self
            .api
            .log(
                &self.cx(),
                Log {
                    repo: self.repo.clone(),
                    from: at,
                    until: None,
                    paths: vec![path],
                    page: Page {
                        limit: LOG_PAGE,
                        token: None,
                    },
                },
            )
            .await
            .map_err(api_error)?;
        let mut lenses = Vec::new();
        let title = match history.changesets.first() {
            None => format!("not in {}", short(at)),
            Some(last) => format!(
                "changed {} {}, last by {} ({}), {}",
                count(history.changesets.len(), history.next.is_some()),
                plural(history.changesets.len(), "time", "times"),
                clip(&last.author.name),
                kind_name(last.author.kind),
                time(last.authored_at)
            ),
        };
        lenses.push(lens(&title, "graft.blameFile", json!([{"uri": uri}])));
        if local && let Some((line, behind, more)) = self.behind().await? {
            let title = if behind == 0 {
                format!("base is up to date with {line}")
            } else {
                format!(
                    "base is {} {} behind {line}",
                    count(behind, more),
                    plural(behind, "changeset", "changesets")
                )
            };
            lenses.push(lens(&title, "graft.sync", json!([])));
        }
        Ok(Value::Array(lenses))
    }

    /// The working copy's line, how many changesets its tip is ahead of the
    /// base (at most [`LOG_PAGE`]), and whether there are more.
    async fn behind(&self) -> Result<Option<(String, usize, bool)>, RpcError> {
        let Some(wc) = &self.workcopy else {
            return Ok(None);
        };
        let (line, base) = (wc.state().line.clone(), wc.state().base);
        let tip = self.resolve(&line).await?;
        if tip == base {
            return Ok(Some((line, 0, false)));
        }
        let history = self
            .api
            .log(
                &self.cx(),
                Log {
                    repo: self.repo.clone(),
                    from: tip,
                    until: Some(base),
                    paths: Vec::new(),
                    page: Page {
                        limit: LOG_PAGE,
                        token: None,
                    },
                },
            )
            .await
            .map_err(api_error)?;
        Ok(Some((
            line,
            history.changesets.len(),
            history.next.is_some(),
        )))
    }

    /// Publishes diagnostics for `only`, or for every open document, every
    /// conflicting path, and every URI that had diagnostics before.
    pub(crate) async fn publish_all(&mut self, only: Option<&str>) {
        self.refresh_workcopy();
        let mut uris: BTreeSet<String> = match only {
            Some(uri) => BTreeSet::from([uri.to_owned()]),
            None => self.docs.union(&self.published).cloned().collect(),
        };
        if only.is_none()
            && let (Some(wc), Some(c)) = (&self.workcopy, &self.conflict)
        {
            uris.extend(
                c.paths
                    .iter()
                    .take(MAX_CONFLICT_URIS)
                    .map(|p| file_uri(&wc.root().join(p.as_str()))),
            );
        }
        let tip = match &self.workcopy {
            Some(wc) => {
                let line = wc.state().line.clone();
                match self.resolve(&line).await {
                    Ok(tip) => Some(tip),
                    Err(e) => {
                        self.log(&format!("cannot resolve {line}: {}", e.message));
                        None
                    }
                }
            }
            None => None,
        };
        for uri in uris {
            let diagnostics = match self.diagnostics(&uri, tip).await {
                Ok(d) => d,
                Err(e) => {
                    self.log(&format!("diagnostics for {uri}: {}", e.message));
                    continue;
                }
            };
            if diagnostics.is_empty() {
                if !self.published.remove(&uri) && only.is_none() {
                    continue;
                }
            } else {
                self.published.insert(uri.clone());
            }
            self.push_publish(&uri, diagnostics);
        }
    }

    /// The diagnostics of one document, given the line's tip.
    async fn diagnostics(
        &self,
        uri: &str,
        tip: Option<ChangesetId>,
    ) -> Result<Vec<Value>, RpcError> {
        let Target::Local(path) = self.target(uri) else {
            return Ok(Vec::new());
        };
        let wc = self.workcopy.as_ref().expect("local targets need one");
        if !wc.sparse().includes(&path) {
            return Ok(vec![diagnostic(
                (0, 1),
                WARNING,
                "outside-slice",
                &format!(
                    "{path} is outside the sparse slice: graft commit leaves it out. Add it with `graft sparse add`."
                ),
            )]);
        }
        let base = wc.state().base;
        let line = &wc.state().line;
        let (to, severity, code, message) = match &self.conflict {
            Some(c) if c.paths.contains(&path) => (
                c.onto,
                ERROR,
                "conflict",
                format!(
                    "conflicts with {} on {line}: changed there and in your stack",
                    short(c.onto)
                ),
            ),
            _ => match tip {
                Some(tip) if tip != base => (
                    tip,
                    INFORMATION,
                    "upstream",
                    format!(
                        "changed on {line} since your base (now at {}); graft sync brings it in",
                        short(tip)
                    ),
                ),
                _ => return Ok(Vec::new()),
            },
        };
        let changed = self.changed(base, to, &path).await.map_err(api_error)?;
        Ok(match changed {
            Changed::No => Vec::new(),
            Changed::Deleted => vec![diagnostic(
                (0, 1),
                severity,
                code,
                &format!("{message} (deleted there)"),
            )],
            Changed::Whole => vec![diagnostic((0, 1), severity, code, &message)],
            Changed::Regions(regions) => {
                let mut out: Vec<Value> = regions
                    .iter()
                    .take(MAX_REGIONS)
                    .map(|r| diagnostic(*r, severity, code, &message))
                    .collect();
                if regions.len() > MAX_REGIONS {
                    let more = regions.len() - MAX_REGIONS;
                    out[MAX_REGIONS - 1] = diagnostic(
                        regions[MAX_REGIONS - 1],
                        severity,
                        code,
                        &format!("{message} (and {more} more regions)"),
                    );
                }
                out
            }
        })
    }

    /// How `path` changed from `from` to `to`: one path-filtered diff with
    /// a capped patch.
    async fn changed(
        &self,
        from: ChangesetId,
        to: ChangesetId,
        path: &RepoPath,
    ) -> Result<Changed, ApiError> {
        let diff = self
            .api
            .diff(
                &self.cx(),
                Diff {
                    repo: self.repo.clone(),
                    from,
                    to,
                    paths: vec![path.clone()],
                    patch: Some(PatchOptions {
                        context: 0,
                        max_bytes_per_file: PATCH_BYTES,
                    }),
                    page: Page {
                        limit: 8,
                        token: None,
                    },
                },
            )
            .await?;
        let Some(file) = diff.files.into_iter().find(|f| f.change.path() == path) else {
            return Ok(Changed::No);
        };
        Ok(match (&file.change, &file.patch) {
            (Change::Delete { .. }, _) => Changed::Deleted,
            (_, Some(patch)) => {
                let regions = old_ranges(patch);
                if regions.is_empty() {
                    Changed::Whole
                } else {
                    Changed::Regions(regions)
                }
            }
            _ => Changed::Whole,
        })
    }
}

/// An API failure as a request failure. The message is the error's
/// display, which names ids and paths but never file contents or tokens.
pub(crate) fn api_error(e: ApiError) -> RpcError {
    let mut data = json!({"error": error_kind(&e)});
    if let ApiError::Conflict { paths, onto } = &e {
        data["onto"] = onto.to_string().into();
        data["paths"] = paths.iter().map(|p| p.path.clone()).collect();
    }
    if let ApiError::StaleBase(detail) = &e {
        data["current_tip"] = detail.current_tip.to_string().into();
        data["paths"] = detail.paths.clone().into();
        data["truncated"] = detail.truncated.into();
    }
    RpcError::failed(e.to_string()).with_data(data)
}

fn error_kind(e: &ApiError) -> &'static str {
    match e {
        ApiError::NotFound { .. } => "not_found",
        ApiError::Collected { .. } => "collected",
        ApiError::InvalidPath { .. } => "invalid_path",
        ApiError::InvalidChange { .. } => "invalid_change",
        ApiError::UnknownParent { .. } => "unknown_parent",
        ApiError::MissingObjects { .. } => "missing_objects",
        ApiError::RefConflict { .. } => "ref_conflict",
        ApiError::Conflict { .. } => "conflict",
        ApiError::StaleBase(_) => "stale_base",
        ApiError::Contended { .. } => "contended",
        ApiError::Rejected { .. } => "rejected",
        ApiError::PermissionDenied { .. } => "permission_denied",
        ApiError::LeaseExpired { .. } => "lease_expired",
        ApiError::LimitExceeded { .. } => "limit_exceeded",
        ApiError::CorruptObject { .. } => "corrupt_object",
        ApiError::Obliterated { .. } => "obliterated",
        ApiError::Unavailable { .. } => "unavailable",
        ApiError::Unauthenticated => "unauthenticated",
        ApiError::IncompatibleProtocol { .. } => "incompatible_protocol",
        _ => "other",
    }
}

/// Lines `start..end`, whole.
pub(crate) fn range(start: u32, end: u32) -> Value {
    json!({
        "start": {"line": start, "character": 0},
        "end": {"line": end, "character": 0},
    })
}

fn diagnostic((start, end): (u32, u32), severity: u8, code: &str, message: &str) -> Value {
    json!({
        "range": range(start, end),
        "severity": severity,
        "source": "graft",
        "code": code,
        "message": message,
    })
}

fn lens(title: &str, command: &str, arguments: Value) -> Value {
    json!({
        "range": range(0, 0),
        "command": {"title": title, "command": command, "arguments": arguments},
    })
}

fn count(n: usize, more: bool) -> String {
    if more { format!("{n}+") } else { n.to_string() }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// At most [`MAX_TEXT`] characters of one line.
fn clip(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    match line.char_indices().nth(MAX_TEXT) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_owned(),
    }
}

/// Escapes Markdown punctuation, so names and subjects render as text.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_clipped_and_escaped() {
        assert_eq!(clip("one\ntwo"), "one");
        let long = "x".repeat(MAX_TEXT + 5);
        assert_eq!(clip(&long).chars().count(), MAX_TEXT + 1);
        assert_eq!(escape("a *b* <c>"), "a \\*b\\* \\<c\\>");
    }

    #[test]
    fn counts_say_when_there_are_more() {
        assert_eq!(count(3, false), "3");
        assert_eq!(count(100, true), "100+");
    }
}
