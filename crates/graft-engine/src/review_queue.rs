//! `ReviewQueue` (v0.2, `docs/roadmap.md`): the landings on a line since an
//! LSN that have no `review` attestation, filtered by path and size. It is a
//! query, not a gate: it works on every line whatever its `checks` setting,
//! and nothing in the land path computes or reads anything for it.
//!
//! A land that moves a line writes only the store's review queue index row
//! ([`Write::PutLand`] with a position: position, LSN, changeset). A
//! `review` attestation takes the landing out of that index, so the query
//! pages it by LSN and never sees reviewed landings.
//!
//! A landing's summary (its first position on the line, the paths it
//! touches, the lines it changes) is computed when the queue is read, and
//! cached as a derived value. The land's old tip comes from its own
//! `RefMoved` entry, read at the row's LSN (a point read, not a scan); the
//! summary is then one id-pruned tree diff of the two snapshots and the
//! changed blobs. When those objects are missing or corrupt the item's
//! summary is unavailable ([`PATHS_UNAVAILABLE`], [`LINES_UNAVAILABLE`])
//! and the rest of the page is answered as usual.
//!
//! The path filter reads the line path index (is there a row for the prefix
//! at a position of this landing?), and needs no blob. A call reads at most
//! [`MAX_REVIEW_SCAN`] queue rows, the page included, and answers
//! `next_lsn` to resume, so its cost is the page plus the landings it
//! filtered out, never the line or the journal. No snapshot is materialized.
use graft_model::{Change, ChangesetId, EntryKind, ObjectId, ObjectKind, TreeId, diff_trees_after};
use graft_proto::types::{MAX_REVIEW_PAGE, ReviewItem, ReviewQueue, ReviewQueuePage};
use graft_proto::{ApiError, CallContext, Frame};
use graft_store::{LandingRow, MetaStore, MetaTxn, ObjectStore, RepoId, Write};
use imara_diff::{Algorithm, InternedInput};

use crate::access::{Need, require};
use crate::diff::textual;
use crate::journal::{LAND_ATTEMPT, REF_MOVED, ref_moved_old};
use crate::land::{KIND_VERSION, key};
use crate::refs::check_ref_name;
use crate::source::{Trees, empty_tree, model_root};
use crate::{Engine, errors};

/// `paths_touched` of an item whose summary is unavailable (its objects
/// are missing or corrupt).
pub const PATHS_UNAVAILABLE: u32 = u32::MAX;

/// `lines_changed` of an item whose summary is unavailable. A `max_lines`
/// filter leaves such an item out.
pub const LINES_UNAVAILABLE: u64 = u64::MAX;

/// Journal entries read per probe for a landing's `RefMoved` entry.
const JOURNAL_PROBE: u32 = 4;

/// The most journal entries read for one landing's `RefMoved` entry. Past
/// it the landing's summary is unavailable.
pub const MAX_JOURNAL_PROBE: u32 = 16;

/// The derived kind of a landing's summary, keyed by the landed changeset
/// and the line.
pub const LAND_SUMMARY: &str = "land-summary";

/// The most review queue rows one `ReviewQueue` call reads, matching or
/// filtered out. Past it the call answers what it found with `next_lsn`.
pub const MAX_REVIEW_SCAN: u32 = 4 * MAX_REVIEW_PAGE;

/// Changes read per page of the tree diff a summary counts.
const SUMMARY_DIFF_PAGE: usize = 1_000;

/// Queue rows read per store call when a filter may drop some.
const FILTERED_BATCH: usize = 64;

/// What a landing changed, computed when the queue is first read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LandSummary {
    /// The landing's first position on the line (a stack lands several).
    pub first_pos: u64,
    /// Paths it adds, changes or deletes.
    pub paths: u32,
    /// Lines it adds or removes. Exact for text files within the patch
    /// input cap; a larger text side counts its size in bytes (an upper
    /// bound), and a binary side counts nothing.
    pub lines: u64,
}

