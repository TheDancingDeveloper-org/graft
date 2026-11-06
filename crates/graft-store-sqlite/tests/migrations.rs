//! Migrations: from an empty database, from each prior version, and the
//! refusal of a database newer than this build.

mod common;

use common::{TempDir, block_on};
use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::{MetaStore, MetaTxn, StoreError, TxnOutcome, Write};
use graft_store_sqlite::migrations::{self, MIGRATIONS, SCHEMA_VERSION};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use rusqlite::Connection;

const TABLES: &[&str] = &[
    "attestations",
    "changesets",
    "derived",
    "doomed",
    "draft_paths",
    "holders",
    "journal",
    "land_checks",
    "lands",
    "line_forks",
    "line_merges",
    "line_paths",
    "line_pos",
    "mirror_leases",
    "mirror_line_slots",
    "mirror_requests",
    "refs",
    "repo_mirrors",
    "repo_owners",
    "repos",
    "review_queue",
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

/// Golden vector for the line fork record (WI-2316, version 5): the table
/// shape and the stored row for `PutLineFork { line: "b", parent: "main",
/// pos: 2 }`. Changing either is a durable format change.
#[test]
fn line_fork_record_golden_vector() {
    let dir = TempDir::new("mig-fork");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let repo = block_on(store.create_repo("forks")).expect("create");
    let txn = MetaTxn {
        preconditions: vec![],
        writes: vec![
            Write::PutLineFork {
                line: "b".to_owned(),
                parent: "main".to_owned(),
                pos: 2,
            },
            Write::Journal {
                kind: "RefMoved".to_owned(),
                body: vec![],
            },
        ],
    };
    assert!(matches!(
        block_on(store.commit(repo, txn)).expect("commit"),
        TxnOutcome::Committed(_)
    ));
    let conn = Connection::open(dir.db()).expect("inspect");
    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'line_forks'",
            [],
            |r| r.get(0),
        )
        .expect("ddl");
    assert_eq!(
        ddl,
        "CREATE TABLE line_forks (
    repo   INTEGER NOT NULL REFERENCES repos(id),
    line   TEXT NOT NULL,
    parent TEXT NOT NULL,
    pos    INTEGER NOT NULL,
    PRIMARY KEY (repo, line)
) WITHOUT ROWID"
    );
    let row: (i64, String, String, i64) = conn
        .query_row("SELECT repo, line, parent, pos FROM line_forks", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .expect("row");
    assert_eq!(
        row,
        (i64::from(repo.0), "b".to_owned(), "main".to_owned(), 2)
    );
}

