//! Part 4 of the adversarial harness (WI-2348,
//! docs/notes/adversarial-harness.md §4): hostile API clients.
//!
//! A client attacks the engine's API and graft-auth's token check to
//! break AGENTS.md rules 7, 8 and 9 and the auth design (§12). The attacks
//! are:
//! - using scopes it doesn't hold;
//! - using expired, revoked, replayed and tampered tokens;
//! - sending oversized and malformed requests;
//! - asking for unbounded answers.
//!
//! Each attack must end one of two ways:
//! - a typed [`ApiError`] of the kind the attack names, with no partial
//!   effect: refs, workspaces, the journal and the planted objects are the
//!   same before and after;
//! - for an unbounded ask, an answer within the configured bound.
//!
//! A canary token and canary file contents are planted. Every error's
//! `Display` and `Debug`, and the `Debug` of the token and key types, are
//! scanned for them.
//!
//! The run is a function of [`SEED`], which picks the signing key and the
//! tampered bytes. `GRAFT_HOSTILE_SEED=<n>` replays another seed.
//!
//! This file covers the engine and graft-auth, in process. The transports
//! (gRPC, the JSON facade, `/mcp`, the node daemon), log lines, metric
//! labels and process arguments need the `graft-server` binary. They go in
//! `graft-server`'s integration tests (note §5), as a follow-up.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Debug;
use std::sync::Arc;

use graft_auth::{AuthError, Claims, MAX_TOKEN_LEN, NoDenylist, SigningKey, Token, mint, verify};
use graft_engine::{Engine, EngineConfig};
use graft_harness::rng::Rng;
use graft_model::{
    Author, AuthorKind, BlobId, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind,
    RepoPath, Timestamp,
};
use graft_proto::types::*;
use graft_proto::{
    AdminApi, ApiError, CallContext, Frame, ObjectApi, Principal, PrincipalKind, RepoApi,
    WorkspaceApi,
};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use graft_testkit::fake_api::block_on;

type Api = Engine<MemoryStore, MemoryStore>;

const REPO: &str = "r";
const LINE: &str = "trunk";
const NOW_MS: i64 = 1_700_000_000_000;

/// The fixed CI seed.
const SEED: u64 = 0x2348;

/// File contents no error may carry.
const CANARY_CONTENT: &[u8] = b"CANARY-CONTENT-7f3a9c1e-do-not-echo\n";

/// The small limits the suite runs under, so "over the limit" is cheap.
const MAX_PAGE: u32 = 4;
const MAX_CHANGES: u32 = 8;
const MAX_LEASE_MS: u64 = 60 * 60 * 1000;
const MAX_PATCH_FILE: u64 = 256;
const MAX_IDS: u32 = 8;

