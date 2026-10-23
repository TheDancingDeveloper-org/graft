//! Conflicts a sync writes into the working copy (`docs/design.md` §8.2).
//!
//! When the server refuses to rebase the stack onto the line tip
//! (`Conflict`), `sync` still moves the working copy there: the workspace
//! goes to the tip with no drafts, the files to the tip's snapshot, and then
//! the stack's net changes go on top as uncommitted edits. A path both sides
//! changed is written as follows:
//!
//! - text on all three sides: the three-way merge (`graft_model::merge3`).
//!   When it is clean the merged file is written ([`ConflictMode::Merged`]);
//!   otherwise the file carries conflict markers ([`ConflictMode::Markers`],
//!   `graft_model::render_conflict_markers`);
//! - anything else (a binary, a symlink, a side that deleted the file, a
//!   kind change): the file holds our side, or the tip's when we deleted it,
//!   and a `<path>.graft-conflict` sidecar beside it names the three
//!   versions ([`ConflictMode::Sidecar`]).
//!
//! `.graft/conflict` records the move (`onto`, the old base and head) and,
//! once they are written, the files. `commit` refuses while a sidecar is
//! still there or a marked file still has a marker line; the commit that
//! succeeds is the stack, now one draft on the tip, and it clears the
//! record. Writing is deterministic, so a sync interrupted while writing
//! writes the same files again when it is finished.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{ErrorKind, Read};

use graft_model::merge::{Merge3, merge3, render_conflict_markers};
use graft_model::{Change, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{Diff, Page};
use graft_proto::{ApiError, CallContext};
use graft_store::ObjectStore;

use super::{
    CONFLICT_SUFFIX, DIFF_PAGE, GRAFT_DIR, Moving, WorkingCopy, kind_name, remove_entry,
    write_atomic, write_entry,
};
use crate::fetch::{FetchOptions, fetch_objects, load_blob, over_budget};
use crate::sparse::{escape, unescape};
use crate::{Error, Result};

/// The conflict record in `.graft/`.
const CONFLICT_FILE: &str = "conflict";
const CONFLICT_HEADER: &str = "graft-conflict 1";

/// The most paths one `Diff` names (the engine's default `max_diff_paths`).
const DIFF_PATHS: usize = 64;

/// The largest marked file `commit` reads to look for markers: the merge's
/// own text limit, plus room for the markers it added.
const MAX_MARKED_BYTES: u64 = 4 * graft_model::merge::MAX_TEXT_BYTES as u64;

/// How a conflicting file was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictMode {
    /// The file carries conflict markers.
    Markers,
    /// The file holds one side, and a sidecar names the others.
    Sidecar,
    /// The three-way merge was clean: the file holds the merge. Nothing to
    /// resolve, but worth a look.
    Merged,
    /// Nothing could be written: the tip has a file where the path needs a
    /// directory. Both versions stay readable with `graft cat`.
    Skipped,
}

impl ConflictMode {
    /// The mode's name, as `.graft/conflict` and the CLI's JSON spell it.
    pub fn name(self) -> &'static str {
        match self {
            ConflictMode::Markers => "markers",
            ConflictMode::Sidecar => "sidecar",
            ConflictMode::Merged => "merged",
            ConflictMode::Skipped => "skipped",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "markers" => ConflictMode::Markers,
            "sidecar" => ConflictMode::Sidecar,
            "merged" => ConflictMode::Merged,
            "skipped" => ConflictMode::Skipped,
            _ => return None,
        })
    }
}

/// One conflicting file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConflictFile {
    /// The path.
    pub path: RepoPath,
    /// How it was written.
    pub mode: ConflictMode,
}

/// The conflicts a sync wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflicts {
    /// The tip the working copy moved to.
    pub onto: ChangesetId,
    /// The base the stack was on.
    pub base: ChangesetId,
    /// The stack's head: "ours".
    pub head: ChangesetId,
    /// The files both sides changed, sorted by path.
    pub files: Vec<ConflictFile>,
    /// Whether the files were written.
    pub(crate) applied: bool,
}

