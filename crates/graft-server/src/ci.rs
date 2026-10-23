//! CI identity exchange (`docs/design.md` §12.1, work items G3.8 and G3.8b):
//! the `[ci]` section of the config, the state behind the `ExchangeCiToken`
//! RPC, and keeping each issuer's key set (JWKS) fresh.
//!
//! ```toml
//! [ci]
//! max_token_ttl_s = 900                  # exchanged token life, at most 900
//!
//! [[ci.issuers]]
//! provider = "github"                    # or "woodpecker"
//! issuer = "https://token.actions.githubusercontent.com"
//! audience = "https://graft.example.com" # unique to this deployment
//! jwks_url = "https://token.actions.githubusercontent.com/.well-known/jwks"
//! # jwks_file = "/etc/graft/ci-jwks.json"  # instead of jwks_url
//! jwks_refresh_s = 3600
//!
//! [[ci.bindings]]
//! provider = "github"
//! ci_repo = "owner/repo"
//! repo_id = "123456789"                  # the CI's numeric repository id
//! graft_repo = "myrepo"
//! refs = ["refs/heads/graft/", "refs/heads/main"]
//! workflows = ["org/graft-ci/.github/workflows/graft-pre-land.yml@refs/heads/main"]
//! scopes = ["repo:myrepo:read", "repo:myrepo:write:.ci"]
//! ```
//!
//! A binding with `write` can attest, so its tokens are pinned to one
//! changeset (`graft_auth::oidc::Exchange::exchange_pinned`), and only when
//! the job's commit is the one exported for that changeset. Where that
//! answer comes from is [`CommitMap`]; with no source, pinned exchanges are
//! refused.
//!
//! The verification itself is `graft_auth::oidc`. This module owns what the
//! server adds: reading the rules, and fetching keys. A key set comes from a
//! file, read once at start-up, or from a URL, fetched at start-up, every
//! `jwks_refresh_s`, and again when a token names a key the set does not
//! hold (at most once a minute, so a stream of made-up key ids costs one
//! fetch a minute, not one per call).

use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use graft_auth::oidc::{Binding, Exchange, Issuer, Jwks, MAX_JWKS_LEN, OidcError, Provider};
use graft_auth::{SigningKey, Token};
use serde::Deserialize;

/// Longest life of an exchanged token: the `graft_auth::oidc` default and
/// cap.
const MAX_CI_TOKEN_TTL_S: u64 = graft_auth::oidc::DEFAULT_TOKEN_TTL_S;

/// Most clock-skew allowance a config may give an issuer, seconds.
const MAX_LEEWAY_S: i64 = 300;

/// The shortest gap between two fetches of one key set caused by unknown
/// key ids.
const ON_DEMAND_GAP: Duration = Duration::from_secs(60);

/// How long one key-set fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The `[ci]` section: trusted issuers and repository bindings. With no
/// issuer, the exchange is off and every `ExchangeCiToken` is refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct CiConfig {
    /// Longest life of an exchanged token, seconds (at most 900). The token
    /// also never outlives the CI token it came from.
    pub(crate) max_token_ttl_s: u64,
    /// The CI issuers whose tokens are trusted.
    pub(crate) issuers: Vec<IssuerConfig>,
    /// Which CI repository maps to which graft repository.
    pub(crate) bindings: Vec<BindingConfig>,
    /// A static map of changeset ids to the commits exported for them, one
    /// `<changeset> <commit>` pair per line, for tests and rehearsals. Without
    /// it no commit is known, and every pinned exchange is refused.
    pub(crate) commit_map_file: Option<PathBuf>,
}

impl Default for CiConfig {
    fn default() -> Self {
        Self {
            max_token_ttl_s: MAX_CI_TOKEN_TTL_S,
            issuers: Vec::new(),
            bindings: Vec::new(),
            commit_map_file: None,
        }
    }
}

/// A CI system, as the config names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProviderName {
    /// GitHub Actions OIDC.
    Github,
    /// A Woodpecker job token.
    Woodpecker,
}

