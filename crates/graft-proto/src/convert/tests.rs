//! Every API type round-trips through protobuf bytes, and the boundary
//! rejects malformed messages.

use std::fmt::Debug;

use graft_model::{
    Author, AuthorKind, BlobId, Change, Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind,
    RepoPath, Timestamp, TreeId,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::WireError;
use crate::error::{ApiError, ConflictPath};
use crate::frame::Frame;
use crate::pb;
use crate::types::*;

/// Rust → protobuf → bytes → protobuf → Rust gives back the same value.
fn check<T, P, S>(name: &str, strategy: S)
where
    S: Strategy<Value = T>,
    T: Clone + Debug + PartialEq + TryFrom<P>,
    <T as TryFrom<P>>::Error: Debug,
    P: From<T> + prost::Message + Default,
{
    let mut runner = TestRunner::new(Config {
        cases: 128,
        ..Config::default()
    });
    runner
        .run(&strategy, |v| {
            let bytes = P::from(v.clone()).encode_to_vec();
            let back = T::try_from(P::decode(bytes.as_slice()).unwrap()).unwrap();
            prop_assert_eq!(back, v);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{name}: {e}"));
}

// ---- strategies -----------------------------------------------------------

fn oid() -> impl Strategy<Value = ObjectId> {
    any::<[u8; 32]>().prop_map(ObjectId)
}
fn cid() -> impl Strategy<Value = ChangesetId> {
    oid().prop_map(ChangesetId)
}
fn ocid() -> impl Strategy<Value = Option<ChangesetId>> {
    proptest::option::of(cid())
}
fn cids() -> impl Strategy<Value = Vec<ChangesetId>> {
    prop::collection::vec(cid(), 0..4)
}
fn oids() -> impl Strategy<Value = Vec<ObjectId>> {
    prop::collection::vec(oid(), 0..4)
}
fn text() -> impl Strategy<Value = String> {
    "\\PC{0,12}"
}
fn otext() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(text())
}
fn path() -> impl Strategy<Value = RepoPath> {
    "[a-z0-9_.-]{1,8}(/[a-z0-9_]{1,8}){0,3}"
        .prop_filter("no dot components", |s| {
            s.split('/').all(|c| c != "." && c != "..")
        })
        .prop_map(|s| RepoPath::new(&s).unwrap())
}
fn paths() -> impl Strategy<Value = Vec<RepoPath>> {
    prop::collection::vec(path(), 0..4)
}
fn page() -> impl Strategy<Value = Page> {
    (any::<u32>(), otext()).prop_map(|(limit, token)| Page { limit, token })
}
fn ts() -> impl Strategy<Value = Timestamp> {
    (any::<i64>(), any::<i16>()).prop_map(|(ms, offset_min)| Timestamp { ms, offset_min })
}
fn author() -> impl Strategy<Value = Author> {
    let kind = prop_oneof![
        Just(AuthorKind::Human),
        Just(AuthorKind::Agent),
        Just(AuthorKind::Bot),
        Just(AuthorKind::Ci)
    ];
    (text(), text(), kind).prop_map(|(name, email, kind)| Author { name, email, kind })
}
fn entry_kind() -> impl Strategy<Value = EntryKind> {
    prop_oneof![
        Just(EntryKind::File),
        Just(EntryKind::Exec),
        Just(EntryKind::Symlink),
        Just(EntryKind::Tree),
        Just(EntryKind::Submodule)
    ]
}
fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        (path(), entry_kind(), oid()).prop_map(|(path, kind, id)| Change::Put { path, kind, id }),
        path().prop_map(|path| Change::Delete { path }),
    ]
}
fn changes() -> impl Strategy<Value = Vec<Change>> {
    prop::collection::vec(change(), 0..4)
}
fn metadata() -> impl Strategy<Value = graft_model::Metadata> {
    prop::collection::btree_map(text(), text(), 0..3)
}
fn frame() -> impl Strategy<Value = Frame> {
    (prop::collection::vec(any::<u8>(), 0..1024), any::<bool>()).prop_map(|(p, zeros)| {
        // Zeros compress, so both codecs appear.
        let p = if zeros { vec![0; p.len()] } else { p };
        Frame::encode(ObjectKind::Blob, &p)
    })
}
fn frames() -> impl Strategy<Value = Vec<Frame>> {
    prop::collection::vec(frame(), 0..3)
}
fn changeset() -> impl Strategy<Value = Changeset> {
    (cids(), oid(), changes(), author(), ts(), text(), metadata()).prop_map(
        |(parents, root, changes, author, authored_at, message, metadata)| Changeset {
            parents,
            root: TreeId(root),
            changes,
            author,
            authored_at,
            message,
            metadata,
        },
    )
}
fn draft_state() -> impl Strategy<Value = DraftState> {
    prop_oneof![
        Just(DraftState::Draft),
        Just(DraftState::Landed),
        Just(DraftState::Superseded),
        Just(DraftState::Abandoned)
    ]
}
fn ref_kind() -> impl Strategy<Value = RefKind> {
    prop_oneof![Just(RefKind::Line), Just(RefKind::Tag), Just(RefKind::Tool)]
}
fn merge_mode() -> impl Strategy<Value = MergeMode> {
    prop_oneof![Just(MergeMode::File), Just(MergeMode::Line)]
}
fn created() -> impl Strategy<Value = Created> {
    (cid(), oid(), any::<u64>(), draft_state()).prop_map(|(id, root, lsn, state)| Created {
        id,
        root: TreeId(root),
        lsn,
        state,
    })
}
fn changeset_view() -> impl Strategy<Value = ChangesetView> {
    (changeset(), any::<u64>(), draft_state(), ocid()).prop_map(
        |(changeset, lsn, state, superseded_by)| ChangesetView {
            changeset,
            lsn,
            state,
            superseded_by,
        },
    )
}
fn ref_view() -> impl Strategy<Value = RefView> {
    (text(), cid(), any::<u64>(), ref_kind()).prop_map(|(name, target, version, kind)| RefView {
        name,
        target,
        version,
        kind,
    })
}
fn entry_view() -> impl Strategy<Value = EntryView> {
    (path(), entry_kind(), oid()).prop_map(|(path, kind, id)| EntryView { path, kind, id })
}
fn summary() -> impl Strategy<Value = ChangesetSummary> {
    (
        cid(),
        author(),
        ts(),
        text(),
        proptest::option::of(any::<u64>()),
    )
        .prop_map(
            |(id, author, authored_at, subject, line_pos)| ChangesetSummary {
                id,
                author,
                authored_at,
                subject,
                line_pos,
            },
        )
}
fn file_diff() -> impl Strategy<Value = FileDiff> {
    (change(), otext(), any::<bool>()).prop_map(|(change, patch, truncated)| FileDiff {
        change,
        patch,
        truncated,
    })
}
fn workspace_view() -> impl Strategy<Value = WorkspaceView> {
    (text(), text(), otext(), text(), cid(), ocid(), any::<i64>()).prop_map(
        |(name, owner, session, line, base, head, lease_until_ms)| WorkspaceView {
            name,
            owner,
            session,
            line,
            base,
            head,
            lease_until_ms,
        },
    )
}
fn workspace_ref() -> impl Strategy<Value = WorkspaceRef> {
    (text(), text()).prop_map(|(repo, name)| WorkspaceRef { repo, name })
}
fn api_error() -> impl Strategy<Value = ApiError> {
    let conflict_path = (
        text(),
        proptest::option::of(oid()),
        proptest::option::of(oid()),
        proptest::option::of(oid()),
    )
        .prop_map(|(path, base, ours, theirs)| ConflictPath {
            path,
            base,
            ours,
            theirs,
        });
    prop_oneof![
        text().prop_map(|what| ApiError::NotFound { what }),
        cid().prop_map(|id| ApiError::Collected { id }),
        (text(), text()).prop_map(|(path, reason)| ApiError::InvalidPath { path, reason }),
        (text(), text()).prop_map(|(path, rule)| ApiError::InvalidChange { path, rule }),
        cid().prop_map(|id| ApiError::UnknownParent { id }),
        oids().prop_map(|ids| ApiError::MissingObjects { ids }),
        (text(), ocid()).prop_map(|(name, actual)| ApiError::RefConflict { name, actual }),
        (prop::collection::vec(conflict_path, 0..3), cid())
            .prop_map(|(paths, onto)| ApiError::Conflict { paths, onto }),
        (text(), cid()).prop_map(|(line, tip)| ApiError::Contended { line, tip }),
        (text(), text()).prop_map(|(policy, detail)| ApiError::Rejected { policy, detail }),
        (text(), otext()).prop_map(|(scope, path)| ApiError::PermissionDenied { scope, path }),
        text().prop_map(|workspace| ApiError::LeaseExpired { workspace }),
        (text(), any::<u64>()).prop_map(|(limit, max)| ApiError::LimitExceeded { limit, max }),
        oid().prop_map(|id| ApiError::CorruptObject { id }),
        oid().prop_map(|id| ApiError::Obliterated { id: BlobId(id) }),
        any::<u64>().prop_map(|retry_after_ms| ApiError::Unavailable { retry_after_ms }),
        Just(ApiError::Unauthenticated),
        text().prop_map(|server| ApiError::IncompatibleProtocol { server }),
    ]
}

