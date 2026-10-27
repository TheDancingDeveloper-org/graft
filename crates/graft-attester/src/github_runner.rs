//! A post-land [`Runner`] that runs the checks as a GitHub Actions workflow
//! (WI-2625; design notes §6 item 2). The checker runs nothing itself: for
//! each (snapshot, attempt) it dispatches the workflow once, on the ref the
//! mirror pushed for that changeset, `<ref_prefix><changeset>`, with the
//! inputs `changeset`, `attempt` and `request_id`. The workflow's
//! `run-name` must carry the `request_id`, which is how the run is found
//! again, after a restart included: a run already there is never
//! dispatched again.
//!
//! The run's jobs map to the line's checks: check `ci/fmt` reads job `fmt`
//! (the name after the last `/`, or an explicit map), and a job of a called
//! workflow, `<caller> / fmt`, counts as `fmt`. The job's page is the run's
//! link ([`Run::url`]). The workflow attests the passes itself, so
//! [`Runner::attests_passes`] is true and the checker posts only those
//! still missing.
//!
//! What counts as a result, and what does not:
//!
//! - **No verdict** ([`Error::Forge`], tried again later): GitHub
//!   unreachable, a 5xx, a rate limit or a refused credential, the
//!   changeset's ref not pushed yet (422 `No ref found`), and a run that
//!   is not found or still queued after the start timeout. A GitHub outage
//!   therefore never makes a land bad, and never reverts one.
//! - **A failed run**: a completed job that did not succeed, a run past
//!   the run timeout once it started (it is cancelled), a run that
//!   completed without the job, and any other 422 on dispatch (the
//!   workflow is not on that ref, or has no `workflow_dispatch`).
//!
//! The token comes from a file (mode 0600 or stricter) opened read-only:
//! the file is graft-mirror's, shared, and never written. It travels only
//! in an `Authorization` header marked sensitive, never in a URL, a body,
//! an error, a log line or a `Debug` string.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use graft_model::ChangesetId;
use serde_json::{Value, json};

use crate::post_land::{Run, Runner};
use crate::{Check, Error, Outcome, log};

/// The GitHub REST API root.
pub const DEFAULT_API: &str = "https://api.github.com";

/// The workflow dispatched for each (snapshot, attempt).
pub const DEFAULT_WORKFLOW: &str = "graft-post-land.yml";

/// Connect and read timeout for one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Characters of a GitHub message kept in an error.
const MESSAGE_CHARS: usize = 200;

/// Most runs of one ref read when looking for a request's run.
const RUNS_PAGE: u32 = 50;

/// Most jobs of one run read.
const JOBS_PAGE: u32 = 100;

/// A GitHub token. Its `Debug` and `Display` never show it.
#[derive(Clone)]
pub struct GitHubToken(String);

impl GitHubToken {
    /// Wraps `value`; `None` when it is empty.
    pub fn new(value: impl Into<String>) -> Option<GitHubToken> {
        let value = value.into();
        (!value.is_empty()).then_some(GitHubToken(value))
    }

    /// Reads the token file at `path`, refusing one that group or others
    /// can read. The file is opened read-only and never written: it is
    /// shared with graft-mirror. Errors name the path only.
    pub fn read(path: &Path) -> Result<GitHubToken, String> {
        read_secret(path).map(GitHubToken)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubToken(<redacted>)")
    }
}

/// Reads a secret from `path`: mode 0600 or stricter, opened read-only,
/// trimmed, not empty. The value is never printed; errors name the path.
pub fn read_secret(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{}: readable by group or others; make it 0600",
            path.display()
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let value = text.trim().to_owned();
    if value.is_empty() {
        return Err(format!("{}: empty", path.display()));
    }
    Ok(value)
}

/// Where and how the workflow runs.
#[derive(Clone, Debug)]
pub struct GitHubConfig {
    /// API root, for example [`DEFAULT_API`].
    pub api: String,
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Workflow file name (or id), for example [`DEFAULT_WORKFLOW`].
    pub workflow: String,
    /// Prefix of the ref dispatched on; the changeset id follows.
    pub ref_prefix: String,
    /// Pause between polls of a run.
    pub poll: Duration,
    /// Longest a dispatched run may take to be found and to leave the
    /// queue; past it the run is an error (no verdict), not a failure.
    pub start_timeout: Duration,
    /// Longest a started run may run; past it the run is cancelled and its
    /// checks failed.
    pub run_timeout: Duration,
    /// Check name to job name, for checks whose job is not the last `/`
    /// segment of the name.
    pub jobs: BTreeMap<String, String>,
}