impl From<ProviderName> for Provider {
    fn from(p: ProviderName) -> Self {
        match p {
            ProviderName::Github => Provider::GitHubActions,
            ProviderName::Woodpecker => Provider::Woodpecker,
        }
    }
}

/// One trusted issuer.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IssuerConfig {
    /// Which CI system it is.
    pub(crate) provider: ProviderName,
    /// The expected `iss`, compared exactly.
    pub(crate) issuer: String,
    /// The audience jobs must ask for: a value unique to this deployment.
    pub(crate) audience: String,
    /// A JWKS document on disk, read at start-up.
    #[serde(default)]
    pub(crate) jwks_file: Option<PathBuf>,
    /// Where to fetch the JWKS (`https://`, or `http://` on loopback).
    #[serde(default)]
    pub(crate) jwks_url: Option<String>,
    /// How often to fetch `jwks_url`, seconds.
    #[serde(default = "default_refresh_s")]
    pub(crate) jwks_refresh_s: u64,
    /// Clock-skew allowance for `nbf` and `iat`, seconds (at most 300).
    #[serde(default = "default_leeway_s")]
    pub(crate) leeway_s: i64,
    /// Longest `exp - iat` accepted, seconds.
    #[serde(default = "default_max_lifetime_s")]
    pub(crate) max_lifetime_s: i64,
}

fn default_refresh_s() -> u64 {
    3600
}

fn default_leeway_s() -> i64 {
    graft_auth::oidc::DEFAULT_LEEWAY_S
}

fn default_max_lifetime_s() -> i64 {
    graft_auth::oidc::DEFAULT_MAX_LIFETIME_S
}

/// One repository binding.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingConfig {
    /// The CI system the repository is on.
    pub(crate) provider: ProviderName,
    /// The CI repository, `owner/name`.
    pub(crate) ci_repo: String,
    /// The CI's numeric repository id; when set, the token must carry it.
    #[serde(default)]
    pub(crate) repo_id: Option<String>,
    /// The graft repository its jobs may act on.
    pub(crate) graft_repo: String,
    /// Refs jobs may run for (`refs/heads/graft/` is a prefix, anything
    /// else exact). Empty: any.
    #[serde(default)]
    pub(crate) refs: Vec<String>,
    /// Workflows (GitHub's `job_workflow_ref`) that may ask for a token,
    /// matched like `refs`. Required for GitHub; empty elsewhere means any.
    #[serde(default)]
    pub(crate) workflows: Vec<String>,
    /// The scopes an exchanged token carries: `read` and `write` on
    /// `graft_repo` only.
    pub(crate) scopes: Vec<String>,
}

impl BindingConfig {
    fn grants_write(&self) -> bool {
        self.scopes
            .iter()
            .any(|s| matches!(graft_auth::Scope::parse(s), Ok(graft_auth::Scope::Write { .. })))
    }

    fn binding(&self) -> Result<Binding, String> {
        let scopes: Vec<&str> = self.scopes.iter().map(String::as_str).collect();
        let binding = Binding::new(
            self.provider.into(),
            &self.ci_repo,
            &self.graft_repo,
            &scopes,
        )
        .map_err(|_| {
            format!(
                "ci binding {}: scopes must be read or write on {} (never land or admin), and at least one",
                self.ci_repo, self.graft_repo
            )
        })?;
        let refs: Vec<&str> = self.refs.iter().map(String::as_str).collect();
        let workflows: Vec<&str> = self.workflows.iter().map(String::as_str).collect();
        let binding = binding.with_refs(&refs).with_workflows(&workflows);
        Ok(match &self.repo_id {
            Some(id) => binding.with_repo_id(id),
            None => binding,
        })
    }
}