// ---- round-trips ----------------------------------------------------------

#[test]
fn shared_types_round_trip() {
    check::<Page, pb::Page, _>("Page", page());
    check::<Timestamp, pb::Timestamp, _>("Timestamp", ts());
    check::<Author, pb::Author, _>("Author", author());
    check::<Change, pb::Change, _>("Change", change());
    check::<Changeset, pb::Changeset, _>("Changeset", changeset());
}

#[test]
fn object_types_round_trip() {
    check::<GetObjects, pb::GetObjectsRequest, _>(
        "GetObjects",
        (text(), oids()).prop_map(|(repo, ids)| GetObjects { repo, ids }),
    );
    check::<Objects, pb::Objects, _>(
        "Objects",
        (
            prop::collection::vec((oid(), frame()), 0..3),
            oids(),
            oids(),
        )
            .prop_map(|(found, missing, deferred)| Objects {
                found,
                missing,
                deferred,
            }),
    );
    check::<HasObjects, pb::HasObjectsRequest, _>(
        "HasObjects",
        (text(), oids()).prop_map(|(repo, ids)| HasObjects { repo, ids }),
    );
    check::<PutObjects, pb::PutObjectsRequest, _>(
        "PutObjects",
        (text(), frames()).prop_map(|(repo, blobs)| PutObjects { repo, blobs }),
    );
    check::<Vec<ObjectId>, pb::ObjectIds, _>("ObjectIds", oids());
}

