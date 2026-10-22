//! A recording, in-memory implementation of the API traits (work item G1.8).
//!
//! [`FakeApi`] answers `ObjectApi`, `AdminApi::import_changesets` and
//! `RepoApi::get_changesets` the way the server will: blobs are verified
//! against their ids, and every imported changeset is checked (parents
//! present, blobs present, store-free rules) and its root re-derived with
//! [`apply_changes_with`], so a client that computes a wrong root or id fails
//! here as it would against the real engine. Every call is recorded.
//!
//! The fake ignores the repository name: it holds one repository. Operations
//! it does not implement return `ApiError::Rejected { policy: "fake" }`.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use graft_model::{
    ApplyOptions, BlobId, Change, Changeset, ChangesetId, CheckOptions, EntryKind, ObjectId,
    ObjectKind, ObjectSource, SourceError, Tree, TreeId, apply_changes_with,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Frame, Principal, PrincipalKind};

/// Runs a future to completion. The fake never waits, so a no-op waker is
/// enough and tests need no async runtime.
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// A call context for an admin service principal.
pub fn admin_cx() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "test-admin".to_owned(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec!["admin".to_owned()],
        },
        request_id: "test".to_owned(),
        deadline_ms: None,
    }
}

/// One recorded call, with the ids it carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// `GetObjects` for these ids.
    GetObjects(Vec<ObjectId>),
    /// `HasObjects` for these ids.
    HasObjects(Vec<ObjectId>),
    /// `PutObjects` of these blobs.
    PutObjects(Vec<BlobId>),
    /// `ImportChangesets` of these changesets, in order.
    ImportChangesets(Vec<ChangesetId>),
    /// `GetChangesets` for these ids.
    GetChangesets(Vec<ChangesetId>),
    /// `CreateRepo` with this name.
    CreateRepo(String),
}

#[derive(Default)]
struct State {
    blobs: HashMap<BlobId, Vec<u8>>,
    changesets: HashMap<ChangesetId, (Changeset, u64)>,
    next_lsn: u64,
    calls: Vec<Call>,
}

/// The recording fake. See the module documentation.
#[derive(Default)]
pub struct FakeApi {
    state: Mutex<State>,
    trees: Mutex<HashMap<TreeId, Tree>>,
}

/// The fake's trees, as an [`ObjectSource`].
struct Trees<'a>(&'a Mutex<HashMap<TreeId, Tree>>);

impl ObjectSource for Trees<'_> {
    fn tree(&self, id: TreeId) -> impl Future<Output = Result<Tree, SourceError>> + Send {
        let found = self.0.lock().expect("trees lock").get(&id).cloned();
        async move { found.ok_or(SourceError::Missing(id)) }
    }
}

/// The empty snapshot's root, which the fake never stores.
fn empty_root() -> TreeId {
    Tree::default().id()
}

fn unsupported<T>() -> ApiResult<T> {
    Err(ApiError::Rejected {
        policy: "fake".to_owned(),
        detail: "not implemented by the fake API".to_owned(),
    })
}

fn invalid(path: Option<&graft_model::RepoPath>, rule: &str) -> ApiError {
    ApiError::InvalidChange {
        path: path.map(|p| p.as_str().to_owned()).unwrap_or_default(),
        rule: rule.to_owned(),
    }
}

impl FakeApi {
    /// An empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call so far, oldest first.
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The number of stored changesets.
    pub fn changeset_count(&self) -> usize {
        self.lock().changesets.len()
    }

    /// A stored changeset.
    pub fn changeset(&self, id: ChangesetId) -> Option<Changeset> {
        self.lock().changesets.get(&id).map(|(c, _)| c.clone())
    }

    /// Stored changesets in import (journal) order.
    pub fn changesets_in_order(&self) -> Vec<Changeset> {
        let state = self.lock();
        let mut all: Vec<&(Changeset, u64)> = state.changesets.values().collect();
        all.sort_by_key(|(_, lsn)| *lsn);
        all.into_iter().map(|(c, _)| c.clone()).collect()
    }

    /// A stored blob's bytes.
    pub fn blob(&self, id: BlobId) -> Option<Vec<u8>> {
        self.lock().blobs.get(&id).cloned()
    }

    /// The number of stored blobs.
    pub fn blob_count(&self) -> usize {
        self.lock().blobs.len()
    }

