//! Shared fixtures for the CLI tests: a served repository, a `graft` command
//! aimed at it, and JSON schema validation.
//!
//! The served repository is a [`Fixture`]: graft-testkit's `SnapshotRepo`
//! keeps the history and the objects, and the fixture adds the reads that
//! `SnapshotRepo` does not answer (`Stat`, `ListTree`, `ReadFile`, `Log`,
//! ancestry, `Blame`), walking the same changesets. Blame is coarse: every
//! line of a file takes the changeset that introduced its current content.

#![allow(dead_code, unreachable_pub)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use assert_cmd::Command;
use graft_model::{BlobId, Changeset, ChangesetId, EntryKind, ObjectId, RepoPath, Tree, TreeId};
use graft_proto::ApiError;
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_testkit::server::{SnapshotRepo, TestServer};
use serde_json::Value;
use tokio::runtime::Runtime;

/// The repository name the fixture serves.
pub const REPO: &str = "demo";

/// The changesets of the fixture history, oldest first.
pub struct Commits {
    pub first: ChangesetId,
    pub second: ChangesetId,
    pub third: ChangesetId,
}

/// The served repository.
pub struct Fixture {
    repo: SnapshotRepo,
}

fn cx() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "fixture".to_owned(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: Vec::new(),
        },
        request_id: "fixture".to_owned(),
        deadline_ms: None,
    }
}

fn path(text: &str) -> RepoPath {
    RepoPath::new(text).expect("valid fixture path")
}

/// The error for a missing snapshot entry.
fn not_found(what: impl std::fmt::Display) -> ApiError {
    ApiError::NotFound {
        what: what.to_string(),
    }
}

impl Fixture {
    /// Builds the history on `main`: three changesets.
    pub async fn build() -> (Fixture, Commits) {
        let repo = SnapshotRepo::new();
        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        files.insert("README.md".into(), b"hello\n".to_vec());
        files.insert("src/lib.rs".into(), b"fn a() {}\n".to_vec());
        files.insert("assets/logo.bin".into(), vec![0xff, 0x00, 0x7f]);
        let first = repo.commit("main", &files).await;

        files.insert("src/lib.rs".into(), b"fn a() {}\nfn b() {}\n".to_vec());
        let second = repo.commit("main", &files).await;

        files.insert("docs/guide.md".into(), b"read me\n".to_vec());
        let third = repo.commit("main", &files).await;

        let fixture = Fixture { repo };
        (
            fixture,
            Commits {
                first,
                second,
                third,
            },
        )
    }

    /// The repository behind the fixture (workspaces, drafts, the fake PR
    /// bridge).
    pub fn repo(&self) -> &SnapshotRepo {
        &self.repo
    }

    /// The tree of `id`, from the object store.
    async fn tree(&self, id: TreeId) -> Option<Tree> {
        let payload = self.object(id.0).await?;
        Tree::decode(&payload).ok()
    }

    /// The payload of an object, verified against its id.
    async fn object(&self, id: ObjectId) -> Option<Vec<u8>> {
        let got = self
            .repo
            .fake()
            .get_objects(
                &cx(),
                GetObjects {
                    repo: String::new(),
                    ids: vec![id],
                },
            )
            .await
            .ok()?;
        let (_, frame) = got.found.first()?;
        frame.decode_verified(id).ok()
    }

    /// The entry at `path` in the snapshot `at`, if there is one.
    async fn entry(&self, at: ChangesetId, path: &RepoPath) -> Option<(EntryKind, ObjectId)> {
        let mut dir = self.repo.root(at)?;
        let comps: Vec<&str> = path.components().collect();
        for (i, name) in comps.iter().enumerate() {
            let tree = self.tree(dir).await?;
            let entry = tree.entries.iter().find(|e| e.name == *name)?;
            if i + 1 == comps.len() {
                return Some((entry.kind, entry.id));
            }
            if entry.kind != EntryKind::Tree {
                return None;
            }
            dir = TreeId(entry.id);
        }
        None
    }

