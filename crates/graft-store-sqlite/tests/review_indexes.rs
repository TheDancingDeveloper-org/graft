//! Attestations, land checks and the review queue (WI-2593, schema 8): the
//! SQLite store answers exactly as the memory store does, every range read
//! is an index search, a failed land leaves neither its land nor its
//! `pending` row, and a version 7 database upgrades with its rows intact.

mod common;

use std::collections::BTreeMap;

use common::{TempDir, block_on};
use graft_model::{ChangesetId, ObjectId};
use graft_store::memory::MemoryStore;
use graft_store::{
    AttestKind, AttestStatus, LandCheckState, MetaStore, MetaTxn, RepoId, StoreError, TxnOutcome,
    Write,
};
use graft_store_sqlite::migrations::{self, SCHEMA_VERSION};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use rusqlite::Connection;

fn cs(n: u8) -> ChangesetId {
    ChangesetId(ObjectId([n; 32]))
}

fn journal(kind: &str) -> Write {
    Write::Journal {
        kind: kind.to_owned(),
        body: kind.as_bytes().to_vec(),
    }
}

fn land(n: u8, line: &str, pos: u64, checked: bool) -> MetaTxn {
    MetaTxn {
        preconditions: vec![],
        writes: vec![
            journal("Landed"),
            Write::PutLand {
                draft: cs(100 + n),
                line: line.to_owned(),
                landed: cs(n),
                pos: Some(pos),
                checked,
            },
        ],
    }
}

fn attest(n: u8, line_pos: Option<(&str, u64)>, kind: AttestKind, name: &str) -> Write {
    Write::PutAttestation {
        changeset: cs(n),
        line_pos: line_pos.map(|(l, p)| (l.to_owned(), p)),
        kind,
        name: name.to_owned(),
        status: AttestStatus::Pass,
        by: Some("ci".to_owned()),
        url: format!("https://ci.example/{n}/{name}"),
    }
}

fn attested(writes: Vec<Write>) -> MetaTxn {
    let mut all = vec![journal("Attested")];
    all.extend(writes);
    MetaTxn {
        preconditions: vec![],
        writes: all,
    }
}

/// The scenario both stores run: lands on two lines (some checked), a
/// retried land, attestations with and without positions, a review, a land
/// check update, and the writes the stores must refuse.
fn scenario() -> Vec<MetaTxn> {
    let mut txns = Vec::new();
    for n in 1..=6u8 {
        txns.push(land(n, "main", u64::from(n), n % 2 == 1));
    }
    txns.push(land(7, "side", 1, true));
    // A retried land: no new rows, and no error for the duplicate check.
    txns.push(land(1, "main", 1, true));
    txns.push(attested(vec![
        attest(1, Some(("main", 1)), AttestKind::Check, "build"),
        attest(1, Some(("main", 1)), AttestKind::Check, "test"),
        attest(3, Some(("main", 3)), AttestKind::Check, "build"),
    ]));
    txns.push(attested(vec![
        attest(2, None, AttestKind::Review, "alice"),
        attest(5, Some(("main", 5)), AttestKind::Approval, "bob"),
        Write::SetLandCheck {
            line: "main".to_owned(),
            pos: 1,
            state: LandCheckState::Good,
            revert: None,
        },
    ]));
    txns.push(MetaTxn {
        preconditions: vec![],
        writes: vec![Write::SetLandCheck {
            line: "main".to_owned(),
            pos: 3,
            state: LandCheckState::Reverted,
            revert: Some(cs(9)),
        }],
    });
    // Reviewed before it lands: it never enters the review queue.
    txns.push(attested(vec![attest(8, None, AttestKind::Review, "carol")]));
    txns.push(land(8, "main", 7, false));
    txns
}

/// Writes both stores must refuse, each leaving nothing behind.
fn refused() -> Vec<MetaTxn> {
    vec![
        // An attestation without its journal entry.
        MetaTxn {
            preconditions: vec![],
            writes: vec![attest(1, None, AttestKind::Check, "lint")],
        },
        // A land check update at a position with no row.
        MetaTxn {
            preconditions: vec![],
            writes: vec![Write::SetLandCheck {
                line: "main".to_owned(),
                pos: 2,
                state: LandCheckState::Good,
                revert: None,
            }],
        },
        // A second checked land at a position that has a row.
        land(20, "main", 1, true),
        // A checked land without a position.
        MetaTxn {
            preconditions: vec![],
            writes: vec![
                journal("Landed"),
                Write::PutLand {
                    draft: cs(121),
                    line: "main".to_owned(),
                    landed: cs(21),
                    pos: None,
                    checked: true,
                },
            ],
        },
        // An indexed land without a journal entry.
        MetaTxn {
            preconditions: vec![],
            writes: vec![Write::PutLand {
                draft: cs(122),
                line: "main".to_owned(),
                landed: cs(22),
                pos: Some(9),
                checked: false,
            }],
        },
    ]
}

