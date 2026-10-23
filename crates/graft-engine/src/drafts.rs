//! `OverlappingDrafts`: which other live workspaces have open drafts on
//! these paths (`docs/notes/draft-overlap.md` §4, D24).
//!
//! The answer comes from the draft path index, which every write that moves
//! a workspace's stack keeps in the same transaction. A page scans at most
//! [`SCAN_FACTOR`] × `limit` index rows, in `(input prefix, path,
//! workspace)` order, plus one workspace read and one record read per
//! distinct workspace among them. Expired leases, the caller's own
//! workspaces and heads that are no longer drafts are filtered here, so a
//! page can come back short with a token. Nothing returned names a head,
//! a changeset or file contents.

use std::collections::HashMap;

use graft_model::RepoPath;
use graft_proto::types::{
    DraftHit, DraftOverlap, DraftWorkspace, MAX_OVERLAP_PAGE, MAX_OVERLAP_PATHS, OverlappingDrafts,
};
use graft_proto::{ApiError, CallContext};
use graft_store::{MetaStore, ObjectStore, RecordState, RepoId};

use crate::access::{Need, require};
use crate::workspace::holder;
use crate::{Engine, errors};

/// Index rows a page may scan, per hit it may return.
pub(crate) const SCAN_FACTOR: u32 = 4;

/// Where a page stopped: the input prefix, and the last row examined in it
/// (`None` to start that prefix from its beginning).
#[derive(Debug, PartialEq, Eq)]
struct Cursor {
    index: usize,
    after: Option<(RepoPath, String)>,
}

/// Sorted, each once, and none beneath another: a path under an earlier
/// prefix would only repeat its rows.
fn normalise(mut paths: Vec<RepoPath>) -> Vec<RepoPath> {
    paths.sort();
    let mut out: Vec<RepoPath> = Vec::with_capacity(paths.len());
    for p in paths {
        if !out.iter().any(|q| p.starts_with(q)) {
            out.push(p);
        }
    }
    out
}

/// A stable fingerprint of the inputs a token was issued for (FNV-1a, so it
/// does not change across builds or restarts).
fn fingerprint(line: &str, prefixes: &[RepoPath], include_own: bool) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes.iter().chain([&0u8]) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    eat(line.as_bytes());
    eat(&[u8::from(include_own)]);
    for p in prefixes {
        eat(p.as_str().as_bytes());
    }
    h
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `<fingerprint>.<index>[.<hex(path NUL workspace)>]`. Paths never hold
/// NUL.
fn encode(fp: u64, cursor: &Cursor) -> String {
    let mut out = format!("{fp:016x}.{}", cursor.index);
    if let Some((path, ws)) = &cursor.after {
        let mut raw = path.as_str().as_bytes().to_vec();
        raw.push(0);
        raw.extend_from_slice(ws.as_bytes());
        out.push('.');
        out.push_str(&hex(&raw));
    }
    out
}

