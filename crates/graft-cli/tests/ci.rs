//! `graft ci exchange` (G3.8b) and the CI path it starts: the job's token
//! comes in on standard input, goes to a stand-in `CiService` with the
//! changeset it is pinned to, and the graft token it answers lands in a 0600
//! file, never on standard output. That file then drives `graft attest` against the testkit server
//! in front of the real engine, and an `attested` land succeeds (decision
//! D6). The server side of the exchange, with real verification against a
//! recorded JWKS, is `crates/graft-server/tests/ci.rs`.

mod support;

#[path = "../../graft-client/tests/support/engine.rs"]
mod engine;

use std::path::Path;
use std::sync::{Arc, Mutex};

use engine::EngineRepo;
use graft_proto::pb::ci_service_server::{CiService, CiServiceServer};
use graft_proto::{ApiError, PROTOCOL_VERSION, VERSION_HEADER, pb};
use graft_testkit::server::TestServer;
use serde_json::json;
use support::{assert_valid, graft, json_out, scratch};
use tokio::runtime::Runtime;
use tonic::{Request, Response, Status};

/// The CI job's token, as GitHub or Woodpecker would hand it over.
const JWT: &str = "eyJhbGciOiJSUzI1NiJ9.eyJyZXBvc2l0b3J5Ijoiby9hcHAifQ.c2ln";
/// What the stand-in server mints for it.
const GRAFT_TOKEN: &str = "v4.public.ci-token-for-tests";
/// A changeset id to pin to.
const CHANGESET: &str = "c0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// What the stand-in server was sent: the CI token, the pin, and whether
/// the protocol version came with them.
type Seen = (String, Option<String>, bool);

/// A `CiService` that accepts [`JWT`] only, and records what it was sent.
#[derive(Clone, Default)]
struct FakeCi {
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[tonic::async_trait]
impl CiService for FakeCi {
    async fn exchange_ci_token(
        &self,
        request: Request<pb::ExchangeCiTokenRequest>,
    ) -> Result<Response<pb::Token>, Status> {
        let versioned = request.metadata().get(VERSION_HEADER).is_some();
        let req = request.into_inner();
        self.seen
            .lock()
            .unwrap()
            .push((req.ci_token.clone(), req.changeset, versioned));
        if req.ci_token != JWT {
            return Err(ApiError::Unauthenticated.into());
        }
        let mut resp = Response::new(pb::Token {
            token: GRAFT_TOKEN.into(),
        });
        resp.metadata_mut().insert(
            VERSION_HEADER,
            PROTOCOL_VERSION.to_string().parse().unwrap(),
        );
        Ok(resp)
    }
}

struct Stage {
    rt: Runtime,
    ci: FakeCi,
    ci_url: String,
}

fn stage() -> Stage {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ci = FakeCi::default();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let ci_url = format!("http://{}", listener.local_addr().unwrap());
    let service = CiServiceServer::new(ci.clone());
    rt.spawn(
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    Stage { rt, ci, ci_url }
}

fn exchange_cmd(dir: &Path, endpoint: &str) -> assert_cmd::Command {
    let mut cmd = graft(dir);
    cmd.args(["--experimental", "--endpoint", endpoint, "ci", "exchange"]);
    cmd
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn exchange_pins_reads_stdin_and_writes_a_private_token_file() {
    let s = stage();
    let dir = scratch();
    let out = dir.path().join("graft-token");
    let ran = exchange_cmd(dir.path(), &s.ci_url)
        .args(["--provider", "github", "--changeset", CHANGESET, "--out"])
        .arg(&out)
        .write_stdin(format!("{JWT}\n"))
        .assert()
        .success();
    let output = ran.get_output();
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains(GRAFT_TOKEN) && !text.contains(JWT), "{text}");
    }
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!("{GRAFT_TOKEN}\n")
    );
    #[cfg(unix)]
    assert_eq!(mode(&out), 0o600);
    assert_eq!(
        *s.ci.seen.lock().unwrap(),
        [(JWT.to_owned(), Some(CHANGESET.to_owned()), true)]
    );