impl LandSummary {
    /// `first_pos`, `paths` and `lines`, little-endian (20 bytes).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20);
        out.extend_from_slice(&self.first_pos.to_le_bytes());
        out.extend_from_slice(&self.paths.to_le_bytes());
        out.extend_from_slice(&self.lines.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 20 {
            return None;
        }
        let (pos, rest) = bytes.split_at(8);
        let (paths, lines) = rest.split_at(4);
        Some(Self {
            first_pos: u64::from_le_bytes(pos.try_into().ok()?),
            paths: u32::from_le_bytes(paths.try_into().ok()?),
            lines: u64::from_le_bytes(lines.try_into().ok()?),
        })
    }
}

/// One side of a changed file, as far as counting its lines needs.
enum Side {
    Absent,
    Text(String),
    /// Text kind over the patch input cap: its size in bytes.
    Large(u64),
    Binary,
}

impl Side {
    /// The lines this side contributes when the other cannot be compared
    /// with it.
    fn alone(&self) -> u64 {
        match self {
            Side::Text(s) => s.lines().count() as u64,
            Side::Large(bytes) => *bytes,
            Side::Absent | Side::Binary => 0,
        }
    }
}

/// Lines added plus lines removed between two sides.
fn changed_lines(old: &Side, new: &Side) -> u64 {
    fn text(s: &Side) -> Option<&str> {
        match s {
            Side::Absent => Some(""),
            Side::Text(t) => Some(t.as_str()),
            Side::Large(_) | Side::Binary => None,
        }
    }
    match (text(old), text(new)) {
        (Some(a), Some(b)) => {
            let input = InternedInput::new(a, b);
            let diff = imara_diff::Diff::compute(Algorithm::Histogram, &input);
            u64::from(diff.count_additions()) + u64::from(diff.count_removals())
        }
        _ => old.alone().saturating_add(new.alone()),
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    async fn summarize(
        &self,
        repo: RepoId,
        from: TreeId,
        to: TreeId,
        first_pos: u64,
    ) -> Result<LandSummary, ApiError> {
        let trees = Trees::new(&*self.objects, repo);
        let mut summary = LandSummary {
            first_pos,
            paths: 0,
            lines: 0,
        };
        let mut after = None;
        loop {
            let page = diff_trees_after(
                &trees,
                model_root(from),
                model_root(to),
                after.as_ref(),
                SUMMARY_DIFF_PAGE,
            )
            .await
            .map_err(errors::source)?;
            for change in &page.changes {
                // The sentinels stay out of reach of a real count.
                summary.paths = summary.paths.saturating_add(1).min(PATHS_UNAVAILABLE - 1);
                let old = trees
                    .lookup(from, change.path())
                    .await
                    .map_err(errors::source)?
                    .filter(|e| !e.kind.is_tree())
                    .map(|e| (e.kind, e.id));
                let new = match change {
                    Change::Put { kind, id, .. } => Some((*kind, *id)),
                    Change::Delete { .. } => None,
                };
                let old = self.side(repo, old).await?;
                let new = self.side(repo, new).await?;
                summary.lines = summary
                    .lines
                    .saturating_add(changed_lines(&old, &new))
                    .min(LINES_UNAVAILABLE - 1);
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(summary),
            }
        }
    }

    async fn side(
        &self,
        repo: RepoId,
        side: Option<(EntryKind, ObjectId)>,
    ) -> Result<Side, ApiError> {
        let Some((kind, id)) = side else {
            return Ok(Side::Absent);
        };
        if !textual(kind) {
            return Ok(Side::Binary);
        }
        let raw = self
            .objects
            .get(repo, id)
            .await
            .map_err(errors::store)?
            .ok_or(ApiError::MissingObjects { ids: vec![id] })?;
        let frame = Frame::from_bytes(&raw)
            .ok()
            .filter(|f| f.kind == ObjectKind::Blob)
            .ok_or(ApiError::CorruptObject { id })?;
        if frame.uncompressed_len > self.config.max_patch_input_bytes {
            return Ok(Side::Large(frame.uncompressed_len));
        }
        let payload = frame
            .decode_verified(id)
            .map_err(|_| ApiError::CorruptObject { id })?;
        Ok(String::from_utf8(payload)
            .ok()
            .filter(|s| !s.contains('\0'))
            .map_or(Side::Binary, Side::Text))
    }

    /// The landing's cached summary, if a read already computed it.
    async fn cached_summary(
        &self,
        repo: RepoId,
        line: &str,
        row: &LandingRow,
    ) -> Result<Option<LandSummary>, ApiError> {
        let stored = self
            .meta
            .derived(repo, LAND_SUMMARY, KIND_VERSION, &key(row.changeset, line))
            .await
            .map_err(errors::store)?;
        Ok(stored.as_deref().and_then(LandSummary::decode))
    }

    /// Where the landing starts: the line's tip before it, from the land's
    /// `RefMoved` entry at the row's LSN, and its first position. The entry
    /// is the first of the land's transaction, after only the
    /// `LandReverted` entries of a revert land, so the probe stops at the
    /// transaction's `LandAttempt` or the journal's end, and the landing is
    /// then taken as its own changeset alone. It reads at most
    /// [`MAX_JOURNAL_PROBE`] entries; `None` when it read that many without
    /// finding either, and the summary is unavailable.
    async fn landing_start(
        &self,
        repo: RepoId,
        line: &str,
        row: &LandingRow,
    ) -> Result<Option<(Option<ChangesetId>, u64)>, ApiError> {
        let mut from = row.lsn;
        let mut read = 0;
        while read < MAX_JOURNAL_PROBE {
            let want = JOURNAL_PROBE.min(MAX_JOURNAL_PROBE - read);
            let entries = self
                .meta
                .journal(repo, from, want)
                .await
                .map_err(errors::store)?;
            let short = entries.len() < want as usize;
            for (lsn, kind, body) in entries {
                read += 1;
                from = lsn + 1;
                if kind == REF_MOVED
                    && let Some(old) = ref_moved_old(&body, line, row.changeset)
                {
                    let below = self
                        .meta
                        .line_pos(repo, line, old)
                        .await
                        .map_err(errors::store)?;
                    let first = below.map_or(row.pos, |p| (p + 1).min(row.pos));
                    return Ok(Some((Some(old), first)));
                }
                if kind == LAND_ATTEMPT {
                    return Ok(Some((None, row.pos)));
                }
            }
            if short {
                return Ok(Some((None, row.pos)));
            }
        }
        Ok(None)
    }

    /// The landing's summary from `old` (its first parent when unknown) to
    /// what landed, cached for the next read; `None` when its objects are
    /// missing or corrupt.
    async fn compute_summary(
        &self,
        repo: RepoId,
        line: &str,
        row: &LandingRow,
        old: Option<ChangesetId>,
        first_pos: u64,
    ) -> Result<Option<LandSummary>, ApiError> {
        let got = async {
            let (cs, _) = self.changeset(repo, row.changeset).await?;
            let from = match old.or(cs.first_parent()) {
                Some(p) => self.root_of(repo, p).await?,
                None => empty_tree(),
            };
            self.summarize(repo, from, cs.root, first_pos).await
        }
        .await;
        let summary = match got {
            Ok(s) => s,
            Err(ApiError::MissingObjects { .. } | ApiError::CorruptObject { .. }) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        // A cache: derived data, recomputable, so a failed write only
        // means the next read computes it again.
        let txn = MetaTxn {
            preconditions: Vec::new(),
            writes: vec![Write::PutDerived {
                kind: LAND_SUMMARY.to_owned(),
                version: KIND_VERSION,
                key: key(row.changeset, line),
                value: summary.encode(),
            }],
        };
        let _ = self.meta.commit(repo, txn).await;
        Ok(Some(summary))
    }

    /// Whether the landing touched `prefix`: the line path index's newest
    /// row for it at or below the landing's last position lies within the
    /// landing. One indexed read.
    async fn touches(
        &self,
        repo: RepoId,
        line: &str,
        prefix: &graft_model::RepoPath,
        first_pos: u64,
        pos: u64,
    ) -> Result<bool, ApiError> {
        let rows = self
            .meta
            .scan_line_path(repo, line, prefix, pos.saturating_add(1), 1)
            .await
            .map_err(errors::store)?;
        Ok(rows.first().is_some_and(|(p, _)| *p >= first_pos))
    }

    /// `ReviewQueue`: the line's landings after `since_lsn` with no `review`
    /// attestation, in LSN order, filtered by path prefix and size.
    pub(crate) async fn do_review_queue(
        &self,
        cx: &CallContext,
        req: ReviewQueue,
    ) -> Result<ReviewQueuePage, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        check_ref_name(&req.line)?;
        if req.page.token.is_some() {
            // The queue resumes from `next_lsn`, passed as `since_lsn`.
            return Err(errors::invalid(None, "page-token"));
        }
        let limit = self.review_page(req.page.limit) as usize;
        let filtered = req.path_prefix.is_some() || req.max_lines.is_some();
        let repo = self.repo_id(&req.repo).await?;
        let line = req.line.as_str();

        let mut items = Vec::new();
        let mut since = req.since_lsn;
        let mut scanned = 0usize;
        let cap = MAX_REVIEW_SCAN as usize;
        loop {
            let want = limit - items.len();
            let batch = if filtered {
                want.max(FILTERED_BATCH)
            } else {
                want
            }
            .min(cap - scanned);
            let rows = self
                .meta
                .review_queue(repo, line, since, batch as u32)
                .await
                .map_err(errors::store)?;
            let caught_up = rows.len() < batch;
            for row in rows {
                scanned += 1;
                since = row.lsn;
                let mut summary = self.cached_summary(repo, line, &row).await?;
                // Without a start (the probe hit its cap) the path filter
                // takes the landing's last position alone and the summary
                // is unavailable.
                let start = match summary {
                    Some(s) => Some((None, s.first_pos)),
                    None => self.landing_start(repo, line, &row).await?,
                };
                let (old, first_pos) = start.unwrap_or((None, row.pos));
                if let Some(prefix) = &req.path_prefix
                    && !self.touches(repo, line, prefix, first_pos, row.pos).await?
                {
                    continue;
                }
                if summary.is_none() && start.is_some() {
                    summary = self
                        .compute_summary(repo, line, &row, old, first_pos)
                        .await?;
                }
                if let Some(max) = req.max_lines
                    && !summary.is_some_and(|s| s.lines <= u64::from(max))
                {
                    continue;
                }
                let (cs, _) = self.changeset(repo, row.changeset).await?;
                items.push(ReviewItem {
                    changeset: row.changeset,
                    pos: row.pos,
                    lsn: row.lsn,
                    author: cs.author,
                    paths_touched: summary.map_or(PATHS_UNAVAILABLE, |s| s.paths),
                    lines_changed: summary.map_or(LINES_UNAVAILABLE, |s| s.lines),
                });
                if items.len() == limit {
                    return Ok(ReviewQueuePage {
                        items,
                        next_lsn: Some(since),
                    });
                }
            }
            if caught_up {
                return Ok(ReviewQueuePage {
                    items,
                    next_lsn: None,
                });
            }
            if scanned >= cap {
                return Ok(ReviewQueuePage {
                    items,
                    next_lsn: Some(since),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Derived data is recomputable, but its encoding is still fixed by a
    // vector so a reader of an older value decodes what was written.
    #[test]
    fn summary_encoding_vector() {
        let s = LandSummary {
            first_pos: 0x0102,
            paths: 3,
            lines: 0x0405,
        };
        let bytes = s.encode();
        assert_eq!(
            bytes,
            [
                0x02, 0x01, 0, 0, 0, 0, 0, 0, //
                3, 0, 0, 0, //
                0x05, 0x04, 0, 0, 0, 0, 0, 0,
            ]
        );
        assert_eq!(LandSummary::decode(&bytes), Some(s));
        assert_eq!(LandSummary::decode(&bytes[..19]), None);
    }

    #[test]
    fn changed_lines_table() {
        let t = |s: &str| Side::Text(s.to_owned());
        let cases = [
            (Side::Absent, t("a\nb\n"), 2),
            (t("a\nb\n"), Side::Absent, 2),
            (t("a\nb\nc\n"), t("a\nB\nc\n"), 2),
            (t("a\n"), t("a\n"), 0),
            (Side::Binary, Side::Binary, 0),
            (Side::Binary, t("x\ny\n"), 2),
            (Side::Large(100), t("x\n"), 101),
            (Side::Absent, Side::Large(7), 7),
        ];
        for (old, new, want) in cases {
            assert_eq!(changed_lines(&old, &new), want);
        }
    }
}
