//! The GitHub calls the mirror makes (`docs/design.md` §11.3): find, open
//! and read pull requests, enqueue one in the merge queue, and read the check
//! runs on a head commit. The mirror talks to them through [`Forge`], so tests
//! can stand in a fake.
//!
//! Every call is bounded: a rate limit or a 5xx answer is retried a fixed
//! number of times, waiting for `Retry-After` when GitHub gives one (capped),
//! and then reported as [`ForgeError::Transient`]. The token goes in an
//! `Authorization` header that is marked sensitive. It is never in a URL, a
//! body, an error or a `Debug` string. Text GitHub echoes back is cleaned and
//! capped before it reaches a log or a record.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::MirrorError;
use crate::credentials::{CredentialProvider, SecretToken};
use crate::text::clean_text;

/// How many characters of a GitHub message are kept.
const MESSAGE_CHARS: usize = 200;

/// Connect and read timeout for one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded retries for transient answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts in all, including the first (at least 1).
    pub attempts: u32,
    /// The wait before the second attempt; doubled each time.
    pub base: Duration,
    /// The longest any one wait may be, whatever GitHub asks.
    pub max_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: 4,
            base: Duration::from_millis(500),
            max_wait: Duration::from_secs(30),
        }
    }
}

/// A failed GitHub call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeError {
    /// Rate limited, a server error or no answer. Retry later.
    Transient(String),
    /// GitHub refused the call. Retrying the same call will not help.
    Refused(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::Transient(what) => write!(f, "GitHub is unavailable: {what}"),
            ForgeError::Refused(what) => write!(f, "GitHub refused: {what}"),
        }
    }
}

impl std::error::Error for ForgeError {}

/// A pull request as the mirror reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pull {
    /// The PR number.
    pub number: u64,
    /// Its web address.
    pub url: String,
    /// Its GraphQL node id (what the merge queue takes).
    pub node_id: String,
    /// Whether it is open.
    pub open: bool,
    /// Whether it was merged.
    pub merged: bool,
    /// The merge commit (40-hex), once merged.
    pub merge_commit: Option<String>,
    /// The head commit of its branch (40-hex).
    pub head_sha: String,
}

/// A PR to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPull {
    /// The title, already cleaned.
    pub title: String,
    /// The body, already cleaned.
    pub body: String,
    /// The branch holding the change.
    pub head: String,
    /// The branch it merges into.
    pub base: String,
}

/// One check run on a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRun {
    /// The check's name.
    pub name: String,
    /// `success`, `failure`, … (`None` while it runs).
    pub conclusion: Option<String>,
    /// Where to read it.
    pub url: String,
}

/// The calls the mirror needs from GitHub.
pub trait Forge: Send + Sync {
    /// The PR whose head is `branch`, open or not, if there is one.
    fn find_pull(
        &self,
        branch: &str,
    ) -> impl Future<Output = Result<Option<Pull>, ForgeError>> + Send;
    /// Opens a PR.
    fn create_pull(&self, new: &NewPull) -> impl Future<Output = Result<Pull, ForgeError>> + Send;
    /// Reads PR `number`.
    fn get_pull(&self, number: u64) -> impl Future<Output = Result<Pull, ForgeError>> + Send;
    /// Puts the PR with `node_id` in the merge queue. Already queued is success.
    fn enqueue(&self, node_id: &str) -> impl Future<Output = Result<(), ForgeError>> + Send;
    /// The check runs on `sha`.
    fn check_runs(
        &self,
        sha: &str,
    ) -> impl Future<Output = Result<Vec<CheckRun>, ForgeError>> + Send;
    /// A token valid now, for authenticating a git push over HTTPS.
    fn token(&self) -> impl Future<Output = Result<SecretToken, ForgeError>> + Send;
    /// Whether the PR with `node_id` is in the merge queue or set to
    /// auto-merge: GitHub may merge it at any moment.
    fn merging(&self, node_id: &str) -> impl Future<Output = Result<bool, ForgeError>> + Send;
    /// Comments `comment` on PR `number`, then closes it unmerged.
    fn close_pr(
        &self,
        number: u64,
        comment: &str,
    ) -> impl Future<Output = Result<(), ForgeError>> + Send;
}

