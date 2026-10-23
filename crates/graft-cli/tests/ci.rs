//! `graft ci exchange` (G3.8b) and the CI path it starts: the job's token
//! comes from the environment (a loopback stand-in for GitHub's OIDC
//! endpoint, or the Woodpecker variable), goes to a stand-in `CiService`,
//! and the graft token it answers lands in a 0600 file, never on standard
//! output. That file then drives `graft attest` against the testkit server
//! in front of the real engine, and an `attested` land succeeds (decision
//! D6). The server side of the exchange, with real verification against a
//! recorded JWKS, is `crates/graft-server/tests/ci.rs`.

mod support;

#[path = "../../graft-client/tests/support/engine.rs"]
mod engine;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
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
const AUDIENCE: &str = "https://graft.e2e.test";
/// GitHub's per-job bearer for the OIDC request.
const REQUEST_TOKEN: &str = "gh-request-bearer";

/// A `CiService` that accepts [`JWT`] only, and records what it was sent.
#[derive(Clone, Default)]
struct FakeCi {
    seen: Arc<Mutex<Vec<(String, bool)>>>,
}

#[tonic::async_trait]
impl CiService for FakeCi {
    async fn exchange_ci_token(
        &self,
        request: Request<pb::ExchangeCiTokenRequest>,
    ) -> Result<Response<pb::Token>, Status> {
        let versioned = request.metadata().get(VERSION_HEADER).is_some();
        let jwt = request.into_inner().ci_token;
        self.seen.lock().unwrap().push((jwt.clone(), versioned));
        if jwt != JWT {
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

/// A loopback stand-in for GitHub's OIDC token endpoint. Answers [`JWT`]
/// to a request bearing [`REQUEST_TOKEN`], 401 otherwise, and records each
/// request line.
fn github_oidc() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/_apis/token?api-version=2.0",
        listener.local_addr().unwrap()
    );
    let lines = Arc::new(Mutex::new(Vec::new()));
    let seen = lines.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut authorized = false;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                if line.trim().eq_ignore_ascii_case(&format!("authorization: Bearer {REQUEST_TOKEN}")) {
                    authorized = true;
                }
                line.clear();
            }
            seen.lock().unwrap().push(first.trim().to_owned());
            let (status, body) = if authorized {
                ("200 OK", json!({"count": 1, "value": JWT}).to_string())
            } else {
                ("401 Unauthorized", String::new())
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, lines)
}

fn exchange_cmd(dir: &Path, endpoint: &str) -> assert_cmd::Command {
    let mut cmd = graft(dir);
    cmd.env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .env_remove("GRAFT_CI_ID_TOKEN")
        .args(["--endpoint", endpoint, "ci", "exchange"]);
    cmd
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn github_exchange_writes_a_private_token_file_and_prints_no_token() {
    let s = stage();
    let (oidc_url, requests) = github_oidc();
    let dir = scratch();
    let out = dir.path().join("graft-token");
    let ran = exchange_cmd(dir.path(), &s.ci_url)
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", &oidc_url)
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", REQUEST_TOKEN)
        .args(["--provider", "github", "--audience", AUDIENCE, "--out"])
        .arg(&out)
        .assert()
        .success();
    let output = ran.get_output();
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains(GRAFT_TOKEN) && !text.contains(JWT), "{text}");
    }
    assert_eq!(std::fs::read_to_string(&out).unwrap(), format!("{GRAFT_TOKEN}\n"));
    #[cfg(unix)]
    assert_eq!(mode(&out), 0o600);

    // GitHub was asked for the deployment's audience, and the server got
    // GitHub's token with the protocol version.
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("api-version=2.0&audience=https%3A%2F%2Fgraft.e2e.test"),
        "{}",
        requests[0]
    );
    assert_eq!(*s.ci.seen.lock().unwrap(), [(JWT.to_owned(), true)]);
}

