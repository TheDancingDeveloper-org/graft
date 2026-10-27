//! `graft-attester`: builds and tests a repository's new drafts and posts
//! `Attest` per check (WI-2276). With `--mode post-land --line <name>` it
//! checks what landed on a checked line instead, and reverts the first bad
//! land (WI-2598). See the library docs and `deploy/README.md`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use graft_attester::github_runner::{
    DEFAULT_API, DEFAULT_MAX_IN_FLIGHT, DEFAULT_WORKFLOW, GitHubConfig, GitHubRunner, GitHubToken,
    read_secret,
};
use graft_attester::post_land::{DEFAULT_WINDOW, PostLand, ProcessRunner, Runner};
use graft_attester::{
    Attester, Check, Config, DEFAULT_CHECKS, DEFAULT_LINE, DEFAULT_REPO, Mode, RetriesSource,
    call_context, effective_retries, log,
};
use graft_client::{Endpoint, Remote};
use graft_proto::api::RepoApi as _;
use graft_proto::types::GetLineConfig;

/// Where post-land checks run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum RunnerKind {
    /// Here, each check as a process on a checkout of the landed snapshot.
    Process,
    /// As a GitHub Actions workflow dispatched on the mirror's per-land ref.
    Github,
}

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
    /// Overrides the line's `retries` from `GetLineConfig`. When omitted,
    /// the line's value is used. An older server that does not implement
    /// `GetLineConfig` falls back to this value, or to 0 when it is omitted.
    #[arg(long)]
    retries: Option<u32>,
    /// Post-land: most lands held at once.
    #[arg(long, default_value_t = DEFAULT_WINDOW)]
    window: usize,
    /// File holding the graft token (mode 0600 or stricter). It needs
    /// `read` and `attest` on the repository, and in post-land mode `land`
    /// on the line too, because the checker lands the reverts.
    #[arg(long, default_value = "/run/secrets/graft-attester-token")]
    token_file: PathBuf,
    /// Post-land: where the checks run. `github` dispatches `--workflow` on
    /// `<ref-prefix><changeset>` and reads its jobs; the workflow attests
    /// the passes itself.
    #[arg(long, value_enum, default_value_t = RunnerKind::Process)]
    runner: RunnerKind,
    /// `--runner github`: the GitHub repository, `owner/name`.
    #[arg(long)]
    github_repo: Option<String>,
    /// `--runner github`: file holding the GitHub token (mode 0600 or
    /// stricter). It is opened read-only and never written, so graft-mirror
    /// can share it. It needs `actions: write` on the repository.
    #[arg(long, default_value = "/run/secrets/github-token")]
    github_token_file: PathBuf,
    /// `--runner github`: the GitHub API root.
    #[arg(long, default_value = DEFAULT_API)]
    github_api: String,
    /// `--runner github`: the workflow dispatched for each snapshot and
    /// attempt. Its `run-name` must carry the `request_id` input.
    #[arg(long, default_value = DEFAULT_WORKFLOW)]
    workflow: String,
    /// `--runner github`: prefix of the ref the mirror pushes for each land;
    /// the changeset id follows (default `graft/check/<line>/`).
    #[arg(long)]
    ref_prefix: Option<String>,
    /// `--runner github`: most workflow runs queued or in progress at once,
    /// on any ref; a new snapshot waits for one to finish.
    #[arg(long, default_value_t = DEFAULT_MAX_IN_FLIGHT, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    max_in_flight: usize,
    /// `--runner github`: seconds between polls of a run.
    #[arg(long, default_value_t = 15)]
    github_poll_s: u64,
    /// `--runner github`: seconds one try waits for a dispatched run to
    /// appear, for room under `--max-in-flight`, or for a queued run to
    /// start. Past it nothing is judged, and the next try waits on the
    /// same run.
    #[arg(long, default_value_t = 1800)]
    github_start_timeout_s: u64,
    /// A check, `name=program args...`, run in order (repeatable; default
    /// rustTorrent's `ci/fmt`, `ci/clippy` and `ci/test`). With `--runner
    /// github` it is `name=job`, and the default is the line's `required`
    /// checks, each read from the job named after its last `/` segment.
    #[arg(long = "check")]
    checks: Vec<String>,
    /// Run every check even after one fails (default: the rest are
    /// attested failed without running, as a CI job's later steps are).
    #[arg(long)]
    keep_going: bool,
    /// Per-check time limit, in seconds. With `--runner github` it counts
    /// from the check job's `started_at` on GitHub: time queued for a
    /// runner never counts, and a restart keeps the clock.
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
    read_secret(path)
}