/// An open PR as the PR import reads it (`docs/design.md` §11.3, mode B).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPull {
    /// The PR number.
    pub number: u64,
    /// Its web address.
    pub url: String,
    /// The head commit (40-hex).
    pub head_sha: String,
    /// The head branch's name (in whichever repository holds it).
    pub head_ref: String,
    /// The branch it merges into.
    pub base_ref: String,
}

/// The calls the PR import needs from GitHub.
pub trait PullSource: Send + Sync {
    /// Open PRs into `base`, oldest first, at most `limit` of them.
    fn open_pulls(
        &self,
        base: &str,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<OpenPull>, ForgeError>> + Send;
    /// Whether PR `number` is approved: some reviewer's latest review
    /// approves it and none requests changes.
    fn approved(&self, number: u64) -> impl Future<Output = Result<bool, ForgeError>> + Send;
    /// Reads PR `number`.
    fn pull(&self, number: u64) -> impl Future<Output = Result<Pull, ForgeError>> + Send;
    /// Comments `comment` on PR `number`, then closes it.
    fn close_pull(
        &self,
        number: u64,
        comment: &str,
    ) -> impl Future<Output = Result<(), ForgeError>> + Send;
}

/// Keeps a repository owner or name to what GitHub accepts.
fn check_slug(what: &str, s: &str) -> Result<(), MirrorError> {
    let ok = !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(MirrorError::Config(format!("not a GitHub {what}: {s:?}")))
    }
}

/// [`Forge`] over GitHub's REST and GraphQL APIs, authenticated by a
/// [`CredentialProvider`] (a static token or a GitHub App).
pub struct HttpForge<C> {
    client: reqwest::Client,
    api: String,
    owner: String,
    repo: String,
    credentials: C,
    retry: RetryPolicy,
}

impl<C> fmt::Debug for HttpForge<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpForge")
            .field("api", &self.api)
            .field("repo", &format_args!("{}/{}", self.owner, self.repo))
            .finish_non_exhaustive()
    }
}

/// What one request came back as, before retries.
enum Answer {
    Done(Value),
    Transient(Option<Duration>, String),
    Refused(String),
}

impl<C: CredentialProvider> HttpForge<C> {
    /// A forge for `owner/repo` at the API root `api` (for example
    /// `https://api.github.com`), authenticated by `credentials`.
    pub fn new(
        api: &str,
        owner: &str,
        repo: &str,
        credentials: C,
        retry: RetryPolicy,
    ) -> Result<HttpForge<C>, MirrorError> {
        check_slug("owner", owner)?;
        check_slug("repository", repo)?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("graft-mirror")
            .build()
            .map_err(|_| MirrorError::Config("cannot build the HTTP client".to_owned()))?;
        Ok(HttpForge {
            client,
            api: api.trim_end_matches('/').to_owned(),
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            credentials,
            retry: RetryPolicy {
                attempts: retry.attempts.max(1),
                ..retry
            },
        })
    }

    fn repo_path(&self) -> String {
        format!("/repos/{}/{}", self.owner, self.repo)
    }

