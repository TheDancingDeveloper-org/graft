//! Conversions between the Rust API types and the protobuf messages in
//! [`crate::pb`].
//!
//! Rust to wire is `From` and cannot fail. Wire to Rust is `TryFrom` and is
//! where the boundary validates: id lengths, paths, frames, enum values and
//! required fields. A failure is a [`WireError`], which becomes an
//! `INVALID_ARGUMENT` status naming the field, never its contents.

use std::fmt;

use graft_model::{
    Author, AuthorKind, BlobId, Change, Changeset, ChangesetId, EntryKind, ObjectId, RepoPath,
    Timestamp, TreeId,
};

use crate::error::{ApiError, ConflictPath};
use crate::frame::Frame;
use crate::pb;
use crate::types::*;

/// A protobuf message that does not convert to a valid Rust value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireError {
    /// The offending field.
    pub field: &'static str,
    /// What was wrong with it.
    pub reason: &'static str,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.field, self.reason)
    }
}

impl std::error::Error for WireError {}

impl From<WireError> for tonic::Status {
    fn from(e: WireError) -> Self {
        tonic::Status::invalid_argument(e.to_string())
    }
}

type R<T> = Result<T, WireError>;

fn bad(field: &'static str, reason: &'static str) -> WireError {
    WireError { field, reason }
}

// ---- helpers --------------------------------------------------------------

fn oid(b: Vec<u8>, field: &'static str) -> R<ObjectId> {
    <[u8; 32]>::try_from(b.as_slice())
        .map(ObjectId)
        .map_err(|_| bad(field, "id is not 32 bytes"))
}

fn cid(b: Vec<u8>, field: &'static str) -> R<ChangesetId> {
    oid(b, field).map(ChangesetId)
}

fn oids(v: Vec<Vec<u8>>, field: &'static str) -> R<Vec<ObjectId>> {
    v.into_iter().map(|b| oid(b, field)).collect()
}

fn cids(v: Vec<Vec<u8>>, field: &'static str) -> R<Vec<ChangesetId>> {
    v.into_iter().map(|b| cid(b, field)).collect()
}

fn opt_oid(b: Option<Vec<u8>>, field: &'static str) -> R<Option<ObjectId>> {
    b.map(|b| oid(b, field)).transpose()
}

fn opt_cid(b: Option<Vec<u8>>, field: &'static str) -> R<Option<ChangesetId>> {
    b.map(|b| cid(b, field)).transpose()
}

fn path(s: String, field: &'static str) -> R<RepoPath> {
    RepoPath::new(&s).map_err(|_| bad(field, "path breaks the path rules"))
}

fn paths(v: Vec<String>, field: &'static str) -> R<Vec<RepoPath>> {
    v.into_iter().map(|s| path(s, field)).collect()
}

fn frame(b: Vec<u8>, field: &'static str) -> R<Frame> {
    Frame::from_bytes(&b).map_err(|_| bad(field, "malformed object frame"))
}

fn frames(v: Vec<Vec<u8>>, field: &'static str) -> R<Vec<Frame>> {
    v.into_iter().map(|b| frame(b, field)).collect()
}

fn req<T>(m: Option<T>, field: &'static str) -> R<T> {
    m.ok_or(bad(field, "missing"))
}

fn ob(id: ObjectId) -> Vec<u8> {
    id.0.to_vec()
}

fn cb(id: ChangesetId) -> Vec<u8> {
    ob(id.0)
}

fn obs(ids: Vec<ObjectId>) -> Vec<Vec<u8>> {
    ids.into_iter().map(ob).collect()
}

fn cbs(ids: Vec<ChangesetId>) -> Vec<Vec<u8>> {
    ids.into_iter().map(cb).collect()
}

fn ps(v: Vec<RepoPath>) -> Vec<String> {
    v.into_iter().map(|p| p.as_str().to_owned()).collect()
}

fn fbs(v: Vec<Frame>) -> Vec<Vec<u8>> {
    v.iter().map(Frame::to_bytes).collect()
}

// ---- enums ----------------------------------------------------------------

fn author_kind(k: AuthorKind) -> i32 {
    (match k {
        AuthorKind::Human => pb::AuthorKind::Human,
        AuthorKind::Agent => pb::AuthorKind::Agent,
        AuthorKind::Bot => pb::AuthorKind::Bot,
        AuthorKind::Ci => pb::AuthorKind::Ci,
    }) as i32
}

fn author_kind_from(v: i32) -> R<AuthorKind> {
    match pb::AuthorKind::try_from(v) {
        Ok(pb::AuthorKind::Human) => Ok(AuthorKind::Human),
        Ok(pb::AuthorKind::Agent) => Ok(AuthorKind::Agent),
        Ok(pb::AuthorKind::Bot) => Ok(AuthorKind::Bot),
        Ok(pb::AuthorKind::Ci) => Ok(AuthorKind::Ci),
        _ => Err(bad("author.kind", "unknown author kind")),
    }
}

fn entry_kind(k: EntryKind) -> i32 {
    (match k {
        EntryKind::File => pb::EntryKind::File,
        EntryKind::Exec => pb::EntryKind::Exec,
        EntryKind::Symlink => pb::EntryKind::Symlink,
        EntryKind::Tree => pb::EntryKind::Tree,
        EntryKind::Submodule => pb::EntryKind::Submodule,
    }) as i32
}

fn entry_kind_from(v: i32, field: &'static str) -> R<EntryKind> {
    match pb::EntryKind::try_from(v) {
        Ok(pb::EntryKind::File) => Ok(EntryKind::File),
        Ok(pb::EntryKind::Exec) => Ok(EntryKind::Exec),
        Ok(pb::EntryKind::Symlink) => Ok(EntryKind::Symlink),
        Ok(pb::EntryKind::Tree) => Ok(EntryKind::Tree),
        Ok(pb::EntryKind::Submodule) => Ok(EntryKind::Submodule),
        _ => Err(bad(field, "unknown entry kind")),
    }
}

fn draft_state(s: DraftState) -> i32 {
    (match s {
        DraftState::Draft => pb::DraftState::Draft,
        DraftState::Landed => pb::DraftState::Landed,
        DraftState::Superseded => pb::DraftState::Superseded,
        DraftState::Abandoned => pb::DraftState::Abandoned,
    }) as i32
}

fn draft_state_from(v: i32) -> R<DraftState> {
    match pb::DraftState::try_from(v) {
        Ok(pb::DraftState::Draft) => Ok(DraftState::Draft),
        Ok(pb::DraftState::Landed) => Ok(DraftState::Landed),
        Ok(pb::DraftState::Superseded) => Ok(DraftState::Superseded),
        Ok(pb::DraftState::Abandoned) => Ok(DraftState::Abandoned),
        _ => Err(bad("state", "unknown draft state")),
    }
}

fn ref_kind(k: RefKind) -> i32 {
    (match k {
        RefKind::Line => pb::RefKind::Line,
        RefKind::Tag => pb::RefKind::Tag,
        RefKind::Tool => pb::RefKind::Tool,
    }) as i32
}

fn ref_kind_from(v: i32) -> R<RefKind> {
    match pb::RefKind::try_from(v) {
        Ok(pb::RefKind::Line) => Ok(RefKind::Line),
        Ok(pb::RefKind::Tag) => Ok(RefKind::Tag),
        Ok(pb::RefKind::Tool) => Ok(RefKind::Tool),
        _ => Err(bad("kind", "unknown ref kind")),
    }
}

