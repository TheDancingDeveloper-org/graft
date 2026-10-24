//! Writing graft changesets into a git repository (`docs/design.md` §11.2).
//!
//! [`Exporter`] turns changesets into git commits, parents first. For each
//! one it writes the blobs the destination lacks, then the trees and the
//! commit, and only then records the commit in the mirror map, so nothing is
//! named before it is stored. An imported changeset reproduces the commit
//! recorded at import (the fidelity invariant). A native changeset is built
//! from its fields and recorded the same way, so exporting the same history
//! again writes the same bytes.
//!
//! Each changeset costs O(change): the export reads the first parent's git
//! tree along the changed paths only and never builds a snapshot. A full
//! export ([`Exporter::export_refs`]) reads the whole history it writes,
//! which is what the exit path is for.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use graft_model::{BlobId, Change, Changeset, ChangesetId, EntryKind, ObjectKind, Timestamp};
use graft_proto::CallContext;
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{GetChangesets, GetObjects};

use crate::export::{Committer, GitSource, commit_object, commit_sha, export_tree};
use crate::git::{Kind, Repo};
use crate::map::{ImportedCommit, MirrorMap};
use crate::{GitError, GitSha};

/// Changesets per `GetChangesets` call.
const FETCH_MAX: usize = 256;

/// Metadata key for the agent session that wrote a changeset. Exported as a
/// `Graft-Session` trailer.
pub const SESSION_KEY: &str = "vogt.session";
/// Metadata key for the work item a changeset belongs to. Exported as a
/// `Vogt-Work-Item` trailer.
pub const WORK_ITEM_KEY: &str = "vogt.work_item";
/// Metadata key a merge landing carries: `<src>@<changeset>`, the line and
/// changeset it merged (D29). Exported as a `Graft-Merged-From` trailer on
/// a one-parent commit; graft makes no git merge commits.
pub const MERGED_FROM_KEY: &str = "graft.merged-from";

/// When each changeset landed on its line. The committer date of a native
/// export is this time, so the land record decides it and it is the same on
/// every export.
pub trait LandTimes: Sync {
    /// The landing time of `id`, with its offset.
    fn landed_at(&self, id: ChangesetId) -> Result<Timestamp, GitError>;
}

/// What an export wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Commits written by this run.
    pub commits: u64,
    /// Blobs written by this run.
    pub blobs: u64,
    /// Each ref set, with the commit it names.
    pub refs: Vec<(String, GitSha)>,
}

/// Writes changesets as git commits into one bare repository.
pub struct Exporter<'a, A, M> {
    api: &'a A,
    cx: &'a CallContext,
    map: &'a M,
    repo: &'a str,
    committer: &'a Committer,
    lands: &'a dyn LandTimes,
    /// Behind a lock so the export future is `Send`: `gix` repositories are
    /// not `Sync`. Never held across an await.
    dest: Mutex<Repo>,
    /// The git sha of each blob this run has placed.
    blob_shas: HashMap<BlobId, GitSha>,
    report: ExportReport,
}