    /// One request, retried while the answer is transient.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, ForgeError> {
        let url = format!("{}{path}", self.api);
        let mut last = String::new();
        for attempt in 0..self.retry.attempts {
            match self.once(&method, &url, query, body).await {
                Answer::Done(v) => return Ok(v),
                Answer::Refused(what) => return Err(ForgeError::Refused(what)),
                Answer::Transient(retry_after, what) => {
                    last = what;
                    if attempt + 1 < self.retry.attempts {
                        let backoff = self.retry.base.saturating_mul(1 << attempt.min(16));
                        let wait = retry_after.unwrap_or(backoff).min(self.retry.max_wait);
                        tokio::time::sleep(wait).await;
                    }
                }
            }
        }
        Err(ForgeError::Transient(last))
    }

    async fn once(
        &self,
        method: &reqwest::Method,
        url: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Answer {
        let token = match self.credentials.token().await {
            Ok(t) => t,
            Err(ForgeError::Transient(what)) => return Answer::Transient(None, what),
            Err(ForgeError::Refused(what)) => return Answer::Refused(what),
        };
        let mut auth =
            match reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.expose())) {
                Ok(v) => v,
                Err(_) => {
                    return Answer::Refused("the token is not a valid header value".to_owned());
                }
            };
        auth.set_sensitive(true);
        let mut req = self
            .client
            .request(method.clone(), url)
            .query(query)
            .header(reqwest::header::AUTHORIZATION, auth)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            // reqwest's error text can carry the URL, never the header.
            Err(e) => return Answer::Transient(None, format!("no answer ({})", kind(&e))),
        };
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        // The primary rate limit: the budget is spent, and the reset time says
        // when it comes back. GitHub sends no Retry-After for it.
        let exhausted = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            == Some("0");
        let reset = header_u64(&resp, "x-ratelimit-reset").map(reset_wait);
        if status.is_success() {
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return Answer::Transient(None, format!("no answer ({})", kind(&e))),
            };
            // 204 No Content (a DELETE) has no body.
            if body.is_empty() {
                return Answer::Done(Value::Null);
            }
            return match serde_json::from_slice::<Value>(&body) {
                Ok(v) if graphql_rate_limited(&v) => {
                    Answer::Transient(None, "GraphQL rate limit".to_owned())
                }
                Ok(v) => Answer::Done(v),
                Err(_) => Answer::Refused("GitHub answered with malformed JSON".to_owned()),
            };
        }
        let message = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
            .map(|m| clean(&m, MESSAGE_CHARS))
            .unwrap_or_default();
        let limited = status.as_u16() == 429
            || (status.as_u16() == 403 && (retry_after.is_some() || exhausted));
        if limited {
            return Answer::Transient(
                retry_after.or(reset),
                format!("rate limited (status {})", status.as_u16()),
            );
        }
        if status.is_server_error() {
            return Answer::Transient(retry_after, format!("status {}", status.as_u16()));
        }
        Answer::Refused(format!("status {}: {message}", status.as_u16()))
    }
}

/// A repository ruleset, as the authority gate reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ruleset {
    /// The ruleset's id.
    pub id: u64,
    /// The ids of the actors that may bypass it.
    pub bypass: Vec<u64>,
}

/// Rulesets one listing reads. The gate's ruleset is found by name among
/// the repository's own rulesets, which are few.
const RULESETS_PAGE: u32 = 100;

impl<C: CredentialProvider> HttpForge<C> {
    /// The repository ruleset named `name`, if there is one. Reads one page
    /// of the repository's own rulesets, then the match.
    pub async fn ruleset_named(&self, name: &str) -> Result<Option<Ruleset>, ForgeError> {
        #[derive(Deserialize)]
        struct Listed {
            id: u64,
            name: String,
        }
        #[derive(Deserialize)]
        struct Actor {
            actor_id: Option<u64>,
        }
        #[derive(Deserialize)]
        struct Full {
            id: u64,
            #[serde(default)]
            bypass_actors: Vec<Actor>,
        }
        let path = format!("{}/rulesets", self.repo_path());
        let query = [
            ("per_page", RULESETS_PAGE.to_string()),
            ("includes_parents", "false".to_owned()),
        ];
        let v = self.send(reqwest::Method::GET, &path, &query, None).await?;
        let listed: Vec<Listed> = serde_json::from_value(v).map_err(|_| {
            ForgeError::Refused("GitHub's rulesets have an unexpected shape".into())
        })?;
        let Some(found) = listed.into_iter().find(|r| r.name == name) else {
            return Ok(None);
        };
        let path = format!("{}/rulesets/{}", self.repo_path(), found.id);
        let v = self.send(reqwest::Method::GET, &path, &[], None).await?;
        let full: Full = serde_json::from_value(v)
            .map_err(|_| ForgeError::Refused("GitHub's ruleset has an unexpected shape".into()))?;
        Ok(Some(Ruleset {
            id: full.id,
            bypass: full
                .bypass_actors
                .into_iter()
                .filter_map(|a| a.actor_id)
                .collect(),
        }))
    }