fn config(args: &Args) -> Result<Config, String> {
    let line = match (args.mode, &args.line) {
        (_, Some(line)) => line.clone(),
        (Mode::Draft, None) => DEFAULT_LINE.to_owned(),
        (Mode::PostLand, None) => return Err("--mode post-land needs --line".to_owned()),
    };
    if args.runner == RunnerKind::Github && args.mode != Mode::PostLand {
        return Err("--runner github needs --mode post-land".to_owned());
    }
    let mut cfg = Config::new(args.repo.clone(), line, &args.root);
    if args.mode == Mode::PostLand {
        // Its own position: the two modes read the journal apart.
        cfg.state_file = Some(args.root.join("state").join("post-land-lsn"));
    }
    cfg.stop_on_failure = !args.keep_going;
    let specs: Vec<&str> = if args.checks.is_empty() && args.runner == RunnerKind::Github {
        // Filled in from the line's `required` once connected.
        Vec::new()
    } else if args.checks.is_empty() {
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

/// The GitHub runner's settings; `cfg.checks` are `name=job`.
fn github_config(args: &Args, cfg: &Config) -> Result<GitHubConfig, String> {
    let slug = args
        .github_repo
        .as_deref()
        .ok_or("--runner github needs --github-repo owner/name")?;
    let prefix = args
        .ref_prefix
        .clone()
        .unwrap_or_else(|| format!("graft/check/{}/", cfg.line));
    let mut gh = GitHubConfig::new(slug, prefix)?;
    gh.api = args.github_api.clone();
    gh.workflow = args.workflow.clone();
    gh.poll = Duration::from_secs(args.github_poll_s);
    gh.start_timeout = Duration::from_secs(args.github_start_timeout_s);
    gh.run_timeout = cfg.timeout;
    gh.max_in_flight = args.max_in_flight;
    for check in &cfg.checks {
        gh.jobs.insert(check.name.clone(), check.argv[0].clone());
    }
    Ok(gh)
}

/// Runs the post-land checker with `runner` until it stops.
async fn check_lands<R: Runner<Remote>>(
    remote: Remote,
    runner: R,
    cfg: Config,
    retries: u32,
    window: usize,
) -> ExitCode {
    let mut checker = match PostLand::new(remote, runner, cfg, retries) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("error: {e}"));
            return ExitCode::FAILURE;
        }
    };
    checker.set_window(window);
    tokio::select! {
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
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    let mut cfg = match config(&args) {
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
    let github_token = match args.runner {
        RunnerKind::Process => None,
        RunnerKind::Github => match GitHubToken::read(&args.github_token_file) {
            Ok(t) => Some(t),
            Err(e) => {
                log(&format!("error: GitHub token: {e}"));
                return ExitCode::from(2);
            }
        },
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
    if args.runner == RunnerKind::Github && cfg.checks.is_empty() {
        let line = match remote
            .get_line_config(
                &call_context(),
                GetLineConfig {
                    repo: cfg.repo.clone(),
                    line: cfg.line.clone(),
                },
            )
            .await
        {
            Ok(l) => l,
            Err(e) => {
                log(&format!(
                    "error: line config: {e}; give the checks with --check name=job"
                ));
                return ExitCode::FAILURE;
            }
        };
        if line.required.is_empty() {
            log(&format!(
                "error: line {} requires no checks; give them with --check name=job",
                cfg.line
            ));
            return ExitCode::from(2);
        }
        cfg.checks = line
            .required
            .iter()
            .map(|name| Check {
                name: name.clone(),
                argv: vec![name.rsplit('/').next().unwrap_or(name).to_owned()],
            })
            .collect();
    }
    let checks: Vec<&str> = cfg.checks.iter().map(|c| c.name.as_str()).collect();
    if args.mode == Mode::PostLand {
        let chosen =
            match effective_retries(&remote, &call_context(), &cfg.repo, &cfg.line, args.retries)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    log(&format!("error: line config: {e}"));
                    return ExitCode::FAILURE;
                }
            };
        if chosen.source == RetriesSource::Fallback {
            log(&format!(
                "warning: the server does not implement GetLineConfig; retries {} for line {}",
                chosen.retries, cfg.line
            ));
        }
        log(&format!(
            "checking lands on {} line {} on {}; checks {}, retries {}",
            cfg.repo,
            cfg.line,
            args.endpoint,
            checks.join(", "),
            chosen.retries
        ));
        let retries = chosen.retries;
        if args.runner == RunnerKind::Process {
            let runner = ProcessRunner::new(cfg.clone());
            return check_lands(remote, runner, cfg, retries, args.window).await;
        }
        let gh = match github_config(&args, &cfg) {
            Ok(g) => g,
            Err(e) => {
                log(&format!("error: {e}"));
                return ExitCode::from(2);
            }
        };
        let Some(token) = github_token else {
            unreachable!("read for --runner github");
        };
        log(&format!(
            "checks run as {} on {}/{}, refs {}<changeset>",
            gh.workflow, gh.owner, gh.repo, gh.ref_prefix
        ));
        let runner = match GitHubRunner::new(gh, token) {
            Ok(r) => r,
            Err(e) => {
                log(&format!("error: {e}"));
                return ExitCode::FAILURE;
            }
        };
        return check_lands(remote, runner, cfg, retries, args.window).await;
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

    /// WI-2625 Done 5: the graft token's help names every scope the
    /// post-land checker uses, `land` for its reverts included.
    #[test]
    fn the_token_file_help_names_read_attest_and_land() {
        use clap::CommandFactory as _;
        let help = Args::command().render_long_help().to_string();
        let start = help.find("--token-file").expect("--token-file in the help");
        let text: String = help[start..]
            .split("--runner")
            .next()
            .expect("text")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for scope in ["`read`", "`attest`", "`land`", "reverts"] {
            assert!(text.contains(scope), "{scope} missing from: {text}");
        }
    }

    #[test]
    fn the_github_runner_reads_jobs_and_defaults_its_ref_prefix() {
        let args = Args::parse_from([
            "graft-attester",
            "--mode",
            "post-land",
            "--line",
            "backlog-1",
            "--runner",
            "github",
            "--github-repo",
            "owner/graft",
            "--check",
            "ci/fmt=fmt",
            "--check",
            "ci/test=unit-tests",
        ]);
        let cfg = config(&args).expect("config");
        let gh = github_config(&args, &cfg).expect("github config");
        assert_eq!(gh.ref_prefix, "graft/check/backlog-1/");
        assert_eq!(gh.workflow, DEFAULT_WORKFLOW);
        assert_eq!((gh.owner.as_str(), gh.repo.as_str()), ("owner", "graft"));
        assert_eq!(gh.job_of("ci/fmt"), "fmt");
        assert_eq!(gh.job_of("ci/test"), "unit-tests");
        assert_eq!(gh.max_in_flight, 2, "two runs in flight by default");
        // The checks default to the line's `required`, read once connected.
        let args = Args::parse_from([
            "graft-attester",
            "--mode",
            "post-land",
            "--line",
            "backlog-1",
            "--runner",
            "github",
        ]);
        let cfg = config(&args).expect("config");
        assert!(cfg.checks.is_empty());
        assert!(github_config(&args, &cfg).is_err(), "needs --github-repo");
        let args = Args::parse_from([
            "graft-attester",
            "--mode",
            "post-land",
            "--line",
            "backlog-1",
            "--runner",
            "github",
            "--github-repo",
            "owner/graft",
            "--max-in-flight",
            "5",
        ]);
        let cfg = config(&args).expect("config");
        assert_eq!(github_config(&args, &cfg).expect("gh").max_in_flight, 5);
        assert!(
            Args::try_parse_from(["graft-attester", "--max-in-flight", "0"]).is_err(),
            "the cap is at least 1"
        );
        // Only post-land mode runs on GitHub.
        let args = Args::parse_from(["graft-attester", "--runner", "github"]);
        assert!(config(&args).is_err());
    }
}