#[test]
fn changeset_types_round_trip() {
    check::<CreateChangeset, pb::CreateChangesetRequest, _>(
        "CreateChangeset",
        (
            text(),
            cids(),
            changes(),
            author(),
            ts(),
            text(),
            metadata(),
            frames(),
            otext(),
        )
            .prop_map(
                |(
                    repo,
                    parents,
                    changes,
                    author,
                    authored_at,
                    message,
                    metadata,
                    inline_blobs,
                    workspace,
                )| CreateChangeset {
                    repo,
                    parents,
                    changes,
                    author,
                    authored_at,
                    message,
                    metadata,
                    inline_blobs,
                    workspace,
                },
            ),
    );
    check::<Created, pb::Created, _>("Created", created());
    check::<Vec<Created>, pb::CreatedList, _>(
        "CreatedList",
        prop::collection::vec(created(), 0..3),
    );
    check::<GetChangesets, pb::GetChangesetsRequest, _>(
        "GetChangesets",
        (text(), cids()).prop_map(|(repo, ids)| GetChangesets { repo, ids }),
    );
    check::<ChangesetView, pb::ChangesetView, _>("ChangesetView", changeset_view());
    check::<Changesets, pb::Changesets, _>(
        "Changesets",
        (
            prop::collection::vec(changeset_view(), 0..3),
            cids(),
            cids(),
        )
            .prop_map(|(found, missing, collected)| Changesets {
                found,
                missing,
                collected,
            }),
    );
    check::<ImportChangesets, pb::ImportChangesetsRequest, _>(
        "ImportChangesets",
        (text(), prop::collection::vec(changeset(), 0..3))
            .prop_map(|(repo, changesets)| ImportChangesets { repo, changesets }),
    );
}

