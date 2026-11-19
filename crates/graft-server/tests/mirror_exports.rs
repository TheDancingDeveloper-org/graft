//! Recording exports (D41 §3.1, §3.6; WI-4388): the mirror service cuts a
//! batch under the authority lease it holds, then records each commit before
//! it pushes. It holds `repo:*:mirror` and not `repo:<name>:admin`, so the
//! record is admitted by the lease — the epoch names it — while an admin
//! caller without an epoch keeps the admin path.

mod common;

use common::{Auth, Site, api};
use graft_model::ChangesetId;
use graft_proto::ApiError;
use graft_proto::pb;
use graft_proto::pb::mirror_service_client::MirrorServiceClient;
use std::collections::BTreeMap;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

type Mirror = MirrorServiceClient<InterceptedService<Channel, Auth>>;

/// Records `changeset` as exported to `commit`, under `epoch` when given.
async fn record(
    client: &mut Mirror,
    repo: &str,
    changeset: ChangesetId,
    commit: &str,
    epoch: Option<u64>,
) -> Result<(), ApiError> {
    client
        .record_exports(pb::RecordExportsRequest {
            repo: repo.into(),
            exports: vec![pb::ExportedCommit {
                changeset: changeset.0.0.to_vec(),
                commit: commit.into(),
            }],
            epoch,
        })
        .await
        .map(|_| ())
        .map_err(api)
}

fn denied(e: ApiError, why: &str) {
    assert!(
        matches!(e, ApiError::PermissionDenied { .. }),
        "{why}: {e:?}"
    );
}

fn lease_lost(e: ApiError, why: &str) {
    assert!(matches!(e, ApiError::LeaseLost { .. }), "{why}: {e:?}");
}

#[tokio::test]
async fn exports_are_recorded_by_an_admin_or_the_mirror_holding_the_lease() {
    let site = Site::new("mirror-exports", "");
    let server = site.start();
    let admin = site.mint(&["repos:admin", "repo:*:admin"]);
    let mut owner = server.clients(Some(&admin)).await;
    owner.create_repo("r").await.expect("repo");
    let snapshot = BTreeMap::from([("a".to_owned(), b"1".to_vec())]);
    let created = owner.import("r", std::slice::from_ref(&snapshot)).await;
    let changeset = created[0].id;
    // The mirror records exports for a repository it is bound to: its
    // changeset existence read is the mirror-kind bound-repository grant.
    let operator = site.mint_kind("op", "human", &["mirror:bind"], 3600);
    let mut op =
        MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&operator)));
    op.set_mirror(pb::SetMirrorRequest {
        repo: "r".into(),
        upstream: "https://github.com/o/r.git".into(),
        lines: vec![pb::MirrorLine {
            branch: "main".into(),
            line: "main".into(),
            direction: "follow".into(),
        }],
        interval_s: 0,
        enabled: true,
        export: None,
        expected_version: None,
        start_at: None,
    })
    .await
    .expect("bind");

    // (a) An admin records without an epoch: the CLI path is unchanged.
    let admin = site.mint(&["repo:r:admin"]);
    let mut c =
        MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&admin)));
    record(&mut c, "r", changeset, "a1", None)
        .await
        .expect("an admin records without a lease");

    // (b) A mirror token with no epoch is not an admin.
    let m1 = site.mint_kind("mirror-a", "mirror", &["repo:*:mirror"], 3600);
    let mut c = MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&m1)));
    denied(
        record(&mut c, "r", changeset, "b2", None)
            .await
            .expect_err("no epoch"),
        "a mirror token without an epoch",
    );

    // (c) Under the lease it holds, the mirror records.
    let epoch = c
        .acquire_lease(pb::AcquireLeaseRequest {
            repo: "r".into(),
            instance: "i1".into(),
        })
        .await
        .expect("lease")
        .into_inner()
        .epoch;
    record(&mut c, "r", changeset, "c3", Some(epoch))
        .await
        .expect("the lease-holding mirror records");

    // (d) A stale epoch is refused.
    lease_lost(
        record(&mut c, "r", changeset, "d4", Some(epoch + 1))
            .await
            .expect_err("stale epoch"),
        "an epoch not held",
    );

    // (e) The epoch alone is guessable: another mirror subject is refused.
    let m2 = site.mint_kind("mirror-b", "mirror", &["repo:*:mirror"], 3600);
    let mut other =
        MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&m2)));
    lease_lost(
        record(&mut other, "r", changeset, "e5", Some(epoch))
            .await
            .expect_err("another subject's lease"),
        "a lease held by another subject",
    );

    // (f) An epoch is a mirror-kind claim: a repository admin passing one is
    // not admitted by it (the admin path is the one without an epoch).
    let reader = site.mint(&["repo:r:admin"]);
    let mut h =
        MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&reader)));
    denied(
        record(&mut h, "r", changeset, "f6", Some(epoch))
            .await
            .expect_err("not a mirror-kind token"),
        "an admin token with an epoch",
    );

    // (g) Releasing the lease takes the epoch with it.
    c.release_lease(pb::LeaseRequest {
        repo: "r".into(),
        epoch,
    })
    .await
    .expect("release");
    lease_lost(
        record(&mut c, "r", changeset, "07", Some(epoch))
            .await
            .expect_err("released lease"),
        "a released lease",
    );
    server.stop();
}
