//! Export of graft changesets as git commits: the round trip (export after
//! import is the identity), byte-identical re-exports, native histories
//! checked against graft's snapshots, the fidelity check, refusals of
//! metadata git cannot hold, and resumed exports.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use common::{EMPTY_TREE, Git, TempDir, git_in, ls_tree_in, varied_repo};
use graft_git::export::Committer;
use graft_git::exporter::{ExportReport, Exporter, LandTimes, SESSION_KEY, WORK_ITEM_KEY};
use graft_git::fidelity::check_changeset;
use graft_git::import::{ImportConfig, import};
use graft_git::map::{MemoryMirrorMap, MirrorMap};
use graft_git::{GitError, GitSha};
use graft_model::{
    ApplyOptions, Author, AuthorKind, BlobId, Change, Changeset, ChangesetId, EntryKind, Metadata,
    ObjectKind, ObjectSource, RepoPath, SourceError, Timestamp, Tree, TreeId, apply_changes_with,
};
use graft_proto::Frame;
use graft_proto::api::{AdminApi, ObjectApi};
use graft_proto::types::{ImportChangesets, PutObjects};
use graft_testkit::fake_api::{FakeApi, admin_cx, block_on};
use graft_testkit::synth::{SynthSpec, generate};
use proptest::prelude::*;

/// Each changeset lands an hour after it was authored, at `+0100`.
struct Lands(HashMap<ChangesetId, Timestamp>);

impl Lands {
    fn of(api: &FakeApi) -> Lands {
        Lands(
            api.changesets_in_order()
                .into_iter()
                .map(|c| {
                    let at = Timestamp {
                        ms: c.authored_at.ms + 3_600_000,
                        offset_min: 60,
                    };
                    (c.id(), at)
                })
                .collect(),
        )
    }
}

impl LandTimes for Lands {
    fn landed_at(&self, id: ChangesetId) -> Result<Timestamp, GitError> {
        self.0
            .get(&id)
            .copied()
            .ok_or_else(|| GitError::Map(format!("{id} has no landing time")))
    }
}

fn committer() -> Committer {
    Committer {
        name: "graft".into(),
        email: "graft@example.com".into(),
    }
}

fn export(
    api: &FakeApi,
    map: &MemoryMirrorMap,
    dest: &Path,
    refs: &[(String, ChangesetId)],
) -> Result<ExportReport, GitError> {
    let committer = committer();
    let lands = Lands::of(api);
    let cx = admin_cx();
    let mut exporter = Exporter::new(api, &cx, map, "r", &committer, &lands, dest)?;
    block_on(exporter.export_refs(refs))
}

/// Imports `git`'s branches and tags, and exports them to `dest`.
fn import_and_export(
    git: &Git,
    lines: &[&str],
    dest: &Path,
) -> (FakeApi, MemoryMirrorMap, ExportReport) {
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let mut config = ImportConfig::new(git.git_dir(), "r");
    config.lines = lines
        .iter()
        .map(|l| (format!("refs/heads/{l}"), (*l).to_owned()))
        .collect();
    block_on(import(&api, &admin_cx(), &map, &config)).expect("import succeeds");
    let mut refs = Vec::new();
    for l in lines {
        let sha = GitSha::from_hex(&git.out(&["rev-parse", l])).unwrap();
        refs.push((
            format!("refs/heads/{l}"),
            map.changeset_of(sha).unwrap().unwrap(),
        ));
    }
    for tag in git.out(&["tag", "--list"]).lines() {
        let sha = GitSha::from_hex(&git.out(&["rev-parse", &format!("{tag}^{{commit}}")])).unwrap();
        refs.push((
            format!("refs/tags/{tag}"),
            map.changeset_of(sha).unwrap().unwrap(),
        ));
    }
    let report = export(&api, &map, dest, &refs).expect("export succeeds");
    (api, map, report)
}

/// Every ref under `heads/` and `tags/`, peeled to its commit: name → sha.
fn commits_by_ref(git_dir: &Path) -> BTreeMap<String, String> {
    let out = git_in(
        git_dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(*objectname)",
            "refs/heads/",
            "refs/tags/",
        ],
    );
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let peeled = parts.get(2).unwrap_or(&parts[1]);
            (parts[0].to_owned(), (*peeled).to_owned())
        })
        .collect()
}