#[test]
fn ref_types_round_trip() {
    check::<RefView, pb::RefView, _>("RefView", ref_view());
    check::<Vec<RefView>, pb::RefViews, _>("RefViews", prop::collection::vec(ref_view(), 0..3));
    check::<ResolveRef, pb::ResolveRefRequest, _>(
        "ResolveRef",
        (text(), text()).prop_map(|(repo, name)| ResolveRef { repo, name }),
    );
    check::<ListRefs, pb::ListRefsRequest, _>(
        "ListRefs",
        (text(), otext(), page()).prop_map(|(repo, prefix, page)| ListRefs { repo, prefix, page }),
    );
    check::<UpdateRef, pb::UpdateRefRequest, _>(
        "UpdateRef",
        (text(), text(), ocid(), cid(), ref_kind()).prop_map(
            |(repo, name, expected, new, kind)| UpdateRef {
                repo,
                name,
                expected,
                new,
                kind,
            },
        ),
    );
    check::<CreateLine, pb::CreateLineRequest, _>(
        "CreateLine",
        (text(), text(), cid()).prop_map(|(repo, name, from)| CreateLine { repo, name, from }),
    );
}

#[test]
fn snapshot_types_round_trip() {
    check::<PathAt, pb::PathAt, _>(
        "PathAt",
        (text(), cid(), path()).prop_map(|(repo, at, path)| PathAt { repo, at, path }),
    );
    check::<EntryView, pb::EntryView, _>("EntryView", entry_view());
    check::<FileContent, pb::FileContent, _>(
        "FileContent",
        (
            entry_kind(),
            oid(),
            prop::collection::vec(any::<u8>(), 0..64),
        )
            .prop_map(|(kind, id, data)| FileContent {
                kind,
                id: BlobId(id),
                data,
            }),
    );
    check::<ListTree, pb::ListTreeRequest, _>(
        "ListTree",
        (
            text(),
            cid(),
            proptest::option::of(path()),
            any::<bool>(),
            page(),
        )
            .prop_map(|(repo, at, path, recursive, page)| ListTree {
                repo,
                at,
                path,
                recursive,
                page,
            }),
    );
    check::<Entries, pb::Entries, _>(
        "Entries",
        (prop::collection::vec(entry_view(), 0..3), otext())
            .prop_map(|(entries, next)| Entries { entries, next }),
    );
}

