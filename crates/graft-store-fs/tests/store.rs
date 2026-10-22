//! Behaviour of the filesystem store beyond the conformance suite: read
//! verification and fault injection, temp-file cleanup, the root lock,
//! recounting at open, and the LRU bound (work item G1.5).

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use graft_model::{ObjectId, ObjectKind};
use graft_proto::frame::{Codec, Frame, HEADER_LEN};
use graft_store::{ObjectStore, RepoId, StoreError, StoreStats};
use graft_store_fs::{FsMode, FsObjectStore};
use graft_testkit::crash::{TempDir, block_on};

const REPO: RepoId = RepoId(1);

/// A payload of `len` bytes; `seed` makes it unique.
fn payload(seed: u64, len: usize) -> Vec<u8> {
    let mut out = format!("payload {seed} ").into_bytes();
    out.extend(
        b"graft "
            .iter()
            .copied()
            .cycle()
            .take(len.saturating_sub(out.len())),
    );
    out.truncate(len.max(1));
    out
}

/// A blob frame and its id.
fn blob(seed: u64, len: usize) -> (ObjectId, Vec<u8>) {
    let payload = payload(seed, len);
    let id = ObjectId::hash(ObjectKind::Blob, &payload);
    (id, Frame::encode(ObjectKind::Blob, &payload).to_bytes())
}

fn open(root: &Path, mode: FsMode) -> FsObjectStore {
    FsObjectStore::open(root, mode).expect("open")
}

fn stats(store: &FsObjectStore) -> StoreStats {
    block_on(store.stats()).expect("stats")
}

/// Every file under the root except the lock file.
fn files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("list") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|n| n != ".lock") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn disk_bytes(root: &Path) -> u64 {
    files(root)
        .iter()
        .map(|p| fs::metadata(p).expect("stat").len())
        .sum()
}

fn assert_corrupt(store: &FsObjectStore, id: ObjectId, what: &str) {
    match block_on(store.get(REPO, id)) {
        Err(StoreError::Corrupt(msg)) => {
            assert!(msg.contains(&id.to_hex()), "{what}: names the object");
        }
        other => panic!("{what}: expected Corrupt, got {other:?}"),
    }
}

#[test]
fn frames_land_in_the_sharded_layout_and_read_back_verified() {
    let dir = TempDir::new("fs-layout");
    let store = open(dir.path(), FsMode::Authoritative);
    let (id, frame) = blob(1, 4096);
    block_on(store.put(REPO, id, frame.clone())).expect("put");

    let hex = id.to_hex();
    let path = dir.path().join("1/objects").join(&hex[..2]).join(&hex[2..]);
    assert_eq!(store.object_path(REPO, id), path);
    assert_eq!(
        fs::read(&path).expect("file"),
        frame,
        "stored as the wire frame"
    );
    assert_eq!(block_on(store.get(REPO, id)).expect("get"), Some(frame));
    assert_eq!(files(dir.path()), vec![path], "no temp file is left behind");
}

#[test]
fn corrupted_files_read_as_corrupt() {
    let dir = TempDir::new("fs-corrupt");
    let store = open(dir.path(), FsMode::Authoritative);
    let (raw_id, raw) = blob(1, 100);
    let (gz_id, gz) = blob(2, 8192);
    assert_eq!(Frame::from_bytes(&raw).unwrap().codec, Codec::Raw);
    assert_eq!(Frame::from_bytes(&gz).unwrap().codec, Codec::Gzip);

    for (id, frame) in [(raw_id, &raw), (gz_id, &gz)] {
        let path = store.object_path(REPO, id);
        let restore = |bytes: &[u8]| fs::write(&path, bytes).expect("rewrite");
        block_on(store.put(REPO, id, frame.clone())).expect("put");

        restore(&frame[..frame.len() - 1]);
        assert_corrupt(&store, id, "truncated body");
        restore(&frame[..HEADER_LEN - 1]);
        assert_corrupt(&store, id, "truncated header");
        restore(&[]);
        assert_corrupt(&store, id, "empty file");

        if id == raw_id {
            // graft-proto's gzip decoder stops at the end of the gzip
            // member, so only a raw body is checked for trailing bytes.
            let mut longer = frame.clone();
            longer.push(0);
            restore(&longer);
            assert_corrupt(&store, id, "trailing byte");
        }

        for at in [0, 4, 5, 6, HEADER_LEN, frame.len() / 2, frame.len() - 1] {
            let mut flipped = frame.clone();
            flipped[at] ^= 0x01;
            restore(&flipped);
            assert_corrupt(&store, id, &format!("byte {at} flipped"));
        }

        restore(frame);
        assert_eq!(
            block_on(store.get(REPO, id)).expect("get"),
            Some(frame.clone())
        );
    }

    // A whole valid frame stored under another object's id.
    let (other, _) = blob(3, 100);
    let path = store.object_path(REPO, other);
    fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    fs::write(path, &raw).expect("misplace");
    assert_corrupt(&store, other, "frame under the wrong id");
}

