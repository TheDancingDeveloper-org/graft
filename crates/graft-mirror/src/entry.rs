//! `graft-mirror`: the only service that speaks git (`docs/design.md` §11).
//!
//! Work items G1.14 (import, follow), G3.3 (export),
//! G3.9 (authority switch), WI-2284 (batch PRs, decision D21).
//!
//! The bearer token comes from `GRAFT_TOKEN` or a token file, never from the
//! command line.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::authority::{
    DEFAULT_LAST, Gate, HookGate, RulesetGate, Stage, Status, Switcher, Unfrozen,
};
use crate::batch::{BatchConfig, BatchOutcome, BatchRecord, Batcher};
use crate::config::{AuthChoice, FileConfig, GithubFile, parse_shas};
use crate::credentials::{CredentialProvider, GithubApp, NoCredentials, StaticToken};
use crate::export::{Export, export_all};
use crate::follow::Follower;
use crate::github::{HttpForge, RetryPolicy};
use crate::import::import_repo;
use crate::land::NativeLand;
use crate::pulls::PrImporter;
use crate::reconcile::{Resolution, reconcile};
use crate::server::Server;
use crate::{MirrorDb, MirrorError};
use graft_git::import::ImportConfig;
use graft_model::ChangesetId;
use graft_proto::{CallContext, Principal, PrincipalKind};
use graft_telemetry::{Telemetry, TelemetryConfig};

const USAGE: &str = "usage: graft-mirror <command> [args]
  import <git-dir> --repo <name>        lossless import with fidelity check
         [--server <url>] [--db <file>] [--token-file <file>]
         [--line <git-ref>=<line>]... [--no-tags] [--allow-lossy <sha>]...
  init-config [--out <file>]            write a commented sample mirror.toml
  follow --config <file>                git-primary: follow GitHub main
  export --config <file>                graft-primary: export lines to GitHub
  export --to <bare-repo> --repo <name> full export (exit path, drill)
         --db <file> [--server <url>] [--token-file <file>]
  verify --config <file> --line <l> [--last N]
                                        re-export the last N and compare shas
  reconcile --config <file> --line <l> (--land | --discard)
                                        resolve an export divergence by hand
  pr-batch <line> --base <branch> --config <file> [--track]
           [--exclude <changeset>]... [--reason <text>]
           [--max-lands <n>] [--max-age <seconds>]
                                        sync an integration line onto the
                                        followed base, export its net change
                                        as one squashed PR; never merges
  authority <status|freeze|verify|flip|unfreeze> --config <file> --line <l>
         [--last N]                     switch a line's authority safely

  The server URL defaults to GRAFT_ENDPOINT; the token comes from
  GRAFT_TOKEN or --token-file / token_file.";

/// Exit status for a usage error (`EX_USAGE`).
const EXIT_USAGE: u8 = 64;