    /// Every entry of the snapshot rooted at `root`, as path → (kind, id),
    /// trees excluded. Materializes the whole snapshot: tests only.
    pub fn snapshot(&self, root: TreeId) -> BTreeMap<String, (EntryKind, ObjectId)> {
        let trees = self.trees.lock().expect("trees lock");
        let mut out = BTreeMap::new();
        if root == empty_root() {
            return out;
        }
        let mut stack = vec![(String::new(), root)];
        while let Some((prefix, id)) = stack.pop() {
            let tree = trees.get(&id).expect("snapshot tree is stored");
            for e in &tree.entries {
                let path = if prefix.is_empty() {
                    e.name.clone()
                } else {
                    format!("{prefix}/{}", e.name)
                };
                if e.kind.is_tree() {
                    stack.push((path, TreeId(e.id)));
                } else {
                    out.insert(path, (e.kind, e.id));
                }
            }
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("state lock")
    }

    fn record(&self, call: Call) {
        self.lock().calls.push(call);
    }

    /// Validates one changeset against the stored state (plus earlier
    /// changesets of the same batch, which are already stored).
    async fn import_one(&self, c: &Changeset) -> ApiResult<Created> {
        let id = c.id();
        if let Some((existing, lsn)) = self.lock().changesets.get(&id) {
            return Ok(Created {
                id,
                root: existing.root,
                lsn: *lsn,
                state: DraftState::Landed,
            });
        }
        c.check(&CheckOptions {
            allow_root: true,
            portable_paths: true,
            ..CheckOptions::default()
        })
        .map_err(|v| invalid(v.path.as_ref(), v.rule.as_str()))?;
        let base = {
            let state = self.lock();
            let mut base = None;
            for (i, p) in c.parents.iter().enumerate() {
                let Some((parent, _)) = state.changesets.get(p) else {
                    return Err(ApiError::UnknownParent { id: *p });
                };
                if i == 0 {
                    base = Some(parent.root);
                }
            }
            let missing: Vec<ObjectId> = c
                .changes
                .iter()
                .filter_map(|ch| match ch {
                    Change::Put { kind, id, .. }
                        if *kind != EntryKind::Submodule
                            && !state.blobs.contains_key(&BlobId(*id)) =>
                    {
                        Some(*id)
                    }
                    _ => None,
                })
                .collect();
            if !missing.is_empty() {
                return Err(ApiError::MissingObjects { ids: missing });
            }
            base.filter(|r| *r != empty_root())
        };
        let applied = apply_changes_with(
            &Trees(&self.trees),
            base,
            &c.changes,
            &ApplyOptions { case_clash: true },
        )
        .await
        .map_err(|e| match e {
            graft_model::ApplyError::Invalid(v) => invalid(v.path.as_ref(), v.rule.as_str()),
            graft_model::ApplyError::Source(e) => invalid(None, &format!("source: {e}")),
        })?;
        let root = applied.root.unwrap_or_else(empty_root);
        if root != c.root {
            return Err(invalid(None, "root-mismatch"));
        }
        self.trees
            .lock()
            .expect("trees lock")
            .extend(applied.new_trees);
        let mut state = self.lock();
        state.next_lsn += 1;
        let lsn = state.next_lsn;
        state.changesets.insert(id, (c.clone(), lsn));
        Ok(Created {
            id,
            root,
            lsn,
            state: DraftState::Landed,
        })
    }
}

impl ObjectApi for FakeApi {
    async fn get_objects(&self, _cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        self.record(Call::GetObjects(req.ids.clone()));
        let mut out = Objects::default();
        for id in req.ids {
            let blob = self.lock().blobs.get(&BlobId(id)).cloned();
            let tree = self
                .trees
                .lock()
                .expect("trees lock")
                .get(&TreeId(id))
                .cloned();
            match (blob, tree) {
                (Some(bytes), _) => out
                    .found
                    .push((id, Frame::encode(ObjectKind::Blob, &bytes))),
                (None, Some(tree)) => out
                    .found
                    .push((id, Frame::encode(ObjectKind::Tree, &tree.encode()))),
                (None, None) => out.missing.push(id),
            }
        }
        Ok(out)
    }

    async fn has_objects(&self, _cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        self.record(Call::HasObjects(req.ids.clone()));
        let state = self.lock();
        let trees = self.trees.lock().expect("trees lock");
        Ok(req
            .ids
            .into_iter()
            .filter(|id| {
                !state.blobs.contains_key(&BlobId(*id)) && !trees.contains_key(&TreeId(*id))
            })
            .collect())
    }

