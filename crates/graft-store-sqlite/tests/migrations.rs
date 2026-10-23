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

/// Version 4 (G3.11) on a version 3 database: existing repositories lack the
/// `gc_protection` marker (the engine's backfill sets it), records get no
/// pins and no expiry, and upload rows get the expiry their holder names.
#[test]
fn version_4_adds_protection_to_a_version_3_database() {
    let dir = TempDir::new("mig-v4");
    let id = ChangesetId(ObjectId([7; 32]));
    let (blob, tree) = (ObjectId([1; 32]), ObjectId([2; 32]));
    {
        let mut conn = Connection::open(dir.db()).expect("open");
        migrations::migrate_to(&mut conn, 3).expect("migrate to v3");
        conn.execute_batch(&format!(
            "INSERT INTO repos (id, name) VALUES (1, 'old');
             INSERT INTO changesets
                 (repo, id, content, lsn, received_at, state, holder, superseded_by)
                 VALUES (1, x'{id}', x'00', 1, 5, 3, NULL, NULL);
             INSERT INTO holders (repo, object, holder) VALUES
                 (1, x'{blob}', 'upload:1234:alice'),
                 (1, x'{blob}', 'upload:bad'),
                 (1, x'{tree}', '{cs}');",
            id = id.0.to_hex(),
            cs = id,
            blob = blob.to_hex(),
            tree = tree.to_hex(),
        ))
        .expect("v3 rows");
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let old = block_on(store.repo("old")).expect("repo").expect("exists");
    assert!(!block_on(store.gc_protection(old)).expect("marker"));
    assert_eq!(
        block_on(store.protection(old, id)).expect("read"),
        Some(graft_store::Protection::default())
    );
    let rows = block_on(store.expired_rows(old, i64::MAX, None, 10)).expect("rows");
    assert_eq!(
        rows,
        [graft_store::TimedRow {
            expires_at_ms: 1234,
            object: blob,
            holder: "upload:1234:alice".to_owned(),
        }],
        "only a well-formed upload row gets an expiry"
    );
    assert_eq!(
        block_on(store.held_objects(old, &id.to_string(), None, 10)).expect("held"),
        [tree]
    );
    let new = block_on(store.create_repo("new")).expect("create");
    assert!(block_on(store.gc_protection(new)).expect("marker"));
}

/// The collection scan reads only the expiry index: its query plan is one
/// index range, not a table scan.
#[test]
fn the_expiry_scans_are_index_ranges() {
    let dir = TempDir::new("mig-plan");
    let _store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let conn = Connection::open(dir.db()).expect("inspect");
    let plan = |sql: &str| -> String {
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("plan");
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        rows.join("; ")
    };
    let records = plan(
        "SELECT expires_at, id FROM changesets
         WHERE repo = 1 AND pins = 0 AND expires_at IS NOT NULL
           AND expires_at <= 10 AND (expires_at, id) > (0, x'')
         ORDER BY expires_at, id LIMIT 5",
    );
    assert!(records.contains("changesets_expiry"), "{records}");
    let rows = plan(
        "SELECT expires, object, holder FROM holders
         WHERE repo = 1 AND expires IS NOT NULL AND expires <= 10
           AND (expires, object, holder) > (0, x'', '')
         ORDER BY expires, object, holder LIMIT 5",
    );
    assert!(rows.contains("holders_expiry"), "{rows}");
    let held = plan(
        "SELECT object FROM holders WHERE repo = 1 AND holder = 'c' AND object > x''
         ORDER BY object LIMIT 5",
    );
    assert!(held.contains("holders_holder"), "{held}");
    let doomed = plan(
        "SELECT object, since, deleting FROM doomed
         WHERE repo = 1 AND since <= 10 AND (since, object) > (0, x'')
         ORDER BY since, object LIMIT 5",
    );
    assert!(doomed.contains("doomed_since"), "{doomed}");
    for p in [&records, &rows, &held, &doomed] {
        assert!(!p.contains("TEMP B-TREE"), "no sort: {p}");
    }
}