/// Runs a mirror command with arguments after the executable name.
pub fn run(args: Vec<String>) -> ExitCode {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match args.first().map(String::as_str) {
        Some("import") => execute(cmd_import(&args[1..])),
        Some("init-config") => cmd_init_config(&args[1..]),
        Some("follow") => execute(cmd_follow(&args[1..])),
        Some("export") => execute(cmd_export(&args[1..])),
        Some("reconcile") => execute(cmd_reconcile(&args[1..])),
        Some("pr-batch") => execute(cmd_pr_batch(&args[1..])),
        Some("verify") => execute(cmd_authority(Step::Verify, &args[1..])),
        Some("authority") => match args.get(1).map(String::as_str).and_then(Step::parse) {
            Some(step) => execute(cmd_authority(step, &args[2..])),
            None => usage(),
        },
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(EXIT_USAGE)
}

fn execute(f: impl Future<Output = Result<(), CmdError>>) -> ExitCode {
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
    let server = server.ok_or_else(|| {
        CmdError::Usage("no server: pass --server, GRAFT_ENDPOINT, or local:<repo>".into())
    })?;
    let db = db.unwrap_or_else(|| git_dir.join("graft-mirror.sqlite"));

    let mut config = ImportConfig::new(&git_dir, &repo);
    if !lines.is_empty() {
        config.lines = lines;
    }
    config.tags = tags;
    config.allow_lossy = parse_shas(&lossy)?;

    let api = Server::open(&server, token(token_file.as_deref())?).await?;
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
    for t in &r.skipped_tags {
        println!("tag {}: skipped, names a {} not a commit", t.name, t.kind);
    }
    Ok(())
}

/// The commented sample `mirror.toml` (`init-config`).
const SAMPLE_CONFIG: &str = include_str!("sample_mirror.toml");

/// Writes the sample config. Default is stdout. Refuses an existing `--out`.
fn cmd_init_config(args: &[String]) -> ExitCode {
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                let Some(path) = args.get(i + 1) else {
                    eprintln!("graft-mirror: init-config --out needs a file");
                    return usage();
                };
                if out.replace(PathBuf::from(path)).is_some() {
                    eprintln!("graft-mirror: init-config takes one --out");
                    return usage();
                }
                i += 2;
            }
            other => {
                eprintln!("graft-mirror: unknown argument {other:?}");
                return usage();
            }
        }
    }
    let Some(path) = out else {
        print!("{SAMPLE_CONFIG}");
        return ExitCode::SUCCESS;
    };
    if path.exists() {
        eprintln!(
            "graft-mirror: {} already exists; init-config will not overwrite it",
            path.display()
        );
        return usage();
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("graft-mirror: cannot create {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }
    match std::fs::write(&path, SAMPLE_CONFIG) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("graft-mirror: cannot write {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
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
    let mirror = file.mirror();
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

/// The default GitHub API root.
const GITHUB_API: &str = "https://api.github.com";

fn system_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Runs `with` with the credential `[github]` chose, or none.
async fn with_credentials<T>(
    github: Option<&GithubFile>,
    with: impl AsyncCredentialFn<T>,
) -> Result<T, CmdError> {
    let Some(gh) = github else {
        return with.call(&NoCredentials).await;
    };
    let api = gh.api.as_deref().unwrap_or(GITHUB_API);
    match gh.auth()? {
        AuthChoice::TokenFile(path) => with.call(&StaticToken::load(&path)?).await,
        AuthChoice::App {
            app_id,
            installation_id,
            private_key_file,
        } => {
            let app = GithubApp::load(api, app_id, installation_id, &private_key_file, system_ms)?;
            with.call(&app).await
        }
    }
}

/// A command body generic over the credential type.
trait AsyncCredentialFn<T> {
    async fn call<C: CredentialProvider>(self, credentials: &C) -> Result<T, CmdError>;
}

async fn cmd_export(args: &[String]) -> Result<(), CmdError> {
    match args.first().map(String::as_str) {
        Some("--config") => {}
        Some("--to") => return cmd_export_to(args).await,
        _ => {
            return Err(CmdError::Usage(
                "export takes --config <file> or --to <bare-repo>".into(),
            ));
        }
    }
    let path = match args {
        [_, path] => PathBuf::from(path),
        _ => return Err(CmdError::Usage("export takes --config <file>".into())),
    };
    let file = FileConfig::load(&path)?;
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
    with_credentials(
        file.github.as_ref(),
        RunExport {
            file: &file,
            api: &api,
            db: &db,
            telemetry: &telemetry,
        },
    )
    .await
}

struct RunExport<'a> {
    file: &'a FileConfig,
    api: &'a Server,
    db: &'a MirrorDb,
    telemetry: &'a Telemetry,
}

impl AsyncCredentialFn<()> for RunExport<'_> {
    async fn call<C: CredentialProvider>(self, credentials: &C) -> Result<(), CmdError> {
        let file = self.file;
        let mirror = file.mirror();
        let config = file.export();
        tracing::info!(
            repo = %config.repo, lines = config.lines.len(),
            interval_secs = config.interval.as_secs(), "mirror export starting"
        );
        let mut export = Export::new(
            self.api,
            mirror_cx(&file.repo),
            self.db,
            mirror.clone(),
            credentials,
            config.clone(),
        )?
        .with_metrics(self.telemetry.metrics().clone())
        .with_export_log(self.api);
        let pulls = file.export.as_ref().is_some_and(|e| e.import_pulls);
        let (Some(gh), true) = (file.github.as_ref(), pulls) else {
            export.run(shutdown()).await;
            tracing::info!("mirror export stopped");
            return Ok(());
        };
        let forge = HttpForge::new(
            gh.api.as_deref().unwrap_or(GITHUB_API),
            &gh.owner,
            &gh.repo,
            credentials,
            RetryPolicy::default(),
        )?;
        let lander = NativeLand::new(self.api);
        let mut importers: Vec<_> = config
            .lines
            .iter()
            .map(|l| {
                PrImporter::new(
                    self.api,
                    mirror_cx(&file.repo),
                    self.db,
                    mirror.clone(),
                    &forge,
                    &lander,
                    file.repo.clone(),
                    l.clone(),
                )
            })
            .collect();
        let mut tick = tokio::time::interval(export.interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop = std::pin::pin!(shutdown());
        loop {
            tokio::select! {
                () = &mut stop => break,
                _ = tick.tick() => {}
            }
            if let Err(e) = export.poll_once().await {
                tracing::warn!(error = %e, "mirror export poll failed");
            }
            for importer in &mut importers {
                if let Err(e) = importer.poll_once().await {
                    tracing::warn!(error = %e, "mirror PR import poll failed");
                }
            }
        }
        tracing::info!("mirror export stopped");
        Ok(())
    }
}

async fn cmd_export_to(args: &[String]) -> Result<(), CmdError> {
    let mut dest = None;
    let mut repo = None;
    let mut server = std::env::var("GRAFT_ENDPOINT").ok();
    let mut db = None;
    let mut token_file = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| CmdError::Usage(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--to" => dest = Some(PathBuf::from(value("--to")?)),
            "--repo" => repo = Some(value("--repo")?),
            "--server" => server = Some(value("--server")?),
            "--db" => db = Some(PathBuf::from(value("--db")?)),
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            s => return Err(CmdError::Usage(format!("unexpected argument {s}"))),
        }
    }
    let dest = dest.ok_or_else(|| CmdError::Usage("export needs --to <bare-repo>".into()))?;
    let repo = repo.ok_or_else(|| CmdError::Usage("export --to needs --repo".into()))?;
    let db = db.ok_or_else(|| {
        CmdError::Usage("export --to needs --db (the mirror DB keeps the exact commits)".into())
    })?;
    let server = server.ok_or_else(|| {
        CmdError::Usage("no server: pass --server, GRAFT_ENDPOINT, or local:<repo>".into())
    })?;
    let api = Server::open(&server, token(token_file.as_deref())?).await?;
    let db = MirrorDb::open(&db).map_err(MirrorError::from)?;
    let report = export_all(&api, &mirror_cx(&repo), &db, &repo, &dest).await?;
    println!(
        "exported {} commits ({} blobs) into {}",
        report.commits,
        report.blobs,
        dest.display()
    );
    for (name, sha) in &report.refs {
        println!("{name} -> {sha}");
    }
    Ok(())
}

async fn cmd_reconcile(args: &[String]) -> Result<(), CmdError> {
    let mut config = None;
    let mut line = None;
    let mut resolution = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| CmdError::Usage(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--config" => config = Some(PathBuf::from(value("--config")?)),
            "--line" => line = Some(value("--line")?),
            "--land" | "--discard" if resolution.is_some() => {
                return Err(CmdError::Usage("choose one of --land and --discard".into()));
            }
            "--land" => resolution = Some(Resolution::Land),
            "--discard" => resolution = Some(Resolution::Discard),
            s => return Err(CmdError::Usage(format!("unexpected argument {s}"))),
        }
    }
    let config = config.ok_or_else(|| CmdError::Usage("reconcile needs --config".into()))?;
    let line = line.ok_or_else(|| CmdError::Usage("reconcile needs --line".into()))?;
    let resolution = resolution.ok_or_else(|| {
        CmdError::Usage("reconcile needs --land or --discard (there is no default)".into())
    })?;
    let file = FileConfig::load(&config)?;
    let line = file
        .lines()
        .into_iter()
        .find(|l| l.line == line)
        .ok_or_else(|| CmdError::Usage(format!("line {line} is not in the config")))?;
    let api = Server::connect(&file.server, token(file.token_file.as_deref())?).await?;
    let db = MirrorDb::open(&file.db).map_err(MirrorError::from)?;
    with_credentials(
        file.github.as_ref(),
        RunReconcile {
            file: &file,
            api: &api,
            db: &db,
            line,
            resolution,
        },
    )
    .await
}

