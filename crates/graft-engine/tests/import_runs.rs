//! `ImportChangesets` commits in chained runs (WI-2323): a build failure
//! mid-run, a lost race that re-queues the rest, runs longer than one
//! `commit_run`, and runs cut by the claim deadline (WI-2329).

mod common;

use std::sync::Arc;

use common::race::{Task, YieldSleeper, Yielding, run_scheduled};
use common::*;
use graft_engine::EngineConfig;
use graft_model::{Changeset, ChangesetId};
use graft_proto::error::ApiResult;
use graft_proto::types::{Created, ImportChangesets};
use graft_proto::{AdminApi, ApiError};
use graft_store::MetaStore;
use graft_store::memory::MemoryStore;

/// A chain of `n` changesets built in a scratch repo, and their blobs.
fn chain<M: MetaStore>(f: &Fixture<M>, n: usize) -> (Vec<Changeset>, Vec<Vec<u8>>) {
    let scratch = Repo::new(f, "scratch");
    let mut parent = Vec::new();
    let mut out = Vec::new();
    let mut blobs = Vec::new();
    for i in 0..n {
        let path = format!("d{}/f{i}", i % 4);
        let body = format!("version {i}\n").into_bytes();
        let id = scratch
            .commit(parent.clone(), &[(path.as_str(), body.as_slice())], &[])
            .id;
        out.push(scratch.changeset(id).changeset);
        blobs.push(body);
        parent = vec![id];
    }
    (out, blobs)
}

fn target<'f, M: MetaStore>(f: &'f Fixture<M>, name: &str, blobs: &[Vec<u8>]) -> Repo<'f, M> {
    let repo = Repo::new(f, name);
    repo.upload(&blobs.iter().map(Vec::as_slice).collect::<Vec<_>>());
    repo
}

fn import<M: MetaStore>(repo: &Repo<'_, M>, changesets: Vec<Changeset>) -> ApiResult<Vec<Created>> {
    block_on(repo.e().import_changesets(
        &repo.cx,
        ImportChangesets {
            repo: repo.name.clone(),
            changesets,
        },
    ))
}

/// `cs` with a second parent that is not stored: its trees derive (on the
/// first parent), so it joins a run, and only its build fails.
fn orphaned(mut cs: Changeset) -> Changeset {
    cs.parents.push(ChangesetId::hash(b"not stored"));
    cs
}

fn ids(cs: &[Changeset]) -> Vec<ChangesetId> {
    cs.iter().map(Changeset::id).collect()
}

/// A build failure mid-run commits the ones built before it, and nothing
/// after it.
#[test]
fn a_mid_run_build_failure_keeps_the_ones_before_it() {
    fn case<M: MetaStore>(f: &Fixture<M>) {
        let (batch, blobs) = chain(f, 8);
        let repo = target(f, "mid", &blobs);
        let mut bad = batch.clone();
        bad[5] = orphaned(bad[5].clone());
        let err = import(&repo, bad).expect_err("missing parent");
        assert!(!matches!(err, ApiError::Contended { .. }), "{err:?}");
        assert_eq!(repo.journal().len(), 5, "0 to 4 committed, nothing after");
        let kept = import(&repo, batch[..5].to_vec()).expect("re-import");
        assert_eq!(
            kept.iter().map(|c| c.id).collect::<Vec<_>>(),
            ids(&batch[..5])
        );
    }
    case(&memory());
    case(&sqlite());
}

/// More changesets than one run holds (256) commit in several runs.
#[test]
fn runs_longer_than_one_commit_run() {
    fn case<M: MetaStore>(f: &Fixture<M>) {
        let (batch, blobs) = chain(f, 300);
        let repo = target(f, "long", &blobs);
        let got = import(&repo, batch.clone()).expect("import");
        assert_eq!(got.iter().map(|c| c.id).collect::<Vec<_>>(), ids(&batch));
        assert_eq!(repo.journal().len(), 300);
    }
    let config = EngineConfig {
        max_import: 1000,
        ..EngineConfig::default()
    };
    case(&memory_with(config.clone()));
    case(&sqlite_with(config));
}

/// With a claim window too short for a second changeset, every run holds
/// one, and the import still stores them all in order.
#[test]
fn the_claim_deadline_cuts_runs() {
    let config = EngineConfig {
        claim_ms: 1,
        ..EngineConfig::default()
    };
    let f = memory_with(config);
    let (batch, blobs) = chain(&f, 6);
    let repo = target(&f, "deadline", &blobs);
    let got = import(&repo, batch.clone()).expect("import");
    assert_eq!(got.iter().map(|c| c.id).collect::<Vec<_>>(), ids(&batch));
    assert_eq!(repo.journal().len(), 6);
}

/// Two imports race on a shared prefix. The loser finds the winner's
/// changesets stored, re-queues the rest and stores them; with a build
/// failure later in its run, the ones before the failure are still
/// committed before its error returns.
#[test]
fn a_lost_race_re_queues_the_rest() {
    for fail in [false, true] {
        for schedule in [[0, 1], [1, 0], [0, 0], [1, 1]] {
            let f = build(
                Arc::new(Yielding(MemoryStore::new())),
                EngineConfig::default(),
                None,
            )
            .map_engine(|e| e.with_sleeper(Arc::new(YieldSleeper)));
            let (batch, blobs) = chain(&f, 6);
            let repo = target(&f, "race", &blobs);
            let mut long = batch.clone();
            if fail {
                long.push(orphaned(batch[5].clone()));
            }
            let tasks: Vec<Task<'_, ApiResult<Vec<Created>>>> = [batch[..2].to_vec(), long]
                .into_iter()
                .map(|changesets| {
                    let (e, cx, name) = (repo.e(), repo.cx.clone(), repo.name.clone());
                    Box::pin(async move {
                        e.import_changesets(
                            &cx,
                            ImportChangesets {
                                repo: name,
                                changesets,
                            },
                        )
                        .await
                    }) as Task<'_, _>
                })
                .collect();
            let results = run_scheduled(tasks, &schedule, || {});
            let short = results[0].as_ref().expect("short import");
            assert_eq!(
                short.iter().map(|c| c.id).collect::<Vec<_>>(),
                ids(&batch[..2])
            );
            match &results[1] {
                Ok(got) => {
                    assert!(!fail, "{schedule:?}");
                    assert_eq!(got.iter().map(|c| c.id).collect::<Vec<_>>(), ids(&batch));
                }
                Err(e) => assert!(fail, "{schedule:?}: {e:?}"),
            }
            assert_eq!(repo.journal().len(), 6, "fail {fail}, {schedule:?}");
        }
    }
}
