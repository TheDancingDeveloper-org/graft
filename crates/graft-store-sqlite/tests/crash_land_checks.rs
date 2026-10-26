//! Accept 3 of WI-2593: a checked land and its `pending` land-checks row
//! commit together. A child process lands on a checked line and prints each
//! acknowledged land; the parent kills it with `SIGKILL`, reopens the
//! database, and checks that every acknowledged land has both rows and that
//! no land row exists without its `pending` row, or the reverse.

#![cfg(unix)]

mod common;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::{TempDir, block_on};
use graft_model::{ChangesetId, ObjectId};
use graft_store::{MetaStore, MetaTxn, TxnOutcome, Write};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use rusqlite::Connection;

const CHILD_DB: &str = "GRAFT_SQLITE_LAND_CHECK_DB";
const ITERATIONS: u8 = 5;
const LINE: &str = "checked";

fn id(tag: u8, pos: u64) -> ChangesetId {
    let mut bytes = [tag; 32];
    bytes[..8].copy_from_slice(&pos.to_be_bytes());
    ChangesetId(ObjectId(bytes))
}

#[test]
fn land_check_child() {
    let Ok(db) = std::env::var(CHILD_DB) else {
        return;
    };
    let store = SqliteMetaStore::open(SqliteConfig::new(db)).expect("open");
    let repo = match block_on(store.repo("lc")).expect("lookup") {
        Some(repo) => repo,
        None => block_on(store.create_repo("lc")).expect("create"),
    };
    // Continue after the last land a previous child left.
    let mut pos = 1;
    loop {
        let page = block_on(store.land_checks(repo, LINE, pos, 1000)).expect("checks");
        let Some(last) = page.last() else { break };
        pos = last.pos + 1;
    }
    loop {
        let txn = MetaTxn {
            preconditions: vec![],
            writes: vec![
                Write::PutLand {
                    draft: id(1, pos),
                    line: LINE.to_owned(),
                    landed: id(2, pos),
                    pos: Some(pos),
                    checked: true,
                },
                Write::Journal {
                    kind: "Landed".to_owned(),
                    body: pos.to_be_bytes().to_vec(),
                },
            ],
        };
        let out = block_on(store.commit(repo, txn)).expect("land");
        assert!(matches!(out, TxnOutcome::Committed(_)), "{out:?}");
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "L {pos}").expect("stdout");
        stdout.flush().expect("flush");
        pos += 1;
    }
}

fn run_child(db: &std::path::Path, delay: Duration) -> Vec<u64> {
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "land_check_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DB, db)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut acks = Vec::new();
        let mut lines = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if !line.ends_with('\n') => break,
                Ok(_) => {}
            }
            let Some(pos) = line.trim_end().strip_prefix("L ") else {
                continue;
            };
            if acks.is_empty() {
                let _ = tx.send(());
            }
            acks.push(pos.parse().expect("position"));
        }
        acks
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the child acknowledged nothing");
    // The delay only varies where the kill lands; no assertion depends on it.
    std::thread::sleep(delay);
    child.kill().expect("kill child");
    let status = child.wait().expect("wait child");
    assert_eq!(status.signal(), Some(9), "the child died by SIGKILL");
    reader.join().expect("reader")
}

/// Positions with a land row, and positions with a `pending` land-checks
/// row, read straight from the tables.
fn rows(db: &std::path::Path) -> (BTreeSet<u64>, BTreeSet<u64>) {
    let conn = Connection::open(db).expect("inspect");
    let lands = conn
        .prepare("SELECT landed FROM lands WHERE line = ?1")
        .expect("lands")
        .query_map([LINE], |r| r.get::<_, Vec<u8>>(0))
        .expect("query")
        .map(|b| {
            let b = b.expect("row");
            assert_eq!(b[8..], [2; 24], "a landed id");
            u64::from_be_bytes(b[..8].try_into().expect("8 bytes"))
        })
        .collect();
    let checks = conn
        .prepare("SELECT pos, changeset, state FROM land_checks WHERE line = ?1")
        .expect("checks")
        .query_map([LINE], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .expect("query")
        .map(|row| {
            let (pos, changeset, state) = row.expect("row");
            let pos = u64::try_from(pos).expect("position");
            assert_eq!(changeset, id(2, pos).0.0.to_vec(), "row names its land");
            assert_eq!(state, 0, "pending");
            pos
        })
        .collect();
    (lands, checks)
}

#[test]
fn a_land_and_its_pending_row_survive_kill_9_together() {
    let dir = TempDir::new("land-check-crash");
    let db = dir.db();
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut acknowledged = BTreeSet::new();
    for _ in 0..ITERATIONS {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let acks = run_child(&db, Duration::from_millis(10 + (seed >> 33) % 200));
        assert!(!acks.is_empty());
        acknowledged.extend(acks);

        // Reopen through the store (recovery), then read the tables.
        drop(SqliteMetaStore::open(SqliteConfig::new(&db)).expect("reopen"));
        let (lands, checks) = rows(&db);
        assert_eq!(lands, checks, "every land has its pending row, and no more");
        assert!(
            acknowledged.is_subset(&lands),
            "every acknowledged land survived"
        );
    }
}