/// Version 5 (WI-2316) on a version 4 database: existing line rows are kept
/// and still read, the line has no fork, and the new table and the lookup
/// by changeset work.
#[test]
fn version_5_adds_line_forks_to_a_version_4_database() {
    let dir = TempDir::new("mig-v5");
    let id = ChangesetId(ObjectId([7; 32]));
    {
        let mut conn = Connection::open(dir.db()).expect("open");
        migrations::migrate_to(&mut conn, 4).expect("migrate to v4");
        conn.execute_batch(&format!(
            "INSERT INTO repos (id, name) VALUES (1, 'old');
             INSERT INTO line_pos (repo, line, changeset, pos)
                 VALUES (1, 'main', X'{}', 3);",
            "07".repeat(32)
        ))
        .expect("seed");
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let conn = Connection::open(dir.db()).expect("inspect");
    assert_eq!(migrations::version(&conn).expect("version"), SCHEMA_VERSION);
    assert!(tables(&conn).contains(&"line_forks".to_owned()));
    let repo = block_on(store.repo("old")).expect("repo").expect("exists");
    assert_eq!(
        block_on(store.line_pos(repo, "main", id)).expect("read"),
        Some(3)
    );
    assert_eq!(
        block_on(store.line_holders(repo, id, 10)).expect("holders"),
        vec![("main".to_owned(), 3)]
    );
    let forks: i64 = conn
        .query_row("SELECT count(*) FROM line_forks", [], |r| r.get(0))
        .expect("count");
    assert_eq!(forks, 0);
}

/// Version 6 (WI-2310) on a version 5 database: an existing workspace reads
/// with no work item and no draft paths, and the new index takes rows.
#[test]
fn version_6_adds_draft_paths_to_a_version_5_database() {
    let dir = TempDir::new("mig-v6");
    {
        let mut conn = Connection::open(dir.db()).expect("open");
        migrations::migrate_to(&mut conn, 5).expect("migrate to v5");
        conn.execute_batch(&format!(
            "INSERT INTO repos (id, name) VALUES (1, 'old');
             INSERT INTO workspaces (repo, name, owner, line, base, lease_until)
                 VALUES (1, 'w', 'alice', 'main', X'{}', 10);",
            "07".repeat(32)
        ))
        .expect("seed");
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let repo = block_on(store.repo("old")).expect("repo").expect("exists");
    let ws = block_on(store.workspace(repo, "w"))
        .expect("read")
        .expect("exists");
    assert_eq!(ws.work_item, None);
    let src = RepoPath::new("src").expect("path");
    assert!(
        block_on(store.workspace_draft_paths(repo, "w", None, 10))
            .expect("paths")
            .is_empty()
    );
    let txn = MetaTxn {
        preconditions: vec![],
        writes: vec![Write::AddDraftPaths {
            workspace: "w".into(),
            line: "main".into(),
            paths: vec![RepoPath::new("src/a").expect("path")],
        }],
    };
    block_on(store.commit(repo, txn)).expect("commit");
    assert_eq!(
        block_on(store.scan_draft_paths(repo, "main", &src, None, 10)).expect("scan"),
        vec![(RepoPath::new("src/a").expect("path"), "w".to_owned())]
    );
}

/// Golden vector for the merge point record (WI-2368, D29, version 7): the
/// table and index shapes and the stored row for `PutLineMerge { dst:
/// "main", dst_pos: 9, src: "release/1.1", src_changeset: [7; 32], src_pos:
/// 4 }`. Changing any of them is a durable format change.
#[test]
fn line_merge_record_golden_vector() {
    let dir = TempDir::new("mig-merge");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let repo = block_on(store.create_repo("merges")).expect("create");
    let txn = MetaTxn {
        preconditions: vec![],
        writes: vec![
            Write::PutLineMerge {
                dst: "main".to_owned(),
                dst_pos: 9,
                src: "release/1.1".to_owned(),
                src_changeset: ChangesetId(ObjectId([7; 32])),
                src_pos: 4,
            },
            Write::Journal {
                kind: "LineMerged".to_owned(),
                body: vec![],
            },
        ],
    };
    assert!(matches!(
        block_on(store.commit(repo, txn)).expect("commit"),
        TxnOutcome::Committed(_)
    ));
    let conn = Connection::open(dir.db()).expect("inspect");
    let ddl = |kind: &str, name: &str| -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
            [kind, name],
            |r| r.get(0),
        )
        .expect("ddl")
    };
    assert_eq!(
        ddl("table", "line_merges"),
        "CREATE TABLE line_merges (
    repo          INTEGER NOT NULL REFERENCES repos(id),
    dst           TEXT NOT NULL,
    dst_pos       INTEGER NOT NULL,
    src           TEXT NOT NULL,
    src_changeset BLOB NOT NULL,
    src_pos       INTEGER NOT NULL,
    PRIMARY KEY (repo, dst, dst_pos)
) WITHOUT ROWID"
    );
    assert_eq!(
        ddl("index", "line_merges_src"),
        "CREATE INDEX line_merges_src ON line_merges (repo, dst, src, dst_pos)"
    );
    let row: (i64, String, i64, String, Vec<u8>, i64) = conn
        .query_row(
            "SELECT repo, dst, dst_pos, src, src_changeset, src_pos FROM line_merges",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("row");
    assert_eq!(
        row,
        (
            i64::from(repo.0),
            "main".to_owned(),
            9,
            "release/1.1".to_owned(),
            vec![7; 32],
            4
        )
    );
}