/// Everything the four reads answer, paged with a small limit so the
/// cursors are exercised.
async fn snapshot<M: MetaStore>(store: &M, repo: RepoId) -> String {
    let mut out = String::new();
    for n in [1u8, 2, 3, 5, 8, 20, 21] {
        let mut after = None;
        loop {
            let page = store
                .attestations_by_changeset(repo, cs(n), after, 1)
                .await
                .expect("by changeset");
            let Some(last) = page.last() else { break };
            after = Some((last.lsn, last.seq));
            out += &format!("cs {n}: {page:?}\n");
        }
    }
    for (from, to) in [(0, u64::MAX), (1, 1), (2, 5), (4, 4)] {
        let mut after = None;
        loop {
            let page = store
                .attestations_by_line(repo, "main", from, to, after, 2)
                .await
                .expect("by line");
            let Some(last) = page.last() else { break };
            let (_, pos) = last.line_pos.clone().expect("indexed by line");
            after = Some((pos, last.lsn, last.seq));
            out += &format!("line {from}..={to}: {page:?}\n");
        }
    }
    for line in ["main", "side", "none"] {
        let mut from = 0;
        loop {
            let page = store
                .land_checks(repo, line, from, 2)
                .await
                .expect("land checks");
            let Some(last) = page.last() else { break };
            from = last.pos + 1;
            out += &format!("checks {line}: {page:?}\n");
        }
        let mut since = 0;
        loop {
            let page = store
                .review_queue(repo, line, since, 2)
                .await
                .expect("review queue");
            let Some(last) = page.last() else { break };
            since = last.lsn;
            out += &format!("queue {line}: {page:?}\n");
        }
    }
    out
}

/// Runs the scenario and the refused writes, and returns what each commit
/// answered and the snapshot.
async fn run<M: MetaStore>(store: &M) -> (Vec<String>, String) {
    let repo = store.create_repo("r").await.expect("repo");
    let mut outcomes = Vec::new();
    for txn in scenario() {
        let out = store.commit(repo, txn).await.expect("scenario write");
        assert!(matches!(out, TxnOutcome::Committed(_)), "{out:?}");
        outcomes.push(format!("{out:?}"));
    }
    let before = snapshot(store, repo).await;
    let journal = store.journal(repo, 1, 1000).await.expect("journal").len();
    for txn in refused() {
        let err = store.commit(repo, txn).await.expect_err("refused write");
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }
    assert_eq!(
        snapshot(store, repo).await,
        before,
        "refused writes left nothing"
    );
    let after = store.journal(repo, 1, 1000).await.expect("journal").len();
    assert_eq!(after, journal, "refused writes journalled nothing");
    (outcomes, before)
}

/// Accept 1: the SQLite store answers every new read and write exactly as
/// the memory store (the reference) does.
#[test]
fn sqlite_matches_the_memory_store() {
    let memory = block_on(run(&MemoryStore::new()));
    let dir = TempDir::new("review-conformance");
    let sqlite = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let got = block_on(run(&sqlite));
    assert_eq!(got.0, memory.0);
    assert_eq!(got.1, memory.1);
    // The scenario reached every read: rows on both lines and the queue.
    for needle in [
        "cs 1:",
        "line 2..=5:",
        "checks side:",
        "queue main:",
        "Reverted",
    ] {
        assert!(got.1.contains(needle), "{needle} in\n{}", got.1);
    }
    // Reviewed changesets 2 and 8 are not queued; the others on main are.
    let repo = block_on(sqlite.repo("r")).expect("lookup").expect("repo");
    let queued: Vec<ChangesetId> = block_on(sqlite.review_queue(repo, "main", 0, 100))
        .expect("queue")
        .into_iter()
        .map(|row| row.changeset)
        .collect();
    assert_eq!(queued, vec![cs(1), cs(3), cs(4), cs(5), cs(6)]);
    // The rows survive a reopen.
    drop(sqlite);
    let reopened = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("reopen");
    let repo = block_on(reopened.repo("r")).expect("lookup").expect("repo");
    assert_eq!(block_on(snapshot(&reopened, repo)), memory.1);
}

