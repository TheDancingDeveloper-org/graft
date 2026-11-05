//! The `repo_mirrors` table (D41): settings CAS, status, pagination, and
//! removal with its followed markers in one transaction. Every test runs on
//! SQLite and on the in-memory fake, which must agree.

mod common;

use common::{TempDir, block_on};
use graft_model::{ChangesetId, ObjectId};
use graft_store::memory::MemoryStore;
use graft_store::{
    LeaseRecord, MetaStore, MetaTxn, MirrorDirection, MirrorExportRecord, MirrorLineRecord,
    MirrorQuery, MirrorRecord, MirrorStatusRecord, Precondition, RefKind, RefRecord, RepoId,
    StoreError, TxnOutcome, Write,
};
use graft_store_sqlite::migrations;
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use rusqlite::Connection;

const UP: &str = "https://github.com/o/r.git";

fn row(upstream: &str, version: u64) -> MirrorRecord {
    MirrorRecord {
        upstream: upstream.to_owned(),
        lines: vec![MirrorLineRecord {
            branch: "main".into(),
            line: "main".into(),
            direction: MirrorDirection::Follow,
        }],
        interval_s: 15,
        enabled: true,
        credential: None,
        kek_id: None,
        export: MirrorExportRecord {
            default_author: Some("Bot <bot@example.com>".into()),
            authors: vec![("alice".into(), "Alice <a@example.com>".into())],
        },
        version,
        status: None,
        created_by: "op".into(),
        updated_by: "op".into(),
        updated_at_ms: 1_000,
    }
}

fn put(expected: Option<u64>, record: MirrorRecord) -> MetaTxn {
    MetaTxn {
        preconditions: vec![Precondition::MirrorVersionIs {
            upstream: record.upstream.clone(),
            version: expected,
        }],
        writes: vec![Write::PutMirror(record)],
    }
}

fn with_both(test: impl Fn(&dyn Fn() -> Box<dyn Store>)) {
    test(&|| Box::new(Sqlite::new()));
    test(&|| Box::new(Memory(MemoryStore::new())));
}

/// The calls the tests make, over either store.
trait Store {
    fn create(&self, name: &str) -> RepoId;
    fn commit(&self, repo: RepoId, txn: MetaTxn) -> Result<TxnOutcome, StoreError>;
    fn get(&self, repo: RepoId, upstream: &str) -> Option<MirrorRecord>;
    fn list(&self, q: MirrorQuery) -> Vec<(String, String)>;
    fn get_ref(&self, repo: RepoId, name: &str) -> Option<RefRecord>;
    fn journal_len(&self, repo: RepoId) -> usize;
    fn lease(&self, repo: RepoId) -> Option<LeaseRecord>;
}