    async fn put_objects(&self, _cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        let mut stored = Vec::with_capacity(req.blobs.len());
        let mut decoded = Vec::with_capacity(req.blobs.len());
        for frame in &req.blobs {
            if frame.kind != ObjectKind::Blob {
                return Err(invalid(None, "put-objects-blobs-only"));
            }
            let bytes = frame.decode().map_err(|_| invalid(None, "frame"))?;
            let id = BlobId::hash(&bytes);
            stored.push(id);
            decoded.push((id, bytes));
        }
        self.record(Call::PutObjects(stored.clone()));
        let mut state = self.lock();
        for (id, bytes) in decoded {
            state.blobs.entry(id).or_insert(bytes);
        }
        Ok(stored.into_iter().map(|b| b.0).collect())
    }
}

impl AdminApi for FakeApi {
    async fn create_repo(&self, _cx: &CallContext, name: String) -> ApiResult<()> {
        self.record(Call::CreateRepo(name));
        Ok(())
    }

    async fn import_changesets(
        &self,
        _cx: &CallContext,
        req: ImportChangesets,
    ) -> ApiResult<Vec<Created>> {
        self.record(Call::ImportChangesets(
            req.changesets.iter().map(Changeset::id).collect(),
        ));
        let mut out = Vec::with_capacity(req.changesets.len());
        for c in &req.changesets {
            out.push(self.import_one(c).await?);
        }
        Ok(out)
    }

    async fn gc(&self, _cx: &CallContext, _req: Gc) -> ApiResult<GcReport> {
        unsupported()
    }

    async fn mint_token(&self, _cx: &CallContext, _req: MintToken) -> ApiResult<String> {
        unsupported()
    }
}

impl RepoApi for FakeApi {
    async fn create_changeset(
        &self,
        _cx: &CallContext,
        _req: CreateChangeset,
    ) -> ApiResult<Created> {
        unsupported()
    }

    async fn get_changesets(&self, _cx: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        self.record(Call::GetChangesets(req.ids.clone()));
        let state = self.lock();
        let mut out = Changesets::default();
        for id in req.ids {
            match state.changesets.get(&id) {
                Some((c, lsn)) => out.found.push(ChangesetView {
                    changeset: c.clone(),
                    lsn: *lsn,
                    state: DraftState::Landed,
                    superseded_by: None,
                }),
                None => out.missing.push(id),
            }
        }
        Ok(out)
    }

    async fn resolve_ref(&self, _cx: &CallContext, _req: ResolveRef) -> ApiResult<RefView> {
        unsupported()
    }

    async fn list_refs(&self, _cx: &CallContext, _req: ListRefs) -> ApiResult<Vec<RefView>> {
        unsupported()
    }

    async fn update_ref(&self, _cx: &CallContext, _req: UpdateRef) -> ApiResult<RefView> {
        unsupported()
    }

