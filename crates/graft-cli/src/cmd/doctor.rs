//! `graft doctor`: config, server, endpoint, protocol, token, working
//! copy, lease, node daemon and ignore-file checks.
//!
//! The endpoint probe calls `resolve_ref` with no token, so an expired
//! token does not hide a server that is down. Claims are peeked only to
//! show expiry and scopes. The token itself is never printed. Outside a
//! working copy the working-copy checks are skipped.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use graft_auth::{Claims, Token};
use graft_client::ignore::IGNORE_FILES;
use graft_client::workcopy::WcInfo;
use graft_client::{Endpoint, NODE_SOCKET, Remote};
use graft_proto::PROTOCOL_VERSION;
use graft_proto::api::{RepoApi, WorkspaceApi};
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use graft_proto::types::{GetChangesets, ResolveRef, WorkspaceRef};
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
    conn::runtime()?.block_on(run_checks(g))
}

async fn run_checks(g: &Global) -> Result<(), Failure> {
    let mut checks = Vec::new();
    let Some(config) = config_check(&mut checks) else {
        return finish(g, checks);
    };
    let wc = conn::working_copy_info()?;
    let resolved = server_check(g, wc.as_ref(), &config, &mut checks);
    let Some(resolved) = resolved else {
        skip(&mut checks, "endpoint", "no server");
        skip(&mut checks, "protocol", "no server");
        skip(&mut checks, "token", "no server");
        working_copy_checks(g, wc.as_ref(), None, &config, &mut checks).await;
        return finish(g, checks);
    };
    if !endpoint_check(&resolved, &mut checks).await {
        skip(&mut checks, "protocol", "endpoint unreachable");
        skip(&mut checks, "token", "endpoint unreachable");
        working_copy_checks(g, wc.as_ref(), None, &config, &mut checks).await;
        return finish(g, checks);
    }
    token_check(g, wc.as_ref(), &config, &resolved.server, &mut checks)?;
    working_copy_checks(g, wc.as_ref(), Some(&resolved), &config, &mut checks).await;
    finish(g, checks)
}

