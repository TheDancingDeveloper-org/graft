//! Shared fixtures for the graft-primary export tests: a deterministic git,
//! the mirror's call context, a fake clock, native lands, and a stand-in for
//! graft's `Land` (G3.1) built from `Rebase` and a line CAS.

#![allow(dead_code)]

use std::cell::Cell;
use std::path::Path;
use std::process::{Command, Stdio};

use graft_mirror::current_ref;
use graft_mirror::land::{Lander, stack_base};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{CreateChangeset, Log, MergeMode, Page, Rebase, RefKind, UpdateRef};
use graft_proto::{ApiError, CallContext, Frame, Principal, PrincipalKind};

pub(crate) const REPO: &str = "r";

pub(crate) fn cx() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-mirror".to_owned(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{REPO}:admin")],
        },
        request_id: "test".to_owned(),
        deadline_ms: None,
    }
}

thread_local! {
    static NOW: Cell<i64> = const { Cell::new(1_700_000_000_000) };
}

/// The test clock: moves only when [`advance`] says so.
pub(crate) fn clock() -> i64 {
    NOW.with(Cell::get)
}

pub(crate) fn advance(ms: i64) {
    NOW.with(|n| n.set(n.get() + ms));
}

pub(crate) fn set_clock(ms: i64) {
    NOW.with(|n| n.set(ms));
}

/// git with a fixed identity, dates and no user configuration.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Out Of Band")
        .env("GIT_AUTHOR_EMAIL", "oob@example.com")
        .env("GIT_COMMITTER_NAME", "Out Of Band")
        .env("GIT_COMMITTER_EMAIL", "oob@example.com")
        .env("GIT_AUTHOR_DATE", "1700000500 +0000")
        .env("GIT_COMMITTER_DATE", "1700000500 +0000")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The commit `branch` names in the bare repository at `dir`, if any.
pub(crate) fn branch(dir: &Path, branch: &str) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("git runs");
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

/// The `Graft-Changeset` trailers along `branch`'s first-parent chain,
/// oldest first; one entry per commit (empty for a commit without one).
pub(crate) fn trailers(dir: &Path, branch: &str) -> Vec<String> {
    let out = git(
        dir,
        &[
            "log",
            "--first-parent",
            "--reverse",
            "--format=%(trailers:key=Graft-Changeset,valueonly,separator=%x2C)",
            &format!("refs/heads/{branch}"),
        ],
    );
    out.lines().map(|l| l.trim().to_owned()).collect()
}

/// A bare "GitHub". With `deny_rewrites`, a non-fast-forward push is refused
/// as GitHub's ruleset refuses it, so a test proves export never needs one.
pub(crate) fn github(dir: &Path, deny_rewrites: bool) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "--bare"]);
    if deny_rewrites {
        git(dir, &["config", "receive.denyNonFastForwards", "true"]);
    }
}

/// One changeset for a native land: `files` put on top of `parent`.
pub(crate) async fn changeset<A: RepoApi>(
    api: &A,
    parent: Option<ChangesetId>,
    files: &[(String, Vec<u8>)],
    message: &str,
    n: u64,
) -> ChangesetId {
    let mut changes = Vec::new();
    let mut inline = Vec::new();
    for (path, body) in files {
        let frame = Frame::encode(ObjectKind::Blob, body);
        changes.push(Change::Put {
            path: RepoPath::new(path).unwrap(),
            kind: EntryKind::File,
            id: graft_model::ObjectId::hash(ObjectKind::Blob, body),
        });
        inline.push(frame);
    }
    api.create_changeset(
        &cx(),
        CreateChangeset {
            repo: REPO.to_owned(),
            parents: parent.into_iter().collect(),
            changes,
            author: Author {
                name: "Tess".to_owned(),
                email: "tess@example.com".to_owned(),
                kind: AuthorKind::Agent,
            },
            authored_at: Timestamp {
                ms: 1_700_000_000_000 + n as i64 * 1000,
                offset_min: 0,
            },
            message: message.to_owned(),
            metadata: Metadata::new(),
            inline_blobs: inline,
            workspace: None,
        },
    )
    .await
    .unwrap()
    .id
}

/// Moves `line` from `expected` to `new` by CAS, standing in for a land.
pub(crate) async fn move_line<A: RepoApi>(
    api: &A,
    line: &str,
    expected: Option<ChangesetId>,
    new: ChangesetId,
) {
    api.update_ref(
        &cx(),
        UpdateRef {
            repo: REPO.to_owned(),
            name: line.to_owned(),
            expected,
            new,
            kind: RefKind::Line,
        },
    )
    .await
    .unwrap();
}

/// Lands a stack of `stacks.len()` changesets on `main` at once.
pub(crate) async fn land<A: RepoApi>(
    api: &A,
    stack: &[Vec<(String, Vec<u8>)>],
    n: u64,
) -> Vec<ChangesetId> {
    let tip = current_ref(api, &cx(), REPO, "main").await.unwrap();
    let mut parent = tip;
    let mut ids = Vec::new();
    for (i, files) in stack.iter().enumerate() {
        let id = changeset(api, parent, files, &format!("land {n}.{i}"), n).await;
        ids.push(id);
        parent = Some(id);
    }
    move_line(api, "main", tip, parent.unwrap()).await;
    ids
}

/// The first-parent chain of `main`, oldest first, through `Log` pages.
pub(crate) async fn line_chain<A: RepoApi>(api: &A) -> Vec<ChangesetId> {
    let Some(tip) = current_ref(api, &cx(), REPO, "main").await.unwrap() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut token = None;
    loop {
        let page = api
            .log(
                &cx(),
                Log {
                    repo: REPO.to_owned(),
                    from: tip,
                    until: None,
                    paths: Vec::new(),
                    page: Page {
                        limit: 256,
                        token: token.clone(),
                    },
                },
            )
            .await
            .unwrap();
        out.extend(page.changesets.iter().map(|c| c.id));
        match page.next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    out.reverse();
    out
}

/// A stand-in for graft's `Land` (G3.1 is not built yet): rebases the stack
/// onto the line tip when it is not already there, then moves the line by
/// CAS.
pub(crate) struct FakeLand<'a, A>(pub(crate) &'a A);

impl<A: RepoApi + ObjectApi> Lander for FakeLand<'_, A> {
    async fn land(
        &self,
        cx: &CallContext,
        repo: &str,
        line: &str,
        head: ChangesetId,
    ) -> Result<ChangesetId, ApiError> {
        let tip = current_ref(self.0, cx, repo, line)
            .await?
            .expect("the line exists");
        let base = stack_base(self.0, cx, repo, head).await?;
        let landed = if base == tip {
            head
        } else {
            self.0
                .rebase(
                    cx,
                    Rebase {
                        repo: repo.to_owned(),
                        changeset: head,
                        onto: tip,
                        merge: MergeMode::File,
                    },
                )
                .await?
        };
        self.0
            .update_ref(
                cx,
                UpdateRef {
                    repo: repo.to_owned(),
                    name: line.to_owned(),
                    expected: Some(tip),
                    new: landed,
                    kind: RefKind::Line,
                },
            )
            .await?;
        Ok(landed)
    }
}

/// A seeded xorshift generator, so a failing run reproduces from its seed.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