/// The directory lives as long as the store.
struct Sqlite(#[allow(dead_code)] TempDir, SqliteMetaStore);
struct Memory(MemoryStore);

impl Sqlite {
    fn new() -> Self {
        let dir = TempDir::new("mirrors");
        let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
        Self(dir, store)
    }
}

macro_rules! store_impl {
    ($t:ty, $s:ident => $e:expr) => {
        impl Store for $t {
            fn create(&self, name: &str) -> RepoId {
                let $s = self;
                block_on($e.create_repo(name)).expect("create")
            }
            fn commit(&self, repo: RepoId, txn: MetaTxn) -> Result<TxnOutcome, StoreError> {
                let $s = self;
                block_on($e.commit(repo, txn))
            }
            fn get(&self, repo: RepoId, upstream: &str) -> Option<MirrorRecord> {
                let $s = self;
                block_on($e.get_mirror(repo, upstream)).expect("get")
            }
            fn list(&self, q: MirrorQuery) -> Vec<(String, String)> {
                let $s = self;
                block_on($e.list_mirrors(q))
                    .expect("list")
                    .into_iter()
                    .map(|l| (l.repo_name, l.record.upstream))
                    .collect()
            }
            fn get_ref(&self, repo: RepoId, name: &str) -> Option<RefRecord> {
                let $s = self;
                block_on($e.get_ref(repo, name)).expect("ref")
            }
            fn journal_len(&self, repo: RepoId) -> usize {
                let $s = self;
                block_on($e.journal_tip(repo))
                    .expect("journal")
                    .map_or(0, |l| l as usize)
            }
            fn lease(&self, repo: RepoId) -> Option<LeaseRecord> {
                let $s = self;
                block_on($e.get_lease(repo)).expect("lease")
            }
        }
    };
}
store_impl!(Sqlite, s => s.1);
store_impl!(Memory, s => s.0);

#[test]
fn a_row_round_trips_and_settings_writes_are_compare_and_swap() {
    with_both(|make| {
        let s = make();
        let repo = s.create("r");
        assert_eq!(
            s.commit(repo, put(None, row(UP, 1))).expect("create"),
            TxnOutcome::Committed(vec![])
        );
        assert_eq!(s.get(repo, UP), Some(row(UP, 1)));

        // A stale expected version (a second creator, or an old etag) fails
        // the precondition and writes nothing.
        let mut other = row(UP, 1);
        other.interval_s = 99;
        assert_eq!(
            s.commit(repo, put(None, other.clone())).expect("stale"),
            TxnOutcome::PreconditionFailed(0)
        );
        other.version = 2;
        assert_eq!(
            s.commit(repo, put(Some(7), other.clone())).expect("stale"),
            TxnOutcome::PreconditionFailed(0)
        );
        assert_eq!(s.get(repo, UP), Some(row(UP, 1)));

        assert!(matches!(
            s.commit(repo, put(Some(1), other.clone())),
            Ok(TxnOutcome::Committed(_))
        ));
        assert_eq!(s.get(repo, UP).expect("row").interval_s, 99);

        // The version must follow the stored one, and the write must be
        // guarded.
        let mut skip = other.clone();
        skip.version = 9;
        assert!(matches!(
            s.commit(repo, put(Some(2), skip)),
            Err(StoreError::Invalid(_))
        ));
        let unguarded = MetaTxn {
            preconditions: vec![],
            writes: vec![Write::PutMirror(row(UP, 3))],
        };
        assert!(matches!(
            s.commit(repo, unguarded),
            Err(StoreError::Invalid(_))
        ));
    });
}

#[test]
fn status_is_kept_across_settings_writes_and_never_creates_a_row() {
    with_both(|make| {
        let s = make();
        let repo = s.create("r");
        let status = MirrorStatusRecord {
            last_poll_at_ms: Some(5),
            last_changeset: Some(ChangesetId(ObjectId([3; 32]))),
            lag_changesets: 2,
            error_code: Some("timeout".into()),
            error_message: Some("timed out".into()),
            lease_epoch: 4,
            ..MirrorStatusRecord::default()
        };
        let set = |upstream: &str| MetaTxn {
            preconditions: vec![],
            writes: vec![Write::SetMirrorStatus {
                upstream: upstream.to_owned(),
                status: status.clone(),
            }],
        };
        s.commit(repo, set(UP)).expect("status on no row");
        assert_eq!(s.get(repo, UP), None);

        s.commit(repo, put(None, row(UP, 1))).expect("create");
        s.commit(repo, set(UP)).expect("status");
        s.commit(repo, put(Some(1), row(UP, 2))).expect("update");
        let got = s.get(repo, UP).expect("row");
        assert_eq!(got.version, 2);
        assert_eq!(got.status, Some(status.clone()));
    });
}

#[test]
fn pages_are_bounded_and_resume_after_the_last_row() {
    with_both(|make| {
        let s = make();
        let a = s.create("a");
        let b = s.create("b");
        for (repo, up) in [
            (a, "https://github.com/1"),
            (a, "https://github.com/2"),
            (b, "https://github.com/3"),
        ] {
            s.commit(repo, put(None, row(up, 1))).expect("create");
        }
        let mut disabled = row("https://github.com/4", 1);
        disabled.enabled = false;
        s.commit(b, put(None, disabled)).expect("create");

        let first = s.list(MirrorQuery {
            limit: 2,
            ..MirrorQuery::default()
        });
        assert_eq!(
            first,
            vec![
                ("a".to_owned(), "https://github.com/1".to_owned()),
                ("a".to_owned(), "https://github.com/2".to_owned())
            ]
        );
        let rest = s.list(MirrorQuery {
            after: Some((a, "https://github.com/2".into())),
            limit: 2,
            ..MirrorQuery::default()
        });
        assert_eq!(
            rest,
            vec![
                ("b".to_owned(), "https://github.com/3".to_owned()),
                ("b".to_owned(), "https://github.com/4".to_owned())
            ]
        );
        let enabled_b = s.list(MirrorQuery {
            repo: Some(b),
            enabled: Some(true),
            limit: 10,
            ..MirrorQuery::default()
        });
        assert_eq!(
            enabled_b,
            vec![("b".to_owned(), "https://github.com/3".to_owned())]
        );
        // A limit of zero still returns a bounded, non-empty page.
        assert_eq!(s.list(MirrorQuery::default()).len(), 1);
    });
}

#[test]
fn removal_clears_markers_with_their_journal_entries_in_one_transaction() {
    with_both(|make| {
        let s = make();
        let repo = s.create("r");
        let target = ChangesetId(ObjectId([9; 32]));
        let marker = "graft.followed/main";
        s.commit(
            repo,
            MetaTxn {
                preconditions: vec![Precondition::RefIs {
                    name: marker.into(),
                    expected: None,
                }],
                writes: vec![Write::SetRef(RefRecord {
                    name: marker.into(),
                    target,
                    version: 1,
                    kind: RefKind::Tool,
                })],
            },
        )
        .expect("marker");
        s.commit(repo, put(None, row(UP, 1))).expect("create");
        let before = s.journal_len(repo);

        let remove = |marker_expected| MetaTxn {
            preconditions: vec![
                Precondition::MirrorVersionIs {
                    upstream: UP.into(),
                    version: Some(1),
                },
                Precondition::RefIs {
                    name: marker.into(),
                    expected: marker_expected,
                },
            ],
            writes: vec![
                Write::DeleteRef(marker.into()),
                Write::Journal {
                    kind: "RefDeleted".into(),
                    body: b"{}".to_vec(),
                },
                Write::DeleteMirror(UP.into()),
            ],
        };
        // A marker that moved since the read fails the whole removal.
        assert_eq!(
            s.commit(repo, remove(Some((target, 2)))).expect("stale"),
            TxnOutcome::PreconditionFailed(1)
        );
        assert!(s.get(repo, UP).is_some());
        assert!(s.get_ref(repo, marker).is_some());

        assert!(matches!(
            s.commit(repo, remove(Some((target, 1)))),
            Ok(TxnOutcome::Committed(l)) if l.len() == 1
        ));
        assert_eq!(s.get(repo, UP), None);
        assert_eq!(s.get_ref(repo, marker), None);
        assert_eq!(s.journal_len(repo), before + 1);

        // Deleting a ref needs a RefIs guard.
        let unguarded = MetaTxn {
            preconditions: vec![],
            writes: vec![Write::DeleteRef("x".into())],
        };
        assert!(matches!(
            s.commit(repo, unguarded),
            Err(StoreError::Invalid(_))
        ));
    });
}

#[test]
fn a_field_with_a_tab_is_refused_not_stored() {
    let s = Sqlite::new();
    let repo = s.create("r");
    let mut bad = row(UP, 1);
    bad.lines[0].branch = "ma\tin".into();
    assert!(matches!(
        s.commit(repo, put(None, bad)),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(s.get(repo, UP), None);
}

#[test]
fn a_version_9_database_migrates_and_keeps_its_data() {
    let dir = TempDir::new("mirrors-v9");
    {
        let mut conn = Connection::open(dir.db()).expect("open");
        migrations::migrate_to(&mut conn, 9).expect("v9");
        conn.execute(
            "INSERT INTO repos (name, gc_protection) VALUES ('old', 1)",
            [],
        )
        .expect("repo");
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let repo = block_on(store.repo("old")).expect("repo").expect("kept");
    assert_eq!(block_on(store.get_mirror(repo, UP)).expect("get"), None);
    block_on(store.commit(repo, put(None, row(UP, 1)))).expect("create");
    assert_eq!(
        block_on(store.get_mirror(repo, UP)).expect("get"),
        Some(row(UP, 1))
    );
    let conn = Connection::open(dir.db()).expect("inspect");
    assert_eq!(
        migrations::version(&conn).expect("version"),
        migrations::SCHEMA_VERSION
    );
}

fn put_lease(expected: u64, holder: Option<&str>, epoch: u64, expires_at_ms: i64) -> MetaTxn {
    MetaTxn {
        preconditions: vec![Precondition::LeaseEpochIs { epoch: expected }],
        writes: vec![Write::PutLease(LeaseRecord {
            holder: holder.map(str::to_owned),
            epoch,
            expires_at_ms,
        })],
    }
}

/// D41 §3.6: the lease row is written by CAS on its epoch, the epoch moves
/// by at most one and survives a release, and `LeaseHeld` fences on epoch,
/// holder and expiry.
#[test]
fn the_lease_is_compare_and_swap_on_its_epoch_and_fences_writes() {
    with_both(|make| {
        let s = make();
        let repo = s.create("r");
        assert_eq!(s.lease(repo), None);

        // Two acquirers read epoch 0; the first CAS wins, the second fails.
        assert_eq!(
            s.commit(repo, put_lease(0, Some("a"), 1, 60_000))
                .expect("a"),
            TxnOutcome::Committed(vec![])
        );
        assert_eq!(
            s.commit(repo, put_lease(0, Some("b"), 1, 60_000))
                .expect("b"),
            TxnOutcome::PreconditionFailed(0)
        );
        assert_eq!(s.lease(repo).and_then(|l| l.holder).as_deref(), Some("a"));

        // An epoch that skips, or has no guard, is refused outright.
        assert!(matches!(
            s.commit(repo, put_lease(1, Some("a"), 3, 60_000)),
            Err(StoreError::Invalid(_))
        ));
        let unguarded = MetaTxn {
            preconditions: vec![],
            writes: vec![Write::PutLease(LeaseRecord {
                holder: None,
                epoch: 1,
                expires_at_ms: 0,
            })],
        };
        assert!(matches!(
            s.commit(repo, unguarded),
            Err(StoreError::Invalid(_))
        ));

        let fenced = |epoch, now_ms| MetaTxn {
            preconditions: vec![Precondition::LeaseHeld { epoch, now_ms }],
            writes: vec![],
        };
        let ok = TxnOutcome::Committed(vec![]);
        let lost = TxnOutcome::PreconditionFailed(0);
        assert_eq!(s.commit(repo, fenced(1, 59_999)).expect("held"), ok);
        assert_eq!(s.commit(repo, fenced(1, 60_000)).expect("lapsed"), lost);
        assert_eq!(s.commit(repo, fenced(2, 0)).expect("wrong epoch"), lost);

        // A release keeps the epoch, and the lease no longer fences in.
        assert_eq!(
            s.commit(repo, put_lease(1, None, 1, 0)).expect("release"),
            ok
        );
        assert_eq!(s.lease(repo).map(|l| (l.holder, l.epoch)), Some((None, 1)));
        assert_eq!(s.commit(repo, fenced(1, 0)).expect("released"), lost);
        assert_eq!(
            s.commit(repo, put_lease(1, Some("b"), 2, 9))
                .expect("again"),
            ok
        );
        assert_eq!(s.lease(repo).map(|l| l.epoch), Some(2));
    });
}
