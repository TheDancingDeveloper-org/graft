//! Migrations: from an empty database, from each prior version, and the
//! refusal of a database newer than this build.

mod common;

use common::{TempDir, block_on};
use graft_model::{ChangesetId, ObjectId};
use graft_store::{MetaStore, MetaTxn, StoreError, TxnOutcome, Write};
use graft_store_sqlite::migrations::{self, MIGRATIONS, SCHEMA_VERSION};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use rusqlite::Connection;

const TABLES: &[&str] = &[
    "changesets",
    "derived",
    "doomed",
    "holders",
    "journal",
    "lands",
    "line_paths",
    "line_pos",
    "refs",
    "repos",
    "tombstones",
    "uploads",
    "workspaces",
];

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// The store works end to end: a repository, a changeset, a journal entry.
fn smoke(store: &SqliteMetaStore) {
    let repo = block_on(store.create_repo("smoke")).expect("create");
    let id = ChangesetId(ObjectId([7; 32]));
    let txn = MetaTxn {
        preconditions: vec![],
        writes: vec![
            Write::PutChangeset {
                id,
                content: b"c".to_vec(),
                record: graft_store::ChangesetRecord {
                    lsn: 0,
                    received_at_ms: 0,
                    state: graft_store::RecordState::Draft,
                    holder: None,
                    superseded_by: None,
                },
            },
            Write::Journal {
                kind: "ChangesetCreated".to_owned(),
                body: vec![],
            },
        ],
    };
    assert_eq!(
        block_on(store.commit(repo, txn)).expect("commit"),
        TxnOutcome::Committed(vec![1])
    );
    assert!(block_on(store.changeset(repo, id)).expect("read").is_some());
}

#[test]
fn an_empty_database_migrates_to_the_current_version() {
    let dir = TempDir::new("mig-empty");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let conn = Connection::open(dir.db()).expect("inspect");
    assert_eq!(migrations::version(&conn).expect("version"), SCHEMA_VERSION);
    assert_eq!(tables(&conn), TABLES);
    smoke(&store);
}

#[test]
fn every_prior_version_migrates_to_the_current_version() {
    assert_eq!(SCHEMA_VERSION as usize, MIGRATIONS.len());
    for prior in 0..=SCHEMA_VERSION {
        let dir = TempDir::new("mig-prior");
        {
            let mut conn = Connection::open(dir.db()).expect("open");
            migrations::migrate_to(&mut conn, prior).expect("migrate to prior");
            assert_eq!(migrations::version(&conn).expect("version"), prior);
        }
        let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
        let conn = Connection::open(dir.db()).expect("inspect");
        assert_eq!(
            migrations::version(&conn).expect("version"),
            SCHEMA_VERSION,
            "from version {prior}"
        );
        smoke(&store);
    }
}

#[test]
fn reopening_keeps_the_schema_and_the_data() {
    let dir = TempDir::new("mig-reopen");
    {
        let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
        smoke(&store);
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("reopen");
    assert!(block_on(store.repo("smoke")).expect("repo").is_some());
}

#[test]
fn a_newer_database_is_refused() {
    let dir = TempDir::new("mig-newer");
    {
        let conn = Connection::open(dir.db()).expect("open");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("set version");
    }
    let refused = SqliteMetaStore::open(SqliteConfig::new(dir.db()));
    assert!(matches!(refused, Err(StoreError::Invalid(_))));
    let conn = Connection::open(dir.db()).expect("inspect");
    assert!(tables(&conn).is_empty(), "a newer database is not touched");
}