struct RunReconcile<'a> {
    file: &'a FileConfig,
    api: &'a Server,
    db: &'a MirrorDb,
    line: crate::follow::FollowLine,
    resolution: Resolution,
}

impl AsyncCredentialFn<()> for RunReconcile<'_> {
    async fn call<C: CredentialProvider>(self, credentials: &C) -> Result<(), CmdError> {
        let mirror = self.file.mirror();
        let lander = NativeLand::new(self.api);
        let out = reconcile(
            self.api,
            &mirror_cx(&self.file.repo),
            self.db,
            &mirror,
            &lander,
            credentials,
            &self.file.repo,
            &self.line,
            self.resolution,
        )
        .await?;
        if out.already_resolved {
            println!("upstream is back where export left it; export resumes");
            return Ok(());
        }
        if let Some(kept) = &out.kept_as {
            println!("stray commits kept in the mirror as {kept}");
        }
        match (self.resolution, out.landed) {
            (Resolution::Land, Some(id)) => {
                println!("landed through graft as {id}; export resumes")
            }
            _ => println!("stray commits replaced by graft's history; export resumes"),
        }
        Ok(())
    }
}

async fn cmd_pr_batch(args: &[String]) -> Result<(), CmdError> {
    let mut line = None;
    let mut base = None;
    let mut config = None;
    let mut track = false;
    let mut exclude = Vec::new();
    let mut reason = None;
    let mut max_lands = None;
    let mut max_age_ms = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| CmdError::Usage(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--base" => base = Some(value("--base")?),
            "--config" => config = Some(PathBuf::from(value("--config")?)),
            "--track" => track = true,
            "--exclude" => {
                let v = value("--exclude")?;
                exclude
                    .push(ChangesetId::parse(&v).map_err(|_| {
                        CmdError::Usage(format!("--exclude: not a changeset: {v}"))
                    })?);
            }
            "--reason" => reason = Some(value("--reason")?),
            "--max-lands" => {
                let n: usize = value("--max-lands")?
                    .parse()
                    .map_err(|_| CmdError::Usage("--max-lands needs a count".into()))?;
                if n == 0 {
                    return Err(CmdError::Usage("--max-lands must be at least 1".into()));
                }
                max_lands = Some(n);
            }
            "--max-age" => {
                let secs: i64 = value("--max-age")?
                    .parse()
                    .ok()
                    .filter(|s| *s >= 0)
                    .ok_or_else(|| CmdError::Usage("--max-age needs seconds".into()))?;
                max_age_ms = Some(secs.saturating_mul(1000));
            }
            s if s.starts_with('-') => return Err(CmdError::Usage(format!("unknown flag {s}"))),
            s if line.is_none() => line = Some(s.to_owned()),
            s => return Err(CmdError::Usage(format!("unexpected argument {s}"))),
        }
    }
    let line = line.ok_or_else(|| CmdError::Usage("pr-batch needs <line>".into()))?;
    let base = base.ok_or_else(|| CmdError::Usage("pr-batch needs --base <branch>".into()))?;
    let config = config.ok_or_else(|| CmdError::Usage("pr-batch needs --config".into()))?;
    let file = FileConfig::load(&config)?;
    let base = file
        .lines()
        .into_iter()
        .find(|l| l.branch == base)
        .ok_or_else(|| CmdError::Usage(format!("branch {base} is not a followed line")))?;
    if file.github.is_none() {
        return Err(MirrorError::Config("pr-batch opens PRs: configure [github]".into()).into());
    }
    let api = Server::connect(&file.server, token(file.token_file.as_deref())?).await?;
    let db = MirrorDb::open(&file.db).map_err(MirrorError::from)?;
    if track && !exclude.is_empty() {
        return Err(CmdError::Usage(
            "--track only reads; --exclude changes the line".into(),
        ));
    }
    let mut batch = BatchConfig::new(line, base, file.follow()?);
    batch.authors = file.author_map();
    batch.default_author = file.default_author()?;
    batch.exclude = exclude;
    batch.drop_reason = reason;
    batch.max_lands = max_lands;
    batch.max_age_ms = max_age_ms;
    with_credentials(
        file.github.as_ref(),
        RunBatch {
            file: &file,
            api: &api,
            db: &db,
            batch,
            track,
        },
    )
    .await
}

