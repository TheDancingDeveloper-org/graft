//! `graft ci exchange --provider github|woodpecker --out FILE`: trades the CI
//! job's identity token for a short graft token (`docs/design.md` §12.1,
//! work item G3.8b) and writes it to `FILE`, mode 0600. Later steps pass it
//! to `graft` with `--token-file FILE`.
//!
//! Neither token is ever printed or put in an argument. The CI token comes
//! from the environment:
//!
//! - **GitHub Actions**: the job asks GitHub for an OIDC token with
//!   `ACTIONS_ID_TOKEN_REQUEST_URL` and `ACTIONS_ID_TOKEN_REQUEST_TOKEN`
//!   (present when the job has `permissions: id-token: write`), for the
//!   `--audience` the graft server's issuer rule names.
//! - **Woodpecker**: the pipeline maps its job token into
//!   [`WOODPECKER_TOKEN_ENV`].
//!
//! The server is `--endpoint` or `GRAFT_ENDPOINT`. The exchange needs no
//! repository and no bearer token: the CI token is the credential.

use std::env;
use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use graft_client::{Endpoint, discover};
use graft_proto::pb::ci_service_client::CiServiceClient;
use graft_proto::{ApiError, PROTOCOL_VERSION, ProtocolVersion, VERSION_HEADER, pb};
use serde_json::json;
use tonic::metadata::MetadataValue;

use crate::Exit;
use crate::cli::{CiAction, CiArgs, CiExchangeArgs, CiProvider, Global};
use crate::conn::runtime;
use crate::error::Failure;
use crate::render::print_json;

/// GitHub's OIDC request URL, set in a job that may ask for a token.
pub(crate) const GITHUB_URL_ENV: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";
/// The bearer token for that request.
pub(crate) const GITHUB_TOKEN_ENV: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";
/// Where a Woodpecker pipeline puts its job token for `graft`.
pub(crate) const WOODPECKER_TOKEN_ENV: &str = "GRAFT_CI_ID_TOKEN";

/// Longest OIDC answer read from GitHub.
const MAX_OIDC_ANSWER: usize = 64 * 1024;
/// How long each of the two requests may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Runs a `graft ci` command.
pub(crate) fn run(g: &Global, args: &CiArgs) -> Result<(), Failure> {
    match &args.action {
        CiAction::Exchange(a) => exchange(g, a),
    }
}

fn exchange(g: &Global, args: &CiExchangeArgs) -> Result<(), Failure> {
    let endpoint = match &g.endpoint {
        Some(text) => Endpoint::parse(text),
        None => discover(None).ok_or_else(|| {
            Failure::usage("no endpoint: pass --endpoint or set GRAFT_ENDPOINT")
        })?,
    };
    let Endpoint::Server(url) = endpoint else {
        return Err(Failure::usage(
            "graft ci exchange talks to the server, not the node socket: pass --endpoint",
        ));
    };
    runtime()?.block_on(async {
        let jwt = match args.provider {
            CiProvider::Github => {
                let audience = args.audience.as_deref().ok_or_else(|| {
                    Failure::usage("--provider github needs --audience (the graft deployment's)")
                })?;
                github_token(audience).await?
            }
            CiProvider::Woodpecker => env_token(WOODPECKER_TOKEN_ENV)?,
        };
        let token = call_exchange(&url, jwt).await?;
        write_private(&args.out, &token).map_err(|e| {
            Failure::new(
                Exit::Failure,
                "token",
                format!("cannot write {}: {}", args.out.display(), e.kind()),
            )
        })?;
        if g.json {
            return print_json(&json!({
                "provider": args.provider.name(),
                "token_file": args.out.display().to_string(),
            }));
        }
        outln!("wrote a graft token to {}", args.out.display());
        Ok(())
    })
}

/// A non-empty environment variable, or a usage error naming it.
fn env_token(name: &str) -> Result<String, Failure> {
    env::var(name)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Failure::usage(format!("{name} is not set")))
}