impl CiConfig {
    /// Checks the section; `Err` names the first problem.
    pub(crate) fn check(&self) -> Result<(), String> {
        if !(1..=MAX_CI_TOKEN_TTL_S).contains(&self.max_token_ttl_s) {
            return Err(format!(
                "ci.max_token_ttl_s must be between 1 and {MAX_CI_TOKEN_TTL_S}"
            ));
        }
        for i in &self.issuers {
            if i.issuer.is_empty() || i.audience.is_empty() {
                return Err("ci issuer: issuer and audience must be set".into());
            }
            match (&i.jwks_file, &i.jwks_url) {
                (Some(_), None) => {}
                (None, Some(url)) if url_allowed(url) => {}
                (None, Some(_)) => {
                    return Err(format!(
                        "ci issuer {}: jwks_url must be https:// (http:// only on loopback)",
                        i.issuer
                    ));
                }
                _ => {
                    return Err(format!(
                        "ci issuer {}: set exactly one of jwks_file and jwks_url",
                        i.issuer
                    ));
                }
            }
            if i.jwks_refresh_s == 0 {
                return Err(format!("ci issuer {}: jwks_refresh_s must be positive", i.issuer));
            }
            if !(0..=MAX_LEEWAY_S).contains(&i.leeway_s) {
                return Err(format!(
                    "ci issuer {}: leeway_s must be between 0 and {MAX_LEEWAY_S}",
                    i.issuer
                ));
            }
            if i.max_lifetime_s <= 0 {
                return Err(format!("ci issuer {}: max_lifetime_s must be positive", i.issuer));
            }
        }
        for (n, b) in self.bindings.iter().enumerate() {
            b.binding()?;
            // Without a pinned workflow, anyone who can push a branch can add
            // a workflow that asks for a token.
            if b.provider == ProviderName::Github && b.workflows.is_empty() {
                return Err(format!(
                    "ci binding {}: GitHub bindings must pin `workflows`",
                    b.ci_repo
                ));
            }
            // A binding that can attest must name its refs, and a Woodpecker
            // pipeline (the branch's own file) cannot be trusted to attest:
            // `exchange_pinned` refuses both, so refuse them at start-up.
            if b.grants_write() {
                if b.refs.is_empty() {
                    return Err(format!(
                        "ci binding {}: a binding with write must name `refs`",
                        b.ci_repo
                    ));
                }
                if b.provider == ProviderName::Woodpecker {
                    return Err(format!(
                        "ci binding {}: a Woodpecker binding may hold read only",
                        b.ci_repo
                    ));
                }
            }
            if !self.issuers.iter().any(|i| i.provider == b.provider) {
                return Err(format!(
                    "ci binding {}: no issuer of its provider is trusted",
                    b.ci_repo
                ));
            }
            // The first binding that matches wins, so two for one CI
            // repository would make the second dead.
            let same = |o: &BindingConfig| {
                o.provider == b.provider && o.ci_repo.eq_ignore_ascii_case(&b.ci_repo)
            };
            if self.bindings[..n].iter().any(same) {
                return Err(format!("ci binding {}: bound twice", b.ci_repo));
            }
        }
        Ok(())
    }
}