struct RunBatch<'a> {
    file: &'a FileConfig,
    api: &'a Server,
    db: &'a MirrorDb,
    batch: BatchConfig,
    track: bool,
}

impl AsyncCredentialFn<()> for RunBatch<'_> {
    async fn call<C: CredentialProvider>(self, credentials: &C) -> Result<(), CmdError> {
        let file = self.file;
        let gh = file
            .github
            .as_ref()
            .ok_or_else(|| MirrorError::Config("pr-batch opens PRs: configure [github]".into()))?;
        let forge = HttpForge::new(
            gh.api.as_deref().unwrap_or(GITHUB_API),
            &gh.owner,
            &gh.repo,
            credentials,
            RetryPolicy::default(),
        )?;
        let line = self.batch.line.clone();
        let mut batcher = Batcher::new(
            self.api,
            mirror_cx(&file.repo),
            self.db,
            file.mirror(),
            &forge,
            self.batch,
        )?;
        if self.track {
            match batcher.track().await? {
                Some(rec) => print_batch(&rec),
                None => println!("line {line}: no batch yet"),
            }
            return Ok(());
        }
        let run = batcher.run().await?;
        if let Some(rec) = &run.settled {
            print_batch(rec);
        }
        for (land, revert) in &run.dropped {
            println!("line {line}: dropped {land} (reverted by {revert})");
        }
        if let Some(m) = run.synced {
            println!("line {line}: synced onto the base as {m}");
        }
        match run.outcome {
            BatchOutcome::Opened(rec) => print_batch(&rec),
            BatchOutcome::Waiting(rec) => {
                print_batch(&rec);
                println!(
                    "line {line}: batch {} is still open; no new batch",
                    rec.number
                );
            }
            BatchOutcome::NothingToExport => {
                println!("line {line}: nothing the base lacks; no batch")
            }
            BatchOutcome::Conflict { onto, paths } => {
                println!("line {line}: does not rebase onto {onto}; nothing moved. Conflicts:");
                for p in paths {
                    println!("  {p}");
                }
                return Err(MirrorError::Config("sync conflict".into()).into());
            }
            BatchOutcome::BaseStopped(why) => {
                return Err(MirrorError::Config(why).into());
            }
            BatchOutcome::NotDue { lands, oldest_ms } => {
                println!(
                    "line {line}: {lands} land(s) waiting (oldest authored at {oldest_ms} ms); \
                     not due for a batch yet"
                );
            }
        }
        Ok(())
    }
}

