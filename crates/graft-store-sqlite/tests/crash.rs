//! The crash harness (design §6.6): a child process writes a mix of ref
//! CAS, changeset inserts and workspace updates from several threads and
//! prints each acknowledged write; the parent kills it with `SIGKILL` at a
//! random point, reopens the database, and checks that
//!
//! - every acknowledged write is present, with its journal entry at the
//!   acknowledged LSN;
//! - the journal's LSNs run from 1 with no gaps;
//! - the journal is topological: a changeset's parent, and a ref's target,
//!   were journalled before it.
//!
//! Twenty kills, all against the same database, so recovery is also tested
//! on a database that has already crashed.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use common::{TempDir, block_on};
use graft_model::{ChangesetId, ObjectId};
use graft_store::{
    ChangesetRecord, MetaStore, MetaTxn, Precondition, RecordState, RefKind, RefRecord, RepoId,
    TxnOutcome, WorkspaceRecord, Write,
};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};

const CHILD_DB: &str = "GRAFT_SQLITE_CRASH_DB";
const CHILD_ITER: &str = "GRAFT_SQLITE_CRASH_ITER";
const ITERATIONS: u8 = 20;
const WRITERS: u8 = 4;

fn hex(id: ChangesetId) -> String {
    id.0.to_hex()
}

fn journal(body: &str) -> Write {
    let kind = match body.as_bytes()[0] {
        b'C' => "ChangesetCreated",
        b'R' => "RefMoved",
        _ => "WorkspaceRenewed",
    };
    Write::Journal {
        kind: kind.to_owned(),
        body: body.as_bytes().to_vec(),
    }
}

fn committed(out: TxnOutcome, what: &str) -> u64 {
    match out {
        TxnOutcome::Committed(lsns) if lsns.len() == 1 => lsns[0],
        other => panic!("{what}: {other:?}"),
    }
}

/// One writer thread of the child: its own ref and workspace, and a chain of
/// changesets, each the parent of the next.
fn child_writer(store: &SqliteMetaStore, repo: RepoId, iter: u8, k: u8) {
    let ref_name = format!("line/t{k}");
    let ws_name = format!("ws-t{k}");
    let mut current = block_on(store.get_ref(repo, &ref_name))
        .expect("read ref")
        .map(|r| (r.target, r.version));
    let mut ws_head = block_on(store.workspace(repo, &ws_name))
        .expect("read workspace")
        .map(|w| w.head);
    let ack = |line: String| {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").expect("stdout");
        out.flush().expect("flush");
    };

    let mut seq: u64 = 0;
    loop {
        seq += 1;
        let mut bytes = [0u8; 32];
        bytes[0] = k;
        bytes[1] = iter;
        bytes[2..10].copy_from_slice(&seq.to_be_bytes());
        let id = ChangesetId(ObjectId(bytes));
        let parent = current.map_or_else(|| "root".to_owned(), |(t, _)| hex(t));

        // A changeset insert.
        let body = format!("C {}", hex(id));
        let txn = MetaTxn {
            preconditions: vec![Precondition::ChangesetAbsent(id)],
            writes: vec![
                Write::PutChangeset {
                    id,
                    content: parent.into_bytes(),
                    record: ChangesetRecord {
                        lsn: 0,
                        received_at_ms: 0,
                        state: RecordState::Draft,
                        holder: None,
                        superseded_by: None,
                    },
                },
                journal(&body),
            ],
        };
        let lsn = committed(block_on(store.commit(repo, txn)).expect("insert"), &body);
        ack(format!("J {lsn} {body}"));

        // A ref CAS onto it.
        let version = current.map_or(1, |(_, v)| v + 1);
        let body = format!("R {ref_name} {version} {}", hex(id));
        let txn = MetaTxn {
            preconditions: vec![Precondition::RefIs {
                name: ref_name.clone(),
                expected: current,
            }],
            writes: vec![
                Write::SetRef(RefRecord {
                    name: ref_name.clone(),
                    target: id,
                    version,
                    kind: RefKind::Line,
                }),
                journal(&body),
            ],
        };
        let lsn = committed(block_on(store.commit(repo, txn)).expect("cas"), &body);
        ack(format!("J {lsn} {body}"));
        current = Some((id, version));

        // A workspace head update.
        let body = format!("W {ws_name} {}", hex(id));
        let preconditions = match ws_head {
            Some(head) => vec![Precondition::WorkspaceHeadIs {
                name: ws_name.clone(),
                head,
            }],
            None => vec![],
        };
        let txn = MetaTxn {
            preconditions,
            writes: vec![
                Write::PutWorkspace(WorkspaceRecord {
                    name: ws_name.clone(),
                    owner: "crash".to_owned(),
                    session: None,
                    line: "main".to_owned(),
                    base: id,
                    head: Some(id),
                    lease_until_ms: i64::MAX,
                }),
                journal(&body),
            ],
        };
        let lsn = committed(block_on(store.commit(repo, txn)).expect("workspace"), &body);
        ack(format!("J {lsn} {body}"));
        ws_head = Some(Some(id));
    }
}