/// Accept 3 (in process): a land whose transaction fails after its land
/// row and `pending` row are written keeps neither.
#[test]
fn a_failed_land_leaves_neither_row() {
    let dir = TempDir::new("review-fault");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let repo = block_on(store.create_repo("r")).expect("repo");
    let mut txn = land(1, "main", 1, true);
    // The fault: a write after the land that the store refuses.
    txn.writes.push(Write::SetLandCheck {
        line: "main".to_owned(),
        pos: 99,
        state: LandCheckState::Good,
        revert: None,
    });
    let err = block_on(store.commit(repo, txn)).expect_err("refused");
    assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    assert_eq!(
        block_on(store.land(repo, cs(101), "main")).expect("land"),
        None
    );
    assert!(
        block_on(store.land_checks(repo, "main", 0, 10))
            .expect("checks")
            .is_empty()
    );
    assert!(
        block_on(store.review_queue(repo, "main", 0, 10))
            .expect("queue")
            .is_empty()
    );
    // The same land without the fault keeps both.
    block_on(store.commit(repo, land(1, "main", 1, true))).expect("land");
    assert_eq!(
        block_on(store.land(repo, cs(101), "main")).expect("land"),
        Some(cs(1))
    );
    let checks = block_on(store.land_checks(repo, "main", 0, 10)).expect("checks");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].state, LandCheckState::Pending);
}

fn plan(conn: &Connection, sql: &str) -> String {
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("plan");
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    rows.join("; ")
}

/// Accept 4: each range read (the store's query text, pinned to its index
/// with `INDEXED BY` so an unanalysed table cannot fall back to the key) is an index
/// search, never a table scan, and needs no sort.
#[test]
fn the_range_reads_use_the_indexes() {
    let dir = TempDir::new("review-plan");
    let _store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let conn = Connection::open(dir.db()).expect("inspect");
    let cols = "lsn, seq, changeset, line, pos, kind, name, status, by, url FROM attestations";
    let reads = [
        (
            format!(
                "SELECT {cols} INDEXED BY attestations_changeset WHERE repo = 1 AND changeset = x'00' AND (lsn, seq) > (0, 0)
                 ORDER BY lsn, seq LIMIT 5"
            ),
            "attestations_changeset",
        ),
        (
            format!(
                "SELECT {cols} INDEXED BY attestations_line WHERE repo = 1 AND line = 'main' AND pos <= 9
                   AND (pos, lsn, seq) > (0, 0, 0) ORDER BY pos, lsn, seq LIMIT 5"
            ),
            "attestations_line",
        ),
        (
            "SELECT pos, changeset, state, revert FROM land_checks
             WHERE repo = 1 AND line = 'main' AND pos >= 0 ORDER BY pos LIMIT 5"
                .to_owned(),
            "PRIMARY KEY",
        ),
        (
            "SELECT lsn, pos, changeset FROM review_queue
             WHERE repo = 1 AND line = 'main' AND lsn > 0 ORDER BY lsn LIMIT 5"
                .to_owned(),
            "PRIMARY KEY",
        ),
        (
            "SELECT 1 FROM attestations INDEXED BY attestations_changeset WHERE repo = 1 AND changeset = x'00' AND kind = 1 LIMIT 1"
                .to_owned(),
            "attestations_changeset",
        ),
        (
            "DELETE FROM review_queue INDEXED BY review_queue_changeset WHERE repo = 1 AND changeset = x'00'".to_owned(),
            "review_queue_changeset",
        ),
    ];
    for (sql, index) in reads {
        let p = plan(&conn, &sql);
        assert!(p.contains("SEARCH"), "{p}");
        assert!(p.contains(index), "{index}: {p}");
        assert!(!p.contains("SCAN"), "no scan: {p}");
        assert!(!p.contains("TEMP B-TREE"), "no sort: {p}");
    }
}

/// Every row of every table, in key order.
fn dump(conn: &Connection, tables: &[&str]) -> BTreeMap<String, Vec<String>> {
    tables
        .iter()
        .map(|t| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT quote(*) FROM (SELECT * FROM {t}) ORDER BY 1"
                ))
                .or_else(|_| conn.prepare(&format!("SELECT * FROM {t}")))
                .expect("select");
            let n = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(|v| v.join("|"))
                })
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("rows");
            rows.sort();
            ((*t).to_owned(), rows)
        })
        .collect()
}