#[test]
fn woodpecker_exchange_reads_its_variable_and_json_names_the_file() {
    let s = stage();
    let dir = scratch();
    let out = dir.path().join("tok");
    let ran = exchange_cmd(dir.path(), &s.ci_url)
        .env("GRAFT_CI_ID_TOKEN", JWT)
        .args(["--json", "--provider", "woodpecker", "--out"])
        .arg(&out)
        .assert()
        .success();
    let value = json_out(ran.get_output());
    assert_valid("ci-exchange", &value);
    assert_eq!(
        value,
        json!({"provider": "woodpecker", "token_file": out.display().to_string()})
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), GRAFT_TOKEN);
}

#[test]
fn refusals_write_nothing() {
    let s = stage();
    let dir = scratch();
    let out = dir.path().join("tok");

    // The server refuses the CI token: exit 6, no file.
    exchange_cmd(dir.path(), &s.ci_url)
        .env("GRAFT_CI_ID_TOKEN", "not-a-good-token")
        .args(["--provider", "woodpecker", "--out"])
        .arg(&out)
        .assert()
        .code(6);
    // No token in the environment, or no audience for GitHub: usage errors.
    exchange_cmd(dir.path(), &s.ci_url)
        .args(["--provider", "woodpecker", "--out"])
        .arg(&out)
        .assert()
        .code(2);
    exchange_cmd(dir.path(), &s.ci_url)
        .args(["--provider", "github", "--audience", AUDIENCE, "--out"])
        .arg(&out)
        .assert()
        .code(2);
    exchange_cmd(dir.path(), &s.ci_url)
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", "http://127.0.0.1:9/x")
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", REQUEST_TOKEN)
        .args(["--provider", "github", "--out"])
        .arg(&out)
        .assert()
        .code(2);
    // GitHub's OIDC request over plain HTTP off loopback is refused.
    exchange_cmd(dir.path(), &s.ci_url)
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", "http://github.example/x")
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", REQUEST_TOKEN)
        .args(["--provider", "github", "--audience", AUDIENCE, "--out"])
        .arg(&out)
        .assert()
        .code(2);
    // GitHub refuses the request bearer: retryable, exit 7.
    let (oidc_url, _) = github_oidc();
    exchange_cmd(dir.path(), &s.ci_url)
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", &oidc_url)
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "wrong")
        .args(["--provider", "github", "--audience", AUDIENCE, "--out"])
        .arg(&out)
        .assert()
        .code(7);
    assert!(!out.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// The whole CI path of a draft: exchange, attest both D6 checks with the
/// token file, then the land succeeds.
#[test]
fn exchange_then_attest_then_the_attested_land_succeeds() {
    let s = stage();
    let files: Vec<(&str, &[u8])> = vec![("README.md", b"hello\n")];
    let repo = s.rt.block_on(EngineRepo::new("demo", &files));
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
    let draft = json_out(commit(&["--json", "commit", "-m", "mine"]).success().get_output());
    let draft = draft["id"].as_str().unwrap().to_owned();

    // Unattested, D6 refuses the land (exit 5).
    commit(&["land"]).code(5);

    let ci_dir = scratch();
    let token = ci_dir.path().join("graft-token");
    exchange_cmd(ci_dir.path(), &s.ci_url)
        .env("GRAFT_CI_ID_TOKEN", JWT)
        .args(["--provider", "woodpecker", "--out"])
        .arg(&token)
        .assert()
        .success();
    for check in ["ci/build", "ci/test"] {
        graft(ci_dir.path())
            .args(["--endpoint", &url, "--repo", "demo", "--token-file"])
            .arg(&token)
            .args(["attest", &draft, "--check", check, "--pass", "--url", "https://ci.example/run/1"])
            .assert()
            .success();
    }
    let landed = json_out(commit(&["--json", "land"]).success().get_output());
    assert_eq!(landed["outcome"], "landed", "{landed}");
}