#[test]
fn history_types_round_trip() {
    check::<Log, pb::LogRequest, _>(
        "Log",
        (text(), cid(), ocid(), paths(), page()).prop_map(|(repo, from, until, paths, page)| Log {
            repo,
            from,
            until,
            paths,
            page,
        }),
    );
    check::<ChangesetSummary, pb::ChangesetSummary, _>("ChangesetSummary", summary());
    check::<History, pb::History, _>(
        "History",
        (prop::collection::vec(summary(), 0..3), otext())
            .prop_map(|(changesets, next)| History { changesets, next }),
    );
    check::<Pair, pb::Pair, _>(
        "Pair",
        (text(), cid(), cid()).prop_map(|(repo, a, b)| Pair { repo, a, b }),
    );
    let patch =
        (any::<u32>(), any::<u64>()).prop_map(|(context, max_bytes_per_file)| PatchOptions {
            context,
            max_bytes_per_file,
        });
    check::<Diff, pb::DiffRequest, _>(
        "Diff",
        (
            text(),
            cid(),
            cid(),
            paths(),
            proptest::option::of(patch),
            page(),
        )
            .prop_map(|(repo, from, to, paths, patch, page)| Diff {
                repo,
                from,
                to,
                paths,
                patch,
                page,
            }),
    );
    check::<FileDiff, pb::FileDiff, _>("FileDiff", file_diff());
    check::<DiffResult, pb::DiffResult, _>(
        "DiffResult",
        (prop::collection::vec(file_diff(), 0..3), otext())
            .prop_map(|(files, next)| DiffResult { files, next }),
    );
    check::<Blame, pb::BlameRequest, _>(
        "Blame",
        (
            text(),
            cid(),
            path(),
            proptest::option::of((any::<u32>(), any::<u32>())),
        )
            .prop_map(|(repo, at, path, lines)| Blame {
                repo,
                at,
                path,
                lines,
            }),
    );
    check::<BlameResult, pb::BlameResult, _>(
        "BlameResult",
        prop::collection::vec(
            (any::<u32>(), any::<u32>(), summary()).prop_map(|(start, end, origin)| BlameRange {
                start,
                end,
                origin,
            }),
            0..3,
        )
        .prop_map(|ranges| BlameResult { ranges }),
    );
}

#[test]
fn land_types_round_trip() {
    check::<Rebase, pb::RebaseRequest, _>(
        "Rebase",
        (text(), cid(), cid(), merge_mode()).prop_map(|(repo, changeset, onto, merge)| Rebase {
            repo,
            changeset,
            onto,
            merge,
        }),
    );
    check::<Revert, pb::RevertRequest, _>(
        "Revert",
        (text(), cid(), cid(), otext()).prop_map(|(repo, changeset, onto, workspace)| Revert {
            repo,
            changeset,
            onto,
            workspace,
        }),
    );
    check::<Land, pb::LandRequest, _>(
        "Land",
        (
            text(),
            cid(),
            text(),
            any::<bool>(),
            merge_mode(),
            proptest::option::of(any::<u64>()),
        )
            .prop_map(|(repo, head, line, squash, merge, deadline_ms)| Land {
                repo,
                head,
                line,
                squash,
                merge,
                deadline_ms,
            }),
    );
    check::<Landed, pb::Landed, _>(
        "Landed",
        prop_oneof![
            (cid(), any::<u64>(), any::<u64>(), any::<bool>(), paths()).prop_map(
                |(landed, line_version, pos, rebased, merged_paths)| Landed::Landed {
                    landed,
                    line_version,
                    pos,
                    rebased,
                    merged_paths,
                }
            ),
            text().prop_map(|pr_url| Landed::Queued { pr_url }),
        ],
    );
    check::<Attest, pb::AttestRequest, _>(
        "Attest",
        (text(), cid(), text(), any::<bool>(), text()).prop_map(
            |(repo, changeset, check, passed, url)| Attest {
                repo,
                changeset,
                check,
                passed,
                url,
            },
        ),
    );
    check::<Watch, pb::WatchRequest, _>(
        "Watch",
        (
            text(),
            any::<u64>(),
            prop::collection::vec(text(), 0..3),
            any::<u32>(),
            any::<u32>(),
        )
            .prop_map(|(repo, from_lsn, kinds, limit, wait_ms)| Watch {
                repo,
                from_lsn,
                kinds,
                limit,
                wait_ms,
            }),
    );
    check::<JournalPage, pb::JournalPage, _>(
        "JournalPage",
        (
            prop::collection::vec(
                (any::<u64>(), text(), text()).prop_map(|(lsn, kind, body)| JournalEntry {
                    lsn,
                    kind,
                    body,
                }),
                0..3,
            ),
            any::<u64>(),
        )
            .prop_map(|(entries, next_lsn)| JournalPage { entries, next_lsn }),
    );
}