fn commit_set(git_dir: &Path) -> Vec<String> {
    let mut shas: Vec<String> = String::from_utf8(git_in(git_dir, &["rev-list", "--all"]))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    shas.sort();
    shas
}

fn fsck(git_dir: &Path) {
    git_in(git_dir, &["fsck", "--strict", "--no-dangling"]);
}

/// Every file under `objects/` and `refs/`, by relative path.
fn stored_bytes(git_dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_owned();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    for part in ["objects", "refs"] {
        walk(git_dir, &git_dir.join(part), &mut out);
    }
    out
}

/// Every commit of `source` exists in `dest` with the same sha, every ref
/// names the same commit, and each changeset passes the fidelity check.
fn assert_round_trip(source: &Path, dest: &Path, api: &FakeApi, map: &MemoryMirrorMap) {
    fsck(dest);
    assert_eq!(commit_set(source), commit_set(dest));
    assert_eq!(commits_by_ref(source), commits_by_ref(dest));
    for sha in commit_set(dest) {
        let id = map
            .changeset_of(GitSha::from_hex(&sha).unwrap())
            .unwrap()
            .expect("every commit is mapped");
        let changeset = api.changeset(id).expect("on the server");
        check_changeset(dest, map, id, &changeset).expect("fidelity holds");
    }
}

fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// Names and modes git will hold that graft must carry exactly: spaces,
/// unicode, leading dashes, executables, empty files, a large binary,
/// dangling symlinks and symlinks to directories, and two commits that differ
/// only in their committer.
fn adversarial_repo() -> Git {
    let mut git = Git::init("adversarial");
    git.write("spaces in name/a b.txt", b"spaces\n");
    git.write("-leading-dash", b"dash\n");
    git.write("ünïcödé/日本語.txt", b"unicode\n");
    git.write("empty.txt", b"");
    git.write("big.bin", &bytes(9, 2 << 20));
    git.write("bin/tool", b"#!/bin/sh\necho tool\n");
    git.chmod_x("bin/tool");
    git.symlink("nowhere/at-all", "dangling");
    git.symlink("ünïcödé", "to-dir");
    git.symlink("-leading-dash", "link-to-dash");
    let base = git.commit("adversarial names, modes and sizes");
    git.run(&["tag", "-a", "annotated", "-m", "an annotated tag"]);
    git.write("twin.txt", b"the change both twins make\n");
    let changed = git.commit("the change both twins make");

    // Same tree, parent and message as `changed`; only the committer differs.
    // Graft has no empty changesets, so both twins make a change.
    let tree = git.out(&["rev-parse", &format!("{changed}^{{tree}}")]);
    let raw = |committer: &str| {
        format!(
            "tree {tree}\nparent {base}\n\
author A U Thor <author@example.com> 1700000600 +0530\n\
committer {committer} 1700000900 +0000\n\nsame change\n"
        )
    };
    let original = git.literal(
        "commit",
        raw("C O Mitter <committer@example.com>").as_bytes(),
    );
    let copy = git.literal("commit", raw("Someone Else <else@example.com>").as_bytes());
    // Both twins sit on `base`, so their change is the same, and it is a
    // real one: `twin.txt` is added.
    let join_tree = git.out(&["rev-parse", &format!("{original}^{{tree}}")]);
    let join = git.commit_tree(&join_tree, &[&original, &copy], "join both");
    git.set_ref("refs/heads/main", &join);
    git
}