/// Accept 2 and "done when" 2: a version 7 database with rows in every
/// table upgrades to version 8 with every old row unchanged, and the new
/// indexes work on its repository.
#[test]
fn a_version_7_database_upgrades_with_its_rows_intact() {
    const OLD: &[&str] = &[
        "changesets",
        "derived",
        "doomed",
        "draft_paths",
        "holders",
        "journal",
        "lands",
        "line_forks",
        "line_merges",
        "line_paths",
        "line_pos",
        "refs",
        "repos",
        "tombstones",
        "uploads",
        "workspaces",
    ];
    let dir = TempDir::new("review-upgrade");
    let before = {
        let mut conn = Connection::open(dir.db()).expect("create");
        migrations::migrate_to(&mut conn, 7).expect("migrate to v7");
        assert_eq!(migrations::version(&conn).expect("version"), 7);
        conn.execute_batch(
            "INSERT INTO repos (id, name, gc_protection) VALUES (1, 'old', 1);
             INSERT INTO changesets (repo, id, content, lsn, received_at, state, holder,
                                     superseded_by, pins, expires_at)
                 VALUES (1, x'0101', x'c0ffee', 1, 10, 1, 'h', NULL, 2, NULL);
             INSERT INTO refs VALUES (1, 'main', x'0101', 3, 0);
             INSERT INTO journal VALUES (1, 1, 'Landed', x'0a0b');
             INSERT INTO journal VALUES (1, 2, 'RefMoved', x'0c');
             INSERT INTO workspaces VALUES (1, 'w', 'o', 's', 'main', x'01', NULL, 99, 'item');
             INSERT INTO derived VALUES (1, 'k', 1, x'01', x'02');
             INSERT INTO line_pos VALUES (1, 'main', x'0101', 1);
             INSERT INTO line_paths VALUES (1, 'main', 'a.txt', 1, x'0101');
             INSERT INTO holders VALUES (1, x'02', 'ref:main', NULL);
             INSERT INTO uploads VALUES (1, x'03', 'o', 50);
             INSERT INTO doomed VALUES (1, x'04', 7, 0);
             INSERT INTO lands VALUES (1, x'0909', 'main', x'0101');
             INSERT INTO tombstones VALUES (1, x'05', 8, 'gone');
             INSERT INTO line_forks VALUES (1, 'b', 'main', 1);
             INSERT INTO line_merges VALUES (1, 'main', 2, 'b', x'0101', 1);
             INSERT INTO draft_paths VALUES (1, 'main', 'a.txt', 'w');",
        )
        .expect("old rows");
        dump(&conn, OLD)
    };
    for (table, rows) in &before {
        assert!(!rows.is_empty(), "{table} has a row before the upgrade");
    }

    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("upgrade");
    let repo = block_on(store.repo("old"))
        .expect("lookup")
        .expect("old repo");
    // The old rows read back through the store as before.
    assert_eq!(
        block_on(store.journal(repo, 1, 10)).expect("journal").len(),
        2
    );
    assert_eq!(
        block_on(store.land(repo, ChangesetId(ObjectId([0; 32])), "main")).expect("land"),
        None
    );
    {
        let conn = Connection::open(dir.db()).expect("inspect");
        assert_eq!(migrations::version(&conn).expect("version"), SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 8);
        assert_eq!(dump(&conn, OLD), before, "every old row is unchanged");
    }
    // The new indexes work on the upgraded repository.
    block_on(store.commit(repo, land(1, "main", 2, true))).expect("land");
    assert_eq!(
        block_on(store.land_checks(repo, "main", 0, 10))
            .expect("checks")
            .len(),
        1
    );
    assert_eq!(
        block_on(store.review_queue(repo, "main", 0, 10))
            .expect("queue")
            .len(),
        1
    );
}

/// `journal_tip` is `max(lsn)` on the journal primary key `(repo, lsn)`,
/// not a table scan and not a walk of the rows.
#[test]
fn journal_tip_uses_the_primary_key() {
    let dir = TempDir::new("journal-tip-plan");
    let _store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    let conn = Connection::open(dir.db()).expect("inspect");
    let p = plan(&conn, "SELECT max(lsn) FROM journal WHERE repo = 1");
    assert!(p.contains("SEARCH") && p.contains("journal"), "{p}");
    assert!(!p.contains("SCAN"), "no scan: {p}");
}