impl<'a, A, M> Exporter<'a, A, M>
where
    A: RepoApi + ObjectApi,
    M: MirrorMap,
{
    /// An exporter that writes into the bare repository at `dest`, creating
    /// it when there is none.
    pub fn new(
        api: &'a A,
        cx: &'a CallContext,
        map: &'a M,
        repo: &'a str,
        committer: &'a Committer,
        lands: &'a dyn LandTimes,
        dest: &Path,
    ) -> Result<Self, GitError> {
        if committer.name.contains(IDENT_BAD) || committer.email.contains(IDENT_BAD) {
            return Err(GitError::Config(
                "the committer's name or email holds a newline, NUL or angle bracket".into(),
            ));
        }
        Ok(Exporter {
            api,
            cx,
            map,
            repo,
            committer,
            lands,
            dest: Mutex::new(Repo::open_or_init(dest)?),
            blob_shas: HashMap::new(),
            report: ExportReport::default(),
        })
    }

    fn dest(&self) -> MutexGuard<'_, Repo> {
        self.dest.lock().expect("destination lock")
    }

    /// What this exporter has written so far.
    pub fn report(&self) -> &ExportReport {
        &self.report
    }

    /// Exports `id` and the ancestors the destination lacks, and returns its
    /// git sha.
    pub async fn export_changeset(&mut self, id: ChangesetId) -> Result<GitSha, GitError> {
        self.write_history(&[id]).await?;
        self.sha_of(id)
    }

    /// Exports the history reachable from each ref's changeset, then points
    /// each ref at its commit. Ref names are full git names
    /// (`refs/heads/main`, `refs/tags/v1`). A repeat run writes only what the
    /// destination lacks.
    pub async fn export_refs(
        &mut self,
        refs: &[(String, ChangesetId)],
    ) -> Result<ExportReport, GitError> {
        let tips: Vec<ChangesetId> = refs.iter().map(|(_, id)| *id).collect();
        self.write_history(&tips).await?;
        for (name, id) in refs {
            let commit = self.sha_of(*id)?;
            let sha = self.tag_objects(name, commit)?.unwrap_or(commit);
            self.dest().set_ref(name, sha)?;
            self.report.refs.push((name.clone(), sha));
        }
        Ok(self.report.clone())
    }

    /// Writes the imported tag objects of `name` (a `refs/tags/` ref), inner
    /// first, and returns the outermost one's sha. `None` when the ref is not
    /// a tag, the tag was lightweight, or its record ends at another commit
    /// (the tag was not imported from this history).
    fn tag_objects(&self, name: &str, commit: GitSha) -> Result<Option<GitSha>, GitError> {
        let Some(tag) = name.strip_prefix("refs/tags/") else {
            return Ok(None);
        };
        let Some(record) = self.map.tag(tag)? else {
            return Ok(None);
        };
        if record.commit != commit {
            return Ok(None);
        }
        let dest = self.dest();
        let mut sha = None;
        for object in record.objects.iter().rev() {
            sha = Some(dest.write(Kind::Tag, object)?);
        }
        Ok(sha)
    }

    async fn write_history(&mut self, tips: &[ChangesetId]) -> Result<(), GitError> {
        let (order, mut fetched) = self.plan(tips).await?;
        for id in order {
            let changeset = fetched
                .remove(&id)
                .ok_or_else(|| GitError::Map(format!("changeset {id} was not fetched")))?;
            self.write(id, &changeset).await?;
        }
        Ok(())
    }

    /// The changesets to write for `tips`, parents first, with their
    /// contents. The walk fetches in batches and stops at changesets the
    /// destination already holds.
    async fn plan(
        &self,
        tips: &[ChangesetId],
    ) -> Result<(Vec<ChangesetId>, HashMap<ChangesetId, Changeset>), GitError> {
        let mut fetched = HashMap::new();
        let mut present = HashSet::new();
        let mut seen = HashSet::new();
        let mut frontier: Vec<ChangesetId> = tips.to_vec();
        while !frontier.is_empty() {
            let mut want = Vec::new();
            for id in frontier.drain(..) {
                if !seen.insert(id) {
                    continue;
                }
                if self.is_present(id)? {
                    present.insert(id);
                } else {
                    want.push(id);
                }
            }
            let mut next = Vec::new();
            for chunk in want.chunks(FETCH_MAX) {
                let got = self
                    .api
                    .get_changesets(
                        self.cx,
                        GetChangesets {
                            repo: self.repo.to_owned(),
                            ids: chunk.to_vec(),
                        },
                    )
                    .await?;
                if let Some(id) = got.missing.first().or(got.collected.first()) {
                    return Err(GitError::Map(format!(
                        "changeset {id} is not on the server"
                    )));
                }
                for view in got.found {
                    let changeset = view.changeset;
                    next.extend(changeset.parents.iter().copied());
                    fetched.insert(changeset.id(), changeset);
                }
            }
            frontier = next;
        }
        // Post-order over the fetched changesets. A changeset is open while
        // its children are being walked, so reaching an open one again is a
        // cycle.
        let mut done = present;
        let mut open = HashSet::new();
        let mut order = Vec::new();
        for &tip in tips {
            let mut stack = vec![(tip, false)];
            while let Some((id, expanded)) = stack.pop() {
                if expanded {
                    open.remove(&id);
                    if done.insert(id) {
                        order.push(id);
                    }
                    continue;
                }
                if done.contains(&id) {
                    continue;
                }
                if !open.insert(id) {
                    return Err(GitError::Read(format!("changeset cycle through {id}")));
                }
                let changeset = fetched
                    .get(&id)
                    .ok_or_else(|| GitError::Map(format!("changeset {id} was not fetched")))?;
                stack.push((id, true));
                for p in changeset.parents.iter().rev() {
                    if !done.contains(p) {
                        stack.push((*p, false));
                    }
                }
            }
        }
        Ok((order, fetched))
    }

    /// Whether the map knows `id` and the destination holds its commit.
    fn is_present(&self, id: ChangesetId) -> Result<bool, GitError> {
        Ok(match self.map.sha_of(id)? {
            Some(sha) => self.dest().has(sha),
            None => false,
        })
    }

    /// The git sha of an exported changeset, which must be in the
    /// destination.
    fn sha_of(&self, id: ChangesetId) -> Result<GitSha, GitError> {
        let sha = self
            .map
            .sha_of(id)?
            .ok_or_else(|| GitError::Map(format!("changeset {id} has no git commit")))?;
        if !self.dest().has(sha) {
            return Err(GitError::Map(format!(
                "commit {sha} of {id} is not in the destination"
            )));
        }
        Ok(sha)
    }

    /// Writes one changeset: its blobs, trees and commit, then its map record
    /// if it is native. Its parents must already be in the destination.
    async fn write(&mut self, id: ChangesetId, changeset: &Changeset) -> Result<GitSha, GitError> {
        let mut parents = Vec::with_capacity(changeset.parents.len());
        for p in &changeset.parents {
            parents.push(self.sha_of(*p)?);
        }
        let parent_tree = match parents.first() {
            Some(p) => Some(self.dest().commit_tree(*p)?),
            None => None,
        };
        self.place_blobs(changeset).await?;
        let (mut record, native) = match self.map.imported(id)? {
            Some(record) => (record, false),
            None => (
                native_record(self.lands, self.committer, id, changeset)?,
                true,
            ),
        };
        let sha = {
            let repo = self.dest();
            let tree = {
                let mut src = Placed {
                    repo: &repo,
                    blob_shas: &self.blob_shas,
                };
                export_tree(&mut src, parent_tree, &changeset.changes, &record.raw_trees)?
            };
            let sha = commit_sha(tree, &parents, &record, &changeset.message);
            if native {
                record.sha = sha;
            } else if record.sha != sha {
                return Err(GitError::Fidelity(vec![record.sha]));
            }
            let raw = commit_object(tree, &parents, &record, &changeset.message);
            if repo.write(Kind::Commit, &raw)? != sha {
                return Err(GitError::Write(format!(
                    "commit {sha} was stored under another sha"
                )));
            }
            sha
        };
        if native {
            self.map.put_imported(id, record)?;
        }
        self.report.commits += 1;
        Ok(sha)
    }

    /// Writes the blobs the changeset puts that the destination lacks. Bytes
    /// come from the API, and each is checked against its id.
    async fn place_blobs(&mut self, changeset: &Changeset) -> Result<(), GitError> {
        let mut fresh = Vec::new();
        for change in &changeset.changes {
            let Change::Put { kind, id, .. } = change else {
                continue;
            };
            if *kind == EntryKind::Submodule {
                continue;
            }
            let id = BlobId(*id);
            if self.blob_shas.contains_key(&id) {
                continue;
            }
            let known = self.map.blob_sha(id)?;
            if let Some(sha) = known
                && self.dest().has(sha)
            {
                self.blob_shas.insert(id, sha);
                continue;
            }
            let bytes = self.fetch_blob(id).await?;
            let sha = self.dest().write(Kind::Blob, &bytes)?;
            match known {
                Some(k) if k != sha => {
                    return Err(GitError::Map(format!(
                        "blob {id} is recorded as {k}, not {sha}"
                    )));
                }
                Some(_) => {}
                None => fresh.push((id, sha)),
            }
            self.blob_shas.insert(id, sha);
            self.report.blobs += 1;
        }
        if !fresh.is_empty() {
            self.map.put_blob_shas(&fresh)?;
        }
        Ok(())
    }

    async fn fetch_blob(&self, id: BlobId) -> Result<Vec<u8>, GitError> {
        let got = self
            .api
            .get_objects(
                self.cx,
                GetObjects {
                    repo: self.repo.to_owned(),
                    ids: vec![id.0],
                },
            )
            .await?;
        let (_, frame) = got
            .found
            .into_iter()
            .find(|(i, f)| *i == id.0 && f.kind == ObjectKind::Blob)
            .ok_or_else(|| GitError::Map(format!("blob {id} is not stored")))?;
        frame
            .decode_verified(id.0)
            .map_err(|_| GitError::Read(format!("blob {id} failed its frame check")))
    }
}

