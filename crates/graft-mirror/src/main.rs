//! `graft-mirror`: the only service that speaks git (`docs/design.md` §11).
//!
//! Work items G1.14 (import, follow), G2.6 (PR bridge), G3.3 (export),
//! G3.9 (authority switch).
//!
//! The bearer token comes from `GRAFT_TOKEN` or a token file, never from the
//! command line.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use graft_git::import::ImportConfig;
use graft_mirror::config::{FileConfig, parse_shas};
use graft_mirror::follow::Follower;
use graft_mirror::import::import_repo;
use graft_mirror::server::Server;
use graft_mirror::{Mirror, MirrorDb, MirrorError};
use graft_proto::{CallContext, Principal, PrincipalKind};
use graft_telemetry::{Telemetry, TelemetryConfig};

const USAGE: &str = "usage: graft-mirror <command> [args]
  import <git-dir> --repo <name>        lossless import with fidelity check
         [--server <url>] [--db <file>] [--token-file <file>]
         [--line <git-ref>=<line>]... [--no-tags] [--allow-lossy <sha>]...
  follow --config <file>                git-primary: follow GitHub main
  bridge --config <file>                git-primary: land drafts as GitHub PRs
  export --config <file>                graft-primary: export lines to GitHub
  export --to <bare-repo> --repo <name> full export (exit path, drill)
  verify --repo <name> [--last N]       re-export and compare shas
  reconcile --repo <name> --line <l>    resolve a divergence by hand
  authority <status|freeze|verify|flip|unfreeze> --line <l>

  The server URL defaults to GRAFT_ENDPOINT; the token comes from
  GRAFT_TOKEN or --token-file / token_file.";

/// Exit status for a usage error (`EX_USAGE`).
const EXIT_USAGE: u8 = 64;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("import") => run(cmd_import(&args[1..])),
        Some("follow") => run(cmd_follow(&args[1..])),
        Some("bridge" | "export" | "verify" | "reconcile" | "authority") => {
            eprintln!("graft-mirror: not implemented yet (see docs/plan.md)");
            ExitCode::from(2)
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(EXIT_USAGE)
}

fn run(f: impl Future<Output = Result<(), CmdError>>) -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("graft-mirror: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(f) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CmdError::Usage(what)) => {
            eprintln!("graft-mirror: {what}");
            usage()
        }
        Err(CmdError::Failed(e)) => {
            eprintln!("graft-mirror: {e}");
            ExitCode::FAILURE
        }
    }
}

enum CmdError {
    Usage(String),
    Failed(MirrorError),
}

impl From<MirrorError> for CmdError {
    fn from(e: MirrorError) -> Self {
        CmdError::Failed(e)
    }
}

/// The context the mirror calls with. The server derives the principal
/// from the token; the scopes here matter only in-process.
fn mirror_cx(repo: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-mirror".to_owned(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{repo}:admin")],
        },
        request_id: "mirror".to_owned(),
        deadline_ms: None,
    }
}

fn token(file: Option<&Path>) -> Result<Option<String>, MirrorError> {
    if let Some(path) = file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| MirrorError::Config(format!("reading the token file: {e}")))?;
        return Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty()));
    }
    Ok(std::env::var("GRAFT_TOKEN").ok().filter(|t| !t.is_empty()))
}