fn print_batch(rec: &BatchRecord) {
    println!(
        "batch {} ({}): {} land(s), {} on {} at {}",
        rec.number,
        rec.state.as_str(),
        rec.lands,
        rec.branch,
        rec.base_branch,
        rec.base_sha
    );
    if let Some(url) = &rec.pr_url {
        println!("  PR:     {url}");
    }
    if !rec.detail.is_empty() {
        println!("  detail: {}", rec.detail);
    }
}

/// One `graft-mirror authority` step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Status,
    Freeze,
    Verify,
    Flip,
    Unfreeze,
}

impl Step {
    fn parse(s: &str) -> Option<Step> {
        Some(match s {
            "status" => Step::Status,
            "freeze" => Step::Freeze,
            "verify" => Step::Verify,
            "flip" => Step::Flip,
            "unfreeze" => Step::Unfreeze,
            _ => return None,
        })
    }
}

async fn cmd_authority(step: Step, args: &[String]) -> Result<(), CmdError> {
    let mut config = None;
    let mut line = None;
    let mut last = DEFAULT_LAST;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| CmdError::Usage(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--config" => config = Some(PathBuf::from(value("--config")?)),
            "--line" => line = Some(value("--line")?),
            "--last" if step == Step::Verify => {
                last = value("--last")?
                    .parse()
                    .map_err(|_| CmdError::Usage("--last takes a number".into()))?;
            }
            s => return Err(CmdError::Usage(format!("unexpected argument {s}"))),
        }
    }
    let config = config.ok_or_else(|| CmdError::Usage("authority needs --config".into()))?;
    let line = line.ok_or_else(|| CmdError::Usage("authority needs --line".into()))?;
    let file = FileConfig::load(&config)?;
    let line = file
        .lines()
        .into_iter()
        .find(|l| l.line == line)
        .ok_or_else(|| CmdError::Usage(format!("line {line} is not in the config")))?;
    let api = Server::connect(&file.server, token(file.token_file.as_deref())?).await?;
    let db = MirrorDb::open(&file.db).map_err(MirrorError::from)?;
    with_credentials(
        file.github.as_ref(),
        RunAuthority {
            file: &file,
            api: &api,
            db: &db,
            line,
            step,
            last,
        },
    )
    .await
}