/// Version 7 (WI-2368) on a version 6 database: existing lines have no
/// merge points, and the new table takes rows and reads them newest first,
/// bounded by position and filtered by source.
#[test]
fn version_7_adds_line_merges_to_a_version_6_database() {
    let dir = TempDir::new("mig-v7");
    let id = ChangesetId(ObjectId([7; 32]));
    {
        let mut conn = Connection::open(dir.db()).expect("open");
        migrations::migrate_to(&mut conn, 6).expect("migrate to v6");
        conn.execute_batch(&format!(
            "INSERT INTO repos (id, name) VALUES (1, 'old');
             INSERT INTO line_pos (repo, line, changeset, pos)
                 VALUES (1, 'main', X'{}', 3);",
            "07".repeat(32)
        ))
        .expect("seed");
        assert!(!tables(&conn).contains(&"line_merges".to_owned()));
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let conn = Connection::open(dir.db()).expect("inspect");
    assert_eq!(migrations::version(&conn).expect("version"), SCHEMA_VERSION);
    let repo = block_on(store.repo("old")).expect("repo").expect("exists");
    assert_eq!(
        block_on(store.line_pos(repo, "main", id)).expect("read"),
        Some(3)
    );
    assert!(
        block_on(store.line_merges(repo, "main", None, u64::MAX, 10))
            .expect("merges")
            .is_empty()
    );
    let row = |dst_pos: u64, src: &str, src_pos: u64| Write::PutLineMerge {
        dst: "main".to_owned(),
        dst_pos,
        src: src.to_owned(),
        src_changeset: id,
        src_pos,
    };
    let txn = MetaTxn {
        preconditions: vec![],
        writes: vec![row(4, "a", 1), row(6, "b", 2), row(8, "a", 3)],
    };
    block_on(store.commit(repo, txn)).expect("commit");
    let got = |src: Option<&str>, at: u64, limit: u32| -> Vec<(u64, String)> {
        block_on(store.line_merges(repo, "main", src, at, limit))
            .expect("merges")
            .into_iter()
            .map(|r| (r.dst_pos, r.src))
            .collect()
    };
    assert_eq!(
        got(None, u64::MAX, 10),
        vec![(8, "a".into()), (6, "b".into()), (4, "a".into())]
    );
    assert_eq!(got(None, 7, 1), vec![(6, "b".into())]);
    assert_eq!(got(Some("a"), 7, 10), vec![(4, "a".into())]);
    assert!(got(Some("c"), u64::MAX, 10).is_empty());
    // One row per target position: a second one there is refused.
    let txn = MetaTxn {
        preconditions: vec![],
        writes: vec![row(4, "b", 1)],
    };
    assert!(block_on(store.commit(repo, txn)).is_err());
}

/// Every row of `table`, every column, in key order.
fn rows_of(conn: &Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table}"))
        .expect("prepare");
    let n = stmt.column_count();
    let mut rows: Vec<Vec<rusqlite::types::Value>> = stmt
        .query_map([], |r| (0..n).map(|i| r.get(i)).collect())
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

/// The mirror tables (versions 9 to 12) on a populated version 8 database:
/// every existing row of every table survives, column for column, and still
/// reads through the store; the new tables exist and are empty.
#[test]
fn a_populated_version_8_database_migrates_to_the_mirror_tables_intact() {
    let dir = TempDir::new("mig-v8-full");
    let cs = ChangesetId(ObjectId([7; 32]));
    let h = |b: u8| format!("X'{}'", format!("{b:02x}").repeat(32));
    let v8_tables: Vec<String>;
    let before: Vec<(String, Vec<Vec<rusqlite::types::Value>>)>;
    {
        let mut conn = Connection::open(dir.db()).expect("open");
        migrations::migrate_to(&mut conn, 8).expect("migrate to v8");
        conn.execute_batch(&format!(
            "INSERT INTO repos (id, name, gc_protection) VALUES (1, 'old', 1), (2, 'other', 0);
             INSERT INTO changesets
                 (repo, id, content, lsn, received_at, state, holder, superseded_by,
                  pins, expires_at)
                 VALUES (1, {c7}, X'636f6e74656e74', 1, 1000, 1, NULL, NULL, 1, NULL),
                        (1, {c8}, X'6472616674', 2, 2000, 0, 'ws:a', NULL, 0, 9000),
                        (2, {c9}, X'78', 1, 3000, 2, NULL, {c7}, 0, NULL);
             INSERT INTO refs (repo, name, target, version, kind)
                 VALUES (1, 'main', {c7}, 3, 0), (1, 'v1', {c7}, 1, 1),
                        (1, 'graft.followed/main', {c7}, 1, 2), (2, 'main', {c9}, 1, 0);
             INSERT INTO journal (repo, lsn, kind, body)
                 VALUES (1, 1, 'ChangesetCreated', X'01'), (1, 2, 'RefMoved', X'0203'),
                        (2, 1, 'ChangesetCreated', X'04');
             INSERT INTO workspaces
                 (repo, name, owner, session, line, base, head, lease_until, work_item)
                 VALUES (1, 'a', 'alice', 's1', 'main', {c7}, {c8}, 5000, 'WI-1');
             INSERT INTO derived (repo, kind, ver, key, value)
                 VALUES (1, 'tree', 1, X'aa', X'bb');
             INSERT INTO line_pos (repo, line, changeset, pos) VALUES (1, 'main', {c7}, 3);
             INSERT INTO line_paths (repo, line, path, pos, changeset)
                 VALUES (1, 'main', 'src/a.rs', 3, {c7});
             INSERT INTO holders (repo, object, holder, expires)
                 VALUES (1, {c8}, 'upload:9000:alice', 9000), (1, {c7}, 'line:main', NULL);
             INSERT INTO uploads (repo, object, owner, expires) VALUES (1, {c8}, 'alice', 9000);
             INSERT INTO doomed (repo, object, since, deleting) VALUES (1, {c9}, 100, 1);
             INSERT INTO lands (repo, draft, line, landed) VALUES (1, {c8}, 'main', {c7});
             INSERT INTO tombstones (repo, id, at, reason) VALUES (2, {c8}, 50, 'gc');
             INSERT INTO line_forks (repo, line, parent, pos) VALUES (1, 'dev', 'main', 3);
             INSERT INTO draft_paths (repo, line, path, workspace)
                 VALUES (1, 'main', 'src/a.rs', 'a');
             INSERT INTO line_merges (repo, dst, dst_pos, src, src_changeset, src_pos)
                 VALUES (1, 'main', 4, 'dev', {c7}, 3);
             INSERT INTO attestations
                 (repo, lsn, seq, changeset, line, pos, kind, name, status, by, url)
                 VALUES (1, 2, 0, {c7}, 'main', 3, 0, 'ci', 0, 'bot', 'https://ci/1'),
                        (1, 2, 1, {c7}, NULL, NULL, 1, 'review', 1, NULL, '');
             INSERT INTO land_checks (repo, line, pos, changeset, state, revert)
                 VALUES (1, 'main', 3, {c7}, 1, NULL), (1, 'main', 4, {c8}, 3, {c9});
             INSERT INTO review_queue (repo, line, lsn, pos, changeset)
                 VALUES (1, 'main', 2, 3, {c7});",
            c7 = h(7),
            c8 = h(8),
            c9 = h(9),
        ))
        .expect("seed");
        v8_tables = tables(&conn);
        before = v8_tables
            .iter()
            .map(|t| (t.clone(), rows_of(&conn, t)))
            .collect();
        // Every version 8 table holds at least one row.
        for (t, rows) in &before {
            assert!(!rows.is_empty(), "{t} is seeded");
        }
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let conn = Connection::open(dir.db()).expect("inspect");
    assert_eq!(migrations::version(&conn).expect("version"), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 12);
    assert_eq!(tables(&conn), TABLES);
    for (t, rows) in &before {
        assert_eq!(&rows_of(&conn, t), rows, "{t} survives intact");
    }
    let new: Vec<&&str> = TABLES
        .iter()
        .filter(|t| !v8_tables.iter().any(|o| o == *t))
        .collect();
    assert_eq!(
        new,
        [
            &"mirror_leases",
            &"mirror_line_slots",
            &"mirror_requests",
            &"repo_mirrors",
            &"repo_owners"
        ]
    );
    for t in new {
        assert!(rows_of(&conn, t).is_empty(), "{t} starts empty");
    }

    // The old rows still read through the store, and the mirror reads are
    // empty.
    let repo = block_on(store.repo("old")).expect("repo").expect("exists");
    let (content, record) = block_on(store.changeset(repo, cs))
        .expect("read")
        .expect("changeset");
    assert_eq!(content, b"content");
    assert_eq!(record.state, graft_store::RecordState::Landed);
    let main = block_on(store.get_ref(repo, "main"))
        .expect("ref")
        .expect("main");
    assert_eq!((main.target, main.version), (cs, 3));
    assert_eq!(block_on(store.journal_tip(repo)).expect("tip"), Some(2));
    assert_eq!(
        block_on(store.line_pos(repo, "main", cs)).expect("pos"),
        Some(3)
    );
    assert_eq!(block_on(store.repo_owner(repo)).expect("owner"), None);
    assert_eq!(block_on(store.get_lease(repo)).expect("lease"), None);
    assert!(
        block_on(store.list_mirrors(graft_store::MirrorQuery {
            repo: None,
            enabled: None,
            after: None,
            limit: 10,
        }))
        .expect("mirrors")
        .is_empty()
    );
    // New writes take the next journal position.
    let txn = MetaTxn {
        preconditions: vec![],
        writes: vec![Write::Journal {
            kind: "RefMoved".to_owned(),
            body: vec![],
        }],
    };
    assert_eq!(
        block_on(store.commit(repo, txn)).expect("commit"),
        TxnOutcome::Committed(vec![3])
    );
}