/// The child process. Under a normal test run it returns at once.
#[test]
fn crash_child() {
    let Ok(db) = std::env::var(CHILD_DB) else {
        return;
    };
    let iter: u8 = std::env::var(CHILD_ITER)
        .expect("iteration")
        .parse()
        .expect("iteration number");
    let store = Arc::new(SqliteMetaStore::open(SqliteConfig::new(db)).expect("open"));
    let repo = match block_on(store.repo("crash")).expect("lookup") {
        Some(repo) => repo,
        None => block_on(store.create_repo("crash")).expect("create"),
    };
    let writers: Vec<_> = (0..WRITERS)
        .map(|k| {
            let store = store.clone();
            std::thread::spawn(move || child_writer(&store, repo, iter, k))
        })
        .collect();
    for w in writers {
        w.join().expect("writer");
    }
}

/// An acknowledged write: its LSN and journal body.
type Ack = (u64, String);

/// Runs one child, kills it after `delay` from its first acknowledgement,
/// and returns what it acknowledged.
fn run_child(db: &std::path::Path, iter: u8, delay: Duration) -> Vec<Ack> {
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
        .env(CHILD_DB, db)
        .env(CHILD_ITER, iter.to_string())
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
                // A line cut short by the kill was never acknowledged.
                Ok(_) if !line.ends_with('\n') => break,
                Ok(_) => {}
            }
            let Some(rest) = line.trim_end().strip_prefix("J ") else {
                continue;
            };
            let (lsn, body) = rest.split_once(' ').expect("ack line");
            if acks.is_empty() {
                let _ = tx.send(());
            }
            acks.push((lsn.parse().expect("lsn"), body.to_owned()));
        }
        acks
    });

    rx.recv_timeout(Duration::from_secs(30))
        .expect("the child acknowledged nothing");
    std::thread::sleep(delay);
    child.kill().expect("kill child");
    let status = child.wait().expect("wait child");
    assert_eq!(status.signal(), Some(9), "the child died by SIGKILL");
    reader.join().expect("reader")
}

/// Checks the database after a crash against what was acknowledged.
fn verify(db: &std::path::Path, acks: &[Ack]) -> u64 {
    let store = SqliteMetaStore::open(SqliteConfig::new(db)).expect("reopen");
    let repo = block_on(store.repo("crash"))
        .expect("lookup")
        .expect("the repository survived");

    // The whole journal, page by page: LSNs 1..=N, no gaps.
    let mut entries: BTreeMap<u64, String> = BTreeMap::new();
    let mut from = 1;
    loop {
        let page = block_on(store.journal(repo, from, 1000)).expect("journal");
        if page.is_empty() {
            break;
        }
        for (lsn, _, body) in page {
            assert_eq!(lsn, from, "journal LSNs are gap-free");
            entries.insert(lsn, String::from_utf8(body).expect("utf-8 body"));
            from += 1;
        }
    }

    // Every acknowledged write is present, at its LSN.
    for (lsn, body) in acks {
        assert_eq!(
            entries.get(lsn),
            Some(body),
            "acknowledged write at LSN {lsn} survived"
        );
    }

    // Changesets by id: the LSN that created each one.
    let mut created: BTreeMap<String, u64> = BTreeMap::new();
    for (lsn, body) in &entries {
        let mut words = body.split(' ');
        match words.next() {
            Some("C") => {
                let id = words.next().expect("id").to_owned();
                created.insert(id, *lsn);
            }
            Some("R") => {
                let (name, version, target) = (
                    words.next().expect("name"),
                    words.next().expect("version").parse::<u64>().expect("n"),
                    words.next().expect("target"),
                );
                assert!(
                    created.get(target).is_some_and(|c| c < lsn),
                    "ref {name} v{version} at {lsn} follows its target's creation"
                );
                let stored = block_on(store.get_ref(repo, name))
                    .expect("read ref")
                    .expect("ref present");
                assert!(stored.version >= version, "ref {name} kept v{version}");
            }
            Some("W") => {
                let name = words.next().expect("name");
                assert!(
                    block_on(store.workspace(repo, name))
                        .expect("read workspace")
                        .is_some(),
                    "workspace {name} present"
                );
            }
            other => panic!("unexpected journal body {other:?}"),
        }
    }

    // Each changeset's record carries its creating LSN, and its parent was
    // created before it.
    for (id_hex, lsn) in &created {
        let id = ChangesetId(ObjectId(decode(id_hex)));
        let (content, record) = block_on(store.changeset(repo, id))
            .expect("read changeset")
            .expect("changeset present");
        assert_eq!(record.lsn, *lsn);
        let parent = String::from_utf8(content).expect("utf-8 content");
        if parent != "root" {
            assert!(
                created.get(&parent).is_some_and(|p| p < lsn),
                "changeset at {lsn} follows its parent"
            );
        }
    }
    entries.len() as u64
}

fn decode(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex");
    }
    out
}

#[test]
fn every_acknowledged_write_survives_kill_9() {
    let dir = TempDir::new("crash");
    let db = dir.db();
    // A fixed-seed generator for the kill delays, so a failure reproduces.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let started = Instant::now();
    let mut total_acks = 0;
    for iter in 1..=ITERATIONS {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let delay = Duration::from_millis(20 + (seed >> 33) % 300);
        let acks = run_child(&db, iter, delay);
        assert!(!acks.is_empty());
        total_acks += acks.len();
        let journalled = verify(&db, &acks);
        assert!(journalled >= acks.len() as u64);
    }
    eprintln!(
        "crash harness: {ITERATIONS} kills, {total_acks} acknowledged writes, {:?}",
        started.elapsed()
    );
}
