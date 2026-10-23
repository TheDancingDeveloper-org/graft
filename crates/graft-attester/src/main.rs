//! `graft-attester`: builds and tests a repository's new drafts and posts
//! `Attest` per check (WI-2276). See the library docs and `deploy/README.md`.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use graft_attester::{Attester, Check, Config, DEFAULT_CHECKS, log};
use graft_client::{Endpoint, Remote};

/// Builds and tests new drafts and posts `Attest` per check.
#[derive(Debug, Parser)]
#[command(name = "graft-attester", version)]
struct Args {
    /// Server endpoint (`http://host:port`).
    #[arg(long, default_value = "http://graft-server:7878")]
    endpoint: String,
    /// Repository to watch and attest.
    #[arg(long)]
    repo: String,
    /// File holding the token (mode 0600 or stricter). It needs `read` and
    /// `attest` on the repository.
    #[arg(long, default_value = "/run/secrets/graft-attester-token")]
    token_file: PathBuf,
    /// A check, `name=program args...`, run in order (repeatable; default
    /// `ci/build=cargo build --locked` and `ci/test=cargo test --locked`).
    #[arg(long = "check")]
    checks: Vec<String>,
    /// Per-check time limit, in seconds.
    #[arg(long, default_value_t = 2700)]
    timeout_s: u64,
    /// Root of the work, log and state directories.
    #[arg(long, default_value = "/var/lib/graft-attester")]
    root: PathBuf,
    /// URL prefix attested with each result, before `<draft>/<check>.log`
    /// (default `file://<root>/logs/`).
    #[arg(long)]
    log_url_prefix: Option<String>,
    /// Most drafts waiting at once.
    #[arg(long, default_value_t = 16)]
    queue: usize,
    /// Run the checks as `uid:gid` (the attester then needs CAP_SETUID,
    /// CAP_SETGID, CAP_CHOWN and CAP_KILL).
    #[arg(long, value_parser = parse_ids)]
    run_as: Option<(u32, u32)>,
    /// `KEY=VALUE` in the checks' environment, which is otherwise empty
    /// (repeatable; `PATH` defaults to the attester's).
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// Most bytes of a check's output kept in its log.
    #[arg(long, default_value_t = 8 << 20)]
    max_log_bytes: u64,
    /// Days a draft's logs are kept.
    #[arg(long, default_value_t = 14)]
    log_days: u64,
}

fn parse_ids(s: &str) -> Result<(u32, u32), String> {
    let (u, g) = s.split_once(':').ok_or("expected uid:gid")?;
    let u = u.parse().map_err(|_| "uid is not a number")?;
    let g = g.parse().map_err(|_| "gid is not a number")?;
    Ok((u, g))
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    let (k, v) = s.split_once('=').ok_or("expected KEY=VALUE")?;
    if k.is_empty() {
        return Err("empty key".to_owned());
    }
    Ok((k.to_owned(), v.to_owned()))
}

/// Reads the token, refusing a file others could read. The value is never
/// printed, and errors name the path only.
fn read_token(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{}: readable by group or others; make it 0600",
            path.display()
        ));
    }
    let token = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(format!("{}: empty", path.display()));
    }
    Ok(token)
}

fn config(args: &Args) -> Result<Config, String> {
    let mut cfg = Config::new(args.repo.clone(), &args.root);
    let specs: Vec<&str> = if args.checks.is_empty() {
        DEFAULT_CHECKS.to_vec()
    } else {
        args.checks.iter().map(String::as_str).collect()
    };
    cfg.checks = specs
        .into_iter()
        .map(Check::parse)
        .collect::<Result<_, _>>()?;
    cfg.timeout = Duration::from_secs(args.timeout_s.max(1));
    if let Some(prefix) = &args.log_url_prefix {
        cfg.log_url_prefix = prefix.clone();
    }
    cfg.queue_cap = args.queue;
    cfg.run_as = args.run_as;
    cfg.env = args.env.clone();
    cfg.max_log_bytes = args.max_log_bytes;
    cfg.log_retention = Duration::from_secs(args.log_days.saturating_mul(24 * 3600));
    Ok(cfg)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    let cfg = match config(&args) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("error: {e}"));
            return ExitCode::from(2);
        }
    };
    let token = match read_token(&args.token_file) {
        Ok(t) => t,
        Err(e) => {
            log(&format!("error: token: {e}"));
            return ExitCode::from(2);
        }
    };

    // The server may still be starting: retry the connection.
    let mut tries = 0u32;
    let remote = loop {
        match Remote::connect(Endpoint::parse(&args.endpoint)).await {
            Ok(r) => break r.with_token(token),
            Err(e) => {
                tries += 1;
                if tries >= 30 {
                    log(&format!("error: connecting to {}: {e}", args.endpoint));
                    return ExitCode::FAILURE;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    };
    let checks: Vec<&str> = cfg.checks.iter().map(|c| c.name.as_str()).collect();
    log(&format!(
        "watching {} on {} for drafts; checks {}",
        cfg.repo,
        args.endpoint,
        checks.join(", ")
    ));
    let mut attester = match Attester::new(remote, cfg) {
        Ok(a) => a,
        Err(e) => {
            log(&format!("error: {e}"));
            return ExitCode::FAILURE;
        }
    };
    tokio::select! {
        r = attester.run(None) => {
            if let Err(e) = r {
                log(&format!("error: {e}"));
            }
            ExitCode::FAILURE
        }
        _ = shutdown() => {
            log("stopping");
            ExitCode::SUCCESS
        }
    }
}

async fn shutdown() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