/// Whether a key-set URL may be fetched: TLS, except on loopback.
fn url_allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = match host.rsplit_once(':') {
        Some((h, port)) if !h.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// A key set fetched from a URL.
#[derive(Debug)]
struct Source {
    issuer: String,
    url: String,
    every: Duration,
    /// When the last on-demand fetch started.
    last_on_demand: Mutex<Option<Instant>>,
}

/// The exchange the server runs: rules, keys, and where to refresh them.
#[derive(Debug)]
pub(crate) struct CiExchange {
    exchange: RwLock<Exchange>,
    sources: Vec<Source>,
    http: reqwest::Client,
    commits: CommitMap,
}

impl CiExchange {
    /// Builds the exchange from a checked config. `None` when no issuer is
    /// configured. Key files are read here; key URLs are fetched by
    /// [`CiExchange::refresh_all`] and the refresh tasks.
    pub(crate) fn load(config: &CiConfig) -> Result<Option<Self>, String> {
        if config.issuers.is_empty() {
            return Ok(None);
        }
        let mut exchange = Exchange::new().with_max_ttl_s(config.max_token_ttl_s);
        let mut sources = Vec::new();
        for i in &config.issuers {
            let jwks = match (&i.jwks_file, &i.jwks_url) {
                (Some(path), _) => read_jwks(path)?,
                (None, Some(url)) => {
                    sources.push(Source {
                        issuer: i.issuer.clone(),
                        url: url.clone(),
                        every: Duration::from_secs(i.jwks_refresh_s),
                        last_on_demand: Mutex::new(None),
                    });
                    Jwks::default()
                }
                (None, None) => return Err("ci issuer without keys".into()),
            };
            let mut issuer = Issuer::new(
                i.provider.into(),
                i.issuer.clone(),
                i.audience.clone(),
                jwks,
            );
            issuer.leeway_s = i.leeway_s;
            issuer.max_lifetime_s = i.max_lifetime_s;
            exchange = exchange.trust(issuer);
        }
        for b in &config.bindings {
            exchange = exchange.bind(b.binding()?);
        }
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("ci: http client: {e}"))?;
        Ok(Some(Self {
            exchange: RwLock::new(exchange),
            sources,
            http,
            commits: CommitMap::new(config.commit_map_file.clone()),
        }))
    }

    /// Exchanges `jwt` for a graft token at `now_s`, pinned to `changeset`
    /// when one is named (a binding that can attest needs it). A token
    /// naming a key the set does not hold makes the URL key sets refetch
    /// (rate limited) and the exchange run once more.
    pub(crate) async fn exchange(
        &self,
        key: &SigningKey,
        jwt: &str,
        changeset: Option<&str>,
        now_s: i64,
    ) -> Result<Token, OidcError> {
        match self.run(key, jwt, changeset, now_s) {
            Err(OidcError::UnknownKey) if self.refresh_on_demand().await => {
                self.run(key, jwt, changeset, now_s)
            }
            result => result,
        }
    }

    fn run(
        &self,
        key: &SigningKey,
        jwt: &str,
        changeset: Option<&str>,
        now_s: i64,
    ) -> Result<Token, OidcError> {
        let exchange = self
            .exchange
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match changeset {
            None => exchange.exchange(key, jwt, now_s),
            Some(cs) => exchange.exchange_pinned(key, jwt, now_s, cs, |commit| {
                self.commits.is_export_of(cs, commit)
            }),
        }
    }

    /// Refetches every URL key set not fetched on demand in the last
    /// minute. Returns whether any set changed.
    async fn refresh_on_demand(&self) -> bool {
        let mut changed = false;
        for source in &self.sources {
            {
                let mut last = source
                    .last_on_demand
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if last.is_some_and(|t| t.elapsed() < ON_DEMAND_GAP) {
                    continue;
                }
                *last = Some(Instant::now());
            }
            changed |= self.refresh(source).await;
        }
        changed
    }

    /// Fetches every URL key set once (start-up).
    pub(crate) async fn refresh_all(&self) {
        for source in &self.sources {
            self.refresh(source).await;
        }
    }

    /// Fetches one key set and installs it. A failure keeps the old keys
    /// and is logged. Returns whether the keys were replaced.
    async fn refresh(&self, source: &Source) -> bool {
        match fetch_jwks(&self.http, &source.url).await {
            Ok(jwks) => {
                tracing::info!(issuer = %source.issuer, keys = jwks.len(), "ci key set fetched");
                self.exchange
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_jwks(&source.issuer, jwks)
            }
            Err(e) => {
                tracing::warn!(issuer = %source.issuer, error = %e, "ci key set fetch failed; keeping the old keys");
                false
            }
        }
    }

    /// Runs the timed refreshes until the task is dropped: one loop per URL
    /// key set, each at its own interval. The first fetch is
    /// [`CiExchange::refresh_all`]'s.
    pub(crate) async fn refresh_forever(self: std::sync::Arc<Self>) {
        let mut tasks = tokio::task::JoinSet::new();
        for n in 0..self.sources.len() {
            let me = self.clone();
            tasks.spawn(async move {
                let source = &me.sources[n];
                let mut tick = tokio::time::interval(source.every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    me.refresh(source).await;
                }
            });
        }
        while tasks.join_next().await.is_some() {}
    }
}