async fn working_copy_checks(
    g: &Global,
    wc: Option<&WcInfo>,
    resolved: Option<&Resolved>,
    config: &UserConfig,
    checks: &mut Vec<Check>,
) {
    match wc {
        Some(info) => {
            let usable = working_copy_check(info, config, checks);
            if let Some(resolved) = resolved.filter(|_| usable) {
                match conn::resolve_settings(g, Some(info), config) {
                    Ok(settings) => {
                        let token = settings.token.filter(|token| !token.is_empty());
                        revisions_check(resolved, info, token.as_deref(), checks).await;
                        lease_check(resolved, info, token.as_deref(), checks).await;
                    }
                    Err(failure) => {
                        let detail = failure.to_string();
                        push(checks, "revisions", Status::Fail, detail.clone());
                        push(checks, "lease", Status::Fail, detail);
                    }
                }
            } else {
                skip(checks, "revisions", "working copy not usable");
                skip(checks, "lease", "working copy not usable");
            }
            node_check(checks).await;
            ignore_check(info.root(), checks);
        }
        None => {
            for name in ["workcopy", "revisions", "lease", "node", "ignore"] {
                skip(checks, name, "not in a working copy");
            }
        }
    }
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

async fn endpoint_check(resolved: &Resolved, checks: &mut Vec<Check>) -> bool {
    let endpoint = resolved.endpoint.clone();
    let shown = endpoint.to_string();
    let probed = async move {
        let remote = Remote::connect(endpoint).await?;
        remote
            .resolve_ref(&probe_context(), probe_request())
            .await
            .map(|_| ())
            .map_err(graft_client::Error::from)
    }
    .await
    .map_err(Failure::from);
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

fn working_copy_check(info: &WcInfo, user: &UserConfig, checks: &mut Vec<Check>) -> bool {
    let Some(name) = info.server() else {
        push(
            checks,
            "workcopy",
            Status::Fail,
            "working copy has no named server".to_owned(),
        );
        return false;
    };
    if !user.servers.contains_key(name) {
        push(
            checks,
            "workcopy",
            Status::Fail,
            format!("recorded server {name} is not a known named server"),
        );
        return false;
    }
    push(
        checks,
        "workcopy",
        Status::Ok,
        format!("present, server {name}"),
    );
    true
}

async fn authed_remote(endpoint: &Endpoint, token: Option<&str>) -> Result<Remote, Failure> {
    let remote = Remote::connect(endpoint.clone()).await?;
    Ok(match token {
        Some(token) => remote.with_token(token),
        None => remote,
    })
}

async fn revisions_check(
    resolved: &Resolved,
    info: &WcInfo,
    token: Option<&str>,
    checks: &mut Vec<Check>,
) {
    let remote = match authed_remote(&resolved.endpoint, token).await {
        Ok(remote) => remote,
        Err(failure) => {
            push(checks, "revisions", Status::Fail, failure.to_string());
            return;
        }
    };
    let cx = probe_context();
    let repo = info.config().repo.clone();
    let state = info.state();
    let mut ids = vec![state.base];
    ids.extend(state.head);
    let page = match remote
        .get_changesets(&cx, GetChangesets { repo, ids })
        .await
    {
        Ok(page) => page,
        Err(failure) => {
            push(
                checks,
                "revisions",
                Status::Fail,
                Failure::from(failure).to_string(),
            );
            return;
        }
    };
    let found: std::collections::BTreeSet<_> =
        page.found.iter().map(|view| view.changeset.id()).collect();
    let mut missing = Vec::new();
    if !found.contains(&state.base) {
        missing.push("base");
    }
    if state.head.is_some_and(|head| !found.contains(&head)) {
        missing.push("head");
    }
    if missing.is_empty() {
        let which = if state.head.is_some() {
            "base and head resolve"
        } else {
            "base resolves"
        };
        push(checks, "revisions", Status::Ok, which.to_owned());
    } else {
        push(
            checks,
            "revisions",
            Status::Fail,
            format!("{} do not resolve on the server", missing.join(" and ")),
        );
    }
}

async fn lease_check(
    resolved: &Resolved,
    info: &WcInfo,
    token: Option<&str>,
    checks: &mut Vec<Check>,
) {
    let Some(name) = info.config().workspace.clone() else {
        push(checks, "lease", Status::Ok, "no workspace".to_owned());
        return;
    };
    let remote = match authed_remote(&resolved.endpoint, token).await {
        Ok(remote) => remote,
        Err(failure) => {
            push(checks, "lease", Status::Fail, failure.to_string());
            return;
        }
    };
    match remote
        .get_workspace(
            &probe_context(),
            WorkspaceRef {
                repo: info.config().repo.clone(),
                name,
            },
        )
        .await
    {
        Ok(view) => {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            if view.lease_until_ms <= now_ms {
                push(
                    checks,
                    "lease",
                    Status::Fail,
                    "workspace lease expired; run `graft workspace renew`".to_owned(),
                );
            } else {
                push(
                    checks,
                    "lease",
                    Status::Ok,
                    format!("alive until {} ms", view.lease_until_ms),
                );
            }
        }
        Err(failure) => {
            let failure = Failure::from(failure);
            if failure.kind() == "lease_expired" {
                push(
                    checks,
                    "lease",
                    Status::Fail,
                    "workspace lease expired; run `graft workspace renew`".to_owned(),
                );
            } else if failure.kind() == "not_found" {
                push(
                    checks,
                    "lease",
                    Status::Fail,
                    "workspace not found; clone again or run `graft workspace list`".to_owned(),
                );
            } else {
                push(checks, "lease", Status::Fail, failure.to_string());
            }
        }
    }
}

async fn node_check(checks: &mut Vec<Check>) {
    let path = Path::new(NODE_SOCKET);
    if !path.exists() {
        push(
            checks,
            "node",
            Status::Ok,
            "not running; graft talks to the server directly".to_owned(),
        );
        return;
    }
    let remote = match Remote::connect(Endpoint::NodeSocket(path.to_owned())).await {
        Ok(remote) => remote,
        Err(_) => {
            push(
                checks,
                "node",
                Status::Fail,
                format!(
                    "socket present at {} but it does not answer",
                    path.display()
                ),
            );
            return;
        }
    };
    match remote.resolve_ref(&probe_context(), probe_request()).await {
        Ok(_) => push(
            checks,
            "node",
            Status::Ok,
            format!("answers at {}", path.display()),
        ),
        Err(failure) => {
            let failure = Failure::from(failure);
            // `unavailable` is a transport failure: the socket did not
            // answer. Any other typed API error means the daemon did answer.
            if failure.kind() == "unavailable" {
                push(
                    checks,
                    "node",
                    Status::Fail,
                    format!(
                        "socket present at {} but it does not answer",
                        path.display()
                    ),
                );
            } else {
                push(
                    checks,
                    "node",
                    Status::Ok,
                    format!("answers at {}", path.display()),
                );
            }
        }
    }
}

fn count_rules(text: &str) -> u64 {
    text.lines()
        .filter(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let line = trim_unescaped_trailing_spaces(line);
            if line.is_empty() || line.starts_with('#') {
                return false;
            }
            let body = line.strip_prefix('!').unwrap_or(line);
            let body = if body.ends_with('/') && !body.ends_with("\\/") {
                &body[..body.len() - 1]
            } else {
                body
            };
            !body.strip_prefix('/').unwrap_or(body).is_empty()
        })
        .count() as u64
}

fn trim_unescaped_trailing_spaces(line: &str) -> &str {
    let mut end = line.len();
    while line[..end].ends_with(' ') {
        let before = &line[..end - 1];
        let slashes = before.len() - before.trim_end_matches('\\').len();
        if slashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

fn ignore_check(root: &Path, checks: &mut Vec<Check>) {
    let mut count = 0u64;
    let mut present = Vec::new();
    for name in IGNORE_FILES {
        let path = root.join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                present.push(name);
                count += count_rules(&text);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                push(
                    checks,
                    "ignore",
                    Status::Fail,
                    format!("cannot read {name}: {}", error.kind()),
                );
                return;
            }
        }
    }
    let files = if present.is_empty() {
        "no ignore files".to_owned()
    } else {
        present.join(" ")
    };
    let rules = if count == 1 { "rule" } else { "rules" };
    push(
        checks,
        "ignore",
        Status::Ok,
        format!("{files}, {count} {rules}"),
    );
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
