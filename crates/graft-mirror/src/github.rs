//! The GitHub calls the PR bridge makes (`docs/design.md` §11.3): find, open
//! and read pull requests, enqueue one in the merge queue, and read the check
//! runs on a head commit. The bridge talks to them through [`Forge`], so tests
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
use crate::bridge::clean_text;

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

/// A pull request as the bridge reads it.
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

/// The calls the PR bridge needs from GitHub.
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

/// [`Forge`] over GitHub's REST and GraphQL APIs.
pub struct HttpForge {
    client: reqwest::Client,
    api: String,
    owner: String,
    repo: String,
    token: String,
    retry: RetryPolicy,
}

impl fmt::Debug for HttpForge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpForge")
            .field("api", &self.api)
            .field("repo", &format_args!("{}/{}", self.owner, self.repo))
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// What one request came back as, before retries.
enum Answer {
    Done(Value),
    Transient(Option<Duration>, String),
    Refused(String),
}

impl HttpForge {
    /// A forge for `owner/repo` at the API root `api` (for example
    /// `https://api.github.com`), authenticated with `token`.
    pub fn new(
        api: &str,
        owner: &str,
        repo: &str,
        token: String,
        retry: RetryPolicy,
    ) -> Result<HttpForge, MirrorError> {
        check_slug("owner", owner)?;
        check_slug("repository", repo)?;
        if token.is_empty() {
            return Err(MirrorError::Config("the GitHub token is empty".to_owned()));
        }
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
            token,
            retry: RetryPolicy {
                attempts: retry.attempts.max(1),
                ..retry
            },
        })
    }

    /// The bridge's clone of the token, for `git push` over HTTPS. Kept out
    /// of argv by the caller.
    pub fn token(&self) -> &str {
        &self.token
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
        let mut auth =
            match reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token)) {
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
        if status.is_success() {
            return match resp.json::<Value>().await {
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
        let limited = status.as_u16() == 429 || (status.as_u16() == 403 && retry_after.is_some());
        if limited || status.is_server_error() {
            return Answer::Transient(retry_after, format!("status {}", status.as_u16()));
        }
        Answer::Refused(format!("status {}: {message}", status.as_u16()))
    }
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

impl Forge for HttpForge {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning_drops_control_characters_and_caps() {
        assert_eq!(clean("a\u{7}b\nc", 10), "abc");
        assert_eq!(clean("abcdef", 3), "abc");
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
        let forge = HttpForge::new(
            "https://api.github.com",
            "o",
            "r",
            "fake-token-for-tests".into(),
            RetryPolicy::default(),
        )
        .unwrap();
        assert!(!format!("{forge:?}").contains("fake-token-for-tests"));
    }
}