fn merge_mode(m: MergeMode) -> i32 {
    (match m {
        MergeMode::File => pb::MergeMode::File,
        MergeMode::Line => pb::MergeMode::Line,
    }) as i32
}

/// An unset merge mode is the default, `File`.
fn merge_mode_from(v: i32) -> R<MergeMode> {
    match pb::MergeMode::try_from(v) {
        Ok(pb::MergeMode::Unspecified | pb::MergeMode::File) => Ok(MergeMode::File),
        Ok(pb::MergeMode::Line) => Ok(MergeMode::Line),
        Err(_) => Err(bad("merge", "unknown merge mode")),
    }
}

// ---- shared ---------------------------------------------------------------

impl From<Page> for pb::Page {
    fn from(p: Page) -> Self {
        pb::Page {
            limit: p.limit,
            token: p.token,
        }
    }
}

impl From<pb::Page> for Page {
    fn from(p: pb::Page) -> Self {
        Page {
            limit: p.limit,
            token: p.token,
        }
    }
}

fn page_from(p: Option<pb::Page>) -> Page {
    p.map(Page::from).unwrap_or_default()
}

impl From<Timestamp> for pb::Timestamp {
    fn from(t: Timestamp) -> Self {
        pb::Timestamp {
            ms: t.ms,
            offset_min: t.offset_min.into(),
        }
    }
}

impl TryFrom<pb::Timestamp> for Timestamp {
    type Error = WireError;
    fn try_from(t: pb::Timestamp) -> R<Self> {
        Ok(Timestamp {
            ms: t.ms,
            offset_min: i16::try_from(t.offset_min)
                .map_err(|_| bad("offset_min", "out of range"))?,
        })
    }
}

fn ts_from(t: Option<pb::Timestamp>, field: &'static str) -> R<Timestamp> {
    req(t, field)?.try_into()
}

impl From<Author> for pb::Author {
    fn from(a: Author) -> Self {
        pb::Author {
            name: a.name,
            email: a.email,
            kind: author_kind(a.kind),
        }
    }
}

impl TryFrom<pb::Author> for Author {
    type Error = WireError;
    fn try_from(a: pb::Author) -> R<Self> {
        Ok(Author {
            name: a.name,
            email: a.email,
            kind: author_kind_from(a.kind)?,
        })
    }
}

fn author_from(a: Option<pb::Author>) -> R<Author> {
    req(a, "author")?.try_into()
}

impl From<Change> for pb::Change {
    fn from(c: Change) -> Self {
        use pb::change::{Change as C, Delete, Put};
        pb::Change {
            change: Some(match c {
                Change::Put { path, kind, id } => C::Put(Put {
                    path: path.as_str().to_owned(),
                    kind: entry_kind(kind),
                    id: ob(id),
                }),
                Change::Delete { path } => C::Delete(Delete {
                    path: path.as_str().to_owned(),
                }),
            }),
        }
    }
}

impl TryFrom<pb::Change> for Change {
    type Error = WireError;
    fn try_from(c: pb::Change) -> R<Self> {
        use pb::change::Change as C;
        Ok(match req(c.change, "change")? {
            C::Put(p) => Change::Put {
                path: path(p.path, "change.path")?,
                kind: entry_kind_from(p.kind, "change.kind")?,
                id: oid(p.id, "change.id")?,
            },
            C::Delete(d) => Change::Delete {
                path: path(d.path, "change.path")?,
            },
        })
    }
}

fn changes(v: Vec<Change>) -> Vec<pb::Change> {
    v.into_iter().map(Into::into).collect()
}

fn changes_from(v: Vec<pb::Change>) -> R<Vec<Change>> {
    v.into_iter().map(TryInto::try_into).collect()
}

impl From<Changeset> for pb::Changeset {
    fn from(c: Changeset) -> Self {
        pb::Changeset {
            parents: cbs(c.parents),
            root: ob(c.root.0),
            changes: changes(c.changes),
            author: Some(c.author.into()),
            authored_at: Some(c.authored_at.into()),
            message: c.message,
            metadata: c.metadata,
        }
    }
}

impl TryFrom<pb::Changeset> for Changeset {
    type Error = WireError;
    fn try_from(c: pb::Changeset) -> R<Self> {
        Ok(Changeset {
            parents: cids(c.parents, "parents")?,
            root: TreeId(oid(c.root, "root")?),
            changes: changes_from(c.changes)?,
            author: author_from(c.author)?,
            authored_at: ts_from(c.authored_at, "authored_at")?,
            message: c.message,
            metadata: c.metadata,
        })
    }
}

// ---- objects --------------------------------------------------------------

impl From<GetObjects> for pb::GetObjectsRequest {
    fn from(r: GetObjects) -> Self {
        pb::GetObjectsRequest {
            repo: r.repo,
            ids: obs(r.ids),
        }
    }
}

impl TryFrom<pb::GetObjectsRequest> for GetObjects {
    type Error = WireError;
    fn try_from(r: pb::GetObjectsRequest) -> R<Self> {
        Ok(GetObjects {
            repo: r.repo,
            ids: oids(r.ids, "ids")?,
        })
    }
}

impl From<Objects> for pb::Objects {
    fn from(r: Objects) -> Self {
        pb::Objects {
            found: r
                .found
                .into_iter()
                .map(|(id, f)| pb::FoundObject {
                    id: ob(id),
                    frame: f.to_bytes(),
                })
                .collect(),
            missing: obs(r.missing),
            deferred: obs(r.deferred),
        }
    }
}

impl TryFrom<pb::Objects> for Objects {
    type Error = WireError;
    fn try_from(r: pb::Objects) -> R<Self> {
        Ok(Objects {
            found: r
                .found
                .into_iter()
                .map(|f| Ok((oid(f.id, "found.id")?, frame(f.frame, "found.frame")?)))
                .collect::<R<_>>()?,
            missing: oids(r.missing, "missing")?,
            deferred: oids(r.deferred, "deferred")?,
        })
    }
}

impl From<HasObjects> for pb::HasObjectsRequest {
    fn from(r: HasObjects) -> Self {
        pb::HasObjectsRequest {
            repo: r.repo,
            ids: obs(r.ids),
        }
    }
}

impl TryFrom<pb::HasObjectsRequest> for HasObjects {
    type Error = WireError;
    fn try_from(r: pb::HasObjectsRequest) -> R<Self> {
        Ok(HasObjects {
            repo: r.repo,
            ids: oids(r.ids, "ids")?,
        })
    }
}

impl From<PutObjects> for pb::PutObjectsRequest {
    fn from(r: PutObjects) -> Self {
        pb::PutObjectsRequest {
            repo: r.repo,
            blobs: fbs(r.blobs),
        }
    }
}

impl TryFrom<pb::PutObjectsRequest> for PutObjects {
    type Error = WireError;
    fn try_from(r: pb::PutObjectsRequest) -> R<Self> {
        Ok(PutObjects {
            repo: r.repo,
            blobs: frames(r.blobs, "blobs")?,
        })
    }
}

impl From<Vec<ObjectId>> for pb::ObjectIds {
    fn from(ids: Vec<ObjectId>) -> Self {
        pb::ObjectIds { ids: obs(ids) }
    }
}

impl TryFrom<pb::ObjectIds> for Vec<ObjectId> {
    type Error = WireError;
    fn try_from(r: pb::ObjectIds) -> R<Self> {
        oids(r.ids, "ids")
    }
}

// ---- changesets -----------------------------------------------------------

