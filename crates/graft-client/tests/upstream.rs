//! `WorkingCopy::upstream` (WI-2309) against the real engine on the memory
//! store: the clean, merge and conflict predictions agree with an immediate
//! land, nothing moves on the server or on disk, and every list is bounded.

#[path = "support/engine.rs"]
mod engine;

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use engine::{EngineRepo, Race};
use graft_client::Error;
use graft_client::Sparse;
use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{
    CommitLimits, CommitOptions, DEFAULT_CACHE_BYTES, LandOptions, LandOutcome, Outcome, Upstream,
    WcConfig, WorkingCopy, clone,
};
use graft_client::workspace::DEFAULT_LEASE_MS;
use graft_model::{Author, AuthorKind, Timestamp};
use graft_proto::ApiError;
use graft_proto::api::WorkspaceApi;
use graft_proto::types::WorkspaceRef;

const REPO: &str = "demo";
const LIB: &str = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n";

struct Env {
    repo: Arc<EngineRepo>,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Env {
        let files: Vec<(&str, &[u8])> =
            vec![("README.md", b"hello\n"), ("src/lib.rs", LIB.as_bytes())];
        Env {
            repo: EngineRepo::direct(REPO, &files).await,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    async fn clone(&self, name: &str) -> WorkingCopy {
        let config = WcConfig {
            server: None,
            endpoint: None,
            repo: REPO.to_owned(),
            workspace: Some(name.to_owned()),
        };
        let cx = self.repo.cx();
        let (mut wc, _) = clone(
            &*self.repo,
            &cx,
            &self.dir.path().join(name),
            config,
            "main",
            None,
            Sparse::full(),
            &FetchOptions::default(),
        )
        .await
        .unwrap();
        wc.begin(&*self.repo, &cx, DEFAULT_LEASE_MS).await.unwrap();
        wc
    }

    async fn upstream(&self, wc: &WorkingCopy, limit: u32) -> Upstream {
        wc.upstream(&*self.repo, &self.repo.cx(), limit, None)
            .await
            .unwrap()
    }

    async fn commit(&self, wc: &mut WorkingCopy) {
        let cx = self.repo.cx();
        wc.begin(&*self.repo, &cx, DEFAULT_LEASE_MS).await.unwrap();
        let opts = CommitOptions {
            message: Some("mine".to_owned()),
            author: Author {
                name: "Tester".into(),
                email: "tester@example.com".into(),
                kind: AuthorKind::Agent,
            },
            authored_at: Timestamp {
                ms: 1_700_000_000_000,
                offset_min: 0,
            },
            metadata: Default::default(),
            amend: false,
            limits: CommitLimits::default(),
        };
        wc.commit(&*self.repo, &cx, &opts).await.unwrap();
    }

    /// Commits the working copy and lands it with the D16 line merge.
    async fn land(&self, wc: &mut WorkingCopy) -> Result<LandOutcome, Error> {
        if !wc.status().unwrap().is_empty() {
            self.commit(wc).await;
        }
        let cx = self.repo.cx();
        wc.begin(&*self.repo, &cx, DEFAULT_LEASE_MS).await?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
        let o = LandOptions {
            contended_wait: std::time::Duration::ZERO,
            ..LandOptions::default()
        };
        wc.land(&*self.repo, &cache, &cx, &o, &FetchOptions::default())
            .await
    }

    /// Everything `upstream` must leave alone.
    async fn snapshot(&self, wc: &WorkingCopy) -> String {
        let ws = self
            .repo
            .get_workspace(
                &self.repo.cx(),
                WorkspaceRef {
                    repo: REPO.to_owned(),
                    name: wc.config().workspace.clone().unwrap(),
                },
            )
            .await
            .unwrap();
        let mut files = BTreeMap::new();
        for entry in walk(wc.root()) {
            files.insert(entry.clone(), fs::read(wc.root().join(&entry)).unwrap());
        }
        format!(
            "{:?} {:?} {:?} {:?} {:?}",
            self.repo.tip().await,
            (ws.base, ws.head),
            wc.state(),
            wc.status().unwrap(),
            files
        )
    }
}

/// Every file under `root`, `.graft/` included, as relative paths.
fn walk(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                if e.file_name() != "cache" {
                    dirs.push(e.path());
                }
            } else {
                out.push(e.path().strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    out.sort();
    out
}

fn write(wc: &WorkingCopy, path: &str, data: &str) {
    fs::write(wc.root().join(path), data).unwrap();
}

fn put(path: &str, data: &str) -> Race {
    vec![(path.to_owned(), Some(data.as_bytes().to_vec()))]
}

fn lib_with(line: usize, text: &str) -> String {
    let mut lines: Vec<String> = LIB.lines().map(str::to_owned).collect();
    lines[line - 1] = text.to_owned();
    lines.join("\n") + "\n"
}

/// Only reads reach the server.
fn assert_reads_only(calls: &[&str]) {
    for c in calls {
        assert!(
            matches!(
                *c,
                "ResolveRef" | "Log" | "Diff" | "ReadFile" | "GetConflicts"
            ),
            "upstream called {c}"
        );
    }
}

#[tokio::test]
async fn nothing_upstream_is_clean_and_behind_zero() {
    let env = Env::new().await;
    let wc = env.clone("a").await;
    write(&wc, "src/lib.rs", &lib_with(1, "fn A() {}"));
    let up = env.upstream(&wc, 10).await;
    assert_eq!(up.tip, up.base);
    assert_eq!((up.behind, up.behind_more), (0, false));
    assert!(up.changed.is_empty() && up.overlaps.is_empty() && up.clean());
}

#[tokio::test]
async fn an_unrelated_upstream_change_is_listed_without_overlap() {
    let env = Env::new().await;
    let wc = env.clone("a").await;
    write(&wc, "src/lib.rs", &lib_with(1, "fn A() {}"));
    env.repo.advance(put("README.md", "theirs\n")).await;
    let up = env.upstream(&wc, 10).await;
    assert_eq!(up.behind, 1);
    assert_eq!(up.changed.len(), 1);
    assert_eq!(up.changed[0].path.as_str(), "README.md");
    assert!(up.overlaps.is_empty() && up.clean());
}

#[tokio::test]
async fn an_uncommitted_edit_that_merges_is_clean_and_lands() {
    let env = Env::new().await;
    let mut wc = env.clone("a").await;
    write(&wc, "src/lib.rs", &lib_with(1, "fn A() {}"));
    env.repo
        .advance(put("src/lib.rs", &lib_with(5, "fn E() {}")))
        .await;

    let before = env.snapshot(&wc).await;
    let mark = env.repo.calls().len();
    let up = env.upstream(&wc, 10).await;
    assert_reads_only(&env.repo.calls()[mark..]);
    assert_eq!(env.snapshot(&wc).await, before);

    assert_eq!(up.overlaps.len(), 1);
    let o = &up.overlaps[0];
    assert_eq!(o.path.as_str(), "src/lib.rs");
    assert!(o.uncommitted);
    assert_eq!(o.outcome, Outcome::Clean);
    assert!(up.clean());

    // The prediction matches an immediate land.
    assert!(matches!(
        env.land(&mut wc).await.unwrap(),
        LandOutcome::Landed { rebased: true, .. }
    ));
}

#[tokio::test]
async fn an_uncommitted_conflict_reports_its_hunks_and_the_land_conflicts() {
    let env = Env::new().await;
    let mut wc = env.clone("a").await;
    write(&wc, "src/lib.rs", &lib_with(2, "fn mine() {}"));
    env.repo
        .advance(put("src/lib.rs", &lib_with(2, "fn theirs() {}")))
        .await;

    let before = env.snapshot(&wc).await;
    let up = env.upstream(&wc, 10).await;
    assert_eq!(env.snapshot(&wc).await, before);
    assert_eq!(
        up.overlaps[0].outcome,
        Outcome::Conflict {
            hunks: vec![(2, 2)],
            reason: None
        }
    );
    assert!(!up.clean());
    let err = env.land(&mut wc).await.unwrap_err();
    assert!(
        matches!(err, Error::Api(ApiError::StaleBase(ref d)) if d.paths == ["src/lib.rs"] && !d.truncated),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_committed_draft_that_conflicts_is_reported_by_the_server_too() {
    let env = Env::new().await;
    let mut wc = env.clone("a").await;
    write(&wc, "src/lib.rs", &lib_with(3, "fn mine() {}"));
    env.commit(&mut wc).await;
    env.repo
        .advance(put("src/lib.rs", &lib_with(3, "fn theirs() {}")))
        .await;

    let before = env.snapshot(&wc).await;
    let mark = env.repo.calls().len();
    let up = env.upstream(&wc, 10).await;
    let calls = env.repo.calls()[mark..].to_vec();
    assert_reads_only(&calls);
    assert!(calls.contains(&"GetConflicts"));
    assert_eq!(env.snapshot(&wc).await, before);

    assert_eq!(up.stack_conflict, wc.state().head);
    let o = &up.overlaps[0];
    assert!(!o.uncommitted);
    assert_eq!(
        o.outcome,
        Outcome::Conflict {
            hunks: vec![(3, 3)],
            reason: None
        }
    );
    assert!(env.land(&mut wc).await.is_err());
}

#[tokio::test]
async fn a_large_upstream_delta_is_paged_and_bounded() {
    let env = Env::new().await;
    let wc = env.clone("a").await;
    for i in 0..30 {
        let race: Race = (0..10)
            .map(|j| (format!("gen/{i:02}/{j}.txt"), Some(b"x\n".to_vec())))
            .collect();
        env.repo.advance(race).await;
    }
    for i in 0..40 {
        write(&wc, &format!("mine{i:02}.txt"), "mine\n");
    }
    let up = env.upstream(&wc, 7).await;
    assert_eq!((up.behind, up.behind_more), (7, true));
    assert_eq!(up.changed.len(), 7);
    assert!(up.next.is_some());
    assert!(up.overlaps.is_empty());

    let next = wc
        .upstream(&*env.repo, &env.repo.cx(), 7, up.next.clone())
        .await
        .unwrap();
    assert_eq!(next.changed.len(), 7);
    assert!(next.changed[0].path > up.changed[6].path);
}
