//! The Forgejo calls the mirror makes (WI-3492): the batcher's [`Forge`]
//! over Forgejo's REST API (`/api/v1`), and the one-shot batch landing that
//! stands in for `land-pr` on a forge with no merge queue.
//!
//! Requests go through [`HttpForge`]'s transport in the Forgejo flavor, so
//! the retry bounds, the rate-limit handling and the token rules are the
//! GitHub client's: the token goes in a sensitive `Authorization` header,
//! never in a URL, a body, an error or a `Debug` string.
//!
//! Forgejo has no merge queue and no GraphQL node ids. A PR's "node id" here
//! is its number, and [`Forge::merging`] is always `false`: nothing merges a
//! batch PR but [`land_once`], which merges with `Do=rebase` (each land keeps
//! its commit, D28) once every required status context is `success`.

use std::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::MirrorError;
use crate::credentials::{CredentialProvider, SecretToken};
use crate::github::{
    Flavor, Forge, ForgeError, HttpForge, MESSAGE_CHARS, NewPull, Pull, RetryPolicy, clean, is_sha,
    parse_utc_ms,
};

/// PRs one `find_pull` listing reads, most recently updated first. Forgejo
/// caps a page at 50 by default; a batch branch's PR is a recent one.
const FIND_PAGE: u32 = 50;

/// Status contexts one combined-status read returns.
const STATUS_PAGE: u32 = 50;

/// [`Forge`] over Forgejo's REST API, authenticated by a token.
pub struct ForgejoForge<C> {
    http: HttpForge<C>,
}

impl<C> fmt::Debug for ForgejoForge<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (owner, repo) = self.http.owner_repo();
        f.debug_struct("ForgejoForge")
            .field("repo", &format_args!("{owner}/{repo}"))
            .finish_non_exhaustive()
    }
}

impl<C: CredentialProvider> ForgejoForge<C> {
    /// A forge for `owner/repo` at the API root `api` (for example
    /// `https://forgejo.example/api/v1`), authenticated by `credentials`.
    pub fn new(
        api: &str,
        owner: &str,
        repo: &str,
        credentials: C,
        retry: RetryPolicy,
    ) -> Result<ForgejoForge<C>, MirrorError> {
        if !(api.starts_with("https://") || api.starts_with("http://")) {
            return Err(MirrorError::Config(format!(
                "[forgejo] api must be an http(s) URL: {api:?}"
            )));
        }
        let http =
            HttpForge::new(api, owner, repo, credentials, retry)?.with_flavor(Flavor::Forgejo);
        Ok(ForgejoForge { http })
    }

    /// The latest status of each context on commit `sha`.
    pub async fn statuses(&self, sha: &str) -> Result<Vec<Status>, ForgeError> {
        if !is_sha(sha) {
            return Err(ForgeError::Refused("not a commit id".into()));
        }
        let path = format!("{}/commits/{sha}/status", self.http.repo_path());
        let query = [("limit", STATUS_PAGE.to_string())];
        let v = self
            .http
            .send(reqwest::Method::GET, &path, &query, None)
            .await?;
        let raw: RawCombined = serde_json::from_value(v).map_err(|_| {
            ForgeError::Refused("Forgejo's commit status has an unexpected shape".into())
        })?;
        Ok(raw
            .statuses
            .into_iter()
            .map(|s| Status {
                context: clean(&s.context, MESSAGE_CHARS),
                state: clean(&s.status, 20),
                url: clean(&s.target_url, 300),
            })
            .collect())
    }

    /// Rebase-merges PR `number`, only while its head is still `head_sha`.
    pub async fn merge_rebase(&self, number: u64, head_sha: &str) -> Result<(), ForgeError> {
        if !is_sha(head_sha) {
            return Err(ForgeError::Refused("not a commit id".into()));
        }
        let path = format!("{}/pulls/{number}/merge", self.http.repo_path());
        let body = json!({
            "Do": "rebase",
            "head_commit_id": head_sha,
            "delete_branch_after_merge": false,
        });
        self.http
            .send(reqwest::Method::POST, &path, &[], Some(&body))
            .await
            .map(drop)
    }
}