struct RunAuthority<'a> {
    file: &'a FileConfig,
    api: &'a Server,
    db: &'a MirrorDb,
    line: crate::follow::FollowLine,
    step: Step,
    last: usize,
}

impl AsyncCredentialFn<()> for RunAuthority<'_> {
    async fn call<C: CredentialProvider>(self, credentials: &C) -> Result<(), CmdError> {
        let Some(gh) = self.file.github.as_ref() else {
            let gate = HookGate::for_upstream(&self.file.upstream).ok_or_else(|| {
                MirrorError::Config(
                    "no gate: configure [github] (its ruleset is the gate), or use a local \
                     bare repository as upstream"
                        .to_owned(),
                )
            })?;
            return self.run(&gate).await;
        };
        let forge = HttpForge::new(
            gh.api.as_deref().unwrap_or(GITHUB_API),
            &gh.owner,
            &gh.repo,
            credentials,
            RetryPolicy::default(),
        )?;
        let gate = RulesetGate::new(&forge, gh.github_app.as_ref().map(|a| a.app_id));
        self.run(&gate).await
    }
}

impl RunAuthority<'_> {
    async fn run<G: Gate>(self, gate: &G) -> Result<(), CmdError> {
        let file = self.file;
        let switcher = Switcher::new(
            self.api,
            mirror_cx(&file.repo),
            self.db,
            file.mirror(),
            gate,
            file.repo.clone(),
            self.line.clone(),
        );
        let line = &self.line.line;
        match self.step {
            Step::Status => print_status(line, &switcher.status().await?),
            Step::Freeze => {
                let s = switcher.freeze().await?;
                println!(
                    "line {line}: frozen for the switch {} -> {}",
                    s.from.as_str(),
                    s.to.as_str()
                );
                if s.from == crate::authority::Authority::GraftPrimary {
                    println!(
                        "graft still accepts lands on {line}: stop them on graft's side \
                         (see the runbook) before verify"
                    );
                }
            }
            Step::Verify => {
                let v = switcher.verify(self.last).await?;
                println!(
                    "line {line}: level at {} = {}; {} changeset(s) re-exported and identical",
                    v.tip, v.sha, v.checked
                );
            }
            Step::Flip => {
                let s = switcher.flip().await?;
                println!(
                    "line {line}: flipped to {}; run unfreeze to open it",
                    s.to.as_str()
                );
                println!(
                    "set line {line} to {} in graft's line policy, and run {} for the line",
                    s.to.as_str(),
                    match s.to {
                        crate::authority::Authority::GitPrimary => "follow (not export)",
                        crate::authority::Authority::GraftPrimary => {
                            "export (not follow)"
                        }
                    }
                );
            }
            Step::Unfreeze => match switcher.unfreeze().await? {
                Unfrozen::Finished(a) => {
                    println!("line {line}: switch finished; {} holds it", a.as_str())
                }
                Unfrozen::Aborted(a) => {
                    println!("line {line}: switch aborted; {} keeps it", a.as_str())
                }
                Unfrozen::NoSwitch(a) => println!(
                    "line {line}: no switch in progress; gate set for {}",
                    a.as_str()
                ),
            },
        }
        Ok(())
    }
}

