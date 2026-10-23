//! Uncontended `Land` latency (work item G3.1: p50 at most 15 ms on the
//! graft-dev VM). The engine runs over the SQLite metadata store and the
//! filesystem object store, as the server does. Each sample creates a
//! one-file draft (not timed) and lands it (timed):
//!
//! - `fast-forward`: a `direct` line, the draft on its tip;
//! - `attested`: `main` opted into D6, the draft on its tip with both checks
//!   attested;
//! - `rebase`: a `direct` line that moved since the draft was made, so the
//!   land rebases it first.
//!
//! The repository starts with 2,000 files in 40 directories. Run it with
//!
//! ```sh
//! cargo bench -p graft-engine --bench land
//! GRAFT_BENCH_DIR=/srv/graft-bench cargo bench -p graft-engine --bench land
//! ```
//!
//! `GRAFT_BENCH_DIR` puts the stores on the volume the server would use
//! (default: the system temp directory). The p50 is the `median` in
//! `target/criterion/land/<bench>/new/estimates.json` (the console line
//! shows the mean).

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use graft_engine::{Engine, EngineConfig, LineConfig};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind, RepoPath,
    SystemClock, Timestamp,
};
use graft_proto::types::{
    Attest, CreateChangeset, Land, Landed, MergeMode, PutObjects, RefKind, UpdateRef,
};
use graft_proto::{AdminApi, CallContext, Frame, ObjectApi, Principal, PrincipalKind, RepoApi};
use graft_store_fs::{FsMode, FsObjectStore};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};

struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        thread::park();
    }
}

type Bench = Engine<SqliteMetaStore, FsObjectStore>;

/// A repository on disk, removed when dropped.
struct Fixture {
    engine: Bench,
    admin: CallContext,
    ci: CallContext,
    dir: PathBuf,
    next: u64,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("path")
}

fn context(kind: PrincipalKind) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "bench".to_owned(),
            kind,
            session: None,
            work_item: None,
            scopes: vec!["repo:bench:admin".to_owned()],
        },
        request_id: "bench".to_owned(),
        deadline_ms: None,
    }
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::var_os("GRAFT_BENCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("graft-land-bench-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("bench dir");
        let meta =
            SqliteMetaStore::open(SqliteConfig::new(dir.join("meta.sqlite"))).expect("sqlite");
        let objects =
            FsObjectStore::open(dir.join("objects"), FsMode::Authoritative).expect("objects");
        let engine = Engine::new(
            Arc::new(meta),
            Arc::new(objects),
            Arc::new(SystemClock),
            // `main` opted into D6 (the default is `direct`, D22).
            EngineConfig {
                lines: vec![LineConfig::d6("main")],
                ..EngineConfig::default()
            },
        );
        let mut f = Self {
            engine,
            admin: context(PrincipalKind::Human),
            ci: context(PrincipalKind::Ci),
            dir,
            next: 0,
        };
        block_on(f.engine.create_repo(&f.admin, "bench".to_owned())).expect("repo");
        let files: Vec<(String, Vec<u8>)> = (0..2000)
            .map(|i| {
                (
                    format!("d{:02}/f{i:04}.rs", i % 40),
                    format!("fn f{i}() {{}}\n").into_bytes(),
                )
            })
            .collect();
        let root = f.commit(None, files);
        for line in ["trunk", "main"] {
            block_on(f.engine.update_ref(
                &f.admin,
                UpdateRef {
                    repo: "bench".to_owned(),
                    name: line.to_owned(),
                    expected: None,
                    new: root,
                    kind: RefKind::Line,
                },
            ))
            .expect("line");
        }
        f
    }

    fn commit(
        &mut self,
        parent: Option<ChangesetId>,
        files: Vec<(String, Vec<u8>)>,
    ) -> ChangesetId {
        for chunk in files.chunks(500) {
            let blobs = chunk
                .iter()
                .map(|(_, b)| Frame::encode(ObjectKind::Blob, b))
                .collect();
            block_on(self.engine.put_objects(
                &self.admin,
                PutObjects {
                    repo: "bench".to_owned(),
                    blobs,
                },
            ))
            .expect("upload");
        }
        let mut changes: Vec<Change> = files
            .iter()
            .map(|(p, b)| Change::Put {
                path: path(p),
                kind: EntryKind::File,
                id: ObjectId::hash(ObjectKind::Blob, b),
            })
            .collect();
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        self.next += 1;
        block_on(self.engine.create_changeset(
            &self.admin,
            CreateChangeset {
                repo: "bench".to_owned(),
                parents: parent.into_iter().collect(),
                changes,
                author: Author {
                    name: "Bench".to_owned(),
                    email: "bench@example.com".to_owned(),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000 + self.next as i64,
                    offset_min: 0,
                },
                message: format!("change {}", self.next),
                metadata: Metadata::new(),
                inline_blobs: Vec::new(),
                workspace: None,
            },
        ))
        .expect("commit")
        .id
    }

    /// A draft editing one existing file, on `parent`.
    fn draft(&mut self, parent: ChangesetId) -> ChangesetId {
        let i = self.next % 2000;
        let file = format!("d{:02}/f{i:04}.rs", i % 40);
        let data = format!("fn f{i}() {{ /* {} */ }}\n", self.next).into_bytes();
        self.commit(Some(parent), vec![(file, data)])
    }

    fn tip(&self, line: &str) -> ChangesetId {
        block_on(self.engine.resolve_ref(
            &self.admin,
            graft_proto::types::ResolveRef {
                repo: "bench".to_owned(),
                name: line.to_owned(),
            },
        ))
        .expect("tip")
        .target
    }

    fn attest(&self, id: ChangesetId) {
        for check in ["ci/build", "ci/test"] {
            block_on(self.engine.attest(
                &self.ci,
                Attest {
                    repo: "bench".to_owned(),
                    changeset: id,
                    check: check.to_owned(),
                    passed: true,
                    url: "https://ci.example/run".to_owned(),
                },
            ))
            .expect("attest");
        }
    }

    /// Lands `head` on `line`, returning how long the call took.
    fn land(&self, head: ChangesetId, line: &str) -> Duration {
        let req = Land {
            repo: "bench".to_owned(),
            head,
            line: line.to_owned(),
            squash: false,
            merge: MergeMode::File,
            deadline_ms: None,
        };
        let start = Instant::now();
        let landed = block_on(self.engine.land(&self.admin, req));
        let took = start.elapsed();
        assert!(matches!(landed, Ok(Landed::Landed { .. })), "{landed:?}");
        took
    }
}

fn uncontended_land(c: &mut Criterion) {
    let mut group = c.benchmark_group("land");
    group.sample_size(200);

    let mut f = Fixture::new("ff");
    group.bench_function("fast-forward", |b| {
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| {
                    let draft = f.draft(f.tip("trunk"));
                    f.land(draft, "trunk")
                })
                .sum()
        })
    });

    let mut f = Fixture::new("attested");
    group.bench_function("attested", |b| {
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| {
                    let draft = f.draft(f.tip("main"));
                    f.attest(draft);
                    f.land(draft, "main")
                })
                .sum()
        })
    });

    let mut f = Fixture::new("rebase");
    group.bench_function("rebase", |b| {
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| {
                    let base = f.tip("trunk");
                    let draft = f.draft(base);
                    // Someone else lands first, on another file.
                    let other = f.draft(base);
                    f.land(other, "trunk");
                    f.land(draft, "trunk")
                })
                .sum()
        })
    });
    group.finish();
}

criterion_group!(benches, uncontended_land);
criterion_main!(benches);