fn seed() -> u64 {
    std::env::var("GRAFT_HOSTILE_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SEED)
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("test path")
}

fn cx(subject: &str, scopes: &[&str]) -> CallContext {
    CallContext {
        principal: Principal {
            subject: subject.to_owned(),
            kind: PrincipalKind::Agent,
            session: None,
            work_item: None,
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        },
        request_id: format!("{subject}-req"),
        deadline_ms: None,
    }
}

fn admin() -> CallContext {
    cx("owner", &["repo:r:admin"])
}

fn commit_req(
    parents: Vec<ChangesetId>,
    changes: Vec<Change>,
    workspace: Option<&str>,
) -> CreateChangeset {
    CreateChangeset {
        repo: REPO.to_owned(),
        parents,
        changes,
        author: Author {
            name: "Hostile".to_owned(),
            email: "hostile@example.com".to_owned(),
            kind: AuthorKind::Agent,
        },
        authored_at: Timestamp {
            ms: NOW_MS,
            offset_min: 0,
        },
        message: "hostile".to_owned(),
        metadata: Metadata::new(),
        inline_blobs: Vec::new(),
        workspace: workspace.map(str::to_owned),
    }
}

fn put(p: &str, data: &[u8]) -> Change {
    Change::Put {
        path: path(p),
        kind: EntryKind::File,
        id: ObjectId::hash(ObjectKind::Blob, data),
    }
}

fn page(limit: u32, token: Option<&str>) -> Page {
    Page {
        limit,
        token: token.map(str::to_owned),
    }
}

fn wref(name: &str) -> WorkspaceRef {
    WorkspaceRef {
        repo: REPO.to_owned(),
        name: name.to_owned(),
    }
}

fn land_req(head: ChangesetId, line: &str) -> Land {
    Land {
        repo: REPO.to_owned(),
        head,
        line: line.to_owned(),
        squash: false,
        merge: MergeMode::Line,
        deadline_ms: None,
        merged_from: None,
    }
}

/// The planted repository.
struct Site {
    api: Api,
    /// The line's tip: a file of canary content plus a chain of edits.
    tip: ChangesetId,
    /// The canary blob.
    canary: BlobId,
    /// A draft of the owner's, in the owner's workspace.
    draft: ChangesetId,
    /// A draft of the attacker's (it holds `repo:r:write`), in its own
    /// workspace; the land attacks try to land it.
    attacker_draft: ChangesetId,
    /// A blob the attacker uploaded for its draft.
    big: Vec<u8>,
}

/// What an attack must not change.
#[derive(Debug, PartialEq, Eq)]
struct State {
    refs: Vec<(String, ChangesetId, u64)>,
    workspaces: BTreeMap<String, (Option<ChangesetId>, ChangesetId, i64)>,
    /// Journal entries and the last lsn.
    journal: (usize, u64),
}

impl Site {
    fn new() -> Site {
        let store = Arc::new(MemoryStore::new());
        let config = EngineConfig {
            max_page: MAX_PAGE,
            max_changes: MAX_CHANGES,
            max_lease_ms: MAX_LEASE_MS,
            max_patch_file_bytes: MAX_PATCH_FILE,
            max_ids: MAX_IDS,
            ..EngineConfig::default()
        };
        let api = Engine::new(
            store.clone(),
            store,
            Arc::new(FakeClock::at(NOW_MS)),
            config,
        );
        let a = admin();
        let big: Vec<u8> = (0..4096u32)
            .map(|i| format!("line {i} of a file\n"))
            .collect::<String>()
            .into_bytes();
        let (tip, draft, attacker_draft) = block_on(async {
            api.create_repo(&a, REPO.to_owned()).await.expect("repo");
            api.put_objects(
                &a,
                PutObjects {
                    repo: REPO.to_owned(),
                    blobs: vec![
                        Frame::encode(ObjectKind::Blob, CANARY_CONTENT),
                        Frame::encode(ObjectKind::Blob, &big),
                    ],
                },
            )
            .await
            .expect("upload");
            let mut tip = api
                .create_changeset(
                    &a,
                    commit_req(
                        vec![],
                        vec![
                            put("secret/canary.txt", CANARY_CONTENT),
                            put("src/big.txt", &big),
                        ],
                        None,
                    ),
                )
                .await
                .expect("root")
                .id;
            api.update_ref(
                &a,
                UpdateRef {
                    repo: REPO.to_owned(),
                    name: LINE.to_owned(),
                    expected: None,
                    new: tip,
                    kind: RefKind::Line,
                },
            )
            .await
            .expect("line");
            // A history longer than a page, and more lines than a page.
            for i in 0..(2 * MAX_PAGE) {
                let data = format!("edit {i}\n").into_bytes();
                api.put_objects(
                    &a,
                    PutObjects {
                        repo: REPO.to_owned(),
                        blobs: vec![Frame::encode(ObjectKind::Blob, &data)],
                    },
                )
                .await
                .expect("upload");
                let next = api
                    .create_changeset(
                        &a,
                        commit_req(vec![tip], vec![put("src/n.txt", &data)], None),
                    )
                    .await
                    .expect("edit")
                    .id;
                api.land(&a, land_req(next, LINE)).await.expect("land");
                tip = next;
                api.create_line(
                    &a,
                    CreateLine {
                        repo: REPO.to_owned(),
                        name: format!("side-{i}"),
                        from: tip,
                    },
                )
                .await
                .expect("side line");
            }
            let edit = b"owner's draft\n".to_vec();
            let attacker_edit = b"attacker's draft\n".to_vec();
            api.put_objects(
                &a,
                PutObjects {
                    repo: REPO.to_owned(),
                    blobs: vec![
                        Frame::encode(ObjectKind::Blob, &edit),
                        Frame::encode(ObjectKind::Blob, &attacker_edit),
                    ],
                },
            )
            .await
            .expect("upload");
            for (who, ws) in [
                (&a, "owner-ws"),
                (&cx("mallory", &["repo:r:write"]), "mal-ws"),
            ] {
                api.open_workspace(
                    who,
                    OpenWorkspace {
                        repo: REPO.to_owned(),
                        name: Some(ws.to_owned()),
                        line: LINE.to_owned(),
                        at: None,
                        lease_ms: 10 * 60 * 1000,
                    },
                )
                .await
                .expect("workspace");
            }
            let draft = api
                .create_changeset(
                    &a,
                    commit_req(vec![tip], vec![put("src/n.txt", &edit)], Some("owner-ws")),
                )
                .await
                .expect("owner draft")
                .id;
            let attacker_draft = api
                .create_changeset(
                    &cx("mallory", &["repo:r:write"]),
                    commit_req(
                        vec![tip],
                        vec![put("src/n.txt", &attacker_edit)],
                        Some("mal-ws"),
                    ),
                )
                .await
                .expect("attacker draft")
                .id;
            (tip, draft, attacker_draft)
        });
        Site {
            api,
            tip,
            canary: BlobId::hash(CANARY_CONTENT),
            draft,
            attacker_draft,
            big,
        }
    }

    fn state(&self) -> State {
        let a = admin();
        block_on(async {
            let mut refs = Vec::new();
            let mut token = None;
            loop {
                let got = self
                    .api
                    .list_refs(
                        &a,
                        ListRefs {
                            repo: REPO.to_owned(),
                            prefix: None,
                            page: Page {
                                limit: MAX_PAGE,
                                token: token.clone(),
                            },
                        },
                    )
                    .await
                    .expect("refs");
                let done = got.len() < MAX_PAGE as usize;
                token = got.last().map(|r| r.name.clone());
                refs.extend(got.into_iter().map(|r| (r.name, r.target, r.version)));
                if done {
                    break;
                }
            }
            let mut workspaces = BTreeMap::new();
            for name in ["owner-ws", "mal-ws"] {
                let w = self.api.get_workspace(&a, wref(name)).await.expect("ws");
                workspaces.insert(w.name, (w.head, w.base, w.lease_until_ms));
            }
            // Page the whole journal from its start: it is small here, and
            // each page is bounded by MAX_PAGE.
            let (mut from, mut entries, mut last) = (1, 0usize, 0u64);
            loop {
                let got = self
                    .api
                    .watch(
                        &a,
                        Watch {
                            repo: REPO.to_owned(),
                            from_lsn: from,
                            kinds: vec![],
                            limit: MAX_PAGE,
                            wait_ms: 0,
                        },
                    )
                    .await
                    .expect("journal");
                if got.entries.is_empty() {
                    break;
                }
                entries += got.entries.len();
                last = got.entries.last().map_or(last, |e| e.lsn);
                from = got.next_lsn;
            }
            State {
                refs,
                workspaces,
                journal: (entries, last),
            }
        })
    }
}

/// Fails if `text` carries a canary.
fn scan(what: &str, text: &str, canaries: &[&str], failures: &mut Vec<String>) {
    for c in canaries {
        if !c.is_empty() && text.contains(c) {
            failures.push(format!("{what}: carries a canary: {text:.200}"));
        }
    }
}

fn scan_debug<T: Debug>(what: &str, v: &T, canaries: &[&str], failures: &mut Vec<String>) {
    scan(what, &format!("{v:?}"), canaries, failures);
}

/// The canaries, as text: the content, and the token planted in the run.
fn canaries(token: &str) -> Vec<String> {
    vec![
        String::from_utf8_lossy(CANARY_CONTENT).trim().to_owned(),
        token.to_owned(),
    ]
}

/// An attack's request.
type Request<'a> = Box<dyn Fn(&Site) -> Result<(), ApiError> + 'a>;

/// One attack: a name, what it must fail with, and the request.
struct Attack<'a> {
    name: &'static str,
    expect: fn(&ApiError) -> bool,
    run: Request<'a>,
}