impl From<CreateChangeset> for pb::CreateChangesetRequest {
    fn from(r: CreateChangeset) -> Self {
        pb::CreateChangesetRequest {
            repo: r.repo,
            parents: cbs(r.parents),
            changes: changes(r.changes),
            author: Some(r.author.into()),
            authored_at: Some(r.authored_at.into()),
            message: r.message,
            metadata: r.metadata,
            inline_blobs: fbs(r.inline_blobs),
            workspace: r.workspace,
        }
    }
}

impl TryFrom<pb::CreateChangesetRequest> for CreateChangeset {
    type Error = WireError;
    fn try_from(r: pb::CreateChangesetRequest) -> R<Self> {
        Ok(CreateChangeset {
            repo: r.repo,
            parents: cids(r.parents, "parents")?,
            changes: changes_from(r.changes)?,
            author: author_from(r.author)?,
            authored_at: ts_from(r.authored_at, "authored_at")?,
            message: r.message,
            metadata: r.metadata,
            inline_blobs: frames(r.inline_blobs, "inline_blobs")?,
            workspace: r.workspace,
        })
    }
}

impl From<Created> for pb::Created {
    fn from(r: Created) -> Self {
        pb::Created {
            id: cb(r.id),
            root: ob(r.root.0),
            lsn: r.lsn,
            state: draft_state(r.state),
        }
    }
}

impl TryFrom<pb::Created> for Created {
    type Error = WireError;
    fn try_from(r: pb::Created) -> R<Self> {
        Ok(Created {
            id: cid(r.id, "id")?,
            root: TreeId(oid(r.root, "root")?),
            lsn: r.lsn,
            state: draft_state_from(r.state)?,
        })
    }
}