/// A linear native history: changeset `i` holds the diff from snapshot
/// `i - 1`, its root derived as the server would. Some changesets carry the
/// session and work item keys, and messages have bodies.
fn native_chain(api: &FakeApi, snapshots: &[BTreeMap<String, Vec<u8>>]) -> Vec<ChangesetId> {
    struct Known<'a>(&'a HashMap<TreeId, Tree>);

    impl ObjectSource for Known<'_> {
        async fn tree(&self, id: TreeId) -> Result<Tree, SourceError> {
            self.0.get(&id).cloned().ok_or(SourceError::Missing(id))
        }
    }

    let empty = Tree::default().id();
    let mut trees: HashMap<TreeId, Tree> = HashMap::new();
    let mut ids = Vec::new();
    let mut prev_snap: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut prev_root: Option<TreeId> = None;
    for (i, snap) in snapshots.iter().enumerate() {
        let mut changes = Vec::new();
        let mut blobs = Vec::new();
        for (path, data) in snap {
            if prev_snap.get(path) != Some(data) {
                changes.push(Change::Put {
                    path: RepoPath::new(path).unwrap(),
                    kind: EntryKind::File,
                    id: BlobId::hash(data).0,
                });
                blobs.push(Frame::encode(ObjectKind::Blob, data));
            }
        }
        for path in prev_snap.keys() {
            if !snap.contains_key(path) {
                changes.push(Change::Delete {
                    path: RepoPath::new(path).unwrap(),
                });
            }
        }
        changes.sort_by(|a, b| a.path().as_str().cmp(b.path().as_str()));
        if !blobs.is_empty() {
            block_on(api.put_objects(
                &admin_cx(),
                PutObjects {
                    repo: "r".into(),
                    blobs,
                },
            ))
            .unwrap();
        }
        let applied = block_on(apply_changes_with(
            &Known(&trees),
            prev_root.filter(|r| *r != empty),
            &changes,
            &ApplyOptions::default(),
        ))
        .unwrap();
        let root = applied.root.unwrap_or(empty);
        trees.extend(applied.new_trees);
        let mut metadata = Metadata::new();
        if i % 3 == 0 {
            metadata.insert(SESSION_KEY.into(), format!("ses_{i:04}"));
            metadata.insert(WORK_ITEM_KEY.into(), format!("WI-{i}"));
        }
        let changeset = Changeset {
            parents: ids.last().copied().into_iter().collect(),
            root,
            changes,
            author: Author {
                name: "Zoë Ångström".into(),
                email: "zoe@example.com".into(),
                kind: AuthorKind::Agent,
            },
            authored_at: Timestamp {
                ms: 1_700_000_000_000 + i as i64 * 60_000,
                offset_min: -330,
            },
            message: format!("step {i}\n\nbody of step {i}\n"),
            metadata,
        };
        block_on(api.import_changesets(
            &admin_cx(),
            ImportChangesets {
                repo: "r".into(),
                changesets: vec![changeset.clone()],
            },
        ))
        .unwrap();
        ids.push(changeset.id());
        prev_snap = snap.clone();
        prev_root = Some(root);
    }
    ids
}

#[test]
fn imported_history_round_trips_ref_for_ref() {
    let git = varied_repo();
    let dest = TempDir::new("varied-dest");
    let (api, map, report) =
        import_and_export(&git, &["main", "other", "side", "discarded"], &dest.0);
    assert_eq!(report.commits, git.rev_list_all().len() as u64);
    assert_round_trip(&git.git_dir(), &dest.0, &api, &map);
}

#[test]
fn adversarial_names_modes_and_twins_round_trip() {
    let git = adversarial_repo();
    let dest = TempDir::new("adversarial-dest");
    let (api, map, report) = import_and_export(&git, &["main"], &dest.0);
    assert_eq!(report.commits, git.rev_list_all().len() as u64);
    assert_round_trip(&git.git_dir(), &dest.0, &api, &map);
    // The base, the two twins and the join: four distinct commits.
    assert_eq!(commit_set(&dest.0).len(), 4);
}

#[test]
fn exporting_twice_writes_the_same_bytes() {
    let git = varied_repo();
    let first = TempDir::new("twice-a");
    let second = TempDir::new("twice-b");
    import_and_export(&git, &["main", "other", "side", "discarded"], &first.0);
    import_and_export(&git, &["main", "other", "side", "discarded"], &second.0);
    assert_eq!(stored_bytes(&first.0), stored_bytes(&second.0));
}

#[test]
fn a_repeat_export_writes_nothing_new() {
    let git = varied_repo();
    let dest = TempDir::new("repeat");
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let mut config = ImportConfig::new(git.git_dir(), "r");
    config.lines = vec![("refs/heads/main".into(), "main".into())];
    let imported = block_on(import(&api, &admin_cx(), &map, &config)).unwrap();
    let refs = vec![("refs/heads/main".to_owned(), imported.lines[0].1)];
    let first = export(&api, &map, &dest.0, &refs).unwrap();
    assert_eq!(first.commits, git.rev_list_all().len() as u64);
    let before = stored_bytes(&dest.0);
    let second = export(&api, &map, &dest.0, &refs).unwrap();
    assert_eq!(second.commits, 0);
    assert_eq!(second.blobs, 0);
    assert_eq!(stored_bytes(&dest.0), before);
}