impl GitHubConfig {
    /// A configuration for `owner/repo` (the `owner/name` slug) with the
    /// default API, workflow and limits.
    pub fn new(slug: &str, ref_prefix: impl Into<String>) -> Result<GitHubConfig, String> {
        let (owner, repo) = slug
            .split_once('/')
            .filter(|(o, r)| valid_slug(o) && valid_slug(r))
            .ok_or_else(|| format!("GitHub repository {slug:?}: expected owner/name"))?;
        Ok(GitHubConfig {
            api: DEFAULT_API.to_owned(),
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            workflow: DEFAULT_WORKFLOW.to_owned(),
            ref_prefix: ref_prefix.into(),
            poll: Duration::from_secs(15),
            start_timeout: Duration::from_secs(30 * 60),
            run_timeout: Duration::from_secs(60 * 60),
            jobs: BTreeMap::new(),
        })
    }

    /// The job that reports `check`.
    pub fn job_of(&self, check: &str) -> String {
        match self.jobs.get(check) {
            Some(job) => job.clone(),
            None => check.rsplit('/').next().unwrap_or(check).to_owned(),
        }
    }
}

fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The id the run of `(at, attempt)` is found by. The same pair always
/// gives the same id, so a restart finds the run it dispatched.
pub fn request_id(at: ChangesetId, attempt: u32) -> String {
    format!("{at}-{attempt}")
}

/// One job's result.
#[derive(Clone, Debug)]
struct Job {
    name: String,
    conclusion: String,
    url: String,
}

/// How one dispatched run ended.
#[derive(Clone, Debug)]
enum Finished {
    /// It completed with these jobs.
    Completed { url: String, jobs: Vec<Job> },
    /// It ran past the run timeout and was cancelled.
    TimedOut { url: String },
    /// GitHub refused the dispatch for this ref (422).
    Refused { url: String, why: String },
}

impl Finished {
    fn run_of(&self, job: &str) -> Run {
        match self {
            Finished::Completed { url, jobs } => {
                let called = format!("/ {job}");
                match jobs
                    .iter()
                    .find(|j| j.name == job || j.name.ends_with(&called))
                {
                    Some(j) => Run {
                        outcome: match j.conclusion.as_str() {
                            "success" => Outcome::Passed,
                            "timed_out" => Outcome::TimedOut,
                            _ => Outcome::Failed(Some(1)),
                        },
                        url: j.url.clone(),
                    },
                    None => Run {
                        outcome: Outcome::NotStarted(format!("no job {job} in the run")),
                        url: url.clone(),
                    },
                }
            }
            Finished::TimedOut { url } => Run {
                outcome: Outcome::TimedOut,
                url: url.clone(),
            },
            Finished::Refused { url, why } => Run {
                outcome: Outcome::NotStarted(why.clone()),
                url: url.clone(),
            },
        }
    }
}

/// A run as listed.
struct RunView {
    id: u64,
    status: String,
    url: String,
}

/// The runner over GitHub Actions.
pub struct GitHubRunner {
    client: reqwest::Client,
    cfg: GitHubConfig,
    token: GitHubToken,
    /// The snapshot `done` holds runs of.
    at: Option<ChangesetId>,
    /// Finished runs of `at`, by attempt.
    done: BTreeMap<u32, Finished>,
    /// Dispatches sent, in all (for metrics and tests).
    dispatched: u64,
}

impl fmt::Debug for GitHubRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubRunner")
            .field("cfg", &self.cfg)
            .field("token", &self.token)
            .field("dispatched", &self.dispatched)
            .finish_non_exhaustive()
    }
}

/// One answer: its status and its JSON body (`Null` when none).
struct Answer {
    status: u16,
    body: Value,
}

