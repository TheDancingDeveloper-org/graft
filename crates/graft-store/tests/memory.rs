//! Runs the conformance suite against the in-memory store (work item G1.2).

#![cfg(feature = "testkit")]

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use graft_store::memory::MemoryStore;

/// Drives a future to completion. The memory store never waits, so its
/// futures are ready on the first poll, and no executor is needed.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}

#[test]
fn memory_store_passes_the_conformance_suite() {
    block_on(graft_store::conformance::run_all(|| {
        let store = MemoryStore::new();
        (store.clone(), store)
    }));
}

#[test]
fn draft_path_index_scans_by_prefix_and_clears_with_its_workspace() {
    use graft_model::RepoPath;
    use graft_store::{MetaStore, MetaTxn, Write};
    let p = |s: &str| RepoPath::new(s).unwrap();
    let store = MemoryStore::new();
    block_on(async {
        let repo = store.create_repo("r").await.unwrap();
        let set = |ws: &str, paths: &[&str]| Write::SetDraftPaths {
            workspace: ws.into(),
            line: "main".into(),
            paths: paths.iter().map(|s| p(s)).collect(),
        };
        let txn = MetaTxn {
            preconditions: vec![],
            writes: vec![
                set("a", &["src", "src/x.rs", "src-y"]),
                set("b", &["src/x.rs", "doc/z"]),
            ],
        };
        store.commit(repo, txn).await.unwrap();
        let src = p("src");
        let scan = |after| store.scan_draft_paths(repo, "main", &src, after, 10);
        let rows = scan(None).await.unwrap();
        let want = |v: &[(&str, &str)]| {
            v.iter()
                .map(|(a, b)| (p(a), (*b).to_owned()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rows,
            want(&[("src", "a"), ("src/x.rs", "a"), ("src/x.rs", "b")])
        );
        let rows = scan(Some((p("src/x.rs"), "a".into()))).await.unwrap();
        assert_eq!(rows, want(&[("src/x.rs", "b")]));
        assert!(
            store
                .scan_draft_paths(repo, "other", &p("src"), None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        // Replace, then delete with the workspace.
        let txn = MetaTxn {
            preconditions: vec![],
            writes: vec![set("a", &["doc/z"]), Write::DeleteWorkspace("b".into())],
        };
        store.commit(repo, txn).await.unwrap();
        assert!(scan(None).await.unwrap().is_empty());
        assert_eq!(
            store
                .workspace_draft_paths(repo, "a", None, 10)
                .await
                .unwrap(),
            vec![p("doc/z")]
        );
        assert!(
            store
                .workspace_draft_paths(repo, "b", None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    });
}