fn print_status(line: &str, s: &Status) {
    let hex = |sha: Option<graft_git::GitSha>| sha.map_or("none".to_owned(), |s| s.to_hex());
    println!("line {line}");
    println!("  authority: {}", s.claim.as_deref().unwrap_or("unclaimed"));
    match &s.switch {
        Some(sw) => println!(
            "  switch:    {} -> {} ({}{})",
            sw.from.as_str(),
            sw.to.as_str(),
            sw.stage.as_str(),
            match (sw.stage, sw.sha) {
                (Stage::Verified, Some(sha)) => format!(" at {sha}"),
                _ => String::new(),
            }
        ),
        None => println!("  switch:    none"),
    }
    println!("  gate:      {}", s.gate.as_str());
    println!("  upstream:  {}", hex(s.upstream));
    println!(
        "  graft:     {} ({})",
        s.tip.map_or("none".to_owned(), |t| t.to_string()),
        hex(s.tip_sha)
    );
    println!("  level:     {}", if s.in_sync() { "yes" } else { "no" });
    if let Some(d) = s.follow.diverged {
        println!("  follow:    stopped on {d}");
    }
    if s.export.as_ref().is_some_and(|e| e.diverged) {
        println!("  export:    stopped on a divergence (reconcile)");
    }
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{SAMPLE_CONFIG, cmd_init_config};
    use crate::config::FileConfig;

    #[test]
    fn sample_parses_with_the_follow_loader() {
        let config = FileConfig::parse(SAMPLE_CONFIG).expect("sample mirror.toml parses");
        let follow = config.follow().expect("follow accepts the sample");
        assert_eq!(follow.repo, "example-repo");
        assert_eq!(config.repo, "example-repo");
        assert!(config.export.is_some(), "export settings are in the sample");
        assert!(config.github.is_some(), "pr-batch needs [github]");
    }

    #[test]
    fn sample_names_no_estate() {
        for forbidden in ["graft-dev", "vogt", "rusttorrent"] {
            assert!(
                !SAMPLE_CONFIG.to_ascii_lowercase().contains(forbidden),
                "{forbidden} must not appear in the sample"
            );
        }
    }

    #[test]
    fn init_config_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.toml");
        fs::write(&path, "keep-me").unwrap();
        let before = fs::read(&path).unwrap();
        let code = cmd_init_config(&["--out".into(), path.display().to_string()]);
        assert_eq!(code, std::process::ExitCode::from(super::EXIT_USAGE));
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