/// Which commit was exported for which changeset: the check
/// `exchange_pinned` needs, that the job ran on the commit belonging to the
/// changeset it attests. The ids are opaque text here.
///
/// The only source today is the config's static `commit_map_file`, reread
/// when it changes. The mirror's own map is not reachable from the server
/// yet; until it is, a server without the file refuses every pinned
/// exchange (fails closed).
#[derive(Debug)]
struct CommitMap {
    file: Option<PathBuf>,
    loaded: Mutex<Option<(std::time::SystemTime, std::collections::HashMap<String, String>)>>,
}

/// Largest commit map file read.
const MAX_COMMIT_MAP: u64 = 16 << 20;

impl CommitMap {
    fn new(file: Option<PathBuf>) -> Self {
        Self {
            file,
            loaded: Mutex::new(None),
        }
    }

    /// Whether `commit` is the commit exported for `changeset`.
    fn is_export_of(&self, changeset: &str, commit: &str) -> bool {
        let Some(path) = &self.file else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            tracing::warn!(path = %path.display(), "ci commit map unreadable");
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        let mut loaded = self
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if loaded.as_ref().is_none_or(|(at, _)| *at != modified) {
            if meta.len() > MAX_COMMIT_MAP {
                tracing::warn!(path = %path.display(), "ci commit map too large");
                return false;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                return false;
            };
            let map = text
                .lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    Some((it.next()?.to_owned(), it.next()?.to_owned()))
                })
                .collect();
            *loaded = Some((modified, map));
        }
        loaded
            .as_ref()
            .and_then(|(_, map)| map.get(changeset))
            .is_some_and(|c| c.eq_ignore_ascii_case(commit))
    }
}

fn read_jwks(path: &std::path::Path) -> Result<Jwks, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let jwks = Jwks::from_json(&bytes)
        .map_err(|_| format!("{}: not a usable JWKS document", path.display()))?;
    if jwks.is_empty() {
        return Err(format!("{}: no usable signing key", path.display()));
    }
    Ok(jwks)
}