impl Conflicts {
    /// The files that still need a decision (all but the clean merges and
    /// the skipped ones).
    pub fn unresolved(&self) -> impl Iterator<Item = &ConflictFile> {
        self.files
            .iter()
            .filter(|f| matches!(f.mode, ConflictMode::Markers | ConflictMode::Sidecar))
    }
}

/// The sidecar path of `path`.
pub fn sidecar_path(path: &RepoPath) -> Option<RepoPath> {
    RepoPath::new(&format!("{}{CONFLICT_SUFFIX}", path.as_str())).ok()
}

/// One side of a conflicting path: its kind and blob, or nothing.
type Side = Option<(EntryKind, ObjectId)>;

fn side(change: &Change) -> Side {
    match change {
        Change::Put { kind, id, .. } => Some((*kind, *id)),
        Change::Delete { .. } => None,
    }
}

fn is_text_kind(side: Side) -> bool {
    matches!(side, Some((EntryKind::File | EntryKind::Exec, _)))
}

impl WorkingCopy {
    fn conflict_path(&self) -> std::path::PathBuf {
        self.root.join(GRAFT_DIR).join(CONFLICT_FILE)
    }

    /// Whether a sync left conflicts that are not committed yet.
    pub fn has_conflicts(&self) -> bool {
        self.conflict_path().exists()
    }

    /// The conflicts a sync left, until they are committed.
    pub fn conflicts(&self) -> Result<Option<Conflicts>> {
        self.load_conflicts()
    }