    // A file that exists (the workflows `mktemp` it) is replaced whole.
    let ran = exchange_cmd(dir.path(), &s.ci_url)
        .args(["--json", "--provider", "woodpecker", "--out"])
        .arg(&out)
        .write_stdin(JWT)
        .assert()
        .success();
    let value = json_out(ran.get_output());
    assert_valid("ci-exchange", &value);
    assert_eq!(
        value,
        json!({"provider": "woodpecker", "changeset": null, "token_file": out.display().to_string()})
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), GRAFT_TOKEN);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn refusals_write_nothing() {
    let s = stage();
    let dir = scratch();
    let out = dir.path().join("tok");
    let run = |stdin: &str, extra: &[&str]| {
        exchange_cmd(dir.path(), &s.ci_url)
            .args(["--provider", "github"])
            .args(extra)
            .arg("--out")
            .arg(&out)
            .write_stdin(stdin.to_owned())
            .assert()
    };
    // The server refuses the CI token: exit 6.
    run("not-a-good-token", &[]).code(6);
    // Nothing on standard input, a pin that is no changeset id, a token
    // longer than the server would read: usage errors, the server not asked.
    run("", &[]).code(2);
    run(JWT, &["--changeset", "main"]).code(2);
    run(&"x".repeat(16 * 1024 + 1), &[]).code(2);
    assert_eq!(s.ci.seen.lock().unwrap().len(), 1);
    assert!(!out.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// The whole CI path of a draft: exchange, attest both D6 checks with the
/// token file, then the land succeeds.
#[test]
fn exchange_then_attest_then_the_attested_land_succeeds() {
    let s = stage();
    let files: Vec<(&str, &[u8])> = vec![("README.md", b"hello\n")];
    let repo = s.rt.block_on(EngineRepo::d6("demo", &files));
    let server = s.rt.block_on(TestServer::tcp_workspaces(repo.clone()));
    let url = server.url().to_owned();

    let dir = scratch();
    graft(dir.path())
        .args(["--endpoint", &url, "clone", "demo", "app"])
        .assert()
        .success();
    let app = dir.path().join("app");
    std::fs::write(app.join("README.md"), "mine\n").unwrap();
    let commit = |args: &[&str]| {
        let mut cmd = graft(&app);
        cmd.env("GRAFT_AUTHOR_NAME", "Test Author")
            .env("GRAFT_AUTHOR_EMAIL", "author@example.com")
            .env("GRAFT_AUTHOR_KIND", "agent");
        cmd.args(args).assert()
    };
    let draft = json_out(
        commit(&["--json", "commit", "-m", "mine"])
            .success()
            .get_output(),
    );
    let draft = draft["id"].as_str().unwrap().to_owned();

    // Unattested, D6 refuses the land (exit 5).
    commit(&["land"]).code(5);

    let ci_dir = scratch();
    let token = ci_dir.path().join("graft-token");
    exchange_cmd(ci_dir.path(), &s.ci_url)
        .args(["--provider", "github", "--changeset", &draft, "--out"])
        .arg(&token)
        .write_stdin(JWT)
        .assert()
        .success();
    assert_eq!(
        s.ci.seen.lock().unwrap()[0].1.as_deref(),
        Some(draft.as_str())
    );
    for check in ["ci/build", "ci/test"] {
        graft(ci_dir.path())
            .args([
                "--experimental",
                "--endpoint",
                &url,
                "--repo",
                "demo",
                "--token-file",
            ])
            .arg(&token)
            .args([
                "attest",
                &draft,
                "--check",
                check,
                "--pass",
                "--url",
                "https://ci.example/run/1",
            ])
            .assert()
            .success();
    }
    let landed = json_out(commit(&["--json", "land"]).success().get_output());
    assert_eq!(landed["outcome"], "landed", "{landed}");
}