    async fn stat(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<EntryView> {
        unsupported()
    }

    async fn list_tree(&self, _cx: &CallContext, _req: ListTree) -> ApiResult<Entries> {
        unsupported()
    }

    async fn read_file(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<FileContent> {
        unsupported()
    }

    async fn log(&self, _cx: &CallContext, _req: Log) -> ApiResult<History> {
        unsupported()
    }

    async fn is_ancestor(&self, _cx: &CallContext, _req: Pair) -> ApiResult<bool> {
        unsupported()
    }

    async fn merge_base(&self, _cx: &CallContext, _req: Pair) -> ApiResult<Option<ChangesetId>> {
        unsupported()
    }

    async fn diff(&self, _cx: &CallContext, _req: Diff) -> ApiResult<DiffResult> {
        unsupported()
    }

    async fn blame(&self, _cx: &CallContext, _req: Blame) -> ApiResult<BlameResult> {
        unsupported()
    }

    async fn rebase(&self, _cx: &CallContext, _req: Rebase) -> ApiResult<ChangesetId> {
        unsupported()
    }

    async fn revert(&self, _cx: &CallContext, _req: Revert) -> ApiResult<ChangesetId> {
        unsupported()
    }

    async fn land(&self, _cx: &CallContext, _req: Land) -> ApiResult<Landed> {
        unsupported()
    }

    async fn attest(&self, _cx: &CallContext, _req: Attest) -> ApiResult<()> {
        unsupported()
    }

    async fn watch(&self, _cx: &CallContext, _req: Watch) -> ApiResult<JournalPage> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use graft_model::{Author, AuthorKind, Metadata, RepoPath, Timestamp};

    use super::*;

    fn put(path: &str, bytes: &[u8]) -> Change {
        Change::Put {
            path: RepoPath::new(path).unwrap(),
            kind: EntryKind::File,
            id: BlobId::hash(bytes).0,
        }
    }

    fn changeset(parents: Vec<ChangesetId>, root: TreeId, changes: Vec<Change>) -> Changeset {
        Changeset {
            parents,
            root,
            changes,
            author: Author {
                name: "a".into(),
                email: "a@example.com".into(),
                kind: AuthorKind::Human,
            },
            authored_at: Timestamp {
                ms: 0,
                offset_min: 0,
            },
            message: "m".into(),
            metadata: Metadata::new(),
        }
    }

    fn upload(api: &FakeApi, bytes: &[u8]) {
        block_on(api.put_objects(
            &admin_cx(),
            PutObjects {
                repo: "r".into(),
                blobs: vec![Frame::encode(ObjectKind::Blob, bytes)],
            },
        ))
        .unwrap();
    }

    fn root_of(changes: &[Change]) -> TreeId {
        let source = Trees(&Mutex::new(HashMap::new()));
        block_on(apply_changes_with(
            &source,
            None,
            changes,
            &ApplyOptions::default(),
        ))
        .unwrap()
        .root
        .unwrap()
    }

    fn import(api: &FakeApi, c: Changeset) -> ApiResult<Vec<Created>> {
        block_on(api.import_changesets(
            &admin_cx(),
            ImportChangesets {
                repo: "r".into(),
                changesets: vec![c],
            },
        ))
    }

    #[test]
    fn imports_a_valid_root_and_records_the_calls() {
        let api = FakeApi::new();
        upload(&api, b"x");
        let changes = vec![put("a/b", b"x")];
        let c = changeset(vec![], root_of(&changes), changes);
        let created = import(&api, c.clone()).unwrap();
        assert_eq!(created[0].id, c.id());
        assert_eq!(api.changeset(c.id()), Some(c.clone()));
        assert_eq!(api.snapshot(c.root).len(), 1);
        assert_eq!(
            api.calls(),
            vec![
                Call::PutObjects(vec![BlobId::hash(b"x")]),
                Call::ImportChangesets(vec![c.id()]),
            ]
        );
        // Idempotent.
        assert_eq!(import(&api, c.clone()).unwrap()[0].lsn, created[0].lsn);
    }

    #[test]
    fn rejects_a_wrong_root_missing_blob_and_unknown_parent() {
        let api = FakeApi::new();
        let changes = vec![put("a", b"x")];
        let c = changeset(vec![], root_of(&changes), changes.clone());
        assert!(matches!(
            import(&api, c.clone()),
            Err(ApiError::MissingObjects { .. })
        ));
        upload(&api, b"x");
        let wrong = changeset(vec![], empty_root(), changes.clone());
        assert!(matches!(
            import(&api, wrong),
            Err(ApiError::InvalidChange { rule, .. }) if rule == "root-mismatch"
        ));
        let orphan = changeset(vec![c.id()], c.root, changes);
        assert!(matches!(
            import(&api, orphan),
            Err(ApiError::UnknownParent { .. })
        ));
    }

    #[test]
    fn serves_blobs_and_trees() {
        let api = FakeApi::new();
        upload(&api, b"x");
        let changes = vec![put("a", b"x")];
        let c = changeset(vec![], root_of(&changes), changes);
        import(&api, c.clone()).unwrap();
        let got = block_on(api.get_objects(
            &admin_cx(),
            GetObjects {
                repo: "r".into(),
                ids: vec![c.root.0, BlobId::hash(b"x").0, BlobId::hash(b"y").0],
            },
        ))
        .unwrap();
        assert_eq!(got.found.len(), 2);
        assert_eq!(got.missing, vec![BlobId::hash(b"y").0]);
        let missing = block_on(api.has_objects(
            &admin_cx(),
            HasObjects {
                repo: "r".into(),
                ids: vec![c.root.0, BlobId::hash(b"y").0],
            },
        ))
        .unwrap();
        assert_eq!(missing, vec![BlobId::hash(b"y").0]);
    }
}
