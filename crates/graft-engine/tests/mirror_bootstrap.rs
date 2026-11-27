//! A mirror service bootstraps a freshly created repository, and nothing
//! wider (D41, WI-4913).
//!
//! The mirror token holds only `repo:*:mirror`. For a line its row follows
//! that does not exist yet, it may upload objects, import changesets and
//! create the followed-line marker (and then the line, which the marker
//! makes a followed line), all under the lease it holds. It may not do any
//! of that for a line that already exists without a marker, for a line its
//! row does not follow, for a disabled or export-only row, without its lease,
//! or on a repository with no row.

mod common;

use std::sync::Arc;

use common::*;
use graft_engine::EngineConfig;
use graft_model::ChangesetId;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind, RepoApi};
use graft_store::memory::MemoryStore;
use graft_store::{
    LeaseRecord, MetaStore, MetaTxn, MirrorDirection, MirrorExportRecord, MirrorLineRecord,
    MirrorRecord, Precondition, TxnOutcome, Write,
};

/// The mirror service as a minted `mirror` token gives it: no write, no
/// admin, only the mirror set.
fn mirror() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-mirror".to_owned(),
            kind: PrincipalKind::Mirror,
            session: None,
            work_item: None,
            scopes: vec!["repo:*:mirror".to_owned()],
        },
        request_id: "mirror".to_owned(),
        deadline_ms: None,
    }
}

struct World {
    store: Arc<MemoryStore>,
    f: Fixture<MemoryStore>,
}

fn world() -> World {
    let store = Arc::new(MemoryStore::new());
    let f = build(store.clone(), EngineConfig::default(), None);
    World { store, f }
}

impl World {
    fn id(&self, repo: &str) -> graft_store::RepoId {
        block_on(self.store.repo(repo))
            .expect("store")
            .expect("repo exists")
    }

    /// Binds `repo` to a mirror row following (or exporting) `lines`.
    fn bind(&self, repo: &str, lines: &[(&str, MirrorDirection)], enabled: bool) {
        let upstream = format!("https://github.com/o/{repo}.git");
        let txn = MetaTxn {
            preconditions: vec![Precondition::MirrorVersionIs {
                upstream: upstream.clone(),
                version: None,
            }],
            writes: vec![Write::PutMirror(MirrorRecord {
                upstream,
                lines: lines
                    .iter()
                    .map(|(line, direction)| MirrorLineRecord {
                        branch: (*line).into(),
                        line: (*line).into(),
                        direction: *direction,
                    })
                    .collect(),
                interval_s: 0,
                enabled,
                credential: None,
                kek_id: None,
                export: MirrorExportRecord {
                    default_author: None,
                    authors: Vec::new(),
                },
                version: 1,
                status: None,
                created_by: "op".into(),
                updated_by: "op".into(),
                updated_at_ms: 0,
                start_at: None,
                auth: None,
            })],
        };
        let outcome = block_on(self.store.commit(self.id(repo), txn)).expect("bind");
        assert!(matches!(outcome, TxnOutcome::Committed(_)));
    }

    /// Gives `repo` a live authority lease held by `holder` at epoch 1.
    fn lease(&self, repo: &str, holder: &str) {
        let txn = MetaTxn {
            preconditions: vec![Precondition::LeaseEpochIs { epoch: 0 }],
            writes: vec![Write::PutLease(LeaseRecord {
                holder: Some(holder.to_owned()),
                epoch: 1,
                expires_at_ms: i64::MAX,
            })],
        };
        let outcome = block_on(self.store.commit(self.id(repo), txn)).expect("lease");
        assert!(matches!(outcome, TxnOutcome::Committed(_)));
    }

    /// A repository with one unlanded changeset (the head an import would
    /// leave), the row `lines`, and the mirror service's lease.
    fn repo(
        &self,
        name: &str,
        row: Option<(&[(&str, MirrorDirection)], bool)>,
        lease: Option<&str>,
    ) -> (Repo<'_, MemoryStore>, ChangesetId) {
        let repo = Repo::new(&self.f, name);
        let head = repo.commit(vec![], &[("a", b"1")], &[]).id;
        if let Some((lines, enabled)) = row {
            self.bind(name, lines, enabled);
        }
        if let Some(holder) = lease {
            self.lease(name, holder);
        }
        (repo, head)
    }

    fn put_objects(&self, repo: &str) -> Result<(), ApiError> {
        block_on(self.f.engine.authorize_put_objects(&mirror(), repo))
    }

    fn update(
        &self,
        repo: &str,
        name: &str,
        new: ChangesetId,
        kind: RefKind,
        epoch: Option<u64>,
    ) -> Result<(), ApiError> {
        block_on(self.f.engine.update_ref(
            &mirror(),
            UpdateRef {
                repo: repo.to_owned(),
                name: name.to_owned(),
                expected: None,
                new,
                kind,
                lease_epoch: epoch,
            },
        ))
        .map(drop)
    }

    fn marker(
        &self,
        repo: &str,
        line: &str,
        new: ChangesetId,
        epoch: Option<u64>,
    ) -> Result<(), ApiError> {
        self.update(repo, &followed_ref(line), new, RefKind::Tool, epoch)
    }
}

const FOLLOW_MAIN: &[(&str, MirrorDirection)] = &[("main", MirrorDirection::Follow)];
const INSTANCE: &str = "graft-mirror/i1";

fn denied(result: Result<(), ApiError>, why: &str) {
    assert!(
        matches!(result, Err(ApiError::PermissionDenied { .. })),
        "{why}: {result:?}"
    );
}