#[test]
fn workspace_types_round_trip() {
    check::<WorkspaceView, pb::WorkspaceView, _>("WorkspaceView", workspace_view());
    check::<Vec<WorkspaceView>, pb::WorkspaceViews, _>(
        "WorkspaceViews",
        prop::collection::vec(workspace_view(), 0..3),
    );
    check::<OpenWorkspace, pb::OpenWorkspaceRequest, _>(
        "OpenWorkspace",
        (text(), otext(), text(), ocid(), any::<u64>()).prop_map(
            |(repo, name, line, at, lease_ms)| OpenWorkspace {
                repo,
                name,
                line,
                at,
                lease_ms,
            },
        ),
    );
    check::<WorkspaceRef, pb::WorkspaceRef, _>("WorkspaceRef", workspace_ref());
    check::<SetHead, pb::SetHeadRequest, _>(
        "SetHead",
        (workspace_ref(), ocid(), ocid()).prop_map(|(workspace, expected, new)| SetHead {
            workspace,
            expected,
            new,
        }),
    );
    check::<Synced, pb::Synced, _>(
        "Synced",
        (workspace_view(), paths()).prop_map(|(workspace, changed_paths)| Synced {
            workspace,
            changed_paths,
        }),
    );
    check::<StackOp, pb::StackOp, _>(
        "StackOp (no split)",
        prop_oneof![
            cids().prop_map(StackOp::Reorder),
            cids().prop_map(StackOp::Squash),
            cid().prop_map(StackOp::Drop),
        ],
    );
    check::<StackOp, pb::StackOp, _>(
        "StackOp::Split",
        (cid(), prop::collection::vec(paths(), 0..3))
            .prop_map(|(draft, groups)| StackOp::Split { draft, groups }),
    );
}

