//! An in-memory API for the MCP tests. It implements the `graft-proto` traits
//! over a small fixed repository. The standard fixture has no files; the
//! path-bearing tests use [`Fake::with_files`], which adds a tree to `main`.
//! Diffs that name paths are not modelled.

#![allow(dead_code, unreachable_pub)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use graft_model::{
    Author, AuthorKind, BlobId, Changeset, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind,
    RepoPath, Timestamp, TreeId,
};
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi, WorkspaceApi};

/// A changeset id from one repeated byte.
pub fn cid(byte: u8) -> ChangesetId {
    ChangesetId(ObjectId([byte; 32]))
}

/// A text file with one line per entry, each line tagged with the changeset
/// that introduced it.
fn file(lines: &[(&str, ChangesetId)]) -> FileSpec {
    let mut data = Vec::new();
    for (text, _) in lines {
        data.extend_from_slice(text.as_bytes());
        data.push(b'\n');
    }
    FileSpec {
        data,
        origins: lines.iter().map(|(_, origin)| *origin).collect(),
    }
}

pub fn principal() -> Principal {
    Principal {
        subject: "test-agent".into(),
        kind: PrincipalKind::Agent,
        session: None,
        work_item: None,
        scopes: vec!["repo:read".into()],
    }
}

/// A file in the fixture tree: its bytes and, per line, the changeset that
/// introduced the line.
struct FileSpec {
    data: Vec<u8>,
    origins: Vec<ChangesetId>,
}

/// A fake repository: a linear history `c1 <- c2 <- c3` on `main`.
pub struct Fake {
    changesets: HashMap<ChangesetId, Changeset>,
    refs: HashMap<String, ChangesetId>,
    /// Changesets in order, oldest first.
    order: Vec<ChangesetId>,
    /// The tree every snapshot shows, by path. Empty unless built with
    /// [`Fake::with_files`].
    files: BTreeMap<String, FileSpec>,
}

impl Fake {
    /// The standard fixture: three changesets, one ref `main` at the last.
    pub fn standard() -> Arc<Self> {
        Self::with_authors(3, |i| format!("author {i}"), |_| "short subject".into())
    }

    /// The standard history with a tree on every snapshot: `README.md`,
    /// `src/lib.rs` and `src/util/mod.rs`. Each line of `README.md` names the
    /// changeset that introduced it, so blame has several origins.
    pub fn with_files() -> Arc<Self> {
        let mut fake = Self::linear(3, |i| format!("author {i}"), |_| "short subject".into());
        let c = cid;
        fake.files.insert(
            "README.md".into(),
            file(&[
                ("# fixture", c(1)),
                ("", c(1)),
                ("A repository for the MCP tests.", c(2)),
                ("Each line names the changeset that added it.", c(2)),
                ("Three changesets, one branch.", c(3)),
                ("", c(3)),
            ]),
        );
        fake.files
            .insert("src/lib.rs".into(), file(&[("pub mod util;", c(2))]));
        fake.files.insert(
            "src/util/mod.rs".into(),
            file(&[("pub fn id() -> u8 { 1 }", c(3))]),
        );
        Arc::new(fake)
    }

    /// A linear chain of `count` changesets (`cid(1)` to `cid(count)`), with
    /// `main` at the last one. Each author name and subject comes from
    /// `name` and `subject`, so tests can push fields to their clip limits.
    pub fn with_authors(
        count: u8,
        name: impl Fn(u8) -> String,
        subject: impl Fn(u8) -> String,
    ) -> Arc<Self> {
        Arc::new(Self::linear(count, name, subject))
    }

    fn linear(count: u8, name: impl Fn(u8) -> String, subject: impl Fn(u8) -> String) -> Self {
        let mut fake = Fake {
            changesets: HashMap::new(),
            refs: HashMap::new(),
            order: Vec::new(),
            files: BTreeMap::new(),
        };
        for i in 1..=count {
            let id = cid(i);
            let parents = if i == 1 { vec![] } else { vec![cid(i - 1)] };
            let changeset = Changeset {
                parents,
                root: TreeId(ObjectId([0x80 + i; 32])),
                changes: vec![],
                author: Author {
                    name: name(i),
                    email: format!("a{i}@example.test"),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000 + i64::from(i) * 60_000,
                    offset_min: 0,
                },
                message: format!("{}\n\nbody {i}", subject(i)),
                metadata: Metadata::from([("graft.test".to_owned(), format!("run {i}"))]),
            };
            fake.changesets.insert(id, changeset);
            fake.order.push(id);
        }
        fake.refs.insert("main".into(), cid(count));
        fake
    }

    fn unavailable<T>() -> ApiResult<T> {
        Err(ApiError::Unavailable { retry_after_ms: 0 })
    }