/// The export's view of the destination: trees are read from it and are
/// written into it, blob shas come from this run.
struct Placed<'a> {
    repo: &'a Repo,
    blob_shas: &'a HashMap<BlobId, GitSha>,
}

impl GitSource for Placed<'_> {
    fn raw_tree(&mut self, sha: GitSha) -> Result<Vec<u8>, GitError> {
        self.repo.read(sha, Kind::Tree)
    }

    fn blob_sha(&mut self, id: BlobId) -> Result<GitSha, GitError> {
        self.blob_shas
            .get(&id)
            .copied()
            .ok_or_else(|| GitError::Map(format!("no git sha for blob {id}")))
    }

    fn put_tree(&mut self, raw: &[u8]) -> Result<(), GitError> {
        self.repo.write(Kind::Tree, raw).map(|_| ())
    }
}

/// The record a native changeset is exported with: the author and committer
/// header lines, and the message with its trailers. Its sha is filled in
/// once the tree is known.
fn native_record(
    lands: &dyn LandTimes,
    committer: &Committer,
    id: ChangesetId,
    changeset: &Changeset,
) -> Result<ImportedCommit, GitError> {
    check_text(&changeset.author.name, IDENT_BAD, "author-name", id)?;
    check_text(&changeset.author.email, IDENT_BAD, "author-email", id)?;
    let landed = lands.landed_at(id)?;
    let mut headers = Vec::new();
    headers.extend_from_slice(
        format!(
            "author {} <{}> {}\n",
            changeset.author.name,
            changeset.author.email,
            git_time(changeset.authored_at)
        )
        .as_bytes(),
    );
    headers.extend_from_slice(
        format!(
            "committer {} <{}> {}\n",
            committer.name,
            committer.email,
            git_time(landed)
        )
        .as_bytes(),
    );
    headers.push(b'\n');
    Ok(ImportedCommit {
        sha: GitSha([0; 20]),
        headers,
        message: Some(message(id, changeset)?),
        raw_trees: Vec::new(),
    })
}