    pub(crate) fn load_conflicts(&self) -> Result<Option<Conflicts>> {
        match fs::read_to_string(self.conflict_path()) {
            Ok(text) => parse_record(&text).map(Some),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save_conflicts(&self, c: &Conflicts) -> Result<()> {
        write_atomic(&self.conflict_path(), record_text(c).as_bytes())?;
        Ok(())
    }

    /// Forgets the conflicts (once they are committed).
    pub(crate) fn drop_conflicts(&self) -> Result<()> {
        match fs::remove_file(self.conflict_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// `sync` after the server refused to rebase the stack onto `onto`:
    /// moves the workspace and the files to `onto`, then writes the stack's
    /// changes on top. The working copy must be clean, so nothing of the
    /// author's is lost: their work is in the drafts until it is in the
    /// files.
    pub(crate) async fn sync_conflicting<A, S>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        onto: ChangesetId,
        lease_ms: u64,
        opts: &FetchOptions,
    ) -> Result<Conflicts>
    where
        A: ObjectApi + RepoApi + WorkspaceApi,
        S: ObjectStore,
    {
        let head = self.state.head.ok_or(Error::NoDraft)?;
        let base = self.state.base;
        let dirty: Vec<RepoPath> = self.status()?.iter().map(|s| s.path().clone()).collect();
        if !dirty.is_empty() {
            return Err(Error::Dirty(dirty));
        }
        // The stack's changes must all be in the slice: they come back as
        // edits to the files, and a commit lists only the slice.
        self.ours(api, cx, base, head, opts).await?;
        let plan = self.plan_move(api, cx, onto, opts).await?;
        self.save_conflicts(&Conflicts {
            onto,
            base,
            head,
            files: Vec::new(),
            applied: false,
        })?;
        let to = Moving::to(onto, None);
        let from = Some((self.state.base, self.state.head));
        let moved = self
            .complete_move(api, cache, cx, to, from, Some(plan), lease_ms, opts)
            .await?;
        moved
            .conflicts
            .ok_or_else(|| Error::State("the conflicts were not written".to_owned()))
    }

    /// The stack's net changes, refusing any outside the slice.
    async fn ours<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        base: ChangesetId,
        head: ChangesetId,
        opts: &FetchOptions,
    ) -> Result<Vec<Change>> {
        let ours = self.diff_all(api, cx, base, head, Vec::new(), opts).await?;
        if let Some(c) = ours.iter().find(|c| !self.sparse.includes(c.path())) {
            return Err(Error::State(format!(
                "the stack changes {}, outside the slice: add it with `graft sparse add`",
                c.path()
            )));
        }
        Ok(ours)
    }

    /// Every change from `from` to `to` under `paths` (all paths when
    /// empty), within the fetch budget.
    async fn diff_all<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        from: ChangesetId,
        to: ChangesetId,
        paths: Vec<RepoPath>,
        opts: &FetchOptions,
    ) -> Result<Vec<Change>> {
        let mut out = Vec::new();
        let mut token = None;
        let mut pages = 0u64;
        loop {
            let page = api
                .diff(
                    cx,
                    Diff {
                        repo: self.config.repo.clone(),
                        from,
                        to,
                        paths: paths.clone(),
                        patch: None,
                        page: Page {
                            limit: DIFF_PAGE,
                            token,
                        },
                    },
                )
                .await?;
            out.extend(page.files.into_iter().map(|f| f.change));
            pages += 1;
            let max = opts.budget.max_entries;
            if out.len() as u64 > max || pages > max {
                return Err(over_budget("max_entries", max));
            }
            match page.next {
                Some(next) => token = Some(next),
                None => return Ok(out),
            }
        }
    }

    /// Writes the stack's changes on top of the tip's files (the working
    /// copy is on `record.onto` by now) and records what it wrote.
    pub(crate) async fn write_conflicts<A, S>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        mut record: Conflicts,
        opts: &FetchOptions,
    ) -> Result<Conflicts>
    where
        A: ObjectApi + RepoApi,
        S: ObjectStore,
    {
        let ours = self.ours(api, cx, record.base, record.head, opts).await?;
        // What the tip did to the same paths, asked for in chunks.
        let mut theirs: BTreeMap<RepoPath, Change> = BTreeMap::new();
        let wanted: HashSet<&RepoPath> = ours.iter().map(Change::path).collect();
        for chunk in ours.chunks(DIFF_PATHS) {
            let paths = chunk.iter().map(|c| c.path().clone()).collect();
            for c in self
                .diff_all(api, cx, record.base, record.onto, paths, opts)
                .await?
            {
                if wanted.contains(c.path()) {
                    theirs.insert(c.path().clone(), c);
                }
            }
        }

        // The base side of the paths both changed: our change undone, which
        // is the diff from our head back to the base.
        let both: Vec<&Change> = ours
            .iter()
            .filter(|c| theirs.get(c.path()).is_some_and(|t| side(t) != side(c)))
            .collect();
        let mut base: BTreeMap<RepoPath, Side> = BTreeMap::new();
        for chunk in both.chunks(DIFF_PATHS) {
            let paths = chunk.iter().map(|c| c.path().clone()).collect();
            for c in self
                .diff_all(api, cx, record.head, record.base, paths, opts)
                .await?
            {
                base.insert(c.path().clone(), side(&c));
            }
        }

        // Decide every path, and gather the blobs that takes.
        enum Plan {
            Ours(Side),
            Conflict {
                base: Side,
                ours: Side,
                theirs: Side,
            },
        }
        let mut plans: Vec<(RepoPath, Plan)> = Vec::new();
        let mut blobs: Vec<ObjectId> = Vec::new();
        for c in &ours {
            let path = c.path().clone();
            let o = side(c);
            match theirs.get(&path) {
                None => {
                    blobs.extend(o.map(|(_, id)| id));
                    plans.push((path, Plan::Ours(o)));
                }
                Some(t) if side(t) == o => {}
                Some(t) => {
                    let t = side(t);
                    let b = base.get(&path).copied().flatten();
                    blobs.extend([b, o, t].into_iter().flatten().map(|(_, id)| id));
                    plans.push((
                        path,
                        Plan::Conflict {
                            base: b,
                            ours: o,
                            theirs: t,
                        },
                    ));
                }
            }
        }
        let mut seen = HashSet::new();
        let ids: Vec<(ObjectId, ObjectKind)> = blobs
            .into_iter()
            .filter(|id| seen.insert(*id))
            .map(|id| (id, ObjectKind::Blob))
            .collect();
        fetch_objects(api, cache, cx, &self.config.repo, &ids, opts).await?;

        let mut files = Vec::new();
        for (path, plan) in plans {
            match plan {
                Plan::Ours(o) => {
                    // A clash with the tip's files is reported, not fatal.
                    if !self.put_side(cache, &path, o).await? {
                        files.push(ConflictFile {
                            path,
                            mode: ConflictMode::Skipped,
                        });
                    }
                }
                Plan::Conflict { base, ours, theirs } => {
                    let mode = self
                        .write_conflict(cache, &record, &path, base, ours, theirs)
                        .await?;
                    files.push(ConflictFile { path, mode });
                }
            }
        }
        files.sort();
        record.files = files;
        record.applied = true;
        self.save_conflicts(&record)?;
        Ok(record)
    }