    /// The changeset with `id`.
    fn changeset(&self, id: ChangesetId) -> Option<Changeset> {
        self.repo.fake().changeset(id)
    }

    /// A summary line for `id`.
    fn summary(&self, id: ChangesetId) -> ChangesetSummary {
        let c = self.changeset(id).expect("known changeset");
        ChangesetSummary {
            id,
            author: c.author.clone(),
            authored_at: c.authored_at,
            subject: c.message.lines().next().unwrap_or("").to_owned(),
            line_pos: None,
        }
    }

    /// Ancestors of `id` along first parents, newest first, including `id`.
    fn first_parent_chain(&self, id: ChangesetId) -> Vec<ChangesetId> {
        let mut out = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            out.push(c);
            cur = self.changeset(c).and_then(|cs| cs.parents.first().copied());
        }
        out
    }

    /// Every ancestor of `id` along any parent, including `id`.
    fn all_ancestors(&self, id: ChangesetId) -> Vec<ChangesetId> {
        let mut seen = vec![id];
        let mut i = 0;
        while i < seen.len() {
            if let Some(c) = self.changeset(seen[i]) {
                for p in c.parents {
                    if !seen.contains(&p) {
                        seen.push(p);
                    }
                }
            }
            i += 1;
        }
        seen
    }
}

impl ObjectApi for Fixture {
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        self.repo.get_objects(cx, req).await
    }

    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        self.repo.has_objects(cx, req).await
    }

    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        self.repo.put_objects(cx, req).await
    }
}

impl RepoApi for Fixture {
    async fn create_changeset(&self, cx: &CallContext, req: CreateChangeset) -> ApiResult<Created> {
        self.repo.create_changeset(cx, req).await
    }

    async fn get_changesets(&self, cx: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        self.repo.get_changesets(cx, req).await
    }

    async fn resolve_ref(&self, cx: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        self.repo.resolve_ref(cx, req).await
    }

    async fn list_refs(&self, cx: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        self.repo.list_refs(cx, req).await
    }

    async fn update_ref(&self, cx: &CallContext, req: UpdateRef) -> ApiResult<RefView> {
        self.repo.update_ref(cx, req).await
    }

    async fn create_line(&self, cx: &CallContext, req: CreateLine) -> ApiResult<RefView> {
        self.repo.create_line(cx, req).await
    }

    async fn stat(&self, _cx: &CallContext, req: PathAt) -> ApiResult<EntryView> {
        let (kind, id) = self
            .entry(req.at, &req.path)
            .await
            .ok_or_else(|| ApiError::NotFound {
                what: req.path.to_string(),
            })?;
        Ok(EntryView {
            path: req.path,
            kind,
            id,
        })
    }