async fn cmd_import(args: &[String]) -> Result<(), CmdError> {
    let mut git_dir = None;
    let mut repo = None;
    let mut server = std::env::var("GRAFT_ENDPOINT").ok();
    let mut db = None;
    let mut token_file = None;
    let mut lines = Vec::new();
    let mut tags = true;
    let mut lossy = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| CmdError::Usage(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--repo" => repo = Some(value("--repo")?),
            "--server" => server = Some(value("--server")?),
            "--db" => db = Some(PathBuf::from(value("--db")?)),
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            "--line" => {
                let v = value("--line")?;
                let (git_ref, line) = v
                    .split_once('=')
                    .ok_or_else(|| CmdError::Usage("--line takes <git-ref>=<line>".into()))?;
                lines.push((git_ref.to_owned(), line.to_owned()));
            }
            "--no-tags" => tags = false,
            "--allow-lossy" => lossy.push(value("--allow-lossy")?),
            s if s.starts_with('-') => return Err(CmdError::Usage(format!("unknown flag {s}"))),
            s if git_dir.is_none() => git_dir = Some(PathBuf::from(s)),
            s => return Err(CmdError::Usage(format!("unexpected argument {s}"))),
        }
    }
    let git_dir = git_dir.ok_or_else(|| CmdError::Usage("import needs <git-dir>".into()))?;
    let repo = repo.ok_or_else(|| CmdError::Usage("import needs --repo".into()))?;
    let server = server
        .ok_or_else(|| CmdError::Usage("no server: pass --server or GRAFT_ENDPOINT".into()))?;
    let db = db.unwrap_or_else(|| git_dir.join("graft-mirror.sqlite"));

    let mut config = ImportConfig::new(&git_dir, &repo);
    if !lines.is_empty() {
        config.lines = lines;
    }
    config.tags = tags;
    config.allow_lossy = parse_shas(&lossy)?;

    let api = Server::connect(&server, token(token_file.as_deref())?).await?;
    let map = MirrorDb::open(&db).map_err(MirrorError::from)?;
    let out = import_repo(&api, &mirror_cx(&repo), &map, &config).await?;
    let r = &out.report;
    println!(
        "imported {} commits ({} blobs, {} verified, {} lossy, {} raw trees)",
        r.commits,
        r.blobs,
        r.verified,
        r.lossy.len(),
        r.raw_trees
    );
    for (line, id) in &r.lines {
        let moved = if out.lines_moved.contains(line) {
            "set"
        } else {
            "unchanged"
        };
        println!("line {line} -> {id} ({moved})");
    }
    println!("tags: {} created", out.tags_created.len());
    for (tag, id) in &out.tags_conflicting {
        println!("tag {tag}: graft already names another changeset (git: {id})");
    }
    Ok(())
}

async fn cmd_follow(args: &[String]) -> Result<(), CmdError> {
    let path = match args {
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => return Err(CmdError::Usage("follow takes --config <file>".into())),
    };
    let file = FileConfig::load(&path)?;
    let follow = file.follow()?;
    let telemetry = graft_telemetry::init(&TelemetryConfig {
        service: "graft-mirror".to_owned(),
        json_logs: file.json_logs,
        metrics_addr: file.metrics_addr.clone(),
        otlp_endpoint: None,
    })
    .map_err(|e| MirrorError::Config(e.to_string()))?;
    let telemetry = std::sync::Arc::new(telemetry);
    if let Some(addr) = &file.metrics_addr {
        serve_metrics(addr, telemetry.clone()).await?;
    }

    let api = Server::connect(&file.server, token(file.token_file.as_deref())?).await?;
    let db = MirrorDb::open(&file.db).map_err(MirrorError::from)?;
    let mirror = Mirror::new(&file.mirror_dir, &file.upstream);
    tracing::info!(
        repo = %follow.repo, lines = follow.lines.len(),
        interval_secs = follow.interval.as_secs(), "mirror follow starting"
    );
    let mut follower = Follower::new(&api, mirror_cx(&file.repo), &db, mirror, follow)
        .with_metrics(telemetry.metrics().clone());
    follower.run(shutdown()).await;
    tracing::info!("mirror follow stopped");
    Ok(())
}

async fn serve_metrics(
    addr: &str,
    telemetry: std::sync::Arc<Telemetry>,
) -> Result<(), MirrorError> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| MirrorError::Config(format!("binding metrics_addr: {e}")))?;
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let t = telemetry.clone();
            async move { t.encode() }
        }),
    );
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "metrics server stopped");
        }
    });
    Ok(())
}

/// Completes on SIGINT or SIGTERM.
async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