    fn check_at(&self, at: ChangesetId) -> ApiResult<()> {
        if self.changesets.contains_key(&at) {
            return Ok(());
        }
        Err(ApiError::NotFound {
            what: format!("changeset {at}"),
        })
    }

    fn file(&self, path: &RepoPath) -> ApiResult<&FileSpec> {
        self.files
            .get(path.as_str())
            .ok_or_else(|| ApiError::NotFound {
                what: format!("file {path}"),
            })
    }

    /// The entry for a file in the fixture tree.
    fn file_entry(path: &str, spec: &FileSpec) -> EntryView {
        EntryView {
            path: RepoPath::new(path).expect("fixture paths are valid"),
            kind: EntryKind::File,
            id: BlobId::hash(&spec.data).0,
        }
    }

    /// The entry for a directory in the fixture tree.
    fn dir_entry(path: &str) -> EntryView {
        EntryView {
            path: RepoPath::new(path).expect("fixture paths are valid"),
            kind: EntryKind::Tree,
            id: ObjectId::hash(ObjectKind::Tree, path.as_bytes()),
        }
    }

    fn summary(&self, id: ChangesetId) -> ChangesetSummary {
        let cs = &self.changesets[&id];
        let subject = cs.message.lines().next().unwrap_or("").to_owned();
        ChangesetSummary {
            id,
            author: cs.author.clone(),
            authored_at: cs.authored_at,
            subject,
            line_pos: None,
        }
    }
}

impl RepoApi for Fake {
    async fn create_changeset(&self, _: &CallContext, _: CreateChangeset) -> ApiResult<Created> {
        Self::unavailable()
    }

    async fn get_changesets(&self, _: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        let mut out = Changesets::default();
        for id in req.ids {
            match self.changesets.get(&id) {
                Some(cs) => out.found.push(ChangesetView {
                    changeset: cs.clone(),
                    lsn: self.order.iter().position(|x| *x == id).unwrap_or(0) as u64 + 1,
                    state: DraftState::Landed,
                    superseded_by: None,
                }),
                None => out.missing.push(id),
            }
        }
        Ok(out)
    }

    async fn resolve_ref(&self, _: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        match self.refs.get(&req.name) {
            Some(target) => Ok(RefView {
                name: req.name,
                target: *target,
                version: 1,
                kind: RefKind::Line,
            }),
            None => Err(ApiError::NotFound {
                what: format!("ref {}", req.name),
            }),
        }
    }

    async fn list_refs(&self, _: &CallContext, _: ListRefs) -> ApiResult<Vec<RefView>> {
        Self::unavailable()
    }

    async fn update_ref(&self, _: &CallContext, _: UpdateRef) -> ApiResult<RefView> {
        Self::unavailable()
    }

    async fn stat(&self, _: &CallContext, _: PathAt) -> ApiResult<EntryView> {
        Self::unavailable()
    }