#[test]
fn unverified_reads_return_the_bytes_as_stored() {
    let dir = TempDir::new("fs-unverified");
    let store = open(dir.path(), FsMode::Authoritative).verify_reads(false);
    let (id, frame) = blob(1, 100);
    block_on(store.put(REPO, id, frame.clone())).expect("put");
    fs::write(store.object_path(REPO, id), b"torn").expect("damage");
    assert_eq!(
        block_on(store.get(REPO, id)).expect("get"),
        Some(b"torn".to_vec())
    );
}

#[test]
fn open_removes_stray_temp_files_and_recounts() {
    let dir = TempDir::new("fs-reopen");
    let mut ids = Vec::new();
    {
        let store = open(dir.path(), FsMode::Authoritative);
        for seed in 0..20 {
            let (id, frame) = blob(seed, 10 + seed as usize * 50);
            block_on(store.put(REPO, id, frame)).expect("put");
            block_on(store.put(RepoId(2), id, blob(seed, 7).1)).expect("put");
            ids.push(id);
        }
    }
    let (first, _) = blob(0, 10);
    let shard = store_dir(dir.path(), REPO, first);
    let hex = first.to_hex();
    for stray in [format!("{}.123.0.tmp", &hex[2..]), "partial.tmp".to_owned()] {
        fs::write(shard.join(stray), b"half a fra").expect("stray");
    }
    let bytes = disk_bytes(dir.path()) - 2 * 10;

    let store = open(dir.path(), FsMode::Authoritative);
    assert!(
        files(dir.path())
            .iter()
            .all(|p| p.extension().is_none_or(|e| e != "tmp")),
        "open removed the temp files"
    );
    let s = stats(&store);
    assert_eq!(
        (s.objects, s.bytes),
        (40, bytes),
        "open recounted the objects"
    );
    assert!(s.free_bytes > 0, "free space is reported");
    for id in ids {
        assert!(block_on(store.get(REPO, id)).expect("get").is_some());
    }
}

fn store_dir(root: &Path, repo: RepoId, id: ObjectId) -> PathBuf {
    root.join(repo.0.to_string())
        .join("objects")
        .join(&id.to_hex()[..2])
}

#[test]
fn a_root_has_one_owner_at_a_time() {
    let dir = TempDir::new("fs-lock");
    let store = open(dir.path(), FsMode::Authoritative);
    assert!(matches!(
        FsObjectStore::open(dir.path(), FsMode::Authoritative),
        Err(StoreError::Backend(_))
    ));
    drop(store);
    open(dir.path(), FsMode::Authoritative);
}