/// Asks GitHub for the job's OIDC token for `audience`.
async fn github_token(audience: &str) -> Result<String, Failure> {
    let missing = |name: &str| {
        Failure::usage(format!(
            "{name} is not set: run inside a GitHub Actions job with `permissions: id-token: write`"
        ))
    };
    let url = env::var(GITHUB_URL_ENV).map_err(|_| missing(GITHUB_URL_ENV))?;
    let bearer = env::var(GITHUB_TOKEN_ENV).map_err(|_| missing(GITHUB_TOKEN_ENV))?;
    let mut url = reqwest::Url::parse(&url)
        .map_err(|_| Failure::usage(format!("{GITHUB_URL_ENV} is not a URL")))?;
    if !secure_url(&url) {
        return Err(Failure::usage(format!(
            "{GITHUB_URL_ENV} must be https:// (http:// only on loopback)"
        )));
    }
    url.query_pairs_mut().append_pair("audience", audience);
    let unavailable = |what: &str| {
        Failure::new(
            Exit::Unavailable,
            "ci_token",
            format!("could not get the job's OIDC token: {what}"),
        )
    };
    let http = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| unavailable("no HTTP client"))?;
    let mut resp = http
        .get(url)
        .bearer_auth(bearer.trim())
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| unavailable(&e.without_url().to_string()))?;
    if !resp.status().is_success() {
        return Err(unavailable(&format!("status {}", resp.status().as_u16())));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| unavailable(&e.without_url().to_string()))?
    {
        if body.len() + chunk.len() > MAX_OIDC_ANSWER {
            return Err(unavailable("answer too large"));
        }
        body.extend_from_slice(&chunk);
    }
    #[derive(serde::Deserialize)]
    struct Answer {
        value: String,
    }
    let answer: Answer =
        serde_json::from_slice(&body).map_err(|_| unavailable("answer has no token"))?;
    Ok(answer.value)
}

/// TLS, except to a loopback address (tests, a local proxy).
fn secure_url(url: &reqwest::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => match url.host_str() {
            Some("localhost") => true,
            Some(host) => host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()),
            None => false,
        },
        _ => false,
    }
}

/// Calls `ExchangeCiToken` on the server at `url`.
async fn call_exchange(url: &str, jwt: String) -> Result<String, Failure> {
    let unavailable = || {
        Failure::from(ApiError::Unavailable {
            retry_after_ms: 1_000,
        })
    };
    let channel = tonic::transport::Endpoint::from_shared(url.to_owned())
        .map_err(|_| Failure::usage("not a valid server URL"))?
        .connect_timeout(graft_client::remote::CONNECT_TIMEOUT)
        .connect()
        .await
        .map_err(|_| unavailable())?;
    let mut req = tonic::Request::new(pb::ExchangeCiTokenRequest { ci_token: jwt });
    req.metadata_mut().insert(
        VERSION_HEADER,
        MetadataValue::try_from(PROTOCOL_VERSION.to_string()).expect("version is ASCII"),
    );
    req.set_timeout(TIMEOUT);
    let resp = CiServiceClient::new(channel)
        .exchange_ci_token(req)
        .await
        .map_err(status_failure)?;
    check_version(resp.metadata().get(VERSION_HEADER).and_then(|v| v.to_str().ok()))?;
    let token = resp.into_inner().token;
    if token.is_empty() {
        return Err(Failure::from(ApiError::Unauthenticated));
    }
    Ok(token)
}

/// Refuses an answer from a server whose version this client can't bind to.
fn check_version(header: Option<&str>) -> Result<(), Failure> {
    match header.map(str::parse::<ProtocolVersion>) {
        Some(Ok(server)) if PROTOCOL_VERSION.binds_to(server) => Ok(()),
        _ => Err(Failure::from(ApiError::IncompatibleProtocol {
            server: header.unwrap_or("none").chars().take(16).collect(),
        })),
    }
}

/// The typed error a status carries, else the closest by code.
fn status_failure(s: tonic::Status) -> Failure {
    let e = ApiError::from_status(&s).unwrap_or_else(|| match s.code() {
        tonic::Code::Unauthenticated => ApiError::Unauthenticated,
        tonic::Code::PermissionDenied => ApiError::PermissionDenied {
            scope: "ci:exchange".into(),
            path: None,
        },
        tonic::Code::Unimplemented => ApiError::IncompatibleProtocol {
            server: "no CI exchange".into(),
        },
        _ => ApiError::Unavailable {
            retry_after_ms: 1_000,
        },
    });
    Failure::from(e)
}

/// Writes `token` to `path`, readable by the owner only. The token goes to a
/// new file beside `path` first and is renamed over it, so `path` never
/// holds a partial token, nor one readable by others.
fn write_private(path: &Path, token: &str) -> io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_only_to_loopback() {
        let ok = |u: &str| secure_url(&reqwest::Url::parse(u).unwrap());
        assert!(ok("https://pipelines.actions.githubusercontent.com/x?api-version=2.0"));
        assert!(ok("http://127.0.0.1:9/token"));
        assert!(ok("http://localhost/token"));
        assert!(ok("http://[::1]/token"));
        assert!(!ok("http://example.com/token"));
        assert!(!ok("http://127.0.0.1.example.com/token"));
        assert!(!ok("file:///etc/passwd"));
    }

    #[test]
    fn the_token_file_is_private_and_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("graft-token");
        fs::write(&file, "old").unwrap();
        write_private(&file, "v4.public.new").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v4.public.new\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Nothing left beside it.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn versions_are_bound() {
        assert!(check_version(Some(&PROTOCOL_VERSION.to_string())).is_ok());
        assert!(check_version(None).is_err());
        assert!(check_version(Some("9.0")).is_err());
    }
}