fn denied(e: &ApiError) -> bool {
    matches!(e, ApiError::PermissionDenied { .. })
}

fn denied_or_missing(e: &ApiError) -> bool {
    matches!(
        e,
        ApiError::PermissionDenied { .. } | ApiError::NotFound { .. }
    )
}

fn limit(e: &ApiError) -> bool {
    matches!(e, ApiError::LimitExceeded { .. })
}

fn invalid(e: &ApiError) -> bool {
    matches!(
        e,
        ApiError::InvalidPath { .. }
            | ApiError::InvalidChange { .. }
            | ApiError::LimitExceeded { .. }
            | ApiError::NotFound { .. }
    )
}

/// Runs each attack, checks its error, its effect and its text.
fn attack_all(site: &Site, attacks: Vec<Attack<'_>>, canaries: &[&str]) -> Vec<String> {
    let mut failures = Vec::new();
    // The setup journalled its writes, so the journal comparison can fail.
    assert!(site.state().journal.0 > 0, "the journal is empty");
    for a in attacks {
        let before = site.state();
        let got = (a.run)(site);
        let after = site.state();
        match &got {
            Ok(()) => failures.push(format!("{}: succeeded", a.name)),
            Err(e) if !(a.expect)(e) => {
                failures.push(format!("{}: wrong error {e:?}", a.name));
            }
            Err(_) => {}
        }
        if let Err(e) = &got {
            scan(a.name, &e.to_string(), canaries, &mut failures);
            scan_debug(a.name, e, canaries, &mut failures);
        }
        if before != after {
            failures.push(format!(
                "{}: partial effect\n  before {before:?}\n  after  {after:?}",
                a.name
            ));
        }
    }
    failures
}

fn report(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} failure(s), seed {} (replay: GRAFT_HOSTILE_SEED={} cargo test -p graft-harness --test hostile_clients):\n{}",
        failures.len(),
        seed(),
        seed(),
        failures.join("\n")
    );
}