#[test]
fn concurrent_puts_of_one_id_store_it_once() {
    let dir = TempDir::new("fs-race");
    let store = Arc::new(open(dir.path(), FsMode::Authoritative));
    let (id, frame) = blob(9, 5000);
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (store, frame) = (store.clone(), frame.clone());
            std::thread::spawn(move || {
                for _ in 0..50 {
                    block_on(store.put(REPO, id, frame.clone())).expect("put");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("writer");
    }
    let s = stats(&store);
    assert_eq!((s.objects, s.bytes), (1, frame.len() as u64));
    assert_eq!(
        files(dir.path()).len(),
        1,
        "every losing temp file was removed"
    );
    assert_eq!(block_on(store.get(REPO, id)).expect("get"), Some(frame));
}

#[test]
fn lru_never_exceeds_its_bound_by_more_than_one_object() {
    let dir = TempDir::new("fs-lru-bound");
    let max_bytes = 20_000;
    let store = open(dir.path(), FsMode::Lru { max_bytes });
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for seed in 0..400 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let (id, frame) = blob(seed, 1 + (x % 4000) as usize);
        let len = frame.len() as u64;
        block_on(store.put(REPO, id, frame)).expect("put");
        let s = stats(&store);
        assert!(
            s.bytes <= max_bytes + len,
            "{} bytes stored, bound {max_bytes} plus {len}",
            s.bytes
        );
        assert_eq!(s.bytes, disk_bytes(dir.path()), "stats match the disk");
        assert_eq!(s.objects, files(dir.path()).len() as u64);
        assert!(block_on(store.has(REPO, &[id])).expect("has") == vec![id]);
    }
}

#[test]
fn lru_evicts_the_least_recently_used_first() {
    let dir = TempDir::new("fs-lru-order");
    let frames: Vec<_> = (0..4).map(|seed| blob(seed, 200)).collect();
    let size = frames[0].1.len() as u64;
    let store = open(
        dir.path(),
        FsMode::Lru {
            max_bytes: 3 * size,
        },
    );
    for (id, frame) in &frames[..3] {
        block_on(store.put(REPO, *id, frame.clone())).expect("put");
    }
    // Use the oldest; the second becomes least recently used.
    assert!(
        block_on(store.get(REPO, frames[0].0))
            .expect("get")
            .is_some()
    );
    block_on(store.put(REPO, frames[3].0, frames[3].1.clone())).expect("put");
    let ids: Vec<_> = frames.iter().map(|f| f.0).collect();
    assert_eq!(
        block_on(store.has(REPO, &ids)).expect("has"),
        vec![ids[0], ids[2], ids[3]]
    );
    // A repeated put counts as a use too.
    block_on(store.put(REPO, ids[2], frames[2].1.clone())).expect("put");
    block_on(store.put(REPO, ids[1], frames[1].1.clone())).expect("put");
    assert_eq!(
        block_on(store.has(REPO, &ids)).expect("has"),
        vec![ids[1], ids[2], ids[3]]
    );
}

#[test]
fn lru_keeps_a_single_object_larger_than_its_bound() {
    let dir = TempDir::new("fs-lru-big");
    let store = open(dir.path(), FsMode::Lru { max_bytes: 100 });
    let (small, small_frame) = blob(1, 50);
    let (big, big_frame) = blob(2, 5000);
    block_on(store.put(REPO, small, small_frame)).expect("put");
    block_on(store.put(REPO, big, big_frame.clone())).expect("put");
    assert_eq!(
        block_on(store.has(REPO, &[small, big])).expect("has"),
        vec![big]
    );
    assert_eq!(stats(&store).bytes, big_frame.len() as u64);
}

#[test]
fn lru_reopened_with_a_smaller_bound_evicts_at_open() {
    let dir = TempDir::new("fs-lru-reopen");
    {
        let store = open(dir.path(), FsMode::Authoritative);
        for seed in 0..50 {
            let (id, frame) = blob(seed, 1000);
            block_on(store.put(REPO, id, frame)).expect("put");
        }
    }
    let store = open(dir.path(), FsMode::Lru { max_bytes: 10_000 });
    let s = stats(&store);
    assert!(s.bytes <= 10_000, "{} bytes after open", s.bytes);
    assert_eq!(s.bytes, disk_bytes(dir.path()));
}

#[test]
fn delete_uncounts_and_lru_forgets() {
    let dir = TempDir::new("fs-delete");
    let store = open(dir.path(), FsMode::Lru { max_bytes: 1 << 20 });
    let (id, frame) = blob(1, 300);
    block_on(store.put(REPO, id, frame)).expect("put");
    block_on(store.delete(REPO, id)).expect("delete");
    block_on(store.delete(REPO, id)).expect("delete again");
    let s = stats(&store);
    assert_eq!((s.objects, s.bytes), (0, 0));
    assert_eq!(block_on(store.get(REPO, id)).expect("get"), None);
    assert!(files(dir.path()).is_empty());
}