#[test]
fn admin_types_round_trip() {
    check::<Gc, pb::GcRequest, _>(
        "Gc",
        (text(), any::<bool>()).prop_map(|(repo, dry_run)| Gc { repo, dry_run }),
    );
    check::<GcReport, pb::GcReport, _>(
        "GcReport",
        (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(|(changesets, objects, bytes)| {
            GcReport {
                changesets,
                objects,
                bytes,
            }
        }),
    );
    check::<MintToken, pb::MintTokenRequest, _>(
        "MintToken",
        (
            text(),
            prop::collection::vec(text(), 0..3),
            any::<u64>(),
            otext(),
            otext(),
        )
            .prop_map(|(subject, scopes, ttl_s, session, work_item)| MintToken {
                subject,
                scopes,
                ttl_s,
                session,
                work_item,
            }),
    );
}

#[test]
fn errors_round_trip_through_protobuf() {
    check::<ApiError, pb::ApiError, _>("ApiError", api_error());
}

#[test]
fn errors_round_trip_through_grpc_status() {
    TestRunner::default()
        .run(&api_error(), |e| {
            let status = tonic::Status::from(e.clone());
            prop_assert_eq!(status.code(), e.grpc_code());
            prop_assert_eq!(ApiError::from_status(&status), Some(e));
            Ok(())
        })
        .unwrap();
}

#[test]
fn foreign_status_carries_no_api_error() {
    assert_eq!(
        ApiError::from_status(&tonic::Status::internal("boom")),
        None
    );
}

// ---- boundary validation --------------------------------------------------

#[test]
fn rejects_short_ids() {
    let r = GetObjects::try_from(pb::GetObjectsRequest {
        repo: "r".into(),
        ids: vec![vec![0; 31]],
    });
    assert_eq!(
        r,
        Err(WireError {
            field: "ids",
            reason: "id is not 32 bytes"
        })
    );
}

#[test]
fn rejects_unspecified_enums_and_missing_fields() {
    let r = RefView::try_from(pb::RefView {
        name: "main".into(),
        target: vec![0; 32],
        version: 1,
        kind: pb::RefKind::Unspecified as i32,
    });
    assert!(r.is_err());
    let r = Created::try_from(pb::Created {
        id: vec![0; 32],
        root: vec![0; 32],
        lsn: 1,
        state: 99,
    });
    assert!(r.is_err());
    assert!(ChangesetSummary::try_from(pb::ChangesetSummary::default()).is_err());
    assert!(ApiError::try_from(pb::ApiError::default()).is_err());
}

#[test]
fn unset_merge_mode_means_file() {
    let r = Rebase::try_from(pb::RebaseRequest {
        repo: "r".into(),
        changeset: vec![1; 32],
        onto: vec![2; 32],
        merge: 0,
    })
    .unwrap();
    assert_eq!(r.merge, MergeMode::File);
}

#[test]
fn rejects_malformed_frames() {
    let r = PutObjects::try_from(pb::PutObjectsRequest {
        repo: "r".into(),
        blobs: vec![b"GRF2b\0\0\0\0\0\0\0\0\0".to_vec()],
    });
    assert_eq!(r.unwrap_err().field, "blobs");
}

#[test]
fn rejects_out_of_range_offset() {
    let r = Timestamp::try_from(pb::Timestamp {
        ms: 0,
        offset_min: i32::from(i16::MAX) + 1,
    });
    assert!(r.is_err());
}

#[test]
fn token_debug_is_redacted() {
    let t = pb::Token {
        token: "secret-value".into(),
    };
    assert!(!format!("{t:?}").contains("secret-value"));
}

#[test]
fn ci_token_debug_is_redacted() {
    let r = pb::ExchangeCiTokenRequest {
        ci_token: "eyJ.secret.jwt".into(),
        changeset: None,
    };
    assert!(!format!("{r:?}").contains("secret"));
}

#[test]
fn conflict_types_round_trip() {
    let side =
        || prop::option::of((oid(), entry_kind()).prop_map(|(id, kind)| ConflictSide { id, kind }));
    let reason = prop::option::of(prop_oneof![
        Just(ConflictReason::Binary),
        Just(ConflictReason::TooLarge),
        Just(ConflictReason::Kind),
        Just(ConflictReason::Clash)
    ]);
    let view = (
        path(),
        side(),
        side(),
        side(),
        any::<bool>(),
        prop::option::of(prop::collection::vec(any::<u8>(), 0..16)),
        reason,
    )
        .prop_map(
            |(path, base, ours, theirs, text_mergeable, markers, reason)| ConflictView {
                path,
                base,
                ours,
                theirs,
                text_mergeable,
                markers,
                reason,
            },
        );
    check::<GetConflicts, pb::GetConflictsRequest, _>(
        "GetConflicts",
        (workspace_ref(), ocid(), page()).prop_map(|(workspace, onto, page)| GetConflicts {
            workspace,
            onto,
            page,
        }),
    );
    check::<Conflicts, pb::Conflicts, _>(
        "Conflicts",
        (cid(), ocid(), prop::collection::vec(view, 0..3), otext()).prop_map(
            |(onto, draft, paths, next)| Conflicts {
                onto,
                draft,
                paths,
                next,
            },
        ),
    );
    let resolution = prop_oneof![
        Just(Resolution::Delete),
        (entry_kind(), oid()).prop_map(|(kind, id)| Resolution::Put {
            kind,
            content: ResolvedContent::Blob(BlobId(id)),
        }),
        (entry_kind(), prop::collection::vec(any::<u8>(), 0..16)).prop_map(|(kind, b)| {
            Resolution::Put {
                kind,
                content: ResolvedContent::Inline(b),
            }
        }),
    ];
    check::<Resolve, pb::ResolveRequest, _>(
        "Resolve",
        (
            workspace_ref(),
            cid(),
            ocid(),
            prop::collection::vec(
                (path(), resolution)
                    .prop_map(|(path, resolution)| PathResolution { path, resolution }),
                0..3,
            ),
        )
            .prop_map(|(workspace, onto, expected_head, resolutions)| Resolve {
                workspace,
                onto,
                expected_head,
                resolutions,
            }),
    );
    check::<Resolved, pb::Resolved, _>(
        "Resolved",
        (workspace_view(), cid(), paths(), paths()).prop_map(
            |(workspace, head, resolved_paths, merged_paths)| Resolved {
                workspace,
                head,
                resolved_paths,
                merged_paths,
            },
        ),
    );
}