impl From<Vec<Created>> for pb::CreatedList {
    fn from(v: Vec<Created>) -> Self {
        pb::CreatedList {
            created: v.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::CreatedList> for Vec<Created> {
    type Error = WireError;
    fn try_from(r: pb::CreatedList) -> R<Self> {
        r.created.into_iter().map(TryInto::try_into).collect()
    }
}

impl From<GetChangesets> for pb::GetChangesetsRequest {
    fn from(r: GetChangesets) -> Self {
        pb::GetChangesetsRequest {
            repo: r.repo,
            ids: cbs(r.ids),
        }
    }
}

impl TryFrom<pb::GetChangesetsRequest> for GetChangesets {
    type Error = WireError;
    fn try_from(r: pb::GetChangesetsRequest) -> R<Self> {
        Ok(GetChangesets {
            repo: r.repo,
            ids: cids(r.ids, "ids")?,
        })
    }
}

impl From<ChangesetView> for pb::ChangesetView {
    fn from(r: ChangesetView) -> Self {
        pb::ChangesetView {
            changeset: Some(r.changeset.into()),
            lsn: r.lsn,
            state: draft_state(r.state),
            superseded_by: r.superseded_by.map(cb),
        }
    }
}

impl TryFrom<pb::ChangesetView> for ChangesetView {
    type Error = WireError;
    fn try_from(r: pb::ChangesetView) -> R<Self> {
        Ok(ChangesetView {
            changeset: req(r.changeset, "changeset")?.try_into()?,
            lsn: r.lsn,
            state: draft_state_from(r.state)?,
            superseded_by: opt_cid(r.superseded_by, "superseded_by")?,
        })
    }
}

impl From<Changesets> for pb::Changesets {
    fn from(r: Changesets) -> Self {
        pb::Changesets {
            found: r.found.into_iter().map(Into::into).collect(),
            missing: cbs(r.missing),
            collected: cbs(r.collected),
        }
    }
}

impl TryFrom<pb::Changesets> for Changesets {
    type Error = WireError;
    fn try_from(r: pb::Changesets) -> R<Self> {
        Ok(Changesets {
            found: r
                .found
                .into_iter()
                .map(TryInto::try_into)
                .collect::<R<_>>()?,
            missing: cids(r.missing, "missing")?,
            collected: cids(r.collected, "collected")?,
        })
    }
}

// ---- refs -----------------------------------------------------------------

impl From<RefView> for pb::RefView {
    fn from(r: RefView) -> Self {
        pb::RefView {
            name: r.name,
            target: cb(r.target),
            version: r.version,
            kind: ref_kind(r.kind),
        }
    }
}

impl TryFrom<pb::RefView> for RefView {
    type Error = WireError;
    fn try_from(r: pb::RefView) -> R<Self> {
        Ok(RefView {
            name: r.name,
            target: cid(r.target, "target")?,
            version: r.version,
            kind: ref_kind_from(r.kind)?,
        })
    }
}

impl From<Vec<RefView>> for pb::RefViews {
    fn from(v: Vec<RefView>) -> Self {
        pb::RefViews {
            refs: v.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::RefViews> for Vec<RefView> {
    type Error = WireError;
    fn try_from(r: pb::RefViews) -> R<Self> {
        r.refs.into_iter().map(TryInto::try_into).collect()
    }
}

impl From<ResolveRef> for pb::ResolveRefRequest {
    fn from(r: ResolveRef) -> Self {
        pb::ResolveRefRequest {
            repo: r.repo,
            name: r.name,
        }
    }
}

impl From<pb::ResolveRefRequest> for ResolveRef {
    fn from(r: pb::ResolveRefRequest) -> Self {
        ResolveRef {
            repo: r.repo,
            name: r.name,
        }
    }
}

impl From<ListRefs> for pb::ListRefsRequest {
    fn from(r: ListRefs) -> Self {
        pb::ListRefsRequest {
            repo: r.repo,
            prefix: r.prefix,
            page: Some(r.page.into()),
        }
    }
}

impl From<pb::ListRefsRequest> for ListRefs {
    fn from(r: pb::ListRefsRequest) -> Self {
        ListRefs {
            repo: r.repo,
            prefix: r.prefix,
            page: page_from(r.page),
        }
    }
}

impl From<UpdateRef> for pb::UpdateRefRequest {
    fn from(r: UpdateRef) -> Self {
        pb::UpdateRefRequest {
            repo: r.repo,
            name: r.name,
            expected: r.expected.map(cb),
            new: cb(r.new),
            kind: ref_kind(r.kind),
        }
    }
}

impl TryFrom<pb::UpdateRefRequest> for UpdateRef {
    type Error = WireError;
    fn try_from(r: pb::UpdateRefRequest) -> R<Self> {
        Ok(UpdateRef {
            repo: r.repo,
            name: r.name,
            expected: opt_cid(r.expected, "expected")?,
            new: cid(r.new, "new")?,
            kind: ref_kind_from(r.kind)?,
        })
    }
}

impl From<CreateLine> for pb::CreateLineRequest {
    fn from(r: CreateLine) -> Self {
        pb::CreateLineRequest {
            repo: r.repo,
            name: r.name,
            from: cb(r.from),
        }
    }
}

impl TryFrom<pb::CreateLineRequest> for CreateLine {
    type Error = WireError;
    fn try_from(r: pb::CreateLineRequest) -> R<Self> {
        Ok(CreateLine {
            repo: r.repo,
            name: r.name,
            from: cid(r.from, "from")?,
        })
    }
}

// ---- snapshot reads -------------------------------------------------------

impl From<PathAt> for pb::PathAt {
    fn from(r: PathAt) -> Self {
        pb::PathAt {
            repo: r.repo,
            at: cb(r.at),
            path: r.path.as_str().to_owned(),
        }
    }
}

impl TryFrom<pb::PathAt> for PathAt {
    type Error = WireError;
    fn try_from(r: pb::PathAt) -> R<Self> {
        Ok(PathAt {
            repo: r.repo,
            at: cid(r.at, "at")?,
            path: path(r.path, "path")?,
        })
    }
}

impl From<EntryView> for pb::EntryView {
    fn from(r: EntryView) -> Self {
        pb::EntryView {
            path: r.path.as_str().to_owned(),
            kind: entry_kind(r.kind),
            id: ob(r.id),
        }
    }
}

impl TryFrom<pb::EntryView> for EntryView {
    type Error = WireError;
    fn try_from(r: pb::EntryView) -> R<Self> {
        Ok(EntryView {
            path: path(r.path, "path")?,
            kind: entry_kind_from(r.kind, "kind")?,
            id: oid(r.id, "id")?,
        })
    }
}

impl From<FileContent> for pb::FileContent {
    fn from(r: FileContent) -> Self {
        pb::FileContent {
            kind: entry_kind(r.kind),
            id: ob(r.id.0),
            data: r.data,
        }
    }
}

impl TryFrom<pb::FileContent> for FileContent {
    type Error = WireError;
    fn try_from(r: pb::FileContent) -> R<Self> {
        Ok(FileContent {
            kind: entry_kind_from(r.kind, "kind")?,
            id: BlobId(oid(r.id, "id")?),
            data: r.data,
        })
    }
}

impl From<ListTree> for pb::ListTreeRequest {
    fn from(r: ListTree) -> Self {
        pb::ListTreeRequest {
            repo: r.repo,
            at: cb(r.at),
            path: r.path.map(|p| p.as_str().to_owned()),
            recursive: r.recursive,
            page: Some(r.page.into()),
        }
    }
}

impl TryFrom<pb::ListTreeRequest> for ListTree {
    type Error = WireError;
    fn try_from(r: pb::ListTreeRequest) -> R<Self> {
        Ok(ListTree {
            repo: r.repo,
            at: cid(r.at, "at")?,
            path: r.path.map(|p| path(p, "path")).transpose()?,
            recursive: r.recursive,
            page: page_from(r.page),
        })
    }
}

impl From<Entries> for pb::Entries {
    fn from(r: Entries) -> Self {
        pb::Entries {
            entries: r.entries.into_iter().map(Into::into).collect(),
            next: r.next,
        }
    }
}

impl TryFrom<pb::Entries> for Entries {
    type Error = WireError;
    fn try_from(r: pb::Entries) -> R<Self> {
        Ok(Entries {
            entries: r
                .entries
                .into_iter()
                .map(TryInto::try_into)
                .collect::<R<_>>()?,
            next: r.next,
        })
    }
}

// ---- history --------------------------------------------------------------

impl From<Log> for pb::LogRequest {
    fn from(r: Log) -> Self {
        pb::LogRequest {
            repo: r.repo,
            from: cb(r.from),
            until: r.until.map(cb),
            paths: ps(r.paths),
            page: Some(r.page.into()),
        }
    }
}

impl TryFrom<pb::LogRequest> for Log {
    type Error = WireError;
    fn try_from(r: pb::LogRequest) -> R<Self> {
        Ok(Log {
            repo: r.repo,
            from: cid(r.from, "from")?,
            until: opt_cid(r.until, "until")?,
            paths: paths(r.paths, "paths")?,
            page: page_from(r.page),
        })
    }
}

impl From<ChangesetSummary> for pb::ChangesetSummary {
    fn from(r: ChangesetSummary) -> Self {
        pb::ChangesetSummary {
            id: cb(r.id),
            author: Some(r.author.into()),
            authored_at: Some(r.authored_at.into()),
            subject: r.subject,
            line_pos: r.line_pos,
        }
    }
}

impl TryFrom<pb::ChangesetSummary> for ChangesetSummary {
    type Error = WireError;
    fn try_from(r: pb::ChangesetSummary) -> R<Self> {
        Ok(ChangesetSummary {
            id: cid(r.id, "id")?,
            author: author_from(r.author)?,
            authored_at: ts_from(r.authored_at, "authored_at")?,
            subject: r.subject,
            line_pos: r.line_pos,
        })
    }
}

impl From<History> for pb::History {
    fn from(r: History) -> Self {
        pb::History {
            changesets: r.changesets.into_iter().map(Into::into).collect(),
            next: r.next,
        }
    }
}

impl TryFrom<pb::History> for History {
    type Error = WireError;
    fn try_from(r: pb::History) -> R<Self> {
        Ok(History {
            changesets: r
                .changesets
                .into_iter()
                .map(TryInto::try_into)
                .collect::<R<_>>()?,
            next: r.next,
        })
    }
}

impl From<Pair> for pb::Pair {
    fn from(r: Pair) -> Self {
        pb::Pair {
            repo: r.repo,
            a: cb(r.a),
            b: cb(r.b),
        }
    }
}

impl TryFrom<pb::Pair> for Pair {
    type Error = WireError;
    fn try_from(r: pb::Pair) -> R<Self> {
        Ok(Pair {
            repo: r.repo,
            a: cid(r.a, "a")?,
            b: cid(r.b, "b")?,
        })
    }
}

impl From<PatchOptions> for pb::PatchOptions {
    fn from(r: PatchOptions) -> Self {
        pb::PatchOptions {
            context: r.context,
            max_bytes_per_file: r.max_bytes_per_file,
        }
    }
}

impl From<pb::PatchOptions> for PatchOptions {
    fn from(r: pb::PatchOptions) -> Self {
        PatchOptions {
            context: r.context,
            max_bytes_per_file: r.max_bytes_per_file,
        }
    }
}

impl From<Diff> for pb::DiffRequest {
    fn from(r: Diff) -> Self {
        pb::DiffRequest {
            repo: r.repo,
            from: cb(r.from),
            to: cb(r.to),
            paths: ps(r.paths),
            patch: r.patch.map(Into::into),
            page: Some(r.page.into()),
        }
    }
}

impl TryFrom<pb::DiffRequest> for Diff {
    type Error = WireError;
    fn try_from(r: pb::DiffRequest) -> R<Self> {
        Ok(Diff {
            repo: r.repo,
            from: cid(r.from, "from")?,
            to: cid(r.to, "to")?,
            paths: paths(r.paths, "paths")?,
            patch: r.patch.map(Into::into),
            page: page_from(r.page),
        })
    }
}

impl From<FileDiff> for pb::FileDiff {
    fn from(r: FileDiff) -> Self {
        pb::FileDiff {
            change: Some(r.change.into()),
            patch: r.patch,
            truncated: r.truncated,
        }
    }
}

impl TryFrom<pb::FileDiff> for FileDiff {
    type Error = WireError;
    fn try_from(r: pb::FileDiff) -> R<Self> {
        Ok(FileDiff {
            change: req(r.change, "change")?.try_into()?,
            patch: r.patch,
            truncated: r.truncated,
        })
    }
}

impl From<DiffResult> for pb::DiffResult {
    fn from(r: DiffResult) -> Self {
        pb::DiffResult {
            files: r.files.into_iter().map(Into::into).collect(),
            next: r.next,
        }
    }
}

impl TryFrom<pb::DiffResult> for DiffResult {
    type Error = WireError;
    fn try_from(r: pb::DiffResult) -> R<Self> {
        Ok(DiffResult {
            files: r
                .files
                .into_iter()
                .map(TryInto::try_into)
                .collect::<R<_>>()?,
            next: r.next,
        })
    }
}

impl From<Blame> for pb::BlameRequest {
    fn from(r: Blame) -> Self {
        pb::BlameRequest {
            repo: r.repo,
            at: cb(r.at),
            path: r.path.as_str().to_owned(),
            lines: r.lines.map(|(start, end)| pb::LineRange { start, end }),
        }
    }
}

impl TryFrom<pb::BlameRequest> for Blame {
    type Error = WireError;
    fn try_from(r: pb::BlameRequest) -> R<Self> {
        Ok(Blame {
            repo: r.repo,
            at: cid(r.at, "at")?,
            path: path(r.path, "path")?,
            lines: r.lines.map(|l| (l.start, l.end)),
        })
    }
}

impl From<BlameRange> for pb::BlameRange {
    fn from(r: BlameRange) -> Self {
        pb::BlameRange {
            start: r.start,
            end: r.end,
            origin: Some(r.origin.into()),
        }
    }
}

impl TryFrom<pb::BlameRange> for BlameRange {
    type Error = WireError;
    fn try_from(r: pb::BlameRange) -> R<Self> {
        Ok(BlameRange {
            start: r.start,
            end: r.end,
            origin: req(r.origin, "origin")?.try_into()?,
        })
    }
}

impl From<BlameResult> for pb::BlameResult {
    fn from(r: BlameResult) -> Self {
        pb::BlameResult {
            ranges: r.ranges.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::BlameResult> for BlameResult {
    type Error = WireError;
    fn try_from(r: pb::BlameResult) -> R<Self> {
        Ok(BlameResult {
            ranges: r
                .ranges
                .into_iter()
                .map(TryInto::try_into)
                .collect::<R<_>>()?,
        })
    }
}

// ---- rebase, revert, land -------------------------------------------------

impl From<Rebase> for pb::RebaseRequest {
    fn from(r: Rebase) -> Self {
        pb::RebaseRequest {
            repo: r.repo,
            changeset: cb(r.changeset),
            onto: cb(r.onto),
            merge: merge_mode(r.merge),
            dry_run: r.dry_run,
        }
    }
}

impl TryFrom<pb::RebaseRequest> for Rebase {
    type Error = WireError;
    fn try_from(r: pb::RebaseRequest) -> R<Self> {
        Ok(Rebase {
            repo: r.repo,
            changeset: cid(r.changeset, "changeset")?,
            onto: cid(r.onto, "onto")?,
            merge: merge_mode_from(r.merge)?,
            dry_run: r.dry_run,
        })
    }
}

impl From<Revert> for pb::RevertRequest {
    fn from(r: Revert) -> Self {
        pb::RevertRequest {
            repo: r.repo,
            changeset: cb(r.changeset),
            onto: cb(r.onto),
            workspace: r.workspace,
        }
    }
}

impl TryFrom<pb::RevertRequest> for Revert {
    type Error = WireError;
    fn try_from(r: pb::RevertRequest) -> R<Self> {
        Ok(Revert {
            repo: r.repo,
            changeset: cid(r.changeset, "changeset")?,
            onto: cid(r.onto, "onto")?,
            workspace: r.workspace,
        })
    }
}

impl From<Land> for pb::LandRequest {
    fn from(r: Land) -> Self {
        pb::LandRequest {
            repo: r.repo,
            head: cb(r.head),
            line: r.line,
            squash: r.squash,
            merge: merge_mode(r.merge),
            deadline_ms: r.deadline_ms,
            merged_from: r.merged_from.map(|m| pb::MergedFrom {
                line: m.line,
                changeset: cb(m.changeset),
            }),
        }
    }
}

impl TryFrom<pb::LandRequest> for Land {
    type Error = WireError;
    fn try_from(r: pb::LandRequest) -> R<Self> {
        Ok(Land {
            repo: r.repo,
            head: cid(r.head, "head")?,
            line: r.line,
            squash: r.squash,
            merge: merge_mode_from(r.merge)?,
            deadline_ms: r.deadline_ms,
            merged_from: r
                .merged_from
                .map(|m| -> R<MergedFrom> {
                    Ok(MergedFrom {
                        line: m.line,
                        changeset: cid(m.changeset, "merged_from.changeset")?,
                    })
                })
                .transpose()?,
        })
    }
}

impl From<Landed> for pb::Landed {
    fn from(r: Landed) -> Self {
        use pb::landed::{OnLine, Outcome, Queued};
        pb::Landed {
            outcome: Some(match r {
                Landed::Landed {
                    landed,
                    line_version,
                    pos,
                    rebased,
                    merged_paths,
                } => Outcome::Landed(OnLine {
                    landed: cb(landed),
                    line_version,
                    pos,
                    rebased,
                    merged_paths: ps(merged_paths),
                }),
                Landed::Queued { pr_url } => Outcome::Queued(Queued { pr_url }),
            }),
        }
    }
}

impl TryFrom<pb::Landed> for Landed {
    type Error = WireError;
    fn try_from(r: pb::Landed) -> R<Self> {
        use pb::landed::Outcome;
        Ok(match req(r.outcome, "outcome")? {
            Outcome::Landed(l) => Landed::Landed {
                landed: cid(l.landed, "landed")?,
                line_version: l.line_version,
                pos: l.pos,
                rebased: l.rebased,
                merged_paths: paths(l.merged_paths, "merged_paths")?,
            },
            Outcome::Queued(q) => Landed::Queued { pr_url: q.pr_url },
        })
    }
}

impl From<Attest> for pb::AttestRequest {
    fn from(r: Attest) -> Self {
        pb::AttestRequest {
            repo: r.repo,
            changeset: cb(r.changeset),
            check: r.check,
            passed: r.passed,
            url: r.url,
        }
    }
}

impl TryFrom<pb::AttestRequest> for Attest {
    type Error = WireError;
    fn try_from(r: pb::AttestRequest) -> R<Self> {
        Ok(Attest {
            repo: r.repo,
            changeset: cid(r.changeset, "changeset")?,
            check: r.check,
            passed: r.passed,
            url: r.url,
        })
    }
}

impl From<Watch> for pb::WatchRequest {
    fn from(r: Watch) -> Self {
        pb::WatchRequest {
            repo: r.repo,
            from_lsn: r.from_lsn,
            kinds: r.kinds,
            limit: r.limit,
            wait_ms: r.wait_ms,
        }
    }
}

impl From<pb::WatchRequest> for Watch {
    fn from(r: pb::WatchRequest) -> Self {
        Watch {
            repo: r.repo,
            from_lsn: r.from_lsn,
            kinds: r.kinds,
            limit: r.limit,
            wait_ms: r.wait_ms,
        }
    }
}

impl From<JournalEntry> for pb::JournalEntry {
    fn from(r: JournalEntry) -> Self {
        pb::JournalEntry {
            lsn: r.lsn,
            kind: r.kind,
            body: r.body,
        }
    }
}

impl From<pb::JournalEntry> for JournalEntry {
    fn from(r: pb::JournalEntry) -> Self {
        JournalEntry {
            lsn: r.lsn,
            kind: r.kind,
            body: r.body,
        }
    }
}

impl From<JournalPage> for pb::JournalPage {
    fn from(r: JournalPage) -> Self {
        pb::JournalPage {
            entries: r.entries.into_iter().map(Into::into).collect(),
            next_lsn: r.next_lsn,
        }
    }
}

impl From<pb::JournalPage> for JournalPage {
    fn from(r: pb::JournalPage) -> Self {
        JournalPage {
            entries: r.entries.into_iter().map(Into::into).collect(),
            next_lsn: r.next_lsn,
        }
    }
}

// ---- workspaces -----------------------------------------------------------

impl From<WorkspaceView> for pb::WorkspaceView {
    fn from(r: WorkspaceView) -> Self {
        pb::WorkspaceView {
            name: r.name,
            owner: r.owner,
            session: r.session,
            line: r.line,
            base: cb(r.base),
            head: r.head.map(cb),
            lease_until_ms: r.lease_until_ms,
        }
    }
}

impl TryFrom<pb::WorkspaceView> for WorkspaceView {
    type Error = WireError;
    fn try_from(r: pb::WorkspaceView) -> R<Self> {
        Ok(WorkspaceView {
            name: r.name,
            owner: r.owner,
            session: r.session,
            line: r.line,
            base: cid(r.base, "base")?,
            head: opt_cid(r.head, "head")?,
            lease_until_ms: r.lease_until_ms,
        })
    }
}

impl From<Vec<WorkspaceView>> for pb::WorkspaceViews {
    fn from(v: Vec<WorkspaceView>) -> Self {
        pb::WorkspaceViews {
            workspaces: v.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::WorkspaceViews> for Vec<WorkspaceView> {
    type Error = WireError;
    fn try_from(r: pb::WorkspaceViews) -> R<Self> {
        r.workspaces.into_iter().map(TryInto::try_into).collect()
    }
}

impl From<OpenWorkspace> for pb::OpenWorkspaceRequest {
    fn from(r: OpenWorkspace) -> Self {
        pb::OpenWorkspaceRequest {
            repo: r.repo,
            name: r.name,
            line: r.line,
            at: r.at.map(cb),
            lease_ms: r.lease_ms,
        }
    }
}

impl TryFrom<pb::OpenWorkspaceRequest> for OpenWorkspace {
    type Error = WireError;
    fn try_from(r: pb::OpenWorkspaceRequest) -> R<Self> {
        Ok(OpenWorkspace {
            repo: r.repo,
            name: r.name,
            line: r.line,
            at: opt_cid(r.at, "at")?,
            lease_ms: r.lease_ms,
        })
    }
}

impl From<WorkspaceRef> for pb::WorkspaceRef {
    fn from(r: WorkspaceRef) -> Self {
        pb::WorkspaceRef {
            repo: r.repo,
            name: r.name,
        }
    }
}

impl From<pb::WorkspaceRef> for WorkspaceRef {
    fn from(r: pb::WorkspaceRef) -> Self {
        WorkspaceRef {
            repo: r.repo,
            name: r.name,
        }
    }
}

impl From<SetHead> for pb::SetHeadRequest {
    fn from(r: SetHead) -> Self {
        pb::SetHeadRequest {
            workspace: Some(r.workspace.into()),
            expected: r.expected.map(cb),
            new: r.new.map(cb),
        }
    }
}

impl TryFrom<pb::SetHeadRequest> for SetHead {
    type Error = WireError;
    fn try_from(r: pb::SetHeadRequest) -> R<Self> {
        Ok(SetHead {
            workspace: req(r.workspace, "workspace")?.into(),
            expected: opt_cid(r.expected, "expected")?,
            new: opt_cid(r.new, "new")?,
        })
    }
}

impl From<Synced> for pb::Synced {
    fn from(r: Synced) -> Self {
        pb::Synced {
            workspace: Some(r.workspace.into()),
            changed_paths: ps(r.changed_paths),
        }
    }
}

impl TryFrom<pb::Synced> for Synced {
    type Error = WireError;
    fn try_from(r: pb::Synced) -> R<Self> {
        Ok(Synced {
            workspace: req(r.workspace, "workspace")?.try_into()?,
            changed_paths: paths(r.changed_paths, "changed_paths")?,
        })
    }
}

impl From<StackOp> for pb::StackOp {
    fn from(r: StackOp) -> Self {
        use pb::stack_op::{Op, Split};
        pb::StackOp {
            op: Some(match r {
                StackOp::Reorder(ids) => Op::Reorder(pb::ChangesetIds { ids: cbs(ids) }),
                StackOp::Squash(ids) => Op::Squash(pb::ChangesetIds { ids: cbs(ids) }),
                StackOp::Split { draft, groups } => Op::Split(Split {
                    draft: cb(draft),
                    groups: groups
                        .into_iter()
                        .map(|g| pb::PathGroup { paths: ps(g) })
                        .collect(),
                }),
                StackOp::Drop(id) => Op::Drop(cb(id)),
            }),
        }
    }
}

impl TryFrom<pb::StackOp> for StackOp {
    type Error = WireError;
    fn try_from(r: pb::StackOp) -> R<Self> {
        use pb::stack_op::Op;
        Ok(match req(r.op, "op")? {
            Op::Reorder(ids) => StackOp::Reorder(cids(ids.ids, "reorder")?),
            Op::Squash(ids) => StackOp::Squash(cids(ids.ids, "squash")?),
            Op::Split(s) => StackOp::Split {
                draft: cid(s.draft, "split.draft")?,
                groups: s
                    .groups
                    .into_iter()
                    .map(|g| paths(g.paths, "split.groups"))
                    .collect::<R<_>>()?,
            },
            Op::Drop(id) => StackOp::Drop(cid(id, "drop")?),
        })
    }
}

// ---- admin ----------------------------------------------------------------

impl From<ImportChangesets> for pb::ImportChangesetsRequest {
    fn from(r: ImportChangesets) -> Self {
        pb::ImportChangesetsRequest {
            repo: r.repo,
            changesets: r.changesets.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::ImportChangesetsRequest> for ImportChangesets {
    type Error = WireError;
    fn try_from(r: pb::ImportChangesetsRequest) -> R<Self> {
        Ok(ImportChangesets {
            repo: r.repo,
            changesets: r
                .changesets
                .into_iter()
                .map(TryInto::try_into)
                .collect::<R<_>>()?,
        })
    }
}

impl From<Gc> for pb::GcRequest {
    fn from(r: Gc) -> Self {
        pb::GcRequest {
            repo: r.repo,
            dry_run: r.dry_run,
        }
    }
}

impl From<pb::GcRequest> for Gc {
    fn from(r: pb::GcRequest) -> Self {
        Gc {
            repo: r.repo,
            dry_run: r.dry_run,
        }
    }
}

impl From<GcReport> for pb::GcReport {
    fn from(r: GcReport) -> Self {
        pb::GcReport {
            changesets: r.changesets,
            objects: r.objects,
            bytes: r.bytes,
        }
    }
}

impl From<pb::GcReport> for GcReport {
    fn from(r: pb::GcReport) -> Self {
        GcReport {
            changesets: r.changesets,
            objects: r.objects,
            bytes: r.bytes,
        }
    }
}

impl From<MintToken> for pb::MintTokenRequest {
    fn from(r: MintToken) -> Self {
        pb::MintTokenRequest {
            subject: r.subject,
            scopes: r.scopes,
            ttl_s: r.ttl_s,
            session: r.session,
            work_item: r.work_item,
        }
    }
}

impl From<pb::MintTokenRequest> for MintToken {
    fn from(r: pb::MintTokenRequest) -> Self {
        MintToken {
            subject: r.subject,
            scopes: r.scopes,
            ttl_s: r.ttl_s,
            session: r.session,
            work_item: r.work_item,
        }
    }
}

// ---- errors ---------------------------------------------------------------

impl From<ConflictPath> for pb::ConflictPath {
    fn from(c: ConflictPath) -> Self {
        pb::ConflictPath {
            path: c.path,
            base: c.base.map(ob),
            ours: c.ours.map(ob),
            theirs: c.theirs.map(ob),
        }
    }
}

impl TryFrom<pb::ConflictPath> for ConflictPath {
    type Error = WireError;
    fn try_from(c: pb::ConflictPath) -> R<Self> {
        Ok(ConflictPath {
            path: c.path,
            base: opt_oid(c.base, "conflict.base")?,
            ours: opt_oid(c.ours, "conflict.ours")?,
            theirs: opt_oid(c.theirs, "conflict.theirs")?,
        })
    }
}

impl From<ApiError> for pb::ApiError {
    fn from(e: ApiError) -> Self {
        use pb::api_error::{self as w, Error as E};
        pb::ApiError {
            error: Some(match e {
                ApiError::NotFound { what } => E::NotFound(w::NotFound { what }),
                ApiError::Collected { id } => E::Collected(w::Collected { id: cb(id) }),
                ApiError::InvalidPath { path, reason } => {
                    E::InvalidPath(w::InvalidPath { path, reason })
                }
                ApiError::InvalidChange { path, rule } => {
                    E::InvalidChange(w::InvalidChange { path, rule })
                }
                ApiError::UnknownParent { id } => E::UnknownParent(w::UnknownParent { id: cb(id) }),
                ApiError::MissingObjects { ids } => {
                    E::MissingObjects(w::MissingObjects { ids: obs(ids) })
                }
                ApiError::RefConflict { name, actual } => E::RefConflict(w::RefConflict {
                    name,
                    actual: actual.map(cb),
                }),
                ApiError::Conflict { paths, onto } => E::Conflict(w::Conflict {
                    paths: paths.into_iter().map(Into::into).collect(),
                    onto: cb(onto),
                }),
                ApiError::Contended { line, tip } => {
                    E::Contended(w::Contended { line, tip: cb(tip) })
                }
                ApiError::Rejected { policy, detail } => {
                    E::Rejected(w::Rejected { policy, detail })
                }
                ApiError::PermissionDenied { scope, path } => {
                    E::PermissionDenied(w::PermissionDenied { scope, path })
                }
                ApiError::LeaseExpired { workspace } => {
                    E::LeaseExpired(w::LeaseExpired { workspace })
                }
                ApiError::LimitExceeded { limit, max } => {
                    E::LimitExceeded(w::LimitExceeded { limit, max })
                }
                ApiError::CorruptObject { id } => E::CorruptObject(w::CorruptObject { id: ob(id) }),
                ApiError::Obliterated { id } => E::Obliterated(w::Obliterated { id: ob(id.0) }),
                ApiError::Unavailable { retry_after_ms } => {
                    E::Unavailable(w::Unavailable { retry_after_ms })
                }
                ApiError::Unauthenticated => E::Unauthenticated(w::Unauthenticated {}),
                ApiError::IncompatibleProtocol { server } => {
                    E::IncompatibleProtocol(w::IncompatibleProtocol { server })
                }
            }),
        }
    }
}

impl TryFrom<pb::ApiError> for ApiError {
    type Error = WireError;
    fn try_from(e: pb::ApiError) -> R<Self> {
        use pb::api_error::Error as E;
        Ok(match req(e.error, "error")? {
            E::NotFound(e) => ApiError::NotFound { what: e.what },
            E::Collected(e) => ApiError::Collected {
                id: cid(e.id, "id")?,
            },
            E::InvalidPath(e) => ApiError::InvalidPath {
                path: e.path,
                reason: e.reason,
            },
            E::InvalidChange(e) => ApiError::InvalidChange {
                path: e.path,
                rule: e.rule,
            },
            E::UnknownParent(e) => ApiError::UnknownParent {
                id: cid(e.id, "id")?,
            },
            E::MissingObjects(e) => ApiError::MissingObjects {
                ids: oids(e.ids, "ids")?,
            },
            E::RefConflict(e) => ApiError::RefConflict {
                name: e.name,
                actual: opt_cid(e.actual, "actual")?,
            },
            E::Conflict(e) => ApiError::Conflict {
                paths: e
                    .paths
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<R<_>>()?,
                onto: cid(e.onto, "onto")?,
            },
            E::Contended(e) => ApiError::Contended {
                line: e.line,
                tip: cid(e.tip, "tip")?,
            },
            E::Rejected(e) => ApiError::Rejected {
                policy: e.policy,
                detail: e.detail,
            },
            E::PermissionDenied(e) => ApiError::PermissionDenied {
                scope: e.scope,
                path: e.path,
            },
            E::LeaseExpired(e) => ApiError::LeaseExpired {
                workspace: e.workspace,
            },
            E::LimitExceeded(e) => ApiError::LimitExceeded {
                limit: e.limit,
                max: e.max,
            },
            E::CorruptObject(e) => ApiError::CorruptObject {
                id: oid(e.id, "id")?,
            },
            E::Obliterated(e) => ApiError::Obliterated {
                id: BlobId(oid(e.id, "id")?),
            },
            E::Unavailable(e) => ApiError::Unavailable {
                retry_after_ms: e.retry_after_ms,
            },
            E::Unauthenticated(_) => ApiError::Unauthenticated,
            E::IncompatibleProtocol(e) => ApiError::IncompatibleProtocol { server: e.server },
        })
    }
}

// ---- conflict resolution (WI-2312) ---------------------------------------

impl From<GetConflicts> for pb::GetConflictsRequest {
    fn from(r: GetConflicts) -> Self {
        pb::GetConflictsRequest {
            workspace: Some(r.workspace.into()),
            onto: r.onto.map(cb),
            page: Some(r.page.into()),
        }
    }
}

impl TryFrom<pb::GetConflictsRequest> for GetConflicts {
    type Error = WireError;
    fn try_from(r: pb::GetConflictsRequest) -> R<Self> {
        Ok(GetConflicts {
            workspace: req(r.workspace, "workspace")?.into(),
            onto: opt_cid(r.onto, "onto")?,
            page: page_from(r.page),
        })
    }
}

// ---- draft overlap (WI-2310) ----------------------------------------------

impl From<OverlappingDrafts> for pb::OverlappingDraftsRequest {
    fn from(r: OverlappingDrafts) -> Self {
        pb::OverlappingDraftsRequest {
            repo: r.repo,
            line: r.line,
            paths: r.paths.into_iter().map(|p| p.as_str().to_owned()).collect(),
            include_own: r.include_own,
            page: Some(r.page.into()),
        }
    }
}

impl TryFrom<pb::OverlappingDraftsRequest> for OverlappingDrafts {
    type Error = WireError;
    fn try_from(r: pb::OverlappingDraftsRequest) -> R<Self> {
        Ok(OverlappingDrafts {
            repo: r.repo,
            line: r.line,
            paths: paths(r.paths, "paths")?,
            include_own: r.include_own,
            page: page_from(r.page),
        })
    }
}

impl From<DraftOverlap> for pb::DraftOverlap {
    fn from(r: DraftOverlap) -> Self {
        pb::DraftOverlap {
            hits: r
                .hits
                .into_iter()
                .map(|h| pb::DraftHit {
                    path: h.path.as_str().to_owned(),
                    workspace: h.workspace,
                })
                .collect(),
            workspaces: r
                .workspaces
                .into_iter()
                .map(|w| pb::DraftWorkspace {
                    workspace: w.workspace,
                    owner: w.owner,
                    session: w.session,
                    work_item: w.work_item,
                    draft_age_ms: w.draft_age_ms,
                    lease_until_ms: w.lease_until_ms,
                })
                .collect(),
            next: r.next,
        }
    }
}

impl TryFrom<pb::DraftOverlap> for DraftOverlap {
    type Error = WireError;
    fn try_from(r: pb::DraftOverlap) -> R<Self> {
        Ok(DraftOverlap {
            hits: r
                .hits
                .into_iter()
                .map(|h| {
                    Ok(DraftHit {
                        path: path(h.path, "hits.path")?,
                        workspace: h.workspace,
                    })
                })
                .collect::<R<_>>()?,
            workspaces: r
                .workspaces
                .into_iter()
                .map(|w| DraftWorkspace {
                    workspace: w.workspace,
                    owner: w.owner,
                    session: w.session,
                    work_item: w.work_item,
                    draft_age_ms: w.draft_age_ms,
                    lease_until_ms: w.lease_until_ms,
                })
                .collect(),
            next: r.next,
        })
    }
}

fn conflict_reason(r: Option<ConflictReason>) -> i32 {
    (match r {
        None => pb::ConflictReason::Unspecified,
        Some(ConflictReason::Binary) => pb::ConflictReason::Binary,
        Some(ConflictReason::TooLarge) => pb::ConflictReason::TooLarge,
        Some(ConflictReason::Kind) => pb::ConflictReason::Kind,
        Some(ConflictReason::Clash) => pb::ConflictReason::Clash,
    }) as i32
}

fn conflict_reason_from(v: i32) -> R<Option<ConflictReason>> {
    match pb::ConflictReason::try_from(v) {
        Ok(pb::ConflictReason::Unspecified) => Ok(None),
        Ok(pb::ConflictReason::Binary) => Ok(Some(ConflictReason::Binary)),
        Ok(pb::ConflictReason::TooLarge) => Ok(Some(ConflictReason::TooLarge)),
        Ok(pb::ConflictReason::Kind) => Ok(Some(ConflictReason::Kind)),
        Ok(pb::ConflictReason::Clash) => Ok(Some(ConflictReason::Clash)),
        Err(_) => Err(bad("reason", "unknown conflict reason")),
    }
}

fn side(s: ConflictSide) -> pb::ConflictSide {
    pb::ConflictSide {
        id: ob(s.id),
        kind: entry_kind(s.kind),
    }
}

fn side_from(s: Option<pb::ConflictSide>, field: &'static str) -> R<Option<ConflictSide>> {
    s.map(|s| {
        Ok(ConflictSide {
            id: oid(s.id, field)?,
            kind: entry_kind_from(s.kind, field)?,
        })
    })
    .transpose()
}

impl From<ConflictView> for pb::ConflictView {
    fn from(r: ConflictView) -> Self {
        pb::ConflictView {
            path: r.path.as_str().to_owned(),
            base: r.base.map(side),
            ours: r.ours.map(side),
            theirs: r.theirs.map(side),
            text_mergeable: r.text_mergeable,
            markers: r.markers,
            reason: conflict_reason(r.reason),
        }
    }
}

impl TryFrom<pb::ConflictView> for ConflictView {
    type Error = WireError;
    fn try_from(r: pb::ConflictView) -> R<Self> {
        if r.markers
            .as_ref()
            .is_some_and(|m| m.len() > MAX_CONFLICT_MARKER_BYTES)
        {
            return Err(bad("markers", "over the marker cap"));
        }
        Ok(ConflictView {
            path: path(r.path, "path")?,
            base: side_from(r.base, "base")?,
            ours: side_from(r.ours, "ours")?,
            theirs: side_from(r.theirs, "theirs")?,
            text_mergeable: r.text_mergeable,
            markers: r.markers,
            reason: conflict_reason_from(r.reason)?,
        })
    }
}

impl From<Conflicts> for pb::Conflicts {
    fn from(r: Conflicts) -> Self {
        pb::Conflicts {
            onto: cb(r.onto),
            draft: r.draft.map(cb),
            paths: r.paths.into_iter().map(Into::into).collect(),
            next: r.next,
        }
    }
}

impl TryFrom<pb::Conflicts> for Conflicts {
    type Error = WireError;
    fn try_from(r: pb::Conflicts) -> R<Self> {
        Ok(Conflicts {
            onto: cid(r.onto, "onto")?,
            draft: opt_cid(r.draft, "draft")?,
            paths: r
                .paths
                .into_iter()
                .map(ConflictView::try_from)
                .collect::<R<_>>()?,
            next: r.next,
        })
    }
}

impl From<PathResolution> for pb::PathResolution {
    fn from(r: PathResolution) -> Self {
        use pb::path_resolution::put::Content;
        use pb::path_resolution::{Put, Resolution as Res};
        pb::PathResolution {
            path: r.path.as_str().to_owned(),
            resolution: Some(match r.resolution {
                Resolution::Put { kind, content } => Res::Put(Put {
                    kind: entry_kind(kind),
                    content: Some(match content {
                        ResolvedContent::Blob(id) => Content::Blob(ob(id.0)),
                        ResolvedContent::Inline(bytes) => Content::Inline(bytes),
                    }),
                }),
                Resolution::Delete => Res::Delete(pb::Empty {}),
            }),
        }
    }
}

impl TryFrom<pb::PathResolution> for PathResolution {
    type Error = WireError;
    fn try_from(r: pb::PathResolution) -> R<Self> {
        use pb::path_resolution::Resolution as Res;
        use pb::path_resolution::put::Content;
        let resolution = match req(r.resolution, "resolution")? {
            Res::Put(put) => Resolution::Put {
                kind: entry_kind_from(put.kind, "resolution.kind")?,
                content: match req(put.content, "resolution.content")? {
                    Content::Blob(id) => ResolvedContent::Blob(BlobId(oid(id, "resolution.blob")?)),
                    Content::Inline(bytes) => ResolvedContent::Inline(bytes),
                },
            },
            Res::Delete(_) => Resolution::Delete,
        };
        Ok(PathResolution {
            path: path(r.path, "resolution.path")?,
            resolution,
        })
    }
}

impl From<Resolve> for pb::ResolveRequest {
    fn from(r: Resolve) -> Self {
        pb::ResolveRequest {
            workspace: Some(r.workspace.into()),
            onto: cb(r.onto),
            expected_head: r.expected_head.map(cb),
            resolutions: r.resolutions.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::ResolveRequest> for Resolve {
    type Error = WireError;
    fn try_from(r: pb::ResolveRequest) -> R<Self> {
        Ok(Resolve {
            workspace: req(r.workspace, "workspace")?.into(),
            onto: cid(r.onto, "onto")?,
            expected_head: opt_cid(r.expected_head, "expected_head")?,
            resolutions: r
                .resolutions
                .into_iter()
                .map(PathResolution::try_from)
                .collect::<R<_>>()?,
        })
    }
}

impl From<Resolved> for pb::Resolved {
    fn from(r: Resolved) -> Self {
        pb::Resolved {
            workspace: Some(r.workspace.into()),
            head: cb(r.head),
            resolved_paths: ps(r.resolved_paths),
            merged_paths: ps(r.merged_paths),
        }
    }
}

impl TryFrom<pb::Resolved> for Resolved {
    type Error = WireError;
    fn try_from(r: pb::Resolved) -> R<Self> {
        Ok(Resolved {
            workspace: req(r.workspace, "workspace")?.try_into()?,
            head: cid(r.head, "head")?,
            resolved_paths: paths(r.resolved_paths, "resolved_paths")?,
            merged_paths: paths(r.merged_paths, "merged_paths")?,
        })
    }
}

#[cfg(test)]
mod tests;