async fn fetch_jwks(http: &reqwest::Client, url: &str) -> Result<Jwks, String> {
    let mut resp = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("request: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status().as_u16()));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_JWKS_LEN as u64)
    {
        return Err("too large".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("body: {}", e.without_url()))?
    {
        if body.len() + chunk.len() > MAX_JWKS_LEN {
            return Err("too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    let jwks = Jwks::from_json(&body).map_err(|_| "not a JWKS document".to_owned())?;
    if jwks.is_empty() {
        return Err("no usable signing key".into());
    }
    Ok(jwks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<CiConfig, String> {
        let c: CiConfig = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        c.check()?;
        Ok(c)
    }

    const ISSUER: &str = r#"
[[issuers]]
provider = "github"
issuer = "https://token.actions.githubusercontent.com"
audience = "https://graft.example.test"
jwks_url = "https://token.actions.githubusercontent.com/.well-known/jwks"
"#;

    const BINDING: &str = r#"
[[bindings]]
provider = "github"
ci_repo = "o/r"
repo_id = "1"
graft_repo = "r"
refs = ["refs/heads/graft/"]
workflows = ["o/ci/.github/workflows/graft-pre-land.yml@refs/heads/main"]
scopes = ["repo:r:read", "repo:r:write:.ci"]
"#;

    #[test]
    fn a_full_section_parses_and_loads() {
        let c = parse(&format!("max_token_ttl_s = 600\n{ISSUER}{BINDING}")).expect("parses");
        assert_eq!(c.max_token_ttl_s, 600);
        assert_eq!(c.issuers[0].leeway_s, 60);
        assert_eq!(c.issuers[0].max_lifetime_s, 3600);
        assert_eq!(c.issuers[0].jwks_refresh_s, 3600);
        let ex = CiExchange::load(&c).expect("loads").expect("on");
        assert_eq!(ex.sources.len(), 1);
        assert!(CiExchange::load(&CiConfig::default()).unwrap().is_none());
    }

    #[test]
    fn bad_sections_are_refused() {
        // CI never lands or administers, and never reaches another repo.
        for scopes in [
            r#"["repo:r:land:main"]"#,
            r#"["repo:r:admin"]"#,
            r#"["repo:other:read"]"#,
            "[]",
        ] {
            let b = BINDING.replace(r#"["repo:r:read", "repo:r:write:.ci"]"#, scopes);
            assert!(parse(&format!("{ISSUER}{b}")).is_err(), "{scopes}");
        }
        // A binding with no issuer of its provider, or bound twice, or a
        // GitHub binding with no workflow pin.
        assert!(parse(BINDING).is_err());
        let unpinned = BINDING.replace(
            "workflows = [\"o/ci/.github/workflows/graft-pre-land.yml@refs/heads/main\"]",
            "",
        );
        assert!(parse(&format!("{ISSUER}{unpinned}")).is_err());
        assert!(parse(&format!("{ISSUER}{BINDING}{}", BINDING.replace("o/r", "O/R"))).is_err());
        // Write needs refs; Woodpecker may only read.
        let any_ref = BINDING.replace("refs = [\"refs/heads/graft/\"]\n", "");
        assert!(parse(&format!("{ISSUER}{any_ref}")).is_err());
        let wp_issuer = ISSUER.replace("\"github\"", "\"woodpecker\"");
        let wp_write = BINDING
            .replace("\"github\"", "\"woodpecker\"")
            .replace("workflows = [\"o/ci/.github/workflows/graft-pre-land.yml@refs/heads/main\"]\n", "");
        assert!(parse(&format!("{wp_issuer}{wp_write}")).is_err());
        let wp_read = wp_write.replace(", \"repo:r:write:.ci\"", "");
        assert!(parse(&format!("{wp_issuer}{wp_read}")).is_ok());
        // Keys: exactly one source, and TLS off loopback.
        let both = ISSUER.replace("jwks_url", "jwks_file = \"/x\"\njwks_url");
        assert!(parse(&both).is_err());
        let neither = ISSUER.replace(
            "jwks_url = \"https://token.actions.githubusercontent.com/.well-known/jwks\"",
            "",
        );
        assert!(parse(&neither).is_err());
        assert!(parse(&ISSUER.replace("https://token.actions.githubusercontent.com/.well", "http://ci.example/.well")).is_err());
        assert!(parse(&format!("{ISSUER}leeway_s = 301")).is_err());
        assert!(parse(&format!("{ISSUER}max_lifetime_s = 0")).is_err());
        assert!(parse("max_token_ttl_s = 901").is_err());
        assert!(parse("max_token_ttl_s = 0").is_err());
        assert!(parse(&format!("{ISSUER}typo = 1")).is_err());
        assert!(parse(&ISSUER.replace("\"github\"", "\"gitlab\"")).is_err());
    }

    #[test]
    fn the_commit_map_fails_closed_and_follows_its_file() {
        assert!(!CommitMap::new(None).is_export_of("c1", "abc"));
        let dir = graft_testkit::crash::TempDir::new("ci-commit-map");
        let path = dir.path().join("map");
        let map = CommitMap::new(Some(path.clone()));
        assert!(!map.is_export_of("c1", "abc"), "a missing file knows nothing");
        std::fs::write(&path, "c1 ABC\n\nmalformed\n").unwrap();
        assert!(map.is_export_of("c1", "abc"));
        assert!(!map.is_export_of("c1", "abd"));
        assert!(!map.is_export_of("c2", "abc"));
    }

    #[test]
    fn plain_http_only_on_loopback() {
        for ok in [
            "https://x.example/jwks",
            "http://127.0.0.1:8080/jwks",
            "http://localhost/jwks",
            "http://[::1]:9/jwks",
        ] {
            assert!(url_allowed(ok), "{ok}");
        }
        for bad in [
            "http://x.example/jwks",
            "http://127.0.0.1.evil.example/jwks",
            "http://localhost.evil.example:80/",
            "file:///etc/jwks",
            "ftp://x",
        ] {
            assert!(!url_allowed(bad), "{bad}");
        }
    }
}