    /// Writes one side's entry at `path` (or removes it). False when the
    /// path cannot be written because a parent is a file.
    async fn put_side<S: ObjectStore>(&self, cache: &S, path: &RepoPath, s: Side) -> Result<bool> {
        let written = match s {
            None => remove_entry(&self.root, path),
            Some((EntryKind::Submodule, _)) => Ok(()),
            Some((kind, id)) => {
                let data = self.blob(cache, id).await?;
                write_entry(&self.root, path, kind, &data).map(|_| ())
            }
        };
        match written {
            Ok(()) => Ok(true),
            Err(Error::UnsafePath(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn blob<S: ObjectStore>(&self, cache: &S, id: ObjectId) -> Result<Vec<u8>> {
        load_blob(cache, id)
            .await?
            .ok_or(Error::Api(ApiError::MissingObjects { ids: vec![id] }))
    }

    /// Writes one path both sides changed, and says how.
    async fn write_conflict<S: ObjectStore>(
        &self,
        cache: &S,
        record: &Conflicts,
        path: &RepoPath,
        base: Side,
        ours: Side,
        theirs: Side,
    ) -> Result<ConflictMode> {
        let textual =
            is_text_kind(ours) && is_text_kind(theirs) && (base.is_none() || is_text_kind(base));
        if let (true, Some((kind, o)), Some((_, t))) = (textual, ours, theirs) {
            let b = match base {
                Some((_, id)) => self.blob(cache, id).await?,
                None => Vec::new(),
            };
            let (o, t) = (self.blob(cache, o).await?, self.blob(cache, t).await?);
            let (mode, data) = match merge3(&b, &o, &t) {
                Merge3::Clean(merged) => (ConflictMode::Merged, merged),
                Merge3::NotText => (ConflictMode::Sidecar, Vec::new()),
                Merge3::Conflicts(_) | Merge3::TooComplex => {
                    (ConflictMode::Markers, render_conflict_markers(&b, &o, &t))
                }
            };
            if mode != ConflictMode::Sidecar {
                return Ok(match write_entry(&self.root, path, kind, &data) {
                    Ok(_) => mode,
                    Err(Error::UnsafePath(_)) => ConflictMode::Skipped,
                    Err(e) => return Err(e),
                });
            }
        }
        // Keep our side; when we deleted the file, the tip's stays.
        if ours.is_some() && !self.put_side(cache, path, ours).await? {
            return Ok(ConflictMode::Skipped);
        }
        let Some(sidecar) = sidecar_path(path) else {
            return Ok(ConflictMode::Skipped);
        };
        let text = sidecar_text(record, path, base, ours, theirs);
        Ok(
            match write_entry(&self.root, &sidecar, EntryKind::File, text.as_bytes()) {
                Ok(_) => ConflictMode::Sidecar,
                Err(Error::UnsafePath(_)) => ConflictMode::Skipped,
                Err(e) => return Err(e),
            },
        )
    }

    /// Refuses a commit while a conflict is unresolved: a sidecar is still
    /// there, or a marked file still has a marker line.
    pub(crate) fn check_resolved(&self, record: &Conflicts) -> Result<()> {
        let mut open = Vec::new();
        for f in record.unresolved() {
            let unresolved = match f.mode {
                ConflictMode::Sidecar => sidecar_path(&f.path)
                    .is_some_and(|s| fs::symlink_metadata(self.root.join(s.as_str())).is_ok()),
                _ => has_markers(&self.root.join(f.path.as_str()))?,
            };
            if unresolved {
                open.push(f.path.clone());
            }
        }
        if open.is_empty() {
            Ok(())
        } else {
            Err(Error::Unresolved(open))
        }
    }
}

/// Whether a regular file still has a line that opens or closes a
/// conflict. Symlinks are not followed, and an absent file has none.
fn has_markers(file: &std::path::Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(file) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut data = Vec::new();
    fs::File::open(file)?
        .take(meta.len().min(MAX_MARKED_BYTES))
        .read_to_end(&mut data)?;
    Ok(data.split(|b| *b == b'\n').any(|line| {
        line.starts_with(b"<<<<<<< ") || line.starts_with(b">>>>>>> ") || line == b"<<<<<<<"
    }))
}

fn side_text(s: Side, absent: &str) -> String {
    match s {
        Some((kind, id)) => format!("{} {}", kind_name(kind), id.to_hex()),
        None => absent.to_owned(),
    }
}

/// The sidecar beside a file that could not carry markers.
fn sidecar_text(
    record: &Conflicts,
    path: &RepoPath,
    base: Side,
    ours: Side,
    theirs: Side,
) -> String {
    let shown = escape(path.as_str());
    let holds = if ours.is_some() { "ours" } else { "theirs" };
    format!(
        "graft: {shown} conflicts and cannot carry conflict markers.\n\
         base    {}  (changeset {})\n\
         ours    {}  (draft {})\n\
         theirs  {}  (changeset {})\n\
         The file holds {holds}. Read each side with `graft cat {shown} --at <changeset>`.\n\
         Put the content you want in the file (or delete it), delete this sidecar,\n\
         then run `graft commit` and `graft land`.\n",
        side_text(base, "none"),
        record.base,
        side_text(ours, "deleted"),
        record.head,
        side_text(theirs, "deleted"),
        record.onto,
    )
}

fn record_text(c: &Conflicts) -> String {
    let mut s = format!(
        "{CONFLICT_HEADER}\nonto {}\nbase {}\nhead {}\n",
        c.onto, c.base, c.head
    );
    if c.applied {
        s.push_str("applied\n");
    }
    for f in &c.files {
        s.push_str(&format!("{} {}\n", f.mode.name(), escape(f.path.as_str())));
    }
    s
}

fn parse_record(text: &str) -> Result<Conflicts> {
    let bad = || Error::State("malformed conflict record".to_owned());
    let mut lines = text.lines();
    if lines.next() != Some(CONFLICT_HEADER) {
        return Err(bad());
    }
    let mut id = |key: &str| -> Result<ChangesetId> {
        let line = lines.next().ok_or_else(bad)?;
        let value = line
            .strip_prefix(key)
            .and_then(|v| v.strip_prefix(' '))
            .ok_or_else(bad)?;
        ChangesetId::parse(value).map_err(|_| bad())
    };
    let (onto, base, head) = (id("onto")?, id("base")?, id("head")?);
    let mut applied = false;
    let mut files = Vec::new();
    for line in lines {
        if line == "applied" {
            applied = true;
            continue;
        }
        let (mode, path) = line.split_once(' ').ok_or_else(bad)?;
        let mode = ConflictMode::from_name(mode).ok_or_else(bad)?;
        let path = unescape(path)
            .and_then(|p| RepoPath::new(&p).ok())
            .ok_or_else(bad)?;
        files.push(ConflictFile { path, mode });
    }
    Ok(Conflicts {
        onto,
        base,
        head,
        files,
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let c = Conflicts {
            onto: ChangesetId::hash(b"onto"),
            base: ChangesetId::hash(b"base"),
            head: ChangesetId::hash(b"head"),
            files: vec![
                ConflictFile {
                    path: RepoPath::new("a/odd\nname").unwrap(),
                    mode: ConflictMode::Markers,
                },
                ConflictFile {
                    path: RepoPath::new("b.bin").unwrap(),
                    mode: ConflictMode::Sidecar,
                },
            ],
            applied: true,
        };
        assert_eq!(parse_record(&record_text(&c)).unwrap(), c);
        let pending = Conflicts {
            files: Vec::new(),
            applied: false,
            ..c
        };
        assert_eq!(parse_record(&record_text(&pending)).unwrap(), pending);
        assert!(parse_record("graft-conflict 1\nonto x\n").is_err());
    }

    #[test]
    fn markers_are_found_only_at_line_starts() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        fs::write(&f, "a\n<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n").unwrap();
        assert!(has_markers(&f).unwrap());
        fs::write(&f, "a\nx <<<<<<< ours\n").unwrap();
        assert!(!has_markers(&f).unwrap());
        assert!(!has_markers(&dir.path().join("absent")).unwrap());
    }
}