    /// Creates a repository ruleset (`id` `None`) or replaces ruleset `id`
    /// with `body`.
    pub async fn put_ruleset(&self, id: Option<u64>, body: &Value) -> Result<(), ForgeError> {
        let (method, path) = match id {
            None => (
                reqwest::Method::POST,
                format!("{}/rulesets", self.repo_path()),
            ),
            Some(id) => (
                reqwest::Method::PUT,
                format!("{}/rulesets/{id}", self.repo_path()),
            ),
        };
        self.send(method, &path, &[], Some(body)).await.map(drop)
    }

    /// Deletes ruleset `id`.
    pub async fn delete_ruleset(&self, id: u64) -> Result<(), ForgeError> {
        let path = format!("{}/rulesets/{id}", self.repo_path());
        self.send(reqwest::Method::DELETE, &path, &[], None)
            .await
            .map(drop)
    }
}

/// A numeric response header, if it is present and well formed.
fn header_u64(resp: &reqwest::Response, name: &str) -> Option<u64> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
}

/// How long until a rate limit resets, from its reset time (epoch seconds).
fn reset_wait(reset: u64) -> Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Duration::from_secs(reset.saturating_sub(now))
}

/// Whether a GraphQL reply (HTTP 200 with `errors`) says the rate limit is hit.
fn graphql_rate_limited(v: &Value) -> bool {
    v.get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errs| {
            errs.iter()
                .any(|e| e.get("type").and_then(Value::as_str) == Some("RATE_LIMITED"))
        })
}

/// A short name for a transport error, without its URL.
fn kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "cannot connect"
    } else {
        "transport error"
    }
}

/// Cleans text GitHub sent back (see [`clean_text`]).
fn clean(text: &str, max: usize) -> String {
    clean_text(text, max, false)
}

#[derive(Deserialize)]
struct RawPull {
    number: u64,
    html_url: String,
    node_id: String,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    head: RawHead,
}

#[derive(Deserialize)]
struct RawHead {
    sha: String,
}

#[derive(Deserialize)]
struct RawChecks {
    check_runs: Vec<RawCheck>,
}

#[derive(Deserialize)]
struct RawCheck {
    name: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: String,
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn pull_from(v: Value) -> Result<Pull, ForgeError> {
    let raw: RawPull = serde_json::from_value(v)
        .map_err(|_| ForgeError::Refused("GitHub's pull request has an unexpected shape".into()))?;
    if !is_sha(&raw.head.sha) || raw.merge_commit_sha.as_deref().is_some_and(|s| !is_sha(s)) {
        return Err(ForgeError::Refused(
            "GitHub gave a malformed commit id".into(),
        ));
    }
    Ok(Pull {
        number: raw.number,
        url: raw.html_url,
        node_id: raw.node_id,
        open: raw.state == "open",
        merged: raw.merged,
        merge_commit: raw.merge_commit_sha,
        head_sha: raw.head.sha,
    })
}

impl<C: CredentialProvider> Forge for HttpForge<C> {
    async fn token(&self) -> Result<SecretToken, ForgeError> {
        self.credentials.token().await
    }

    async fn merging(&self, node_id: &str) -> Result<bool, ForgeError> {
        let body = json!({
            "query": "query($id: ID!) { node(id: $id) { ... on PullRequest { isInMergeQueue autoMergeRequest { enabledAt } } } }",
            "variables": { "id": node_id },
        });
        let v = self
            .send(reqwest::Method::POST, "/graphql", &[], Some(&body))
            .await?;
        if let Some(err) = v
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|e| e.first())
        {
            let text = err
                .get("message")
                .and_then(Value::as_str)
                .map(|m| clean(m, MESSAGE_CHARS))
                .unwrap_or_default();
            return Err(ForgeError::Refused(format!("pull request state: {text}")));
        }
        let pr = v.pointer("/data/node").ok_or_else(|| {
            ForgeError::Refused("GitHub's pull request state has an unexpected shape".into())
        })?;
        let queued = pr
            .get("isInMergeQueue")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let auto = pr.get("autoMergeRequest").is_some_and(|a| !a.is_null());
        Ok(queued || auto)
    }

    async fn close_pr(&self, number: u64, comment: &str) -> Result<(), ForgeError> {
        PullSource::close_pull(self, number, comment).await
    }