impl GitHubRunner {
    /// A runner for `cfg`, authenticated by `token`.
    pub fn new(cfg: GitHubConfig, token: GitHubToken) -> Result<GitHubRunner, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("graft-attester")
            .build()
            .map_err(|_| "cannot build the HTTP client".to_owned())?;
        Ok(GitHubRunner {
            client,
            cfg: GitHubConfig {
                api: cfg.api.trim_end_matches('/').to_owned(),
                ..cfg
            },
            token,
            at: None,
            done: BTreeMap::new(),
            dispatched: 0,
        })
    }

    /// Workflow dispatches sent so far.
    pub fn dispatched(&self) -> u64 {
        self.dispatched
    }

    fn repo_path(&self) -> String {
        format!("/repos/{}/{}", self.cfg.owner, self.cfg.repo)
    }

    /// The workflow's page, the link of a run that never ran.
    fn workflow_url(&self) -> String {
        let web = if self.cfg.api == DEFAULT_API {
            "https://github.com"
        } else {
            self.cfg.api.as_str()
        };
        format!(
            "{web}/{}/{}/actions/workflows/{}",
            self.cfg.owner, self.cfg.repo, self.cfg.workflow
        )
    }

    /// GitHub's message in `body`, cleaned, capped and without the token.
    fn message(&self, body: &Value) -> String {
        let raw = match body.get("message").and_then(Value::as_str) {
            Some(m) => m.to_owned(),
            None => body.as_str().unwrap_or_default().to_owned(),
        };
        raw.replace(self.token.expose(), "<redacted>")
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MESSAGE_CHARS)
            .collect()
    }

    fn forge_error(&self, what: &str, answer: &Answer) -> Error {
        let message = self.message(&answer.body);
        Error::Forge(if message.is_empty() {
            format!("{what}: GitHub answered {}", answer.status)
        } else {
            format!("{what}: GitHub answered {}: {message}", answer.status)
        })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Answer, Error> {
        let mut auth =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token.expose()))
                .map_err(|_| Error::Forge("the GitHub token is not a header value".to_owned()))?;
        auth.set_sensitive(true);
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.cfg.api))
            .query(query)
            .header(reqwest::header::AUTHORIZATION, auth)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(b) = body {
            req = req.json(b);
        }
        // reqwest's error text can carry the URL; only its kind is kept.
        let resp = req.send().await.map_err(|e| {
            let kind = if e.is_timeout() {
                "timed out"
            } else if e.is_connect() {
                "cannot connect"
            } else {
                "request failed"
            };
            Error::Forge(format!("GitHub: no answer ({kind})"))
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok(Answer { status, body })
    }

    /// The newest run of `ref_name` whose title carries `rid`.
    async fn find(&self, ref_name: &str, rid: &str) -> Result<Option<RunView>, Error> {
        let path = format!(
            "{}/actions/workflows/{}/runs",
            self.repo_path(),
            self.cfg.workflow
        );
        let query = [
            ("branch", ref_name.to_owned()),
            ("event", "workflow_dispatch".to_owned()),
            ("per_page", RUNS_PAGE.to_string()),
        ];
        let answer = self.send(reqwest::Method::GET, &path, &query, None).await?;
        if answer.status != 200 {
            return Err(self.forge_error("listing runs", &answer));
        }
        let runs = answer
            .body
            .get("workflow_runs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(runs
            .iter()
            .filter(|r| {
                ["display_title", "name"].iter().any(|k| {
                    r.get(*k)
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.split_whitespace().any(|w| w == rid))
                })
            })
            .filter_map(run_view)
            .max_by_key(|r| r.id))
    }

    async fn get_run(&self, id: u64) -> Result<RunView, Error> {
        let path = format!("{}/actions/runs/{id}", self.repo_path());
        let answer = self.send(reqwest::Method::GET, &path, &[], None).await?;
        if answer.status != 200 {
            return Err(self.forge_error("reading a run", &answer));
        }
        run_view(&answer.body).ok_or_else(|| Error::Forge("GitHub: a run without an id".to_owned()))
    }

    async fn jobs(&self, id: u64) -> Result<Vec<Job>, Error> {
        let path = format!("{}/actions/runs/{id}/jobs", self.repo_path());
        let query = [("per_page", JOBS_PAGE.to_string())];
        let answer = self.send(reqwest::Method::GET, &path, &query, None).await?;
        if answer.status != 200 {
            return Err(self.forge_error("reading a run's jobs", &answer));
        }
        let text = |j: &Value, k: &str| {
            j.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Ok(answer
            .body
            .get("jobs")
            .and_then(Value::as_array)
            .map(|jobs| {
                jobs.iter()
                    .map(|j| Job {
                        name: text(j, "name"),
                        conclusion: text(j, "conclusion"),
                        url: text(j, "html_url"),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Dispatches the workflow; `Some(why)` when GitHub refuses it for this
    /// ref for good.
    async fn dispatch(
        &mut self,
        ref_name: &str,
        at: ChangesetId,
        attempt: u32,
        rid: &str,
    ) -> Result<Option<String>, Error> {
        let path = format!(
            "{}/actions/workflows/{}/dispatches",
            self.repo_path(),
            self.cfg.workflow
        );
        let body = json!({
            "ref": ref_name,
            "inputs": {
                "changeset": at.to_string(),
                "attempt": attempt.to_string(),
                "request_id": rid,
            },
        });
        let answer = self
            .send(reqwest::Method::POST, &path, &[], Some(&body))
            .await?;
        match answer.status {
            200..=299 => {
                self.dispatched += 1;
                Ok(None)
            }
            // The mirror has not pushed the ref yet: try again later.
            422 if self.message(&answer.body).starts_with("No ref found") => {
                Err(self.forge_error("dispatching", &answer))
            }
            422 => Ok(Some(format!(
                "GitHub refused the dispatch on {ref_name}: {}",
                self.message(&answer.body)
            ))),
            _ => Err(self.forge_error("dispatching", &answer)),
        }
    }

    /// Cancels run `id`, best effort.
    async fn cancel(&self, id: u64) {
        let path = format!("{}/actions/runs/{id}/cancel", self.repo_path());
        if let Err(e) = self.send(reqwest::Method::POST, &path, &[], None).await {
            log(&format!("cancelling run {id}: {e}"));
        }
    }

    /// Finds or dispatches the run of `(at, attempt)` and waits for it.
    async fn finish(&mut self, at: ChangesetId, attempt: u32) -> Result<Finished, Error> {
        let ref_name = format!("{}{at}", self.cfg.ref_prefix);
        let rid = request_id(at, attempt);
        let since = Instant::now();
        let mut run = self.find(&ref_name, &rid).await?;
        if run.is_none() {
            if let Some(why) = self.dispatch(&ref_name, at, attempt, &rid).await? {
                return Ok(Finished::Refused {
                    url: self.workflow_url(),
                    why,
                });
            }
            run = self.find(&ref_name, &rid).await?;
        }
        let mut started: Option<Instant> = None;
        loop {
            match &run {
                None => {}
                Some(r) if r.status == "completed" => {
                    let jobs = self.jobs(r.id).await?;
                    return Ok(Finished::Completed {
                        url: r.url.clone(),
                        jobs,
                    });
                }
                Some(r) if queued(&r.status) => {}
                Some(r) => {
                    let s = *started.get_or_insert_with(Instant::now);
                    if s.elapsed() >= self.cfg.run_timeout {
                        self.cancel(r.id).await;
                        return Ok(Finished::TimedOut { url: r.url.clone() });
                    }
                }
            }
            if started.is_none() && since.elapsed() >= self.cfg.start_timeout {
                return Err(Error::Forge(match &run {
                    None => format!("run {rid} on {ref_name} not found after dispatch"),
                    Some(_) => format!("run {rid} on {ref_name} still queued"),
                }));
            }
            tokio::time::sleep(self.cfg.poll).await;
            run = match &run {
                Some(r) => Some(self.get_run(r.id).await?),
                None => self.find(&ref_name, &rid).await?,
            };
        }
    }
}

/// Statuses of a run that has not started.
fn queued(status: &str) -> bool {
    matches!(status, "queued" | "waiting" | "pending" | "requested")
}

fn run_view(v: &Value) -> Option<RunView> {
    Some(RunView {
        id: v.get("id")?.as_u64()?,
        status: v
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        url: v
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

impl<A> Runner<A> for GitHubRunner {
    async fn run(
        &mut self,
        _api: &A,
        at: ChangesetId,
        check: &Check,
        attempt: u32,
    ) -> Result<Run, Error> {
        if self.at != Some(at) {
            self.at = Some(at);
            self.done.clear();
        }
        if !self.done.contains_key(&attempt) {
            let finished = self.finish(at, attempt).await?;
            self.done.insert(attempt, finished);
        }
        Ok(self.done[&attempt].run_of(&self.cfg.job_of(&check.name)))
    }

    async fn release(&mut self) {
        self.at = None;
        self.done.clear();
    }

    fn attests_passes(&self) -> bool {
        true
    }
}