    async fn list_tree(&self, _cx: &CallContext, req: ListTree) -> ApiResult<Entries> {
        let root: TreeId = match &req.path {
            None => self.repo.root(req.at).ok_or_else(|| not_found(req.at))?,
            Some(p) => match self.entry(req.at, p).await {
                Some((EntryKind::Tree, id)) => TreeId(id),
                _ => return Err(not_found(p)),
            },
        };
        let mut out = Vec::new();
        let mut stack = vec![(root, req.path.clone())];
        while let Some((id, prefix)) = stack.pop() {
            let Some(tree) = self.tree(id).await else {
                continue;
            };
            for e in tree.entries {
                let full = match &prefix {
                    None => path(&e.name),
                    Some(p) => p.join(&e.name).expect("valid name"),
                };
                if req.recursive && e.kind == EntryKind::Tree {
                    stack.push((TreeId(e.id), Some(full.clone())));
                }
                out.push(EntryView {
                    path: full,
                    kind: e.kind,
                    id: e.id,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Entries {
            entries: out,
            next: None,
        })
    }

    async fn read_file(&self, _cx: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        let (kind, id) = self
            .entry(req.at, &req.path)
            .await
            .ok_or_else(|| ApiError::NotFound {
                what: req.path.to_string(),
            })?;
        let data = self.object(id).await.ok_or(ApiError::NotFound {
            what: req.path.to_string(),
        })?;
        Ok(FileContent {
            kind,
            id: BlobId(id),
            data,
        })
    }

    async fn log(&self, _cx: &CallContext, req: Log) -> ApiResult<History> {
        let mut changesets = Vec::new();
        for id in self.first_parent_chain(req.from) {
            if Some(id) == req.until {
                break;
            }
            let touches = req.paths.is_empty()
                || self.changeset(id).is_some_and(|c| {
                    c.changes
                        .iter()
                        .any(|ch| req.paths.iter().any(|p| ch.path().starts_with(p)))
                });
            if touches {
                changesets.push(self.summary(id));
            }
        }
        let limit = req.page.limit as usize;
        let next = (changesets.len() > limit).then(|| "more".to_owned());
        changesets.truncate(limit);
        Ok(History { changesets, next })
    }

    async fn is_ancestor(&self, _cx: &CallContext, req: Pair) -> ApiResult<bool> {
        Ok(req.a == req.b || self.all_ancestors(req.b).contains(&req.a))
    }

    async fn merge_base(&self, _cx: &CallContext, req: Pair) -> ApiResult<Option<ChangesetId>> {
        let of_a = self.all_ancestors(req.a);
        Ok(self
            .all_ancestors(req.b)
            .into_iter()
            .find(|id| of_a.contains(id)))
    }

    async fn diff(&self, cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        self.repo.diff(cx, req).await
    }

    async fn blame(&self, _cx: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        let (_, blob) = self
            .entry(req.at, &req.path)
            .await
            .ok_or_else(|| ApiError::NotFound {
                what: req.path.to_string(),
            })?;
        // The changeset that introduced the current blob: walk back while the
        // file still holds it.
        let mut origin = req.at;
        for parent in self.first_parent_chain(req.at).into_iter().skip(1) {
            match self.entry(parent, &req.path).await {
                Some((_, id)) if id == blob => origin = parent,
                _ => break,
            }
        }
        let data = self.object(blob).await.unwrap_or_default();
        let Ok(text) = std::str::from_utf8(&data) else {
            return Ok(BlameResult::default());
        };
        let lines = text.lines().count() as u32;
        let (start, end) = req.lines.unwrap_or((0, lines));
        let end = end.min(lines);
        if start >= end {
            return Ok(BlameResult::default());
        }
        Ok(BlameResult {
            ranges: vec![BlameRange {
                start,
                end,
                origin: self.summary(origin),
            }],
        })
    }

    async fn rebase(&self, cx: &CallContext, req: Rebase) -> ApiResult<ChangesetId> {
        self.repo.rebase(cx, req).await
    }

    async fn revert(&self, cx: &CallContext, req: Revert) -> ApiResult<ChangesetId> {
        self.repo.revert(cx, req).await
    }

    async fn land(&self, cx: &CallContext, req: Land) -> ApiResult<Landed> {
        self.repo.land(cx, req).await
    }

    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        self.repo.attest(cx, req).await
    }

    async fn watch(&self, cx: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        self.repo.watch(cx, req).await
    }
}

impl WorkspaceApi for Fixture {
    async fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        self.repo.open_workspace(cx, req).await
    }

    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        self.repo.get_workspace(cx, req).await
    }

    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        self.repo.list_workspaces(cx, repo, page).await
    }

    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        self.repo.renew_lease(cx, req, lease_ms).await
    }

    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        self.repo.set_head(cx, req).await
    }

    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        self.repo.sync(cx, req, onto).await
    }

    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        self.repo.stack(cx, req, op).await
    }

    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        self.repo.close_workspace(cx, req, keep_drafts).await
    }

    async fn get_conflicts(
        &self,
        _cx: &graft_proto::CallContext,
        _req: graft_proto::types::GetConflicts,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Conflicts> {
        Err(graft_proto::ApiError::unimplemented("GetConflicts"))
    }

    async fn resolve(
        &self,
        _cx: &graft_proto::CallContext,
        _req: graft_proto::types::Resolve,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Resolved> {
        Err(graft_proto::ApiError::unimplemented("Resolve"))
    }
}

/// A runtime, a served fixture, and the history it holds.
pub struct Served {
    pub rt: Runtime,
    pub fixture: Arc<Fixture>,
    pub server: TestServer,
    pub url: String,
    pub commits: Commits,
}

/// Serves the fixture on a loopback socket. The runtime keeps the server's
/// tasks running while the tests drive the `graft` binary.
pub fn serve() -> Served {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (fixture, commits) = rt.block_on(Fixture::build());
    let fixture = Arc::new(fixture);
    let server = rt.block_on(TestServer::tcp_workspaces(fixture.clone()));
    let url = server.url().to_owned();
    Served {
        rt,
        fixture,
        server,
        url,
        commits,
    }
}

/// A `graft` command run in `cwd`, with the ambient endpoint and token
/// removed so only what a test passes takes effect.
pub fn graft(cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("graft").expect("graft binary");
    cmd.current_dir(cwd)
        .env_remove("GRAFT_ENDPOINT")
        .env_remove("GRAFT_TOKEN")
        .env("GRAFT_CONFIG", cwd.join(".graft-test-config.toml"))
        // Tests run inside Vogt sessions and as any user; keep the caller's
        // identity out so ids and output don't depend on it.
        .env_remove("GRAFT_SESSION_ID")
        .env_remove("GRAFT_WORK_ITEM")
        .env_remove("VOGT_SESSION_ID")
        .env_remove("VOGT_WORK_ITEM")
        .env_remove("GRAFT_AUTHOR_NAME")
        .env_remove("GRAFT_AUTHOR_EMAIL")
        .env_remove("GRAFT_AUTHOR_KIND")
        .env_remove("GRAFT_AUTHOR_DATE")
        .env("USER", "graft-test")
        .env("NO_COLOR", "1");
    cmd
}

/// A scratch directory for a test.
pub fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Asserts that public output does not expose internal planning references.
pub fn assert_no_internal_references(command: &[&str], output: &str) {
    const LITERALS: &[&str] = &[
        "pr bridge",
        "crates/",
        "owned by",
        "vogt",
        "trial",
        "graft-dev",
        "phase",
    ];
    let lowercase = output.to_ascii_lowercase();
    for leak in LITERALS {
        assert!(
            !lowercase.contains(leak),
            "graft {} output leaked {leak:?}:\n{output}",
            command.join(" ")
        );
    }
    for word in
        output.split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
    {
        let numbered = (word.starts_with("WI-")
            && word.len() > 3
            && word[3..]
                .chars()
                .all(|character| character.is_ascii_digit()))
            || ((word.starts_with('D') || word.starts_with('G'))
                && word.len() > 1
                && word[1..]
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit()));
        assert!(
            !numbered,
            "graft {} output leaked internal reference {word:?}:\n{output}",
            command.join(" ")
        );
    }
}

/// The schema file for a command's JSON output.
pub fn schema_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schemas")
        .join(format!("{name}.schema.json"))
}

/// Asserts that `value` satisfies the schema for `name`, reporting the first
/// violation with its location.
pub fn assert_valid(name: &str, value: &Value) {
    let text = std::fs::read_to_string(schema_path(name)).expect("schema file");
    let schema: Value = serde_json::from_str(&text).expect("schema is JSON");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    if let Err(error) = validator.validate(value) {
        panic!(
            "{name} output does not match its schema at {}: {error}\n{}",
            error.instance_path(),
            serde_json::to_string_pretty(value).unwrap()
        );
    }
}

/// Parses a command's standard output as JSON.
pub fn json_out(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}