#[test]
fn a_bound_mirror_with_its_lease_bootstraps_a_line_that_does_not_exist_yet() {
    let w = world();
    let (repo, head) = w.repo("r", Some((FOLLOW_MAIN, true)), Some(INSTANCE));

    // Before anything exists: objects and changesets are accepted, because
    // the row follows `main` and `main` does not exist yet.
    w.put_objects("r")
        .expect("objects into a fresh followed line");

    // The marker first, then the line, which the marker makes a followed
    // line the mirror may create by CAS under its lease.
    w.marker("r", "main", head, Some(1)).expect("marker");
    w.update("r", "main", head, RefKind::Line, Some(1))
        .expect("line");
    w.put_objects("r").expect("objects once the line is marked");

    let tip = block_on(repo.e().resolve_ref(
        &repo.cx,
        ResolveRef {
            repo: "r".into(),
            name: "main".into(),
        },
    ))
    .expect("line");
    assert_eq!(tip.target, head);
}

#[test]
fn the_bootstrap_does_not_reach_past_the_followed_line_that_is_absent() {
    let w = world();

    // No row: the repository is not bound to a mirror at all.
    let (_r, head) = w.repo("norow", None, Some(INSTANCE));
    denied(w.put_objects("norow"), "no row, objects");
    denied(w.marker("norow", "main", head, Some(1)), "no row, marker");

    // A disabled row.
    let (_r, head) = w.repo("off", Some((FOLLOW_MAIN, false)), Some(INSTANCE));
    denied(w.put_objects("off"), "disabled row, objects");
    denied(
        w.marker("off", "main", head, Some(1)),
        "disabled row, marker",
    );

    // An export-only line is not followed.
    let export = &[("main", MirrorDirection::Export)];
    let (_r, head) = w.repo("export", Some((export, true)), Some(INSTANCE));
    denied(w.put_objects("export"), "export-only line, objects");
    denied(
        w.marker("export", "main", head, Some(1)),
        "export-only line, marker",
    );

    // A line the row does not follow.
    let (_r, head) = w.repo("other", Some((FOLLOW_MAIN, true)), Some(INSTANCE));
    denied(
        w.marker("other", "side", head, Some(1)),
        "line not in the row",
    );

    // The line itself still needs its marker first: the mirror may not
    // create an unmarked line.
    denied(
        w.update("other", "main", head, RefKind::Line, Some(1)),
        "unmarked line",
    );
}

#[test]
fn a_line_that_already_exists_without_a_marker_stays_admin_only() {
    let w = world();
    let (repo, head) = w.repo("old", Some((FOLLOW_MAIN, true)), Some(INSTANCE));
    // An admin landed on `main`: it has history and no marker.
    repo.update_ref("main", None, head, RefKind::Line)
        .expect("admin line");

    // The mirror may neither upload into it nor mark it: locking an
    // existing line is `mark-followed`, which needs the repository admin.
    denied(w.put_objects("old"), "existing unmarked line, objects");
    denied(
        w.marker("old", "main", head, Some(1)),
        "existing unmarked line, marker",
    );
}

#[test]
fn the_bootstrap_is_fenced_by_the_mirror_lease() {
    let w = world();

    // No lease at all: the objects are refused, and the marker needs an epoch.
    let (_r, head) = w.repo("nolease", Some((FOLLOW_MAIN, true)), None);
    denied(w.put_objects("nolease"), "no lease, objects");
    assert!(
        matches!(
            w.marker("nolease", "main", head, None),
            Err(ApiError::LeaseLost { .. })
        ),
        "no epoch"
    );

    // Another holder's lease is not the mirror service's: the object upload
    // checks the holder. (A ref move is fenced by the lease epoch.)
    w.repo(
        "foreign",
        Some((FOLLOW_MAIN, true)),
        Some("someone-else/i1"),
    );
    denied(w.put_objects("foreign"), "foreign lease, objects");

    // Our lease, but the wrong epoch.
    let (_r, head) = w.repo("stale", Some((FOLLOW_MAIN, true)), Some(INSTANCE));
    assert!(
        matches!(
            w.marker("stale", "main", head, Some(2)),
            Err(ApiError::LeaseLost { .. })
        ),
        "wrong epoch"
    );
}

#[test]
fn only_the_mirror_kind_gets_the_bootstrap() {
    let w = world();
    let (_r, head) = w.repo("r", Some((FOLLOW_MAIN, true)), Some(INSTANCE));
    // A reader, and a writer without admin, may not create the marker; and a
    // human holding the mirror scope is not the mirror kind. (A writer may
    // upload objects: that is the write scope, not the bootstrap.)
    for (who, scopes, uploads) in [
        ("reader", vec!["repo:*:read"], false),
        ("writer", vec!["repo:r:write"], true),
        ("human with the mirror scope", vec!["repo:*:mirror"], false),
    ] {
        let mut cx = cx(&scopes);
        cx.principal.subject = "graft-mirror".to_owned();
        let result = block_on(w.f.engine.update_ref(
            &cx,
            UpdateRef {
                repo: "r".into(),
                name: followed_ref("main"),
                expected: None,
                new: head,
                kind: RefKind::Tool,
                lease_epoch: Some(1),
            },
        ));
        assert!(
            matches!(result, Err(ApiError::PermissionDenied { .. })),
            "{who}: {result:?}"
        );
        let objects = block_on(w.f.engine.authorize_put_objects(&cx, "r"));
        if uploads {
            assert!(objects.is_ok(), "{who}: {objects:?}");
        } else {
            denied(objects, who);
        }
    }
}

#[test]
fn a_marker_must_point_at_a_changeset_that_exists() {
    let w = world();
    w.repo("r", Some((FOLLOW_MAIN, true)), Some(INSTANCE));
    let nowhere = ChangesetId(blob_id(b"not a changeset"));
    assert!(w.marker("r", "main", nowhere, Some(1)).is_err());
}
