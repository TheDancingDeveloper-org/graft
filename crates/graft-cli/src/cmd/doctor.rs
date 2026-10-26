//! `graft doctor`: config, server, endpoint, protocol and token checks.
//!
//! The endpoint probe calls `resolve_ref` with no token, so an expired
//! token does not hide a server that is down. Claims are peeked only to
//! show expiry and scopes. The token itself is never printed.

use std::time::{SystemTime, UNIX_EPOCH};

use graft_auth::{Claims, Token};
use graft_client::{Endpoint, Remote};
use graft_proto::PROTOCOL_VERSION;
use graft_proto::api::RepoApi;
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use graft_proto::types::ResolveRef;
use serde_json::{Value, json};

use crate::cli::{DEFAULT_LINE, Global};
use crate::config::{self, UserConfig};
use crate::conn::{self, WcView, resolve_connection_from_sources};
use crate::error::Failure;
use crate::render;

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

enum Status {
    Ok,
    Fail,
    Skip,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }

    fn json_status(&self) -> &'static str {
        match self {
            Self::Ok => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }

    fn passed(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Runs the checks. Exit 0 only when every check passes.
pub(crate) fn run(g: &Global) -> Result<(), Failure> {
    let mut checks = Vec::new();
    let Some(config) = config_check(&mut checks) else {
        return finish(g, checks);
    };
    let wc = conn::working_copy_info()?;
    let Some(resolved) = server_check(g, wc.as_ref(), &config, &mut checks) else {
        skip(&mut checks, "endpoint", "no server");
        skip(&mut checks, "protocol", "no server");
        skip(&mut checks, "token", "no server");
        return finish(g, checks);
    };
    if !endpoint_check(&resolved, &mut checks) {
        skip(&mut checks, "protocol", "endpoint unreachable");
        skip(&mut checks, "token", "endpoint unreachable");
        return finish(g, checks);
    }
    token_check(g, wc.as_ref(), &config, &resolved.server, &mut checks)?;
    finish(g, checks)
}

fn config_check(checks: &mut Vec<Check>) -> Option<UserConfig> {
    let path = match config::config_path() {
        Ok(path) => path,
        Err(failure) => {
            push(checks, "config", Status::Fail, failure.to_string());
            return None;
        }
    };
    if !path.is_file() {
        push(
            checks,
            "config",
            Status::Fail,
            format!("user config is missing at {}", path.display()),
        );
        return None;
    }
    match config::load() {
        Ok(config) => {
            push(checks, "config", Status::Ok, path.display().to_string());
            Some(config)
        }
        Err(failure) => {
            push(checks, "config", Status::Fail, failure.to_string());
            None
        }
    }
}

struct Resolved {
    endpoint: Endpoint,
    server: Option<String>,
}

fn server_check(
    g: &Global,
    wc: Option<&impl WcView>,
    user: &UserConfig,
    checks: &mut Vec<Check>,
) -> Option<Resolved> {
    let endpoint_override = endpoint_override(g);
    let selected = g.server.as_deref().or_else(|| {
        wc.and_then(WcView::server)
            .or(user.default_server.as_deref())
    });
    let named = selected.and_then(|name| user.servers.get(name));
    let legacy = wc
        .filter(|view| view.server().is_none())
        .and_then(|view| view.config().endpoint.as_deref())
        .map(Endpoint::parse);
    if endpoint_override.is_none() && legacy.is_none() && selected.is_some() && named.is_none() {
        let name = selected.unwrap_or_default();
        push(
            checks,
            "server",
            Status::Fail,
            format!("server {name} is not configured; run `graft login {name} --endpoint URL`"),
        );
        return None;
    }
    let named_endpoint = named.map(|server| Endpoint::parse(&server.endpoint));
    match resolve_connection_from_sources(
        wc.map(|view| (view.config(), view.server())),
        selected,
        named_endpoint.as_ref(),
        legacy,
        endpoint_override,
    ) {
        Ok((endpoint, server)) => {
            let detail = match &server {
                Some(name) => format!("{name} at {endpoint}"),
                None => endpoint.to_string(),
            };
            push(checks, "server", Status::Ok, detail);
            Some(Resolved { endpoint, server })
        }
        Err(failure) => {
            push(checks, "server", Status::Fail, failure.to_string());
            None
        }
    }
}

fn endpoint_override(g: &Global) -> Option<Endpoint> {
    g.endpoint.as_deref().map(Endpoint::parse).or_else(|| {
        std::env::var(graft_client::ENDPOINT_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| Endpoint::parse(&value))
    })
}

fn endpoint_check(resolved: &Resolved, checks: &mut Vec<Check>) -> bool {
    let endpoint = resolved.endpoint.clone();
    let shown = endpoint.to_string();
    let probed = conn::runtime().and_then(|rt| {
        rt.block_on(async move {
            let remote = Remote::connect(endpoint).await?;
            remote
                .resolve_ref(&probe_context(), probe_request())
                .await
                .map(|_| ())
                .map_err(graft_client::Error::from)
        })
        .map_err(Failure::from)
    });
    match probed {
        Ok(()) => {
            push(
                checks,
                "endpoint",
                Status::Ok,
                format!("reachable at {shown}"),
            );
            push(checks, "protocol", Status::Ok, protocol_ok());
            true
        }
        Err(failure) => classify_probe(&failure, &shown, checks),
    }
}

fn probe_context() -> CallContext {
    CallContext {
        principal: Principal {
            subject: String::new(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: Vec::new(),
        },
        request_id: "doctor".to_owned(),
        deadline_ms: None,
    }
}

fn probe_request() -> ResolveRef {
    ResolveRef {
        repo: String::new(),
        name: DEFAULT_LINE.to_owned(),
    }
}

fn protocol_ok() -> String {
    format!(
        "client {}.{} binds",
        PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor
    )
}

fn classify_probe(failure: &Failure, endpoint: &str, checks: &mut Vec<Check>) -> bool {
    if failure.kind() == "unavailable" {
        push(
            checks,
            "endpoint",
            Status::Fail,
            format!("server down or unreachable at {endpoint}"),
        );
        return false;
    }
    if failure.kind() == "incompatible_protocol" {
        push(
            checks,
            "endpoint",
            Status::Ok,
            format!("reachable at {endpoint}"),
        );
        let server = failure
            .to_string()
            .split_whitespace()
            .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("unknown")
            .to_owned();
        push(
            checks,
            "protocol",
            Status::Fail,
            format!(
                "client {}.{} server {server}; upgrade graft",
                PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor
            ),
        );
        return true;
    }
    // Any other typed answer means the server spoke this protocol.
    push(
        checks,
        "endpoint",
        Status::Ok,
        format!("reachable at {endpoint}"),
    );
    push(checks, "protocol", Status::Ok, protocol_ok());
    true
}

fn token_check(
    g: &Global,
    wc: Option<&impl WcView>,
    user: &UserConfig,
    server: &Option<String>,
    checks: &mut Vec<Check>,
) -> Result<(), Failure> {
    let settings = conn::resolve_settings(g, wc, user)?;
    let Some(raw) = settings.token.filter(|token| !token.is_empty()) else {
        let where_ = server.as_deref().unwrap_or("the resolved server");
        push(
            checks,
            "token",
            Status::Fail,
            format!("no token for {where_}; run `graft login` again"),
        );
        return Ok(());
    };
    let claims = match Claims::peek_unverified(&Token::from_encoded(raw)) {
        Ok(claims) => claims,
        Err(_) => {
            push(
                checks,
                "token",
                Status::Fail,
                "token is malformed; run `graft login` again".to_owned(),
            );
            return Ok(());
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    if claims.exp <= now {
        push(
            checks,
            "token",
            Status::Fail,
            "token expired; run `graft login` again".to_owned(),
        );
        return Ok(());
    }
    let scopes = if claims.principal.scopes.is_empty() {
        "none".to_owned()
    } else {
        claims.principal.scopes.join(",")
    };
    push(
        checks,
        "token",
        Status::Ok,
        format!("expires {} scopes {scopes}", claims.exp),
    );
    Ok(())
}

fn skip(checks: &mut Vec<Check>, name: &'static str, detail: &str) {
    push(checks, name, Status::Skip, detail.to_owned());
}

fn push(checks: &mut Vec<Check>, name: &'static str, status: Status, detail: String) {
    checks.push(Check {
        name,
        status,
        detail,
    });
}

fn finish(g: &Global, checks: Vec<Check>) -> Result<(), Failure> {
    let ok = checks.iter().all(|check| check.status.passed());
    if g.json {
        let rows: Vec<Value> = checks
            .iter()
            .map(|check| {
                json!({
                    "name": check.name,
                    "ok": check.status.passed(),
                    "status": check.status.json_status(),
                    "detail": check.detail,
                })
            })
            .collect();
        render::print_json(&json!({ "checks": rows }))?;
    } else {
        for check in &checks {
            outln!("{}  {}  {}", check.status.label(), check.name, check.detail);
        }
    }
    if ok {
        Ok(())
    } else {
        // The report is already printed. A second error line would mix
        // shapes, and under --json it would not be one JSON document.
        Err(Failure::silent(crate::Exit::Failure))
    }
}