/// One status context on a commit, as Forgejo reports its latest state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The context (`ci/woodpecker/pr/test`, `ci / test (pull_request)`).
    pub context: String,
    /// `pending`, `success`, `error`, `failure` or `warning`.
    pub state: String,
    /// Where to read it.
    pub url: String,
}

#[derive(Deserialize)]
struct RawCombined {
    #[serde(default)]
    statuses: Vec<RawStatus>,
}

#[derive(Deserialize)]
struct RawStatus {
    context: String,
    status: String,
    #[serde(default)]
    target_url: String,
}

#[derive(Deserialize)]
struct RawPull {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    head: RawHead,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    closed_at: Option<String>,
}

#[derive(Deserialize)]
struct RawHead {
    sha: String,
    #[serde(rename = "ref", default)]
    name: String,
}

/// Parses Forgejo's RFC 3339 timestamp (`Z` or a `±HH:MM` offset, whole
/// seconds) to Unix ms; anything else is `None`.
fn parse_time_ms(s: &str) -> Option<i64> {
    if s.len() == 20 {
        return parse_utc_ms(s);
    }
    if s.len() != 25 || !s.is_char_boundary(19) {
        return None;
    }
    let (local, offset) = s.split_at(19);
    let o = offset.as_bytes();
    let sign = match o[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    if o[3] != b':' || !o[1..3].iter().chain(&o[4..6]).all(u8::is_ascii_digit) {
        return None;
    }
    let hh: i64 = offset[1..3].parse().ok()?;
    let mm: i64 = offset[4..6].parse().ok()?;
    let ms = parse_utc_ms(&format!("{local}Z"))?;
    Some(ms - sign * (hh * 3_600 + mm * 60) * 1_000)
}

fn pull_from(v: Value) -> Result<Pull, ForgeError> {
    let raw: RawPull = serde_json::from_value(v).map_err(|_| {
        ForgeError::Refused("Forgejo's pull request has an unexpected shape".into())
    })?;
    // Forgejo reports an unmerged PR's merge commit as null or "".
    let merge_commit = raw.merge_commit_sha.filter(|s| !s.is_empty());
    if !is_sha(&raw.head.sha) || merge_commit.as_deref().is_some_and(|s| !is_sha(s)) {
        return Err(ForgeError::Refused(
            "Forgejo gave a malformed commit id".into(),
        ));
    }
    Ok(Pull {
        number: raw.number,
        url: raw.html_url,
        node_id: raw.number.to_string(),
        open: raw.state == "open",
        merged: raw.merged,
        merge_commit: if raw.merged { merge_commit } else { None },
        head_sha: raw.head.sha,
        head_ref: raw.head.name,
        body: raw.body.unwrap_or_default(),
        settled_ms: raw
            .merged_at
            .as_deref()
            .or(raw.closed_at.as_deref())
            .and_then(parse_time_ms),
    })
}

impl<C: CredentialProvider> Forge for ForgejoForge<C> {
    async fn token(&self) -> Result<SecretToken, ForgeError> {
        self.http.token().await
    }

    /// Forgejo has no merge queue, and graft never schedules an auto-merge:
    /// only [`land_once`] merges a batch PR.
    async fn merging(&self, _node_id: &str) -> Result<bool, ForgeError> {
        Ok(false)
    }

    async fn close_pr(&self, number: u64, comment: &str) -> Result<(), ForgeError> {
        let path = format!("{}/issues/{number}/comments", self.http.repo_path());
        self.http
            .send(
                reqwest::Method::POST,
                &path,
                &[],
                Some(&json!({ "body": comment })),
            )
            .await?;
        let path = format!("{}/pulls/{number}", self.http.repo_path());
        self.http
            .send(
                reqwest::Method::PATCH,
                &path,
                &[],
                Some(&json!({ "state": "closed" })),
            )
            .await
            .map(drop)
    }

    /// Forgejo's PR list has no head filter: one page of the most recently
    /// updated PRs is read, and the newest whose head is `branch` in this
    /// repository (not a fork) is the answer.
    async fn find_pull(&self, branch: &str) -> Result<Option<Pull>, ForgeError> {
        let (owner, repo) = self.http.owner_repo();
        let full_name = format!("{owner}/{repo}");
        let query = [
            ("state", "all".to_owned()),
            ("sort", "recentupdate".to_owned()),
            ("limit", FIND_PAGE.to_string()),
        ];
        let path = format!("{}/pulls", self.http.repo_path());
        let v = self
            .http
            .send(reqwest::Method::GET, &path, &query, None)
            .await?;
        let list: Vec<Value> = serde_json::from_value(v)
            .map_err(|_| ForgeError::Refused("Forgejo's pull list is not a list".into()))?;
        let mut found: Option<Pull> = None;
        for item in list {
            let same_repo = item
                .pointer("/head/repo/full_name")
                .and_then(Value::as_str)
                .is_none_or(|n| n.eq_ignore_ascii_case(&full_name));
            let on_branch = item.pointer("/head/ref").and_then(Value::as_str) == Some(branch);
            if !(same_repo && on_branch) {
                continue;
            }
            let pull = pull_from(item)?;
            if found.as_ref().is_none_or(|f| pull.number > f.number) {
                found = Some(pull);
            }
        }
        Ok(found)
    }

    async fn create_pull(&self, new: &NewPull) -> Result<Pull, ForgeError> {
        let body = json!({
            "title": new.title,
            "body": new.body,
            "head": new.head,
            "base": new.base,
        });
        let path = format!("{}/pulls", self.http.repo_path());
        let v = self
            .http
            .send(reqwest::Method::POST, &path, &[], Some(&body))
            .await?;
        pull_from(v)
    }

    async fn get_pull(&self, number: u64) -> Result<Pull, ForgeError> {
        let path = format!("{}/pulls/{number}", self.http.repo_path());
        let v = self
            .http
            .send(reqwest::Method::GET, &path, &[], None)
            .await?;
        pull_from(v)
    }

    async fn update_pull(&self, number: u64, body: &str) -> Result<(), ForgeError> {
        let path = format!("{}/pulls/{number}", self.http.repo_path());
        self.http
            .send(
                reqwest::Method::PATCH,
                &path,
                &[],
                Some(&json!({ "body": body })),
            )
            .await
            .map(drop)
    }

    async fn enqueue(&self, _node_id: &str) -> Result<(), ForgeError> {
        Err(ForgeError::Refused(
            "Forgejo has no merge queue; land the batch with land-batch".into(),
        ))
    }

    /// Forgejo reports commit statuses, not check runs; each status is
    /// returned as a check run with its state as the conclusion (`None`
    /// while pending).
    async fn check_runs(&self, sha: &str) -> Result<Vec<crate::github::CheckRun>, ForgeError> {
        Ok(self
            .statuses(sha)
            .await?
            .into_iter()
            .map(|s| crate::github::CheckRun {
                conclusion: (s.state != "pending").then_some(s.state),
                name: s.context,
                url: s.url,
            })
            .collect())
    }
}

/// How the required contexts stand on a head commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every required context is `success`.
    Green,
    /// None failed, but these are missing or still pending.
    Pending(Vec<String>),
    /// These required contexts ended other than `success` (context, state).
    Failed(Vec<(String, String)>),
}