#[test]
fn native_history_exports_deterministically_and_checks_out() {
    let snapshots = generate(&SynthSpec {
        files: 40,
        depth: 3,
        history: 12,
        file_bytes: 64,
        seed: 7,
    })
    .snapshots;
    let api = FakeApi::new();
    let ids = native_chain(&api, &snapshots);
    let map = MemoryMirrorMap::new();
    let one = TempDir::new("native-a");
    let two = TempDir::new("native-b");
    let refs = vec![("refs/heads/main".to_owned(), *ids.last().unwrap())];
    let report = export(&api, &map, &one.0, &refs).unwrap();
    export(&api, &map, &two.0, &refs).unwrap();
    assert_eq!(report.commits, ids.len() as u64);
    assert_eq!(stored_bytes(&one.0), stored_bytes(&two.0));
    fsck(&one.0);

    // A checkout of every commit equals graft's snapshot, and the changes
    // match git's diffs.
    for id in &ids {
        let changeset = api.changeset(*id).unwrap();
        let sha = map.sha_of(*id).unwrap().expect("exported");
        assert_eq!(
            ls_tree_in(&one.0, &sha.to_hex()),
            api.snapshot(changeset.root),
            "snapshot of {sha} matches git ls-tree -r"
        );
        check_changeset(&one.0, &map, *id, &changeset).expect("fidelity holds");
    }

    // The trailers and the committer line follow the rules.
    let last = map.sha_of(ids[3]).unwrap().unwrap().to_hex();
    let raw = String::from_utf8(git_in(&one.0, &["cat-file", "commit", &last])).unwrap();
    assert!(raw.contains("\ncommitter graft <graft@example.com> "));
    assert!(
        raw.contains(" +0100\n"),
        "the landed offset is written: {raw}"
    );
    let body = String::from_utf8(git_in(&one.0, &["log", "-1", "--format=%B", &last])).unwrap();
    assert!(body.starts_with("step 3\n\nbody of step 3\n\n"), "{body}");
    assert!(body.contains(&format!("Graft-Changeset: {}\n", ids[3])));
    assert!(body.contains("Graft-Session: ses_0003\n"));
    assert!(body.contains("Vogt-Work-Item: WI-3\n"));
    assert!(!body.contains("mirror.git-commit"), "D19: never exported");
}

#[test]
fn native_history_survives_export_then_import() {
    let snapshots = generate(&SynthSpec {
        files: 25,
        depth: 2,
        history: 6,
        file_bytes: 48,
        seed: 11,
    })
    .snapshots;
    let api = FakeApi::new();
    let ids = native_chain(&api, &snapshots);
    let map = MemoryMirrorMap::new();
    let first = TempDir::new("loop-a");
    let refs = vec![("refs/heads/main".to_owned(), *ids.last().unwrap())];
    export(&api, &map, &first.0, &refs).unwrap();

    // Import the exported repository, which verifies every commit, and export
    // the result: the same commits, under the same refs.
    let api2 = FakeApi::new();
    let map2 = MemoryMirrorMap::new();
    let mut config = ImportConfig::new(first.0.clone(), "r");
    config.lines = vec![("refs/heads/main".into(), "main".into())];
    let imported = block_on(import(&api2, &admin_cx(), &map2, &config)).expect("import succeeds");
    let second = TempDir::new("loop-b");
    export(
        &api2,
        &map2,
        &second.0,
        &[("refs/heads/main".to_owned(), imported.lines[0].1)],
    )
    .unwrap();
    assert_eq!(commit_set(&first.0), commit_set(&second.0));
    assert_eq!(commits_by_ref(&first.0), commits_by_ref(&second.0));
}

#[test]
fn the_fidelity_check_rejects_a_changed_change() {
    let snapshots = generate(&SynthSpec {
        files: 12,
        depth: 2,
        history: 4,
        file_bytes: 32,
        seed: 3,
    })
    .snapshots;
    let api = FakeApi::new();
    let ids = native_chain(&api, &snapshots);
    let map = MemoryMirrorMap::new();
    let dest = TempDir::new("fidelity");
    let refs = vec![("refs/heads/main".to_owned(), *ids.last().unwrap())];
    export(&api, &map, &dest.0, &refs).unwrap();
    let genuine = api.changeset(ids[2]).unwrap();
    check_changeset(&dest.0, &map, ids[2], &genuine).expect("the genuine changeset passes");

    let mut wrong_kind = genuine.clone();
    let put = wrong_kind
        .changes
        .iter_mut()
        .find(|c| matches!(c, Change::Put { .. }))
        .unwrap();
    if let Change::Put { kind, .. } = put {
        *kind = EntryKind::Exec;
    }
    assert!(matches!(
        check_changeset(&dest.0, &map, ids[2], &wrong_kind),
        Err(GitError::Fidelity(_))
    ));

    let mut dropped = genuine.clone();
    dropped.changes.pop();
    assert!(matches!(
        check_changeset(&dest.0, &map, ids[2], &dropped),
        Err(GitError::Fidelity(_))
    ));
}