    async fn find_pull(&self, branch: &str) -> Result<Option<Pull>, ForgeError> {
        let query = [
            ("head", format!("{}:{branch}", self.owner)),
            ("state", "all".to_owned()),
            ("per_page", "5".to_owned()),
        ];
        let path = format!("{}/pulls", self.repo_path());
        let v = self.send(reqwest::Method::GET, &path, &query, None).await?;
        let list = v
            .as_array()
            .ok_or_else(|| ForgeError::Refused("GitHub's pull list is not a list".into()))?;
        match list.first() {
            Some(first) => pull_from(first.clone()).map(Some),
            None => Ok(None),
        }
    }

    async fn create_pull(&self, new: &NewPull) -> Result<Pull, ForgeError> {
        let body = json!({
            "title": new.title,
            "body": new.body,
            "head": new.head,
            "base": new.base,
        });
        let path = format!("{}/pulls", self.repo_path());
        let v = self
            .send(reqwest::Method::POST, &path, &[], Some(&body))
            .await?;
        pull_from(v)
    }

    async fn get_pull(&self, number: u64) -> Result<Pull, ForgeError> {
        let path = format!("{}/pulls/{number}", self.repo_path());
        let v = self.send(reqwest::Method::GET, &path, &[], None).await?;
        pull_from(v)
    }