/// The commit message: the changeset's message, then the trailers in a
/// block of their own. Only these four keys are ever written. The
/// `mirror.git-commit` provenance key never is (decision D19).
fn message(id: ChangesetId, changeset: &Changeset) -> Result<Vec<u8>, GitError> {
    check_text(&changeset.message, MESSAGE_BAD, "message", id)?;
    let mut trailers = vec![format!("Graft-Changeset: {id}")];
    if let Some(session) = changeset.metadata.get(SESSION_KEY) {
        check_text(session, TRAILER_BAD, "trailer", id)?;
        trailers.push(format!("Graft-Session: {session}"));
    }
    if let Some(item) = changeset.metadata.get(WORK_ITEM_KEY) {
        check_text(item, TRAILER_BAD, "trailer", id)?;
        trailers.push(format!("Vogt-Work-Item: {item}"));
    }
    if let Some(from) = changeset.metadata.get(MERGED_FROM_KEY) {
        check_text(from, TRAILER_BAD, "trailer", id)?;
        trailers.push(format!("Graft-Merged-From: {from}"));
    }
    let mut out = changeset.message.clone().into_bytes();
    if !out.is_empty() {
        if !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.push(b'\n');
    }
    for t in trailers {
        out.extend_from_slice(t.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

/// `<seconds> <+HHMM>`, as git writes a date.
fn git_time(at: Timestamp) -> String {
    let off = i32::from(at.offset_min);
    let (sign, abs) = if off < 0 { ('-', -off) } else { ('+', off) };
    format!(
        "{} {sign}{:02}{:02}",
        at.ms.div_euclid(1000),
        abs / 60,
        abs % 60
    )
}

/// Characters a name or email may not hold: they end a git identity line.
const IDENT_BAD: [char; 4] = ['\n', '\0', '<', '>'];
/// Characters a trailer value may not hold: it must fit on one line.
const TRAILER_BAD: [char; 2] = ['\n', '\0'];
/// Characters a message may not hold: a NUL cannot be written back out
/// faithfully. Newlines are the message's own.
const MESSAGE_BAD: [char; 1] = ['\0'];

/// Refuses text that holds one of `bad`, so a native changeset can never
/// write a malformed commit.
fn check_text(
    text: &str,
    bad: impl IntoIterator<Item = char> + Copy,
    reason: &'static str,
    id: ChangesetId,
) -> Result<(), GitError> {
    if bad.into_iter().any(|c| text.contains(c)) {
        return Err(GitError::Unexportable {
            changeset: id,
            reason,
        });
    }
    Ok(())
}