    async fn list_tree(&self, _: &CallContext, req: ListTree) -> ApiResult<Entries> {
        self.check_at(req.at)?;
        let prefix = req
            .path
            .as_ref()
            .map(|p| format!("{p}/"))
            .unwrap_or_default();
        let mut entries = Vec::new();
        let mut dirs = BTreeSet::new();
        for (path, spec) in &self.files {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((dir, _)) if !req.recursive => {
                    if dirs.insert(format!("{prefix}{dir}")) {
                        entries.push(Self::dir_entry(&format!("{prefix}{dir}")));
                    }
                }
                _ => entries.push(Self::file_entry(path, spec)),
            }
        }
        if let Some(dir) = req.path.as_ref().filter(|_| entries.is_empty()) {
            return Err(ApiError::NotFound {
                what: format!("directory {dir}"),
            });
        }
        entries.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
        let start = match &req.page.token {
            Some(token) => token.parse::<usize>().map_err(|_| ApiError::NotFound {
                what: "cursor".into(),
            })?,
            None => 0,
        };
        let start = start.min(entries.len());
        let end = (start + req.page.limit as usize).min(entries.len());
        let next = (end < entries.len()).then(|| end.to_string());
        Ok(Entries {
            entries: entries[start..end].to_vec(),
            next,
        })
    }

    async fn read_file(&self, _: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        self.check_at(req.at)?;
        let spec = self.file(&req.path)?;
        Ok(FileContent {
            kind: EntryKind::File,
            id: BlobId::hash(&spec.data),
            data: spec.data.clone(),
        })
    }

    async fn log(&self, _: &CallContext, req: Log) -> ApiResult<History> {
        let start = self
            .order
            .iter()
            .position(|id| *id == req.from)
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {}", req.from),
            })?;
        // The cursor is the order index of the next changeset to return.
        let first = match &req.page.token {
            Some(token) => token.parse::<usize>().map_err(|_| ApiError::NotFound {
                what: "cursor".into(),
            })?,
            None => start,
        };
        let indices: Vec<usize> = (0..=first).rev().collect();
        let take = (req.page.limit as usize).min(indices.len());
        let changesets = indices[..take]
            .iter()
            .map(|i| self.summary(self.order[*i]))
            .collect();
        let next = indices.get(take).map(|i| i.to_string());
        Ok(History { changesets, next })
    }

    async fn is_ancestor(&self, _: &CallContext, _: Pair) -> ApiResult<bool> {
        Self::unavailable()
    }

    async fn merge_base(&self, _: &CallContext, _: Pair) -> ApiResult<Option<ChangesetId>> {
        Self::unavailable()
    }

    async fn diff(&self, _: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        // The fixture has no file changes, so a diff between known
        // snapshots is empty. Paths are not modelled.
        if !req.paths.is_empty() {
            return Self::unavailable();
        }
        for id in [req.from, req.to] {
            if !self.changesets.contains_key(&id) {
                return Err(ApiError::NotFound {
                    what: format!("changeset {id}"),
                });
            }
        }
        Ok(DiffResult::default())
    }

    async fn blame(&self, _: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        self.check_at(req.at)?;
        let spec = self.file(&req.path)?;
        let (start, end) = req.lines.unwrap_or((0, u32::MAX));
        let end = end.min(spec.origins.len() as u32);
        let mut ranges: Vec<BlameRange> = Vec::new();
        for line in start..end {
            let origin = spec.origins[line as usize];
            match ranges.last_mut() {
                Some(run) if run.origin.id == origin && run.end == line => run.end = line + 1,
                _ => ranges.push(BlameRange {
                    start: line,
                    end: line + 1,
                    origin: self.summary(origin),
                }),
            }
        }
        Ok(BlameResult { ranges })
    }

    async fn rebase(&self, _: &CallContext, _: Rebase) -> ApiResult<ChangesetId> {
        Self::unavailable()
    }

    async fn revert(&self, _: &CallContext, _: Revert) -> ApiResult<ChangesetId> {
        Self::unavailable()
    }

    async fn land(&self, _: &CallContext, _: Land) -> ApiResult<Landed> {
        Self::unavailable()
    }

    async fn attest(&self, _: &CallContext, _: Attest) -> ApiResult<()> {
        Self::unavailable()
    }

    async fn watch(&self, _: &CallContext, _: Watch) -> ApiResult<JournalPage> {
        Self::unavailable()
    }
}

impl WorkspaceApi for Fake {
    async fn open_workspace(&self, _: &CallContext, _: OpenWorkspace) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn get_workspace(&self, _: &CallContext, _: WorkspaceRef) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn list_workspaces(
        &self,
        _: &CallContext,
        _: String,
        _: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        Self::unavailable()
    }

    async fn renew_lease(
        &self,
        _: &CallContext,
        _: WorkspaceRef,
        _: u64,
    ) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn set_head(&self, _: &CallContext, _: SetHead) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn sync(
        &self,
        _: &CallContext,
        _: WorkspaceRef,
        _: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        Self::unavailable()
    }

    async fn stack(
        &self,
        _: &CallContext,
        _: WorkspaceRef,
        _: StackOp,
    ) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn close_workspace(&self, _: &CallContext, _: WorkspaceRef, _: bool) -> ApiResult<()> {
        Self::unavailable()
    }
}

use graft_mcp::McpServer;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};

/// Serves `api` over an in-memory duplex with stdio framing and returns a
/// connected client.
pub async fn connect(api: Arc<Fake>) -> RunningService<RoleClient, ()> {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = McpServer::new(api, principal());
    tokio::spawn(async move {
        let _ = server.serve_io(server_read, server_write).await;
    });
    let (client_read, client_write) = tokio::io::split(client_io);
    ().serve((client_read, client_write))
        .await
        .expect("the MCP handshake completes")
}

/// Calls a tool with a JSON object of arguments.
pub async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    args: serde_json::Value,
) -> CallToolResult {
    let arguments = args.as_object().expect("arguments are an object").clone();
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await
        .expect("the call completes")
}

/// Compares `actual` with `tests/golden/<name>.json`, byte for byte. With
/// `GRAFT_MCP_BLESS=1` the golden is written instead; review the diff before
/// committing it.
pub fn assert_golden(name: &str, actual: &serde_json::Value) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let pretty = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
    if std::env::var_os("GRAFT_MCP_BLESS").is_some() {
        std::fs::write(&path, &pretty).expect("write golden");
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no golden {name}; run with GRAFT_MCP_BLESS=1"));
    assert_eq!(expected, pretty, "golden {name} differs");
}