#[test]
fn metadata_git_cannot_hold_is_refused_and_nothing_is_recorded() {
    let snapshots = generate(&SynthSpec {
        files: 6,
        depth: 1,
        history: 1,
        file_bytes: 16,
        seed: 5,
    })
    .snapshots;
    let genuine_api = FakeApi::new();
    let genuine = genuine_api
        .changeset(native_chain(&genuine_api, &snapshots)[0])
        .unwrap();
    type Tamper = fn(&mut Changeset);
    let cases: [(&str, Tamper, &str); 4] = [
        (
            "author-name",
            |c| c.author.name = "Eve\nCommitter".into(),
            "author-name",
        ),
        (
            "author-email",
            |c| c.author.email = "eve<evil>@example.com".into(),
            "author-email",
        ),
        (
            "message",
            |c| c.message = "subject\0hidden".into(),
            "message",
        ),
        (
            "trailer",
            |c| {
                c.metadata
                    .insert(SESSION_KEY.into(), "ses\nForged-Trailer: yes".into());
            },
            "trailer",
        ),
    ];
    for (name, tamper, reason) in cases {
        // The same root and blobs, under a changeset git cannot hold.
        let api = FakeApi::new();
        block_on(
            api.put_objects(
                &admin_cx(),
                PutObjects {
                    repo: "r".into(),
                    blobs: snapshots[0]
                        .values()
                        .map(|d| Frame::encode(ObjectKind::Blob, d))
                        .collect(),
                },
            ),
        )
        .unwrap();
        let mut bad = genuine.clone();
        tamper(&mut bad);
        block_on(api.import_changesets(
            &admin_cx(),
            ImportChangesets {
                repo: "r".into(),
                changesets: vec![bad.clone()],
            },
        ))
        .unwrap_or_else(|e| panic!("{name}: the server takes it: {e:?}"));
        let map = MemoryMirrorMap::new();
        let dest = TempDir::new("refused");
        let refs = vec![("refs/heads/main".to_owned(), bad.id())];
        match export(&api, &map, &dest.0, &refs) {
            Err(GitError::Unexportable {
                changeset,
                reason: got,
            }) => {
                assert_eq!(changeset, bad.id(), "{name}");
                assert_eq!(got, reason, "{name}");
            }
            other => panic!("{name}: expected a refusal, got {other:?}"),
        }
        assert_eq!(
            map.sha_of(bad.id()).unwrap(),
            None,
            "{name}: nothing recorded"
        );
        assert!(
            !dest.0.join("refs/heads/main").exists(),
            "{name}: no ref moved"
        );
    }
}

#[test]
fn messages_with_newlines_round_trip_unchanged() {
    // Newlines are the message's own: they export as written, with no
    // mangling of the body, and the trailer block follows it.
    let snapshots = generate(&SynthSpec {
        files: 4,
        depth: 1,
        history: 1,
        file_bytes: 8,
        seed: 21,
    })
    .snapshots;
    let api = FakeApi::new();
    let ids = native_chain(&api, &snapshots);
    let map = MemoryMirrorMap::new();
    let dest = TempDir::new("newlines");
    export(
        &api,
        &map,
        &dest.0,
        &[("refs/heads/main".to_owned(), ids[0])],
    )
    .unwrap();
    let sha = map.sha_of(ids[0]).unwrap().unwrap().to_hex();
    let body = String::from_utf8(git_in(&dest.0, &["log", "-1", "--format=%B", &sha])).unwrap();
    assert!(
        body.starts_with("step 0\n\nbody of step 0\n\nGraft-Changeset: "),
        "{body}"
    );
}