/// Judges `statuses` against `required`. Only `success` passes; `pending`
/// or an absent context waits; any other state (`failure`, `error`,
/// `warning`, ...) fails. Contexts not in `required` are ignored.
pub fn evaluate(required: &[String], statuses: &[Status]) -> Verdict {
    let mut waiting = Vec::new();
    let mut failed = Vec::new();
    for want in required {
        match statuses.iter().find(|s| &s.context == want) {
            Some(s) if s.state == "success" => {}
            Some(s) if s.state == "pending" => waiting.push(want.clone()),
            Some(s) => failed.push((want.clone(), s.state.clone())),
            None => waiting.push(want.clone()),
        }
    }
    if !failed.is_empty() {
        Verdict::Failed(failed)
    } else if !waiting.is_empty() {
        Verdict::Pending(waiting)
    } else {
        Verdict::Green
    }
}

/// What one landing attempt did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandOutcome {
    /// Merged now (by this attempt).
    Merged,
    /// It was merged already.
    AlreadyMerged,
    /// It was closed unmerged: rejected.
    Closed,
    /// Required contexts are missing or pending; try again later.
    Pending(Vec<String>),
    /// Required contexts failed; nothing was merged.
    Failed(Vec<(String, String)>),
}

/// One landing attempt for batch PR `number`: reads the PR, reports a merge
/// or a close it already had, judges its head's statuses against
/// `required`, and rebase-merges it only when they are all green. Never
/// waits: a pending answer is the caller's to retry.
pub async fn land_once<C: CredentialProvider>(
    forge: &ForgejoForge<C>,
    number: u64,
    required: &[String],
) -> Result<LandOutcome, MirrorError> {
    if required.is_empty() {
        return Err(MirrorError::Config(
            "[forgejo] required_contexts is empty: landing needs at least one required status"
                .into(),
        ));
    }
    let pull = forge.get_pull(number).await?;
    if pull.merged {
        return Ok(LandOutcome::AlreadyMerged);
    }
    if !pull.open {
        return Ok(LandOutcome::Closed);
    }
    let statuses = forge.statuses(&pull.head_sha).await?;
    match evaluate(required, &statuses) {
        Verdict::Pending(w) => Ok(LandOutcome::Pending(w)),
        Verdict::Failed(f) => Ok(LandOutcome::Failed(f)),
        Verdict::Green => {
            forge.merge_rebase(number, &pull.head_sha).await?;
            Ok(LandOutcome::Merged)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(context: &str, state: &str) -> Status {
        Status {
            context: context.to_owned(),
            state: state.to_owned(),
            url: String::new(),
        }
    }

    #[test]
    fn only_success_on_every_required_context_is_green() {
        let req = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(
            evaluate(
                &req,
                &[st("a", "success"), st("b", "success"), st("x", "failure")]
            ),
            Verdict::Green
        );
        assert_eq!(
            evaluate(&req, &[st("a", "success")]),
            Verdict::Pending(vec!["b".to_owned()])
        );
        assert_eq!(
            evaluate(&req, &[st("a", "pending"), st("b", "success")]),
            Verdict::Pending(vec!["a".to_owned()])
        );
        assert_eq!(
            evaluate(&req, &[st("a", "pending"), st("b", "error")]),
            Verdict::Failed(vec![("b".to_owned(), "error".to_owned())])
        );
        assert_eq!(
            evaluate(&req, &[st("a", "warning"), st("b", "success")]),
            Verdict::Failed(vec![("a".to_owned(), "warning".to_owned())])
        );
    }

    #[test]
    fn forgejo_times_with_an_offset_are_utc() {
        assert_eq!(
            parse_time_ms("2023-11-14T22:13:20Z"),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_time_ms("2023-11-15T09:13:20+11:00"),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_time_ms("2023-11-14T20:43:20-01:30"),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_time_ms("2023-11-14T22:13:20+1100"), None);
        assert_eq!(parse_time_ms("yesterday"), None);
    }

    #[test]
    fn an_unmerged_pull_has_no_merge_commit() {
        let sha = "a".repeat(40);
        let v = json!({
            "number": 3, "html_url": "u", "state": "closed", "merged": false,
            "merge_commit_sha": "", "head": { "sha": sha, "ref": "graft/batch-3" }
        });
        let pull = pull_from(v).unwrap();
        assert_eq!(pull.node_id, "3");
        assert!(!pull.open && !pull.merged && pull.merge_commit.is_none());
    }

    #[test]
    fn a_non_url_api_is_refused() {
        let token = SecretToken::new("t".to_owned()).unwrap();
        let made = ForgejoForge::new(
            "forgejo.example",
            "o",
            "r",
            crate::credentials::StaticToken::new(token),
            RetryPolicy::default(),
        );
        assert!(made.is_err());
    }

    #[test]
    fn a_token_never_prints() {
        let token = SecretToken::new("fake-forgejo-token".to_owned()).unwrap();
        let forge = ForgejoForge::new(
            "https://forgejo.example/api/v1",
            "o",
            "r",
            crate::credentials::StaticToken::new(token),
            RetryPolicy::default(),
        )
        .unwrap();
        assert!(!format!("{forge:?}").contains("fake-forgejo-token"));
    }
}
