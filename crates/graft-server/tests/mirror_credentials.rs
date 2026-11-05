//! Mirror credentials end to end (D41 §3.4): seal over gRPC, read back only
//! as the lease-holding mirror token, seed and rotate on the host, and never
//! a plaintext in the server log.

mod common;

use std::path::Path;
use std::process::Stdio;

use common::{Auth, Site};
use graft_proto::pb;
use graft_proto::pb::mirror_service_client::MirrorServiceClient;
use tonic::Code;

const UP: &str = "https://github.com/o/r.git";
const PAT: &str = "ghp_first_interim_row_pat";
const SEEDED: &str = "ghp_seeded_from_the_deploy_env";

fn write_key(path: &Path, byte: u8) {
    std::fs::write(path, [byte; 32]).expect("key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// Rewrites the site's `[mirror]` key settings.
fn set_keys(site: &Site, keys: &str) {
    let text = std::fs::read_to_string(&site.config).expect("config");
    let base = text.split("[mirror]").next().expect("base").to_owned();
    std::fs::write(&site.config, format!("{base}[mirror]\n{keys}\n")).expect("config");
}

async fn client(
    server: &common::Server,
    token: &str,
) -> MirrorServiceClient<
    tonic::service::interceptor::InterceptedService<tonic::transport::Channel, Auth>,
> {
    MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(token)))
}

#[tokio::test]
async fn credentials_seal_open_only_for_the_lease_holder_and_survive_rotation() {
    let site = Site::new("mirror-cred", "");
    let k1 = site.dir.path().join("kek-k1");
    let k2 = site.dir.path().join("kek-k2");
    write_key(&k1, 0x11);
    write_key(&k2, 0x22);
    set_keys(
        &site,
        &format!(
            "credential_kek = {{ id = \"k1\", file = \"{}\" }}",
            k1.display()
        ),
    );
    let log = site.dir.path().join("server.log");
    let server = site.start_with_stderr(Stdio::from(std::fs::File::create(&log).expect("log")));

    let admin = site.mint(&["repos:admin"]);
    common::Clients::new(server.channel().await, Auth::new(Some(&admin)))
        .create_repo("r")
        .await
        .expect("repo");
    let operator = site.mint_kind("op", "human", &["mirror:bind"], 3600);
    let mut op = client(&server, &operator).await;
    let row = op
        .set_mirror(pb::SetMirrorRequest {
            repo: "r".into(),
            upstream: UP.into(),
            lines: vec![pb::MirrorLine {
                branch: "main".into(),
                line: "main".into(),
                direction: "follow".into(),
            }],
            interval_s: 0,
            enabled: true,
            export: None,
            expected_version: None,
        })
        .await
        .expect("row")
        .into_inner();
    let sealed = op
        .set_mirror_credential(pb::SetMirrorCredentialRequest {
            repo: "r".into(),
            upstream: UP.into(),
            credential: PAT.as_bytes().to_vec(),
            expected_version: row.version,
        })
        .await
        .expect("seal")
        .into_inner();
    assert!(sealed.credential_set);
    assert_eq!(sealed.kek_id.as_deref(), Some("k1"));
    assert_eq!(sealed.version, row.version + 1);

    // The operator who wrote it cannot read it back.
    let denied = op
        .get_mirror_credential(pb::GetMirrorCredentialRequest {
            repo: "r".into(),
            upstream: UP.into(),
            epoch: 1,
        })
        .await
        .expect_err("write-only");
    assert_eq!(denied.code(), Code::PermissionDenied);

    let mirror = site.mint_kind("mirror-svc", "mirror", &["repo:*:mirror"], 3600);
    let mut m = client(&server, &mirror).await;
    let lease = m
        .acquire_lease(pb::AcquireLeaseRequest {
            repo: "r".into(),
            instance: "i1".into(),
        })
        .await
        .expect("lease")
        .into_inner();
    let read = |epoch| pb::GetMirrorCredentialRequest {
        repo: "r".into(),
        upstream: UP.into(),
        epoch,
    };
    let got = m
        .get_mirror_credential(read(lease.epoch))
        .await
        .expect("holder reads")
        .into_inner();
    assert_eq!(got.credential, PAT.as_bytes());
    let stale = m
        .get_mirror_credential(read(lease.epoch + 1))
        .await
        .expect_err("stale epoch");
    assert_ne!(stale.code(), Code::Ok);
    assert!(!stale.message().contains("ghp_"));
    m.release_lease(pb::LeaseRequest {
        repo: "r".into(),
        epoch: lease.epoch,
    })
    .await
    .expect("release");
    server.stop();

    // Seed on the host from stdin, as from GRAFT_DEV_MIRROR_TOKEN_*.
    let out = site.admin_stdin(&["mirror", "credential", "r", UP], &format!("{SEEDED}\n"));
    assert!(
        out.status.success(),
        "seed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Rotate: k2 active, k1 retired; rewrap; then drop k1 entirely.
    set_keys(
        &site,
        &format!(
            "credential_kek = {{ id = \"k2\", file = \"{}\" }}\nretired_keks = [{{ id = \"k1\", file = \"{}\" }}]",
            k2.display(),
            k1.display()
        ),
    );
    let out = site.admin(&["mirror", "rewrap"]);
    assert!(
        out.status.success(),
        "rewrap: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("rewrapped 1 credential(s) under k2"));
    set_keys(
        &site,
        &format!(
            "credential_kek = {{ id = \"k2\", file = \"{}\" }}",
            k2.display()
        ),
    );
    let server = site.start_with_stderr(Stdio::from(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("log"),
    ));
    let mut m = client(&server, &mirror).await;
    let lease = m
        .acquire_lease(pb::AcquireLeaseRequest {
            repo: "r".into(),
            instance: "i2".into(),
        })
        .await
        .expect("re-take the released lease")
        .into_inner();
    let got = m
        .get_mirror_credential(read(lease.epoch))
        .await
        .expect("opens under k2 alone")
        .into_inner();
    assert_eq!(got.credential, SEEDED.as_bytes());
    assert_eq!(got.kek_id, "k2");
    server.stop();

    let text = std::fs::read_to_string(&log).expect("log");
    for secret in [PAT, SEEDED] {
        assert!(!text.contains(secret), "plaintext in the server log");
    }
}