    async fn enqueue(&self, node_id: &str) -> Result<(), ForgeError> {
        let body = json!({
            "query": "mutation($id: ID!) { enqueuePullRequest(input: {pullRequestId: $id}) { clientMutationId } }",
            "variables": { "id": node_id },
        });
        let v = self
            .send(reqwest::Method::POST, "/graphql", &[], Some(&body))
            .await?;
        match v
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|e| e.first())
        {
            None => Ok(()),
            Some(err) => {
                let text = err
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|m| clean(m, MESSAGE_CHARS))
                    .unwrap_or_default();
                // Queued already is the outcome asked for.
                if text.contains("already") {
                    Ok(())
                } else {
                    Err(ForgeError::Refused(format!("merge queue: {text}")))
                }
            }
        }
    }

    async fn check_runs(&self, sha: &str) -> Result<Vec<CheckRun>, ForgeError> {
        if !is_sha(sha) {
            return Err(ForgeError::Refused("not a commit id".into()));
        }
        let path = format!("{}/commits/{sha}/check-runs", self.repo_path());
        let v = self
            .send(
                reqwest::Method::GET,
                &path,
                &[("per_page", "100".to_owned())],
                None,
            )
            .await?;
        let raw: RawChecks = serde_json::from_value(v).map_err(|_| {
            ForgeError::Refused("GitHub's check runs have an unexpected shape".into())
        })?;
        Ok(raw
            .check_runs
            .into_iter()
            .map(|c| CheckRun {
                name: clean(&c.name, MESSAGE_CHARS),
                conclusion: c.conclusion,
                url: c.html_url,
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct RawOpenPull {
    number: u64,
    html_url: String,
    head: RawRef,
    base: RawRef,
}

#[derive(Deserialize)]
struct RawRef {
    sha: String,
    #[serde(rename = "ref")]
    name: String,
}

#[derive(Deserialize)]
struct RawReview {
    #[serde(default)]
    user: Option<RawUser>,
    state: String,
}

#[derive(Deserialize)]
struct RawUser {
    id: u64,
}

/// Whether reviews (oldest first) approve: each reviewer's latest deciding
/// review counts, a comment does not change it.
fn reviews_approve(reviews: &[RawReview]) -> bool {
    let mut latest: Vec<(u64, &str)> = Vec::new();
    for r in reviews {
        let Some(user) = &r.user else { continue };
        let state = r.state.as_str();
        if !matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue;
        }
        match latest.iter_mut().find(|(id, _)| *id == user.id) {
            Some(slot) => slot.1 = state,
            None => latest.push((user.id, state)),
        }
    }
    latest.iter().any(|(_, s)| *s == "APPROVED")
        && !latest.iter().any(|(_, s)| *s == "CHANGES_REQUESTED")
}

impl<C: CredentialProvider> PullSource for HttpForge<C> {
    async fn open_pulls(&self, base: &str, limit: u32) -> Result<Vec<OpenPull>, ForgeError> {
        let query = [
            ("state", "open".to_owned()),
            ("base", base.to_owned()),
            ("sort", "created".to_owned()),
            ("direction", "asc".to_owned()),
            ("per_page", limit.clamp(1, 100).to_string()),
        ];
        let path = format!("{}/pulls", self.repo_path());
        let v = self.send(reqwest::Method::GET, &path, &query, None).await?;
        let raw: Vec<RawOpenPull> = serde_json::from_value(v).map_err(|_| {
            ForgeError::Refused("GitHub's pull list has an unexpected shape".into())
        })?;
        let mut out = Vec::with_capacity(raw.len());
        for p in raw {
            if !is_sha(&p.head.sha) {
                return Err(ForgeError::Refused(
                    "GitHub gave a malformed commit id".into(),
                ));
            }
            out.push(OpenPull {
                number: p.number,
                url: p.html_url,
                head_sha: p.head.sha,
                head_ref: clean(&p.head.name, MESSAGE_CHARS),
                base_ref: clean(&p.base.name, MESSAGE_CHARS),
            });
        }
        Ok(out)
    }

    async fn approved(&self, number: u64) -> Result<bool, ForgeError> {
        let path = format!("{}/pulls/{number}/reviews", self.repo_path());
        let v = self
            .send(
                reqwest::Method::GET,
                &path,
                &[("per_page", "100".to_owned())],
                None,
            )
            .await?;
        let raw: Vec<RawReview> = serde_json::from_value(v)
            .map_err(|_| ForgeError::Refused("GitHub's reviews have an unexpected shape".into()))?;
        Ok(reviews_approve(&raw))
    }

    async fn pull(&self, number: u64) -> Result<Pull, ForgeError> {
        self.get_pull(number).await
    }

    async fn close_pull(&self, number: u64, comment: &str) -> Result<(), ForgeError> {
        let path = format!("{}/issues/{number}/comments", self.repo_path());
        let body = json!({ "body": comment });
        self.send(reqwest::Method::POST, &path, &[], Some(&body))
            .await?;
        let path = format!("{}/pulls/{number}", self.repo_path());
        let body = json!({ "state": "closed" });
        self.send(reqwest::Method::PATCH, &path, &[], Some(&body))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::StaticToken;

    #[test]
    fn cleaning_drops_control_characters_and_caps() {
        assert_eq!(clean("a\u{7}b\nc", 10), "abc");
        assert_eq!(clean("abcdef", 3), "abc");
    }

    #[test]
    fn the_latest_review_of_each_reviewer_decides() {
        let r = |id: u64, state: &str| RawReview {
            user: Some(RawUser { id }),
            state: state.to_owned(),
        };
        assert!(!reviews_approve(&[]));
        assert!(reviews_approve(&[r(1, "APPROVED"), r(1, "COMMENTED")]));
        assert!(!reviews_approve(&[
            r(1, "APPROVED"),
            r(2, "CHANGES_REQUESTED")
        ]));
        assert!(reviews_approve(&[
            r(2, "CHANGES_REQUESTED"),
            r(1, "APPROVED"),
            r(2, "APPROVED")
        ]));
        assert!(!reviews_approve(&[r(1, "APPROVED"), r(1, "DISMISSED")]));
    }

    #[test]
    fn slugs_are_checked() {
        assert!(check_slug("owner", "indexarr").is_ok());
        assert!(check_slug("owner", "a/b").is_err());
        assert!(check_slug("owner", "").is_err());
    }

    #[test]
    fn malformed_commit_ids_are_refused() {
        let v = json!({
            "number": 1, "html_url": "u", "node_id": "n", "state": "open",
            "head": { "sha": "not-a-sha" }
        });
        assert!(matches!(pull_from(v), Err(ForgeError::Refused(_))));
    }

    #[test]
    fn a_token_never_prints() {
        let token = SecretToken::new("fake-token-for-tests".to_owned()).unwrap();
        let forge = HttpForge::new(
            "https://api.github.com",
            "o",
            "r",
            StaticToken::new(token),
            RetryPolicy::default(),
        )
        .unwrap();
        assert!(!format!("{forge:?}").contains("fake-token-for-tests"));
    }
}