/// Scope escalation: every attempt is refused, typed, with no effect.
#[test]
fn a_client_cannot_use_a_scope_it_does_not_hold() {
    let site = Site::new();
    let writer = &cx("mallory", &["repo:r:write"]);
    let wrong_line = &cx("mallory", &["repo:r:write", "repo:r:land:side-0"]);
    let src_only = &cx("mallory", &["repo:r:write:src"]);
    let other_repo = &cx("mallory", &["repo:other:admin"]);
    let objects_only = &cx("mallory", &["objects:read"]);
    let reader = &cx("mallory", &["repo:r:read"]);
    // Scope strings that look close to a grant and must grant nothing.
    let near: Vec<String> = [
        "repo:r",
        "repo:r:",
        "repo:r:land",
        "repo:r:land:",
        "repo:r:drafts:x",
        "repo:r:Admin",
        "repo:r :admin",
        "repo:*:admin",
        "repo::admin",
        "admin",
        "repo:r:land:trunk ",
        "repo:r:land:trunk/x",
        "repo:r:write:../",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let near_cx = &CallContext {
        principal: Principal {
            scopes: near.clone(),
            ..writer.principal.clone()
        },
        ..writer.clone()
    };
    let tip = site.tip;
    let draft = site.draft;
    let mine = site.attacker_draft;
    let attacks = vec![
        Attack {
            name: "land without land:trunk",
            expect: denied,
            run: Box::new(|s| block_on(s.api.land(writer, land_req(mine, LINE))).map(drop)),
        },
        Attack {
            name: "land holding land on another line",
            expect: denied,
            run: Box::new(|s| block_on(s.api.land(wrong_line, land_req(mine, LINE))).map(drop)),
        },
        Attack {
            name: "land with near-miss scopes",
            expect: denied,
            run: Box::new(|s| block_on(s.api.land(near_cx, land_req(mine, LINE))).map(drop)),
        },
        Attack {
            name: "write outside the path scope",
            expect: denied,
            run: Box::new(move |s| {
                block_on(s.api.create_changeset(
                    src_only,
                    commit_req(vec![tip], vec![put("secret/canary.txt", b"x")], None),
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "path scope escaped with a sibling prefix",
            expect: denied,
            run: Box::new(|s| {
                let c = cx("mallory", &["repo:r:write:src"]);
                block_on(s.api.create_changeset(
                    &c,
                    commit_req(vec![tip], vec![put("srcx/a.txt", CANARY_CONTENT)], None),
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "read drafts without repo:r:drafts (D24)",
            expect: denied,
            run: Box::new(|s| {
                block_on(s.api.overlapping_drafts(
                    writer,
                    OverlappingDrafts {
                        repo: REPO.to_owned(),
                        line: LINE.to_owned(),
                        paths: vec![path("src")],
                        include_own: false,
                        page: page(0, None),
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "move another owner's workspace head",
            expect: denied_or_missing,
            run: Box::new(move |s| {
                block_on(s.api.set_head(
                    writer,
                    SetHead {
                        workspace: wref("owner-ws"),
                        expected: Some(draft),
                        new: Some(tip),
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "sync another owner's workspace",
            expect: denied_or_missing,
            run: Box::new(|s| block_on(s.api.sync(writer, wref("owner-ws"), None)).map(drop)),
        },
        Attack {
            name: "renew another owner's lease",
            expect: denied_or_missing,
            run: Box::new(|s| {
                block_on(s.api.renew_lease(writer, wref("owner-ws"), MAX_LEASE_MS)).map(drop)
            }),
        },
        Attack {
            name: "close another owner's workspace",
            expect: denied_or_missing,
            run: Box::new(|s| {
                block_on(s.api.close_workspace(writer, wref("owner-ws"), false)).map(drop)
            }),
        },
        Attack {
            name: "commit into another owner's workspace",
            expect: denied_or_missing,
            run: Box::new(move |s| {
                block_on(s.api.create_changeset(
                    writer,
                    commit_req(vec![draft], vec![put("src/n.txt", b"x")], Some("owner-ws")),
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "read file of another repository's grant",
            expect: denied,
            run: Box::new(move |s| {
                block_on(s.api.read_file(
                    other_repo,
                    PathAt {
                        repo: REPO.to_owned(),
                        at: tip,
                        path: path("secret/canary.txt"),
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "read file with objects:read only",
            expect: denied,
            run: Box::new(move |s| {
                block_on(s.api.read_file(
                    objects_only,
                    PathAt {
                        repo: REPO.to_owned(),
                        at: tip,
                        path: path("secret/canary.txt"),
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "move a line with read only",
            expect: denied,
            run: Box::new(move |s| {
                block_on(s.api.update_ref(
                    reader,
                    UpdateRef {
                        repo: REPO.to_owned(),
                        name: LINE.to_owned(),
                        expected: None,
                        new: draft,
                        kind: RefKind::Tool,
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "create a line with write only",
            expect: denied,
            run: Box::new(move |s| {
                block_on(s.api.create_line(
                    writer,
                    CreateLine {
                        repo: REPO.to_owned(),
                        name: "stolen".to_owned(),
                        from: tip,
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "upload with read only",
            expect: denied,
            run: Box::new(|s| {
                block_on(s.api.put_objects(
                    reader,
                    PutObjects {
                        repo: REPO.to_owned(),
                        blobs: vec![Frame::encode(ObjectKind::Blob, b"reader's upload")],
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "create a repository without admin",
            expect: denied,
            run: Box::new(|s| block_on(s.api.create_repo(writer, "mine".to_owned()))),
        },
    ];
    let c = canaries("");
    let c: Vec<&str> = c.iter().map(String::as_str).collect();
    let mut failures = attack_all(&site, attacks, &c);
    // The read-only upload must not have stored its blob.
    let stored = block_on(site.api.has_objects(
        &admin(),
        HasObjects {
            repo: REPO.to_owned(),
            ids: vec![ObjectId::hash(ObjectKind::Blob, b"reader's upload")],
        },
    ))
    .expect("has_objects");
    // HasObjects answers with the ids not stored.
    if stored.is_empty() {
        failures.push("upload with read only: the blob was stored".to_owned());
    }
    // Asked as an admin of "mine", so only absence can refuse it.
    if block_on(site.api.resolve_ref(
        &cx("owner", &["repo:mine:admin"]),
        ResolveRef {
            repo: "mine".to_owned(),
            name: LINE.to_owned(),
        },
    ))
    .is_ok()
    {
        failures.push("create a repository without admin: it exists".to_owned());
    }
    report(&failures);
}

/// Oversized and malformed requests: typed errors, nothing written.
#[test]
fn oversized_and_malformed_requests_are_refused_whole() {
    let site = Site::new();
    let a = &admin();
    let tip = site.tip;
    let made_up = ChangesetId::hash(b"no such changeset");
    let too_many: Vec<Change> = (0..=MAX_CHANGES)
        .map(|i| put(&format!("src/f{i:03}.txt"), CANARY_CONTENT))
        .collect();
    let deep = vec!["d"; 3000].join("/");
    let long = "x".repeat(5000);
    let attacks = vec![
        Attack {
            name: "changeset over max_changes",
            expect: limit,
            run: Box::new(move |s| {
                block_on(
                    s.api
                        .create_changeset(a, commit_req(vec![tip], too_many.clone(), None)),
                )
                .map(drop)
            }),
        },
        Attack {
            name: "unknown parent",
            expect: |e| {
                matches!(
                    e,
                    ApiError::UnknownParent { .. } | ApiError::NotFound { .. }
                )
            },
            run: Box::new(move |s| {
                block_on(s.api.create_changeset(
                    &admin(),
                    commit_req(vec![made_up], vec![put("src/a.txt", CANARY_CONTENT)], None),
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "change naming a blob never uploaded",
            expect: |e| matches!(e, ApiError::MissingObjects { .. }),
            run: Box::new(move |s| {
                block_on(s.api.create_changeset(
                    &admin(),
                    commit_req(vec![tip], vec![put("src/a.txt", b"never uploaded")], None),
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "inline blob hashing to another id",
            expect: |e| {
                matches!(
                    e,
                    ApiError::CorruptObject { .. }
                        | ApiError::MissingObjects { .. }
                        | ApiError::InvalidChange { .. }
                )
            },
            run: Box::new(move |s| {
                let mut req = commit_req(vec![tip], vec![put("src/a.txt", b"claimed")], None);
                // A frame whose bytes are canary content, claimed for another id.
                req.inline_blobs = vec![Frame::encode(ObjectKind::Blob, CANARY_CONTENT)];
                block_on(s.api.create_changeset(&admin(), req)).map(drop)
            }),
        },
        Attack {
            name: "too many ids in one GetChangesets",
            expect: limit,
            run: Box::new(|s| {
                block_on(
                    s.api.get_changesets(
                        &admin(),
                        GetChangesets {
                            repo: REPO.to_owned(),
                            ids: (0..=MAX_IDS)
                                .map(|i| ChangesetId::hash(&i.to_le_bytes()))
                                .collect(),
                        },
                    ),
                )
                .map(drop)
            }),
        },
        Attack {
            name: "ref name over the length limit",
            expect: invalid,
            run: Box::new(move |s| {
                block_on(s.api.create_line(
                    &admin(),
                    CreateLine {
                        repo: REPO.to_owned(),
                        name: long.clone(),
                        from: tip,
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "line from an unknown changeset",
            expect: invalid,
            run: Box::new(move |s| {
                block_on(s.api.create_line(
                    &admin(),
                    CreateLine {
                        repo: REPO.to_owned(),
                        name: "ghost".to_owned(),
                        from: made_up,
                    },
                ))
                .map(drop)
            }),
        },
        Attack {
            name: "land of an unknown head",
            expect: invalid,
            run: Box::new(move |s| {
                block_on(s.api.land(&admin(), land_req(made_up, LINE))).map(drop)
            }),
        },
        Attack {
            name: "unknown repository",
            expect: |e| {
                matches!(
                    e,
                    ApiError::NotFound { .. } | ApiError::PermissionDenied { .. }
                )
            },
            run: Box::new(move |s| {
                block_on(s.api.resolve_ref(
                    &admin(),
                    ResolveRef {
                        repo: "nope".to_owned(),
                        name: LINE.to_owned(),
                    },
                ))
                .map(drop)
            }),
        },
    ];
    let c = canaries("");
    let c: Vec<&str> = c.iter().map(String::as_str).collect();
    let mut failures = attack_all(&site, attacks, &c);
    // Hostile path text never becomes a RepoPath, so it never reaches the
    // engine (rule 9: validate at the boundary).
    // A path 3000 levels deep is one of them.
    for bad in [
        &deep,
        "",
        "/abs",
        "a//b",
        "a/../b",
        "./a",
        "a/",
        "a\0b",
        &"y".repeat(5000),
    ] {
        if RepoPath::new(bad).is_ok() {
            failures.push(format!("RepoPath accepted {bad:.40?}"));
        }
    }
    // Hostile id text never parses.
    for bad in [
        "",
        "c",
        "c00",
        "zzzz",
        &format!("c{}", "0".repeat(63)),
        &format!("b{}", "0".repeat(64)),
    ] {
        if ChangesetId::parse(bad).is_ok() {
            failures.push(format!("ChangesetId accepted {bad:?}"));
        }
    }
    report(&failures);
}

/// Requests for unbounded answers get bounded ones; bad cursors are typed
/// errors that don't echo the cursor.
#[test]
fn every_answer_is_bounded_and_cursors_are_not_echoed() {
    let site = Site::new();
    let a = admin();
    let mut failures = Vec::new();
    // The planted canary token stands in for a pasted secret in a cursor.
    let key = SigningKey::from_seed(&[7; 32]);
    let token = mint(&key, &claims("cursor", NOW_MS / 1000 + 600, "jti-cursor"))
        .expect("mint")
        .expose()
        .to_owned();
    let c = canaries(&token);
    let c: Vec<&str> = c.iter().map(String::as_str).collect();
    let bounded = |what: &str, n: usize, failures: &mut Vec<String>| {
        if n > MAX_PAGE as usize {
            failures.push(format!("{what}: {n} items over a page of {MAX_PAGE}"));
        }
    };
    for limit in [0, MAX_PAGE + 1, u32::MAX] {
        let refs = block_on(site.api.list_refs(
            &a,
            ListRefs {
                repo: REPO.to_owned(),
                prefix: None,
                page: page(limit, None),
            },
        ))
        .expect("refs");
        bounded("list_refs", refs.len(), &mut failures);
        let log = block_on(site.api.log(
            &a,
            Log {
                repo: REPO.to_owned(),
                from: site.tip,
                until: None,
                paths: vec![],
                page: page(limit, None),
            },
        ))
        .expect("log");
        bounded("log", log.changesets.len(), &mut failures);
        let ws = block_on(
            site.api
                .list_workspaces(&a, REPO.to_owned(), page(limit, None)),
        )
        .expect("workspaces");
        bounded("list_workspaces", ws.len(), &mut failures);
        let tree = block_on(site.api.list_tree(
            &a,
            ListTree {
                repo: REPO.to_owned(),
                at: site.tip,
                path: None,
                recursive: true,
                page: page(limit, None),
            },
        ))
        .expect("tree");
        bounded("list_tree", tree.entries.len(), &mut failures);
        let journal = block_on(site.api.watch(
            &a,
            Watch {
                repo: REPO.to_owned(),
                from_lsn: 0,
                kinds: vec![],
                limit,
                wait_ms: 0,
            },
        ))
        .expect("watch");
        bounded("watch", journal.entries.len(), &mut failures);
    }
    // A patch over the per-file cap is cut, not sent whole: a draft that
    // changes every line of the big file.
    let big2: Vec<u8> = String::from_utf8_lossy(&site.big)
        .replace("line", "LINE")
        .into_bytes();
    let rewrite = block_on(async {
        site.api
            .put_objects(
                &a,
                PutObjects {
                    repo: REPO.to_owned(),
                    blobs: vec![Frame::encode(ObjectKind::Blob, &big2)],
                },
            )
            .await
            .expect("upload");
        site.api
            .create_changeset(
                &a,
                commit_req(vec![site.tip], vec![put("src/big.txt", &big2)], None),
            )
            .await
            .expect("rewrite")
            .id
    });
    let diff = block_on(site.api.diff(
        &a,
        Diff {
            repo: REPO.to_owned(),
            from: site.tip,
            to: rewrite,
            paths: vec![],
            patch: Some(PatchOptions {
                context: u32::MAX,
                max_bytes_per_file: u64::MAX,
            }),
            page: page(u32::MAX, None),
        },
    ));
    match diff {
        Ok(d) => {
            bounded("diff files", d.files.len(), &mut failures);
            // Each patch is held to the per-file cap, and the big file's is
            // cut (so the check is not vacuous).
            for f in &d.files {
                let n = f.patch.as_ref().map_or(0, String::len) as u64;
                if n > MAX_PATCH_FILE {
                    failures.push(format!("diff: a {n} byte patch for {}", f.change.path()));
                }
            }
            if !d
                .files
                .iter()
                .any(|f| f.change.path().to_string() == "src/big.txt" && f.truncated)
            {
                failures.push("diff: src/big.txt is not in the diff, cut".to_owned());
            }
            if site.big.len() as u64 <= MAX_PATCH_FILE {
                failures.push("diff: the big file is under the cap".to_owned());
            }
        }
        Err(e) if limit(&e) => {}
        Err(e) => failures.push(format!("diff: {e:?}")),
    }
    // Blame over the widest range answers or refuses, typed.
    match block_on(site.api.blame(
        &a,
        Blame {
            repo: REPO.to_owned(),
            at: site.tip,
            path: path("src/big.txt"),
            lines: Some((0, u32::MAX)),
        },
    )) {
        Ok(b) => {
            if b.ranges.len() > site.big.len() {
                failures.push("blame: more ranges than lines".to_owned());
            }
        }
        Err(e) => scan("blame", &format!("{e} {e:?}"), &c, &mut failures),
    }
    // Overlap queries over too many paths are refused.
    let paths: Vec<RepoPath> = (0..=MAX_OVERLAP_PATHS)
        .map(|i| path(&format!("p{i}")))
        .collect();
    match block_on(site.api.overlapping_drafts(
        &a,
        OverlappingDrafts {
            repo: REPO.to_owned(),
            line: LINE.to_owned(),
            paths,
            include_own: true,
            page: page(u32::MAX, None),
        },
    )) {
        Err(e) if limit(&e) => {}
        other => failures.push(format!("overlap over MAX_OVERLAP_PATHS: {other:?}")),
    }
    // Bad cursors on the other lists; `list_tree` and `diff` are
    // `a_bad_cursor_is_never_echoed`.
    failures.extend(hostile_cursors(
        &site,
        &token,
        &["list_refs", "log", "list_workspaces"],
    ));
    // A lease asked for forever is held to the cap.
    let w = block_on(site.api.open_workspace(
        &cx("lessee", &["repo:r:write"]),
        OpenWorkspace {
            repo: REPO.to_owned(),
            name: Some("forever".to_owned()),
            line: LINE.to_owned(),
            at: None,
            lease_ms: u64::MAX,
        },
    ));
    match w {
        Ok(w) if w.lease_until_ms - NOW_MS > MAX_LEASE_MS as i64 => {
            failures.push(format!("lease of {} ms", w.lease_until_ms - NOW_MS));
        }
        Ok(_) => {}
        Err(e) if limit(&e) => {}
        Err(e) => failures.push(format!("open_workspace forever: {e:?}")),
    }
    report(&failures);
}

/// Lists asked to resume from garbage and from a secret: each answers with
/// a typed error or a page, and neither carries the cursor. `only` names
/// the lists to try.
fn hostile_cursors(site: &Site, token: &str, only: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let failures = &mut out;
    let c = canaries(token);
    let c: &[&str] = &c.iter().map(String::as_str).collect::<Vec<_>>();
    // Garbage and secret-bearing cursors: a typed error or an empty or
    // short page, never the cursor echoed back.
    let cursors = [
        token.to_owned(),
        "\u{0}\u{ffff}".to_owned(),
        "~".repeat(10_000),
        "-1".to_owned(),
    ];
    for cur in &cursors {
        let results: Vec<(&str, Result<String, ApiError>)> = vec![
            (
                "list_refs",
                block_on(site.api.list_refs(
                    &admin(),
                    ListRefs {
                        repo: REPO.to_owned(),
                        prefix: None,
                        page: page(2, Some(cur)),
                    },
                ))
                .map(|v| format!("{v:?}")),
            ),
            (
                "log",
                block_on(site.api.log(
                    &admin(),
                    Log {
                        repo: REPO.to_owned(),
                        from: site.tip,
                        until: None,
                        paths: vec![],
                        page: page(2, Some(cur)),
                    },
                ))
                .map(|v| format!("{v:?}")),
            ),
            (
                "list_workspaces",
                block_on(
                    site.api
                        .list_workspaces(&admin(), REPO.to_owned(), page(2, Some(cur))),
                )
                .map(|v| format!("{v:?}")),
            ),
            (
                "list_tree",
                block_on(site.api.list_tree(
                    &admin(),
                    ListTree {
                        repo: REPO.to_owned(),
                        at: site.tip,
                        path: None,
                        recursive: true,
                        page: page(2, Some(cur)),
                    },
                ))
                .map(|v| format!("{v:?}")),
            ),
            (
                "diff",
                block_on(site.api.diff(
                    &admin(),
                    Diff {
                        repo: REPO.to_owned(),
                        from: site.tip,
                        to: site.draft,
                        paths: vec![],
                        patch: None,
                        page: page(2, Some(cur)),
                    },
                ))
                .map(|v| format!("{v:?}")),
            ),
        ];
        for (what, r) in results.into_iter().filter(|(w, _)| only.contains(w)) {
            let text = match r {
                Ok(t) => t,
                Err(e) => format!("{e} {e:?}"),
            };
            scan(&format!("{what} with a hostile cursor"), &text, c, failures);
        }
    }
    out
}

/// WI-2367: `list_tree` and `diff` once quoted a bad cursor in
/// `InvalidPath`, so a secret pasted as a cursor came back in the error
/// (rule 8). Now they refuse it as `InvalidChange { rule: "page-token" }`
/// without quoting it.
#[test]
fn a_bad_cursor_is_never_echoed() {
    let site = Site::new();
    let key = SigningKey::from_seed(&[7; 32]);
    let token = mint(&key, &claims("cursor", NOW_MS / 1000 + 600, "jti-cursor"))
        .expect("mint")
        .expose()
        .to_owned();
    let mut failures = hostile_cursors(&site, &token, &["list_tree", "diff"]);
    let a = admin();
    let tree = block_on(site.api.list_tree(
        &a,
        ListTree {
            repo: REPO.to_owned(),
            at: site.tip,
            path: None,
            recursive: true,
            page: page(2, Some(&token)),
        },
    ))
    .map(drop);
    let diff = block_on(site.api.diff(
        &a,
        Diff {
            repo: REPO.to_owned(),
            from: site.tip,
            to: site.draft,
            paths: vec![],
            patch: None,
            page: page(2, Some(&token)),
        },
    ))
    .map(drop);
    for (what, r) in [("list_tree", tree), ("diff", diff)] {
        match r {
            Err(ApiError::InvalidChange { path, rule })
                if path.is_empty() && rule == "page-token" => {}
            other => failures.push(format!("{what} with a token cursor: {other:?}")),
        }
    }
    report(&failures);
}

fn claims(subject: &str, exp: i64, jti: &str) -> Claims {
    Claims {
        principal: Principal {
            subject: subject.to_owned(),
            kind: PrincipalKind::Agent,
            session: Some("ses_hostile".to_owned()),
            work_item: Some("WI-2348".to_owned()),
            scopes: vec!["repo:r:write".to_owned(), "repo:r:land:trunk".to_owned()],
        },
        exp,
        jti: jti.to_owned(),
    }
}

/// Expired, revoked, replayed, tampered and forged tokens are refused as
/// `Unauthenticated`, and no error or `Debug` output carries the token.
#[test]
fn bad_tokens_are_unauthenticated_and_never_echoed() {
    let mut rng = Rng::new(seed());
    let mut seed_bytes = [0u8; 32];
    for b in &mut seed_bytes {
        *b = rng.below(256) as u8;
    }
    let key = SigningKey::from_seed(&seed_bytes);
    let other = SigningKey::from_seed(&[0x55; 32]);
    let vk = key.verifying_key();
    let now = NOW_MS / 1000;
    let good = mint(&key, &claims("alice", now + 600, "jti-good")).expect("mint");
    let canary = good.expose().to_owned();
    let seed_hex: String = seed_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut c = canaries(&canary);
    c.push(seed_hex);
    // The payload half of the token, which a careless error might quote.
    c.push(canary["v4.public.".len()..][..40].to_owned());
    let c: Vec<&str> = c.iter().map(String::as_str).collect();
    let mut failures = Vec::new();

    // The good token is good: the suite would pass vacuously otherwise.
    if verify(&vk, &good, now, &NoDenylist).is_err() {
        failures.push("the planted token does not verify".to_owned());
    }

    let mut cases: Vec<(String, Token, i64, HashSet<String>)> = Vec::new();
    let none = HashSet::new();
    cases.push((
        "expired at exp".into(),
        good.clone(),
        now + 600,
        none.clone(),
    ));
    cases.push(("long expired".into(), good.clone(), i64::MAX, none.clone()));
    let revoked = HashSet::from(["jti-good".to_owned()]);
    cases.push(("revoked".into(), good.clone(), now, revoked.clone()));
    // A replay of the revoked token after a restart: a fresh denylist read
    // back from storage, the same jti.
    cases.push((
        "revoked, replayed after restart".into(),
        Token::from_encoded(canary.clone()),
        now + 1,
        revoked.into_iter().collect(),
    ));
    cases.push((
        "signed by another key".into(),
        mint(&other, &claims("alice", now + 600, "jti-good")).expect("mint"),
        now,
        none.clone(),
    ));
    cases.push((
        "empty".into(),
        Token::from_encoded(String::new()),
        now,
        none.clone(),
    ));
    cases.push((
        "truncated".into(),
        Token::from_encoded(canary[..canary.len() - 1].to_owned()),
        now,
        none.clone(),
    ));
    cases.push((
        "local header".into(),
        Token::from_encoded(canary.replacen("v4.public.", "v4.local.", 1)),
        now,
        none.clone(),
    ));
    cases.push((
        "with a footer".into(),
        Token::from_encoded(format!("{canary}.e30")),
        now,
        none.clone(),
    ));
    cases.push((
        "over MAX_TOKEN_LEN".into(),
        Token::from_encoded(format!("{canary}{}", "A".repeat(MAX_TOKEN_LEN))),
        now,
        none.clone(),
    ));
    // Seeded tampering: flip one character of the body at a time.
    let body_start = "v4.public.".len();
    let alphabet: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        .chars()
        .collect();
    for _ in 0..64 {
        let mut chars: Vec<char> = canary.chars().collect();
        let at = body_start + rng.below((chars.len() - body_start) as u64) as usize;
        let old = chars[at];
        let mut new = old;
        while new == old {
            new = alphabet[rng.below(alphabet.len() as u64) as usize];
        }
        chars[at] = new;
        let tampered: String = chars.into_iter().collect();
        // The last character may carry only padding bits that base64url
        // decoding ignores; such a flip decodes to the same bytes.
        if tampered == canary {
            continue;
        }
        cases.push((
            format!("tampered at {at}"),
            Token::from_encoded(tampered),
            now,
            none.clone(),
        ));
    }

    for (name, token, at, deny) in &cases {
        match verify(&vk, token, *at, deny) {
            Ok(claims) => {
                // A flip in the final character's unused bits decodes to
                // the same signature: that is the same token, not a forgery.
                if claims.jti != "jti-good" || !name.starts_with("tampered") {
                    failures.push(format!("{name}: verified as {claims:?}"));
                } else if token.expose().len() != canary.len() {
                    failures.push(format!("{name}: a different length verified"));
                }
            }
            Err(e) => {
                let api: ApiError = e.clone().into();
                if api != ApiError::Unauthenticated {
                    failures.push(format!("{name}: {e:?} became {api:?}"));
                }
                if matches!(e, AuthError::Denied { .. }) {
                    failures.push(format!("{name}: refused as a scope, not a token"));
                }
                scan(name, &e.to_string(), &c, &mut failures);
                scan_debug(name, &e, &c, &mut failures);
                scan(name, &api.to_string(), &c, &mut failures);
                scan_debug(name, &api, &c, &mut failures);
            }
        }
        scan_debug(&format!("{name}: Token"), token, &c, &mut failures);
    }
    scan_debug("SigningKey", &key, &c, &mut failures);
    scan_debug("VerifyingKey", &vk, &c, &mut failures);
    report(&failures);
}

/// File contents never ride on an error, even when the attacker can read
/// the file's id, and a read the attacker is allowed returns them.
#[test]
fn errors_about_a_file_never_carry_its_contents() {
    let site = Site::new();
    let mut failures = Vec::new();
    let c = canaries("");
    let c: Vec<&str> = c.iter().map(String::as_str).collect();
    let src_only = &cx("mallory", &["repo:r:write:src"]);
    let a = admin();
    // An allowed read returns the contents: the canary is live.
    let got = block_on(site.api.read_file(
        &a,
        PathAt {
            repo: REPO.to_owned(),
            at: site.tip,
            path: path("secret/canary.txt"),
        },
    ))
    .expect("read");
    if got.data != CANARY_CONTENT || got.id != site.canary {
        failures.push("the canary file reads back wrong".to_owned());
    }
    let errors: Vec<(&str, ApiError)> = [
        (
            "edit the canary outside the write scope",
            block_on(site.api.create_changeset(
                src_only,
                commit_req(
                    vec![site.tip],
                    vec![Change::Delete {
                        path: path("secret/canary.txt"),
                    }],
                    None,
                ),
            ))
            .map(drop),
        ),
        (
            "a file where the canary is a directory",
            block_on(site.api.create_changeset(
                &a,
                commit_req(
                    vec![site.tip],
                    vec![put("secret/canary.txt/under", CANARY_CONTENT)],
                    None,
                ),
            ))
            .map(drop),
        ),
        (
            "read the canary as a directory",
            block_on(site.api.read_file(
                &a,
                PathAt {
                    repo: REPO.to_owned(),
                    at: site.tip,
                    path: path("secret"),
                },
            ))
            .map(drop),
        ),
    ]
    .into_iter()
    .filter_map(|(n, r)| match r {
        Ok(()) => {
            failures.push(format!("{n}: succeeded"));
            None
        }
        Err(e) => Some((n, e)),
    })
    .collect();
    for (name, e) in &errors {
        scan(name, &e.to_string(), &c, &mut failures);
        scan_debug(name, e, &c, &mut failures);
    }
    report(&failures);
}