fn decode(token: &str, fp: u64, prefixes: usize) -> Result<Cursor, ApiError> {
    let bad = || errors::invalid(None, "page-token");
    let mut parts = token.split('.');
    let issued = parts
        .next()
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .ok_or_else(bad)?;
    let index: usize = parts.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    if issued != fp || index >= prefixes {
        return Err(bad());
    }
    let after = match parts.next() {
        None => None,
        Some(s) => {
            let raw = unhex(s).ok_or_else(bad)?;
            let text = String::from_utf8(raw).map_err(|_| bad())?;
            let (path, ws) = text.split_once('\0').ok_or_else(bad)?;
            Some((RepoPath::new(path).map_err(|_| bad())?, ws.to_owned()))
        }
    };
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(Cursor { index, after })
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_overlapping_drafts(
        &self,
        cx: &CallContext,
        req: OverlappingDrafts,
    ) -> Result<DraftOverlap, ApiError> {
        require(cx, &req.repo, Need::Drafts)?;
        if req.paths.is_empty() {
            return Err(errors::invalid(None, "paths-required"));
        }
        if req.paths.len() > MAX_OVERLAP_PATHS {
            return Err(errors::limit("paths", MAX_OVERLAP_PATHS as u64));
        }
        let limit = match req.page.limit {
            0 => MAX_OVERLAP_PAGE,
            n if n > MAX_OVERLAP_PAGE => {
                return Err(errors::limit("limit", MAX_OVERLAP_PAGE.into()));
            }
            n => n,
        };
        let repo = self.repo_id(&req.repo).await?;
        self.line_ref(repo, &req.line).await?;
        let prefixes = normalise(req.paths);
        let fp = fingerprint(&req.line, &prefixes, req.include_own);
        let mut cursor = match &req.page.token {
            Some(t) => decode(t, fp, prefixes.len())?,
            None => Cursor {
                index: 0,
                after: None,
            },
        };

        let now = self.clock.now().ms;
        let mut budget = limit * SCAN_FACTOR;
        let mut seen: HashMap<String, Option<DraftWorkspace>> = HashMap::new();
        let mut out = DraftOverlap::default();
        while cursor.index < prefixes.len() {
            if budget == 0 || out.hits.len() == limit as usize {
                out.next = Some(encode(fp, &cursor));
                break;
            }
            let rows = self
                .meta
                .scan_draft_paths(
                    repo,
                    &req.line,
                    &prefixes[cursor.index],
                    cursor.after.clone(),
                    budget,
                )
                .await
                .map_err(errors::store)?;
            let exhausted = (rows.len() as u32) < budget;
            let mut stopped = false;
            for (path, ws) in rows {
                budget -= 1;
                if !seen.contains_key(&ws) {
                    let found = self.live_draft(repo, cx, &ws, req.include_own, now).await?;
                    seen.insert(ws.clone(), found);
                }
                if seen[&ws].is_some() {
                    out.hits.push(DraftHit {
                        path: path.clone(),
                        workspace: ws.clone(),
                    });
                }
                cursor.after = Some((path, ws));
                if out.hits.len() == limit as usize {
                    stopped = true;
                    break;
                }
            }
            if exhausted && !stopped {
                cursor = Cursor {
                    index: cursor.index + 1,
                    after: None,
                };
            }
        }
        // One entry per workspace named in `hits`, in first-hit order.
        for hit in &out.hits {
            if let Some(ws) = seen.get_mut(&hit.workspace).and_then(Option::take) {
                out.workspaces.push(ws);
            }
        }
        Ok(out)
    }

    /// The workspace `name` as an overlap entry, or `None` when it does not
    /// count: gone, lease lapsed, the caller's own (unless asked for), or
    /// with no draft head (an empty stack, or a head already landed).
    async fn live_draft(
        &self,
        repo: RepoId,
        cx: &CallContext,
        name: &str,
        include_own: bool,
        now: i64,
    ) -> Result<Option<DraftWorkspace>, ApiError> {
        let Some(ws) = self
            .meta
            .workspace(repo, name)
            .await
            .map_err(errors::store)?
        else {
            return Ok(None);
        };
        if ws.lease_until_ms <= now || (!include_own && ws.owner == cx.principal.subject) {
            return Ok(None);
        }
        let Some(head) = ws.head else {
            return Ok(None);
        };
        let Some((_, record)) = self
            .meta
            .changeset(repo, head)
            .await
            .map_err(errors::store)?
        else {
            return Ok(None);
        };
        if record.state != RecordState::Draft || record.holder != Some(holder(&ws.name)) {
            return Ok(None);
        }
        Ok(Some(DraftWorkspace {
            workspace: ws.name,
            owner: ws.owner,
            session: ws.session,
            work_item: ws.work_item,
            draft_age_ms: u64::try_from(now - record.received_at_ms).unwrap_or(0),
            lease_until_ms: ws.lease_until_ms,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).expect("valid path")
    }

    #[test]
    fn prefixes_are_sorted_deduped_and_not_nested() {
        let got = normalise(vec![p("src/a"), p("doc"), p("src"), p("src"), p("src-x")]);
        assert_eq!(got, vec![p("doc"), p("src"), p("src-x")]);
    }

    #[test]
    fn tokens_round_trip_and_bind_to_their_inputs() {
        let fp = fingerprint("main", &[p("src")], false);
        assert_ne!(fp, fingerprint("main", &[p("src")], true));
        assert_ne!(fp, fingerprint("dev", &[p("src")], false));
        for cursor in [
            Cursor {
                index: 0,
                after: None,
            },
            Cursor {
                index: 0,
                after: Some((p("src/a.rs"), "w/one".to_owned())),
            },
        ] {
            let token = encode(fp, &cursor);
            assert_eq!(decode(&token, fp, 1).expect("decodes"), cursor);
            assert!(decode(&token, fp ^ 1, 1).is_err());
        }
        assert!(
            decode(
                &encode(
                    fp,
                    &Cursor {
                        index: 1,
                        after: None
                    }
                ),
                fp,
                1
            )
            .is_err()
        );
        for junk in ["", "x", "0.0", "zz.0.00", &format!("{fp:016x}.0.0g")] {
            assert!(decode(junk, fp, 1).is_err(), "{junk}");
        }
    }
}
