//! `graft-attester`: builds and tests a repository's new drafts and posts
//! `Attest` per check (WI-2276). With `--mode post-land --line <name>` it
//! checks what landed on a checked line instead, and reverts the first bad
//! land (WI-2598). See the library docs and `deploy/README.md`.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use graft_attester::post_land::{DEFAULT_WINDOW, PostLand, ProcessRunner};
use graft_attester::{
    Attester, Check, Config, DEFAULT_CHECKS, DEFAULT_LINE, DEFAULT_REPO, Mode, log,
};
use graft_client::{Endpoint, Remote};

/// Builds and tests new drafts and posts `Attest` per check.
#[derive(Debug, Parser)]
#[command(name = "graft-attester", version)]
struct Args {
    /// Server endpoint (`http://host:port`).
    #[arg(long, default_value = "http://graft-server:7878")]
    endpoint: String,
    /// `draft` checks new drafts; `post-land` checks what landed on
    /// `--line` and reverts the first bad land (design §8.7).
    #[arg(long, default_value = "draft", value_parser = Mode::parse)]
    mode: Mode,
    /// Repository to watch and attest.
    #[arg(long, default_value = DEFAULT_REPO)]
    repo: String,
    /// Line whose drafts are attested (drafts in workspaces tracking it;
    /// default `trial-1`), or, in post-land mode, whose lands are checked
    /// (required there).
    #[arg(long)]
    line: Option<String>,
    /// Post-land: times a failed check is run again before the land is bad.
    /// The checker does not read the line's `retries` from the server yet,
    /// so set this to the line's `retries` setting; a different value makes
    /// the checker's verdicts and the server's land-checks rows disagree.
    #[arg(long, default_value_t = 1)]
    retries: u32,
    /// Post-land: most lands held at once.
    #[arg(long, default_value_t = DEFAULT_WINDOW)]
    window: usize,
    /// File holding the token (mode 0600 or stricter). It needs `read` and
    /// `attest` on the repository.
    #[arg(long, default_value = "/run/secrets/graft-attester-token")]
    token_file: PathBuf,
    /// A check, `name=program args...`, run in order (repeatable; default
    /// rustTorrent's `ci/fmt`, `ci/clippy` and `ci/test`).
    #[arg(long = "check")]
    checks: Vec<String>,
    /// Run every check even after one fails (default: the rest are
    /// attested failed without running, as a CI job's later steps are).
    #[arg(long)]
    keep_going: bool,
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
    let line = match (args.mode, &args.line) {
        (_, Some(line)) => line.clone(),
        (Mode::Draft, None) => DEFAULT_LINE.to_owned(),
        (Mode::PostLand, None) => return Err("--mode post-land needs --line".to_owned()),
    };
    let mut cfg = Config::new(args.repo.clone(), line, &args.root);
    if args.mode == Mode::PostLand {
        // Its own position: the two modes read the journal apart.
        cfg.state_file = Some(args.root.join("state").join("post-land-lsn"));
    }
    cfg.stop_on_failure = !args.keep_going;
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
    if args.mode == Mode::PostLand {
        log(&format!(
            "checking lands on {} line {} on {}; checks {}, retries {}",
            cfg.repo,
            cfg.line,
            args.endpoint,
            checks.join(", "),
            args.retries
        ));
        let runner = ProcessRunner::new(cfg.clone());
        let mut checker = match PostLand::new(remote, runner, cfg, args.retries) {
            Ok(c) => c,
            Err(e) => {
                log(&format!("error: {e}"));
                return ExitCode::FAILURE;
            }
        };
        checker.set_window(args.window);
        return tokio::select! {
            r = checker.run(None) => match r {
                // Reported by the run; the operator decides.
                Ok(_) => ExitCode::from(3),
                Err(e) => {
                    log(&format!("error: {e}"));
                    ExitCode::FAILURE
                }
            },
            _ = shutdown() => {
                log("stopping");
                ExitCode::SUCCESS
            }
        };
    }
    log(&format!(
        "watching {} line {} on {} for drafts; checks {}",
        cfg.repo,
        cfg.line,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_mode_checks_drafts_on_the_trial_line() {
        let args = Args::parse_from(["graft-attester"]);
        assert_eq!(args.mode, Mode::Draft);
        let cfg = config(&args).expect("config");
        assert_eq!(cfg.line, DEFAULT_LINE);
        assert!(cfg.state_file.expect("state").ends_with("state/lsn"));
    }

    #[test]
    fn post_land_needs_its_line_and_keeps_its_own_position() {
        let args = Args::parse_from(["graft-attester", "--mode", "post-land"]);
        assert!(config(&args).is_err());
        let args = Args::parse_from([
            "graft-attester",
            "--mode",
            "post-land",
            "--line",
            "integration",
        ]);
        assert_eq!(args.mode, Mode::PostLand);
        let cfg = config(&args).expect("config");
        assert_eq!(cfg.line, "integration");
        assert!(
            cfg.state_file
                .expect("state")
                .ends_with("state/post-land-lsn")
        );
        assert!(Args::try_parse_from(["graft-attester", "--mode", "revert"]).is_err());
    }
}
