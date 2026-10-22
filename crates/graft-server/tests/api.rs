//! The server's edges through the binary: authentication and scopes,
//! protocol binding, the free-space floor, health, metrics, and the JSON
//! facade.

mod common;

use std::time::{Duration, Instant};

use common::{Auth, Clients, Site, SynthSpec, api, generate};
use graft_proto::pb;
use graft_proto::types::{RefKind, UpdateRef};
use graft_proto::{ApiError, PROTOCOL_VERSION};

fn resolve(name: &str) -> pb::ResolveRefRequest {
    pb::ResolveRefRequest {
        repo: "r".into(),
        name: name.into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_and_out_of_scope_calls_are_refused() {
    let site = Site::new("server-auth", "");
    let admin = site.mint(&["repo:r:admin"]);
    let reader = site.mint(&["repo:r:read"]);
    let other = site.mint(&["repo:other:admin"]);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");

    // No token, a malformed one, and one signed by another server's key.
    let foreign = Site::new("server-auth-foreign", "").mint(&["repo:r:admin"]);
    for token in [None, Some("v4.public.garbage"), Some(foreign.as_str())] {
        let mut c = server.clients(token).await;
        let e = c
            .repo
            .resolve_ref(resolve("main"))
            .await
            .expect_err("refused");
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
        assert_eq!(api(e), ApiError::Unauthenticated, "token {token:?}");
    }

    // No protocol version: refused naming the server's.
    let mut c = Clients::new(
        server.channel().await,
        Auth::new(Some(&admin)).without_version(),
    );
    let e = c
        .repo
        .resolve_ref(resolve("main"))
        .await
        .expect_err("refused");
    assert_eq!(
        api(e),
        ApiError::IncompatibleProtocol {
            server: PROTOCOL_VERSION.to_string()
        }
    );

    // A token for another repository learns nothing about this one.
    let mut c = server.clients(Some(&other)).await;
    let e = c
        .repo
        .resolve_ref(resolve("main"))
        .await
        .expect_err("refused");
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    assert!(matches!(api(e), ApiError::PermissionDenied { scope, .. } if scope == "repo:r:read"));

    // A reader cannot write, create repositories or mint broader tokens.
    let mut c = server.clients(Some(&reader)).await;
    let e = c
        .repo
        .resolve_ref(resolve("main"))
        .await
        .expect_err("no ref yet");
    assert!(matches!(api(e), ApiError::NotFound { .. }));
    let e = c
        .put_blobs("r", &[b"hello".to_vec()])
        .await
        .expect_err("refused");
    assert!(matches!(e, ApiError::PermissionDenied { .. }), "{e:?}");
    let e = c
        .commit(
            "r",
            None,
            &Default::default(),
            &[("a".to_string(), b"x".to_vec())].into(),
            0,
        )
        .await
        .expect_err("refused");
    assert!(matches!(e, ApiError::PermissionDenied { .. }), "{e:?}");
    let e = c.create_repo("r2").await.expect_err("refused");
    assert!(matches!(e, ApiError::PermissionDenied { .. }), "{e:?}");
    let e = c
        .admin
        .mint_token(pb::MintTokenRequest {
            subject: "x".into(),
            scopes: vec!["repo:r:write".into()],
            ttl_s: 60,
            session: None,
            work_item: None,
        })
        .await
        .expect_err("refused");
    assert!(matches!(api(e), ApiError::PermissionDenied { .. }));

    // MintToken within the caller's scopes, for itself, works, and the
    // token is usable.
    let mut c = server.clients(Some(&admin)).await;
    let minted = c
        .admin
        .mint_token(pb::MintTokenRequest {
            subject: String::new(),
            scopes: vec!["repo:r:read".into()],
            ttl_s: 60,
            session: None,
            work_item: None,
        })
        .await
        .expect("mint")
        .into_inner()
        .token;
    let mut c = server.clients(Some(&minted)).await;
    let e = c
        .repo
        .resolve_ref(resolve("main"))
        .await
        .expect_err("no ref");
    assert!(matches!(api(e), ApiError::NotFound { .. }));

    // The JSON facade refuses the same way.
    let r = server.get("/v1/repos/r/refs", None);
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    assert_eq!(r.json()["error"]["kind"], "unauthenticated");
    let r = server.get("/v1/repos/r/refs", Some(&other));
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["error"]["kind"], "permission_denied");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_metrics_and_the_json_facade() {
    let site = Site::new("server-http", "metrics = \"127.0.0.1:0\"");
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    assert_eq!(server.get("/healthz", None).status, 200);
    assert_eq!(server.get("/readyz", None).text(), "ready\n");

    let synth = generate(&SynthSpec {
        files: 12,
        depth: 2,
        history: 3,
        file_bytes: 200,
        seed: 5,
    });
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");
    let created = c.import("r", &synth.snapshots).await;
    let tip = created.last().expect("tip").id;
    c.repo
        .update_ref(pb::UpdateRefRequest::from(UpdateRef {
            repo: "r".into(),
            name: "main".into(),
            expected: None,
            new: tip,
            kind: RefKind::Line,
        }))
        .await
        .expect("move main");

    let refs = server.get("/v1/repos/r/refs", Some(&token)).json();
    assert_eq!(refs["refs"][0]["name"], "main");
    assert_eq!(refs["refs"][0]["target"], tip.to_string());
    assert_eq!(refs["refs"][0]["kind"], "line");
    let one = server.get("/v1/repos/r/refs/main", Some(&token)).json();
    assert_eq!(one["version"], 1);

    let cs = server
        .get(&format!("/v1/repos/r/changesets/{tip}"), Some(&token))
        .json();
    assert_eq!(cs["id"], tip.to_string());
    assert_eq!(cs["parents"][0], created[created.len() - 2].id.to_string());
    assert_eq!(cs["message"], "step 2");

    let last = synth.snapshots.last().expect("tip");
    let (path, bytes) = last.iter().next().expect("a file");
    let file = server.get(&format!("/v1/repos/r/file/main?path={path}"), Some(&token));
    assert_eq!(file.status, 200);
    assert_eq!(&file.body, bytes);
    assert_eq!(file.header("graft-kind"), Some("file"));

    let tree = server
        .get(
            "/v1/repos/r/tree/main?recursive=true&limit=1000",
            Some(&token),
        )
        .json();
    let files: Vec<&str> = tree["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|e| e["kind"] == "file")
        .map(|e| e["path"].as_str().expect("path"))
        .collect();
    assert_eq!(files.len(), last.len());

    let stat = server
        .get(&format!("/v1/repos/r/stat/{tip}?path={path}"), Some(&token))
        .json();
    assert_eq!(stat["kind"], "file");

    let first = created[0].id;
    let diff = server
        .get(
            &format!("/v1/repos/r/diff?from={first}&to=main&patch=true"),
            Some(&token),
        )
        .json();
    assert!(!diff["files"].as_array().expect("files").is_empty());

    let journal = server
        .get("/v1/repos/r/journal?from_lsn=1", Some(&token))
        .json();
    assert_eq!(journal["entries"][0]["kind"], "ChangesetCreated");
    assert_eq!(journal["entries"][0]["body"]["id"], first.to_string());

    // Log, paged by token.
    let log = server.get("/v1/repos/r/log?from=main&limit=2", Some(&token));
    assert_eq!(log.status, 200, "{}", log.text());
    let log = log.json();
    let subjects: Vec<&str> = log["changesets"]
        .as_array()
        .expect("changesets")
        .iter()
        .map(|c| c["subject"].as_str().expect("subject"))
        .collect();
    assert_eq!(subjects, ["step 2", "step 1"]);
    let next = log["next"].as_str().expect("a next page");
    assert_eq!(next, first.to_string());
    let rest = server
        .get(
            &format!("/v1/repos/r/log?from=main&limit=2&page_token={next}"),
            Some(&token),
        )
        .json();
    assert_eq!(rest["changesets"][0]["subject"], "step 0");
    assert!(rest["next"].is_null());

    // Blame covers the file with origins from its history.
    let blame = server.get(&format!("/v1/repos/r/blame/main?path={path}"), Some(&token));
    assert_eq!(blame.status, 200, "{}", blame.text());
    let ids: Vec<String> = created.iter().map(|c| c.id.to_string()).collect();
    let ranges = blame.json()["ranges"].as_array().expect("ranges").clone();
    assert!(!ranges.is_empty());
    assert!(
        ranges
            .iter()
            .all(|r| ids.contains(&r["origin"]["id"].as_str().expect("id").to_owned()))
    );
    let missing_file = server.get("/v1/repos/r/blame/main?path=no/such/file", Some(&token));
    assert_eq!(missing_file.status, 404, "{}", missing_file.text());

    // The same over gRPC, with ancestry.
    let history = c
        .repo
        .log(pb::LogRequest {
            repo: "r".into(),
            from: tip.0.0.to_vec(),
            until: None,
            paths: Vec::new(),
            page: None,
        })
        .await
        .expect("log")
        .into_inner();
    assert_eq!(history.changesets.len(), created.len());
    let pair = pb::Pair {
        repo: "r".into(),
        a: first.0.0.to_vec(),
        b: tip.0.0.to_vec(),
    };
    assert!(
        c.repo
            .is_ancestor(pair.clone())
            .await
            .expect("ancestry")
            .into_inner()
            .value
    );
    let base = c
        .repo
        .merge_base(pair)
        .await
        .expect("merge base")
        .into_inner();
    assert_eq!(base.id, Some(first.0.0.to_vec()));

    // Sync (G2.7) is built: a workspace that does not exist is NotFound.
    let e = c
        .workspaces
        .sync(pb::SyncRequest {
            workspace: Some(pb::WorkspaceRef {
                repo: "r".into(),
                name: "w".into(),
            }),
            onto: None,
        })
        .await
        .expect_err("no workspace");
    assert_eq!(e.code(), tonic::Code::NotFound);

    // An operation the engine has not built yet (Revert, G3.7) answers
    // Unimplemented, not a crash, and the server still serves afterwards.
    let e = c
        .repo
        .revert(pb::RevertRequest {
            repo: "r".into(),
            changeset: first.0.0.to_vec(),
            onto: first.0.0.to_vec(),
            workspace: None,
        })
        .await
        .expect_err("unimplemented");
    assert_eq!(e.code(), tonic::Code::Unimplemented);
    assert_eq!(server.get("/healthz", None).status, 200);

    let bad = server.get("/v1/repos/r/file/main?path=../x", Some(&token));
    assert_eq!(bad.status, 400);
    let missing = server.get("/v1/repos/r/refs/nope", Some(&token));
    assert_eq!(missing.status, 404);

    let metrics = server.get("/metrics", None).text();
    assert!(
        metrics.contains("graft_rpc_duration_seconds_count{op=\"CreateChangeset\",outcome=\"ok\",tier=\"server\"}"),
        "{metrics}"
    );
    let side = common::http_get(server.metrics.expect("metrics listener"), "/metrics", None);
    assert_eq!(side.status, 200);
    assert!(side.text().contains("graft_rpc_duration_seconds"));
    let side = common::http_get(server.metrics.expect("metrics listener"), "/readyz", None);
    assert_eq!(side.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_the_free_space_floor_writes_are_refused_and_reads_continue() {
    let site = Site::new("server-full", "min_free_fraction = 1.0");
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    let e = c.create_repo("r").await.expect_err("refused");
    assert!(matches!(e, ApiError::Unavailable { .. }), "{e:?}");
    let e = c
        .repo
        .resolve_ref(resolve("main"))
        .await
        .expect_err("no repo");
    assert!(matches!(api(e), ApiError::NotFound { .. }));
    let r = server.get("/v1/repos/r/refs", Some(&token));
    assert_eq!(r.status, 404);
}

#[test]
fn graceful_shutdown_drains_and_readiness_drops() {
    let site = Site::new("server-stop", "");
    let server = site.start();
    let addr = server.addr;
    let started = Instant::now();
    let status = server.stop();
    assert!(status.success(), "{status:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "still listening"
    );
}

/// A SIGTERM during start-up is a clean stop: exit 0, no `listening` line,
/// and no listener left bound. The server is held in start-up by the test
/// hook (`GRAFT_TEST_HOLD_START_UP`) until it has been signalled, so the
/// signal always lands before readiness. Every wait has a deadline: a server
/// without the hook fails the test rather than hanging it.
#[test]
fn sigterm_during_start_up_exits_clean_with_nothing_bound() {
    const WAIT: Duration = Duration::from_secs(20);
    let site = Site::new("server-early-stop", "");
    // Give the server a port the test knows, to check that nothing is left bound.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe port")
        .local_addr()
        .expect("probe address")
        .port();
    let text = std::fs::read_to_string(&site.config).expect("read config");
    std::fs::write(
        &site.config,
        text.replace("127.0.0.1:0", &format!("127.0.0.1:{port}")),
    )
    .expect("write config");

    let mut child = std::process::Command::new(common::bin())
        .arg("--config")
        .arg(&site.config)
        .arg("serve")
        .env("GRAFT_TEST_HOLD_START_UP", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let stdout = child.stdout.take().expect("stdout");
    // A reader thread turns stdout into lines; its channel closes at EOF,
    // which is when the server has exited.
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Wait for the server to hold in start-up before signalling it.
    loop {
        match lines.recv_timeout(WAIT) {
            Ok(line) if line == "graft-server holding start-up" => break,
            Ok(_) => {}
            Err(_) => abort(&mut child, "start-up never paused for the test hook"),
        }
    }
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill -TERM");
    assert!(killed.success());

    // Read until the server closes stdout, then reap it.
    let mut out = Vec::new();
    loop {
        match lines.recv_timeout(WAIT) {
            Ok(line) => out.push(line),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                abort(&mut child, "server did not stop")
            }
        }
    }
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(
        !out.iter().any(|line| line.contains("listening")),
        "start-up went on past the signal: {out:?}"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "still listening"
    );
}

/// Kills a server the test cannot wait for, then fails the test.
fn abort(child: &mut std::process::Child, why: &str) -> ! {
    let _ = child.kill();
    let _ = child.wait();
    panic!("{why}");
}

#[test]
fn admin_commands_and_their_exit_codes() {
    let site = Site::new("server-admin", "");
    let out = site.admin(&["repo", "create", "r"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Idempotent.
    assert!(site.admin(&["repo", "create", "r"]).status.success());
    let out = site.admin(&["repo", "create", "bad/name"]);
    assert_eq!(out.status.code(), Some(1));
    let out = site.admin(&["fsck", "r"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(site.admin(&["fsck", "nope"]).status.code(), Some(1));
    assert_eq!(site.admin(&["gc", "r"]).status.code(), Some(2));
    assert_eq!(site.admin(&["bogus"]).status.code(), Some(64));
    assert_eq!(
        site.admin(&["token", "mint", "--subject", "x"])
            .status
            .code(),
        Some(64)
    );
    // A token is printed once, to stdout, and never echoed to stderr.
    let out = site.admin(&["token", "mint", "--subject", "x", "--scope", "repo:r:read"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("v4.public."));
    assert!(out.stderr.is_empty());

    // A key file others can read is refused by mint and by serve.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let key = site.dir.path().join("meta/signing.key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let out = site.admin(&["token", "mint", "--subject", "x", "--scope", "repo:r:read"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("chmod 600"), "{stderr}");
        let out = std::process::Command::new(common::bin())
            .arg("--config")
            .arg(&site.config)
            .arg("serve")
            .output()
            .expect("run");
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("chmod 600"));
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    // The repository made offline is served.
    let token = stdout.trim().to_owned();
    let server = site.start();
    let r = server.get("/v1/repos/r/refs", Some(&token));
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["refs"], serde_json::json!([]));
    drop(server);

    let out = std::process::Command::new(common::bin())
        .args(["--config", "/nonexistent/graft.toml", "serve"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(78));
}