#[test]
fn the_export_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let dest = TempDir::new("send");
    let committer = committer();
    let lands = Lands(HashMap::new());
    let cx = admin_cx();
    let mut exporter = Exporter::new(&api, &cx, &map, "r", &committer, &lands, &dest.0).unwrap();
    let future = exporter.export_refs(&[]);
    assert_send(&future);
    block_on(future).unwrap();
}

#[test]
fn an_empty_tree_root_has_the_well_known_sha() {
    // A changeset that removes its whole snapshot exports the empty tree.
    let git = {
        let mut git = Git::init("emptied");
        git.write("only.txt", b"x\n");
        git.commit("add");
        git.run(&["rm", "-q", "only.txt"]);
        git.commit("remove everything");
        git
    };
    let dest = TempDir::new("emptied-dest");
    let (api, map, _) = import_and_export(&git, &["main"], &dest.0);
    assert_round_trip(&git.git_dir(), &dest.0, &api, &map);
    let head = git.out(&["rev-parse", "main"]);
    assert_eq!(
        git.out(&["rev-parse", &format!("{head}^{{tree}}")]),
        EMPTY_TREE
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// Export after import is the identity on synthetic histories, however
    /// the files churn.
    #[test]
    fn export_after_import_is_the_identity(seed in any::<u64>(), files in 1usize..24, history in 1usize..6) {
        let snapshots = generate(&SynthSpec { files, depth: 3, history, file_bytes: 40, seed }).snapshots;
        let mut git = Git::init("prop");
        let mut prev: Option<&BTreeMap<String, Vec<u8>>> = None;
        for snap in &snapshots {
            // Graft has no empty changesets, so a step that changes nothing is
            // no commit here.
            if snap.is_empty() || prev == Some(snap) {
                continue;
            }
            prev = Some(snap);
            for (path, data) in snap {
                git.write(path, data);
            }
            // Remove what the snapshot no longer holds, then commit it all.
            for path in git.out(&["ls-files"]).lines().map(str::to_owned).collect::<Vec<_>>() {
                if !snap.contains_key(&path) {
                    let _ = std::fs::remove_file(git.path().join(&path));
                }
            }
            git.commit("step");
        }
        let dest = TempDir::new("prop-dest");
        let (api, map, report) = import_and_export(&git, &["main"], &dest.0);
        prop_assert_eq!(report.commits, git.rev_list_all().len() as u64);
        assert_round_trip(&git.git_dir(), &dest.0, &api, &map);
    }
}

#[test]
#[ignore = "clones rustTorrent from GitHub, or reads GRAFT_RUSTTORRENT"]
fn rust_torrent_round_trips_ref_for_ref() {
    // The G2.2 acceptance check on the real repository. Annotated tags come
    // back as lightweight refs to the same commits: their tag objects are not
    // kept, so the comparison peels every ref to its commit.
    let dir = TempDir::new("rusttorrent-export");
    let source = match std::env::var("GRAFT_RUSTTORRENT") {
        Ok(local) => PathBuf::from(local),
        Err(_) => {
            let path = dir.0.join("rustTorrent.git");
            let status = std::process::Command::new("git")
                .args(["clone", "-q", "--bare"])
                .arg("https://github.com/TheDancingDeveloper-org/rustTorrent.git")
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success());
            path
        }
    };
    let api = FakeApi::new();
    let map = MemoryMirrorMap::new();
    let mut config = ImportConfig::new(&source, "rustTorrent");
    config.lines = String::from_utf8(git_in(
        &source,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    ))
    .unwrap()
    .lines()
    .map(|r| (r.to_owned(), r.trim_start_matches("refs/heads/").to_owned()))
    .collect();
    let imported = block_on(import(&api, &admin_cx(), &map, &config)).expect("rustTorrent imports");
    let mut refs: Vec<(String, ChangesetId)> = imported
        .lines
        .iter()
        .map(|(l, id)| (format!("refs/heads/{l}"), *id))
        .collect();
    refs.extend(
        imported
            .tags
            .iter()
            .map(|(t, id)| (format!("refs/tags/{t}"), *id)),
    );
    let out = dir.0.join("export.git");
    let report = export(&api, &map, &out, &refs).expect("export succeeds");
    eprintln!(
        "rustTorrent exported: {} commits, {} blobs, {} refs",
        report.commits,
        report.blobs,
        report.refs.len()
    );
    assert_round_trip(&source, &out, &api, &map);
}
