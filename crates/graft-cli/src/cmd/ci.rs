//! `graft ci exchange --provider github|woodpecker [--changeset ID] --out FILE`:
//! trades the CI job's identity token for a short graft token
//! (`docs/design.md` §12.1, work item G3.8b) and writes it to `FILE`, mode
//! 0600. Later steps pass it to `graft` with `--token-file FILE`.
//!
//! The CI token (a JWT: GitHub's OIDC token, or a Woodpecker job token) is
//! read from standard input, so it is never an argument, and the graft token
//! is never printed. `--changeset` pins the token to the changeset the job
//! attests: a binding that can attest mints only pinned tokens, and only
//! when the job ran on the commit exported for that changeset.
//!
//! The server is `--endpoint` or `GRAFT_ENDPOINT`. The exchange needs no
//! repository and no bearer token: the CI token is the credential.

use std::fs;
use std::io::{self, IsTerminal as _, Read as _, Write as _};
use std::path::Path;
use std::time::Duration;

use graft_client::{Endpoint, discover};
use graft_model::ChangesetId;
use graft_proto::pb::ci_service_client::CiServiceClient;
use graft_proto::{ApiError, PROTOCOL_VERSION, ProtocolVersion, VERSION_HEADER, pb};
use serde_json::json;
use tonic::metadata::MetadataValue;

use crate::Exit;
use crate::cli::{CiAction, CiArgs, CiExchangeArgs, Global};
use crate::conn::runtime;
use crate::error::Failure;
use crate::render::print_json;

/// Longest CI token read from standard input: the server looks at no more
/// than 16 KiB.
const MAX_CI_TOKEN: u64 = 16 * 1024;
/// How long the exchange may take.
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
        None => discover(None)
            .ok_or_else(|| Failure::usage("no endpoint: pass --endpoint or set GRAFT_ENDPOINT"))?,
    };
    let Endpoint::Server(url) = endpoint else {
        return Err(Failure::usage(
            "graft ci exchange talks to the server, not the node socket: pass --endpoint",
        ));
    };
    if let Some(cs) = &args.changeset {
        ChangesetId::parse(cs)
            .map_err(|_| Failure::usage(format!("--changeset {cs:?} is not a changeset id")))?;
    }
    let jwt = read_ci_token()?;
    runtime()?.block_on(async {
        let token = call_exchange(&url, jwt, args.changeset.clone()).await?;
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
                "changeset": args.changeset,
                "token_file": args.out.display().to_string(),
            }));
        }
        outln!("wrote a graft token to {}", args.out.display());
        Ok(())
    })
}

/// The CI token from standard input, which must not be a terminal.
fn read_ci_token() -> Result<String, Failure> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Err(Failure::usage(
            "pipe the CI job's token to standard input (it is never taken from an argument)",
        ));
    }
    let mut text = String::new();
    stdin
        .lock()
        .take(MAX_CI_TOKEN + 1)
        .read_to_string(&mut text)
        .map_err(|_| Failure::usage("the CI token on standard input is not text"))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(Failure::usage("no CI token on standard input"));
    }
    if token.len() as u64 > MAX_CI_TOKEN {
        return Err(Failure::usage("the CI token on standard input is too long"));
    }
    Ok(token.to_owned())
}

/// Calls `ExchangeCiToken` on the server at `url`.
async fn call_exchange(
    url: &str,
    jwt: String,
    changeset: Option<String>,
) -> Result<String, Failure> {
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
    let mut req = tonic::Request::new(pb::ExchangeCiTokenRequest {
        ci_token: jwt,
        changeset,
    });
    req.metadata_mut().insert(
        VERSION_HEADER,
        MetadataValue::try_from(PROTOCOL_VERSION.to_string()).expect("version is ASCII"),
    );
    req.set_timeout(TIMEOUT);
    let resp = CiServiceClient::new(channel)
        .exchange_ci_token(req)
        .await
        .map_err(status_failure)?;
    check_version(
        resp.metadata()
            .get(VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
    )?;
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
