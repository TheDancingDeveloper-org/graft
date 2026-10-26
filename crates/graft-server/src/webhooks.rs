//! Outbound journal webhooks.
//!
//! A background task per configured endpoint reads the journal after a land
//! has committed. A slow or dead receiver never holds a land or a `Watch`:
//! delivery is a later read of rows that are already durable.
//!
//! The cursor is the next LSN to send. A new endpoint starts at the journal
//! tip (`start = "tip"`, the default) or at an explicit LSN (`start = "<lsn>"`)
//! when a replay is wanted. It moves after a 2xx, after a filter skip, and
//! after a 4xx (that entry is final). A crash, a timeout or a 5xx retries the
//! same entry. The secret is the value of the environment variable `secret_env`
//! names. It is never logged and never appears in `Debug`.

use std::sync::Arc;
use std::time::Duration;

use graft_engine::Engine;
#[cfg(test)]
use graft_engine::EngineConfig;
use graft_store::{Lsn, MetaStore, MetaTxn, ObjectStore, Precondition, RepoId, Write};
use graft_telemetry::Telemetry;
use hmac::{Hmac, Mac};
use prometheus::{IntCounterVec, Opts};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::watch;

/// How often a caught-up endpoint looks for new journal rows.
const POLL: Duration = Duration::from_millis(200);

/// Journal page size. Delivery cost follows this page, never the history.
/// HTTP timeout for one delivery. A timeout is a failure: the cursor stays.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Derived-value kind of a delivery cursor. The value is the next LSN as text.
const CURSOR_KIND: &str = "webhook-cursor";
const CURSOR_VERSION: u32 = 1;

/// Backoff after a failed delivery. The last step repeats.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(200),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];

const METRIC: &str = "graft_webhook_delivery_total";

type HmacSha256 = Hmac<Sha256>;

/// One `[[webhooks]]` entry.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct WebhookSpec {
    /// Receiver URL (`http` or `https`).
    pub(crate) url: String,
    /// Journal kinds to deliver. Empty means every kind.
    pub(crate) kinds: Vec<String>,
    /// Lines to deliver. Empty means every line.
    pub(crate) lines: Vec<String>,
    /// Environment variable holding the HMAC key. The value is not stored.
    pub(crate) secret_env: String,
    /// Repositories to deliver. Required: the meta store has no repo list,
    /// and delivery must not scan history to discover one.
    pub(crate) repos: Vec<String>,
    /// Where a new cursor starts: `"tip"` (the default) or a decimal LSN.
    /// `"tip"` delivers only entries written after the hook is configured.
    #[serde(default = "default_start")]
    pub(crate) start: String,
}

fn default_start() -> String {
    "tip".into()
}

impl Default for WebhookSpec {
    fn default() -> Self {
        Self {
            url: String::new(),
            kinds: Vec::new(),
            lines: Vec::new(),
            secret_env: String::new(),
            repos: Vec::new(),
            start: default_start(),
        }
    }
}

impl std::fmt::Debug for WebhookSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookSpec")
            .field("url", &self.url)
            .field("kinds", &self.kinds)
            .field("lines", &self.lines)
            .field("secret_env", &self.secret_env)
            .field("repos", &self.repos)
            .field("start", &self.start)
            .finish()
    }
}

/// Refuses a webhook that cannot be delivered. The secret's value is not
/// read: only the variable's name, the URL and the repository list.
pub(crate) fn check(specs: &[WebhookSpec]) -> Result<(), String> {
    for spec in specs {
        let url = reqwest::Url::parse(&spec.url)
            .map_err(|_| format!("webhooks: {:?} is not a URL", spec.url))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(format!("webhooks: {} must be http or https", spec.url));
        }
        if spec.secret_env.is_empty() {
            return Err("webhooks: secret_env is empty".into());
        }
        if spec.repos.is_empty() {
            return Err("webhooks: repos is empty".into());
        }
        if spec.start != "tip" && spec.start.parse::<u64>().is_err() {
            return Err("webhooks: start must be \"tip\" or an lsn".into());
        }
    }
    Ok(())
}

/// Delivers every configured webhook until `stop` is set.
pub(crate) async fn deliver_forever<M, O>(
    engine: Arc<Engine<M, O>>,
    telemetry: Arc<Telemetry>,
    specs: Vec<WebhookSpec>,
    mut stop: watch::Receiver<bool>,
) where
    M: MetaStore + 'static,
    O: ObjectStore + 'static,
{
    let metric = match IntCounterVec::new(
        Opts::new(METRIC, "Outbound journal webhook deliveries."),
        &["outcome"],
    ) {
        Ok(counter) => {
            if let Err(error) = telemetry.registry().register(Box::new(counter.clone())) {
                tracing::error!(%error, "webhook metric was not registered");
            }
            Some(counter)
        }
        Err(error) => {
            tracing::error!(%error, "webhook metric was not built");
            None
        }
    };
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut tasks = tokio::task::JoinSet::new();
    for (index, spec) in specs.into_iter().enumerate() {
        let secret = match std::env::var(&spec.secret_env) {
            Ok(value) if !value.is_empty() => value,
            _ => {
                tracing::error!(
                    index,
                    secret_env = %spec.secret_env,
                    "webhook secret is unset or empty; this endpoint will not deliver"
                );
                continue;
            }
        };
        let engine = engine.clone();
        let metric = metric.clone();
        let client = client.clone();
        tasks.spawn(async move {
            let mut failures: u32 = 0;
            loop {
                match deliver_step(&engine, &client, &spec, index, &secret, &metric).await {
                    Step::Progress => failures = 0,
                    Step::Idle => tokio::time::sleep(POLL).await,
                    Step::Failed => {
                        failures = failures.saturating_add(1);
                        let step = BACKOFF[(failures as usize - 1).min(BACKOFF.len() - 1)];
                        tokio::time::sleep(step).await;
                    }
                }
            }
        });
    }
    let _ = stop.wait_for(|stopped| *stopped).await;
    tasks.abort_all();
}

enum Step {
    /// A cursor moved. Try the next entry at once.
    Progress,
    /// Nothing pending. Wait, then look again.
    Idle,
    /// A post failed. The cursor stayed; back off before retrying.
    Failed,
}

/// One pass over every configured repository.
async fn deliver_step<M, O>(
    engine: &Engine<M, O>,
    client: &reqwest::Client,
    spec: &WebhookSpec,
    index: usize,
    secret: &str,
    metric: &Option<IntCounterVec>,
) -> Step
where
    M: MetaStore,
    O: ObjectStore,
{
    let mut idle = true;
    let (meta, _) = engine.stores();
    for name in &spec.repos {
        let Ok(Some(repo)) = meta.repo(name).await else {
            continue;
        };
        match deliver_repo(meta, client, spec, index, repo, secret, metric).await {
            Ok(true) => idle = false,
            Ok(false) => {}
            Err(_) => return Step::Failed,
        }
    }
    if idle { Step::Idle } else { Step::Progress }
}

/// Delivers or skips the next journal entry of one repository.
/// `Ok(true)` when the cursor moved.
async fn deliver_repo<M: MetaStore>(
    meta: &M,
    client: &reqwest::Client,
    spec: &WebhookSpec,
    index: usize,
    repo: RepoId,
    secret: &str,
    metric: &Option<IntCounterVec>,
) -> Result<bool, String> {
    let key = cursor_key(index);
    let cursor = read_cursor(meta, spec, repo, &key).await?;
    let rows = meta
        .journal(repo, cursor, 1)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let Some((lsn, kind, body)) = rows.into_iter().next() else {
        // Record a new tip cursor so a later entry is not hidden by a
        // recomputed tip, and so a configured replay LSN is kept.
        return match meta
            .derived(repo, CURSOR_KIND, CURSOR_VERSION, &key)
            .await
            .map_err(|e| format!("{e:?}"))?
        {
            None => {
                advance(meta, repo, &key, cursor, cursor).await?;
                Ok(false)
            }
            Some(_) => Ok(false),
        };
    };
    if lsn != cursor {
        return Err(format!("journal returned {lsn}, cursor is {cursor}"));
    }
    if !want(spec, &kind, &body) {
        return advance(meta, repo, &key, cursor, cursor.saturating_add(1)).await;
    }
    let payload = payload(repo, lsn, &kind, &body);
    match post(client, spec, secret, &payload).await {
        Post::Delivered => {
            count(metric, "delivered");
            advance(meta, repo, &key, cursor, cursor.saturating_add(1)).await
        }
        Post::Rejected(status) => {
            count(metric, "failed");
            tracing::error!(lsn, status, "webhook receiver rejected the entry");
            advance(meta, repo, &key, cursor, cursor.saturating_add(1)).await
        }
        Post::Failed => {
            count(metric, "failed");
            Err("failed".into())
        }
    }
}

enum Post {
    Delivered,
    /// A 4xx. The status is logged; the body is not.
    Rejected(u16),
    Failed,
}

fn payload(repo: RepoId, lsn: Lsn, kind: &str, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(format!("{}", repo.0).as_bytes());
    out.push(b'\n');
    out.extend(lsn.to_string().as_bytes());
    out.push(b'\n');
    out.extend(kind.as_bytes());
    out.push(b'\n');
    out.extend(body);
    out
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0xf) as usize] as char);
    }
    format!("sha256={hex}")
}

const HEX: &[u8; 16] = b"0123456789abcdef";

async fn post(client: &reqwest::Client, spec: &WebhookSpec, secret: &str, body: &[u8]) -> Post {
    let signature = sign(secret, body);
    let response = client
        .post(&spec.url)
        .header("X-Graft-Signature", signature)
        .header("content-type", "application/octet-stream")
        .body(body.to_vec())
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => Post::Delivered,
        Ok(response) if response.status().is_client_error() => {
            Post::Rejected(response.status().as_u16())
        }
        Ok(_) => Post::Failed,
        Err(_) => Post::Failed,
    }
}

/// Kind filter, then line filter. An entry with no line matches only when
/// `lines` is empty.
fn want(spec: &WebhookSpec, kind: &str, body: &[u8]) -> bool {
    if !spec.kinds.is_empty() && !spec.kinds.iter().any(|item| item == kind) {
        return false;
    }
    if spec.lines.is_empty() {
        return true;
    }
    line_of(kind, body).is_some_and(|line| spec.lines.iter().any(|item| item == &line))
}

fn line_of(kind: &str, body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let key = match kind {
        "RefMoved" => "name",
        "LineMerged" => "dst",
        _ => "line",
    };
    value.get(key)?.as_str().map(str::to_owned)
}

fn cursor_key(index: usize) -> Vec<u8> {
    index.to_string().into_bytes()
}

async fn read_cursor<M: MetaStore>(
    meta: &M,
    spec: &WebhookSpec,
    repo: RepoId,
    key: &[u8],
) -> Result<Lsn, String> {
    match meta
        .derived(repo, CURSOR_KIND, CURSOR_VERSION, key)
        .await
        .map_err(|e| format!("{e:?}"))?
    {
        None => initial_cursor(meta, spec, repo).await,
        Some(bytes) => parse_lsn(&bytes),
    }
}

fn parse_lsn(bytes: &[u8]) -> Result<Lsn, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "cursor: not utf-8".to_owned())?;
    text.parse::<u64>()
        .map_err(|_| "cursor: not an lsn".to_owned())
}

/// The first cursor for an endpoint that has never delivered.
///
/// `"tip"` is one past [`MetaStore::journal_tip`]: one indexed lookup, not a
/// walk of the journal. An explicit LSN replays from there.
async fn initial_cursor<M: MetaStore>(
    meta: &M,
    spec: &WebhookSpec,
    repo: RepoId,
) -> Result<Lsn, String> {
    if spec.start != "tip" {
        return spec
            .start
            .parse::<u64>()
            .map_err(|_| "webhooks: start must be \"tip\" or an lsn".into());
    }
    match meta.journal_tip(repo).await.map_err(|e| format!("{e:?}"))? {
        None => Ok(1),
        Some(lsn) => Ok(lsn.saturating_add(1)),
    }
}

/// Moves the cursor from `expected` to `next`. An absent row matches any
/// expected LSN (a new hook may start past 1). A stored row matches only
/// when its text is that LSN.
async fn advance<M: MetaStore>(
    meta: &M,
    repo: RepoId,
    key: &[u8],
    expected: Lsn,
    next: Lsn,
) -> Result<bool, String> {
    let current = meta
        .derived(repo, CURSOR_KIND, CURSOR_VERSION, key)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let matches = match &current {
        None => true,
        Some(bytes) => bytes.as_slice() == expected.to_string().as_bytes(),
    };
    if !matches {
        return Ok(false);
    }
    let outcome = meta
        .commit(
            repo,
            MetaTxn {
                preconditions: vec![Precondition::DerivedIs {
                    kind: CURSOR_KIND.to_owned(),
                    version: CURSOR_VERSION,
                    key: key.to_vec(),
                    value: current,
                }],
                writes: vec![Write::PutDerived {
                    kind: CURSOR_KIND.to_owned(),
                    version: CURSOR_VERSION,
                    key: key.to_vec(),
                    value: next.to_string().into_bytes(),
                }],
            },
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(matches!(outcome, graft_store::TxnOutcome::Committed(_)))
}

fn count(metric: &Option<IntCounterVec>, outcome: &str) {
    if let Some(metric) = metric {
        metric.with_label_values(&[outcome]).inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::SystemClock;
    use graft_store_fs::{FsMode, FsObjectStore};
    use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
    use std::future::Future;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use graft_store::{StoreResult, TxnOutcome};

    struct Harness {
        engine: Engine<SqliteMetaStore, FsObjectStore>,
        meta: Arc<SqliteMetaStore>,
        _dir: tempfile::TempDir,
    }

    fn engine() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let meta =
            Arc::new(SqliteMetaStore::open(SqliteConfig::new(dir.path().join("meta.db"))).unwrap());
        let objects = Arc::new(
            FsObjectStore::open(dir.path().join("objects"), FsMode::Authoritative).unwrap(),
        );
        let engine = Engine::new(
            meta.clone(),
            objects,
            Arc::new(SystemClock),
            EngineConfig::default(),
        );
        Harness {
            engine,
            meta,
            _dir: dir,
        }
    }

    async fn repo(engine: &Engine<SqliteMetaStore, FsObjectStore>) -> RepoId {
        engine.stores().0.create_repo("graft").await.unwrap()
    }

    async fn journal(
        engine: &Engine<SqliteMetaStore, FsObjectStore>,
        id: RepoId,
        kind: &str,
        body: &str,
    ) -> Lsn {
        let outcome = engine
            .stores()
            .0
            .commit(
                id,
                MetaTxn {
                    preconditions: vec![],
                    writes: vec![Write::Journal {
                        kind: kind.into(),
                        body: body.as_bytes().to_vec(),
                    }],
                },
            )
            .await
            .unwrap();
        match outcome {
            graft_store::TxnOutcome::Committed(lsns) => lsns[0],
            other => panic!("journal commit: {other:?}"),
        }
    }

    struct Inbox {
        listener: tokio::net::TcpListener,
        status: Mutex<u16>,
        seen: Mutex<Vec<(u16, Vec<u8>, String)>>,
    }

    impl Inbox {
        async fn bind() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            assert!(listener.local_addr().unwrap().ip().is_loopback());
            Self {
                listener,
                status: Mutex::new(200),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/hook", self.listener.local_addr().unwrap())
        }

        fn set_status(&self, status: u16) {
            *self.status.lock().unwrap() = status;
        }

        async fn serve(self: Arc<Self>, stop: watch::Receiver<bool>) {
            loop {
                if *stop.borrow() {
                    return;
                }
                let accepted = tokio::select! {
                    value = self.listener.accept() => value,
                    _ = async {
                        let mut stop = stop.clone();
                        let _ = stop.changed().await;
                    } => return,
                };
                let (mut socket, _) = accepted.unwrap();
                let inbox = self.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0_u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    buf.truncate(n);
                    let text = String::from_utf8_lossy(&buf);
                    let signature = text
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("x-graft-signature"))
                                .map(|(_, value)| value.trim().to_owned())
                        })
                        .unwrap_or_default();
                    let body = buf
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|at| buf[at + 4..].to_vec())
                        .unwrap_or_default();
                    let status = *inbox.status.lock().unwrap();
                    inbox.seen.lock().unwrap().push((status, body, signature));
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        }
    }

    fn spec(url: &str) -> WebhookSpec {
        WebhookSpec {
            url: url.into(),
            kinds: vec![],
            lines: vec![],
            secret_env: "GRAFT_WEBHOOK_SECRET".into(),
            repos: vec!["graft".into()],
            start: "1".into(),
        }
    }

    fn counter() -> IntCounterVec {
        IntCounterVec::new(Opts::new("test_webhook", "test"), &["outcome"]).unwrap()
    }

    fn value(counter: &IntCounterVec, outcome: &str) -> u64 {
        counter.with_label_values(&[outcome]).get()
    }

    #[test]
    fn config_names_the_endpoint_and_debug_omits_the_secret() {
        let parsed: WebhookSpec = toml::from_str(
            r#"
            url = "https://hooks.example/graft"
            kinds = ["LandAttempt"]
            lines = ["main"]
            secret_env = "GRAFT_WEBHOOK_SECRET"
            repos = ["graft"]
            "#,
        )
        .unwrap();
        assert_eq!(parsed.kinds, ["LandAttempt"]);
        assert_eq!(parsed.lines, ["main"]);
        assert_eq!(parsed.repos, ["graft"]);
        let shown = format!("{parsed:?}");
        assert!(shown.contains("secret_env"));
        assert!(!shown.contains("super-secret-value"));
        assert!(check(&[parsed]).is_ok());
        assert!(check(&[spec("ftp://example/hook")]).is_err());
        let mut empty = spec("http://127.0.0.1/hook");
        empty.repos.clear();
        assert!(check(&[empty]).is_err());
    }

    #[tokio::test]
    async fn delivery_is_signed_and_a_failure_retries_the_same_entry() {
        let harness = engine();
        let engine = &harness.engine;
        let id = repo(engine).await;
        let first = journal(engine, id, "LandAttempt", r#"{"line":"main"}"#).await;
        let second = journal(engine, id, "LandAttempt", r#"{"line":"main"}"#).await;
        let inbox = Arc::new(Inbox::bind().await);
        inbox.set_status(500);
        let (tx, rx) = watch::channel(false);
        let serving = inbox.clone();
        let task = tokio::spawn(async move { serving.serve(rx).await });
        let client = reqwest::Client::new();
        let spec = spec(&inbox.url());
        let metric = counter();
        let secret = "super-secret-value";
        assert!(
            deliver_repo(
                engine.stores().0,
                &client,
                &spec,
                0,
                id,
                secret,
                &Some(metric.clone())
            )
            .await
            .is_err()
        );
        assert_eq!(value(&metric, "failed"), 1);
        assert_eq!(
            read_cursor(engine.stores().0, &spec, id, &cursor_key(0))
                .await
                .unwrap(),
            first
        );
        inbox.set_status(200);
        assert!(
            deliver_repo(
                engine.stores().0,
                &client,
                &spec,
                0,
                id,
                secret,
                &Some(metric.clone())
            )
            .await
            .unwrap()
        );
        assert!(
            deliver_repo(
                engine.stores().0,
                &client,
                &spec,
                0,
                id,
                secret,
                &Some(metric.clone())
            )
            .await
            .unwrap()
        );
        assert_eq!(value(&metric, "delivered"), 2);
        let seen = inbox.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].1, seen[1].1, "the failed entry is retried");
        assert!(
            seen[1]
                .1
                .windows(first.to_string().len())
                .any(|w| w == first.to_string().as_bytes())
        );
        assert!(
            seen[2]
                .1
                .windows(second.to_string().len())
                .any(|w| w == second.to_string().as_bytes())
        );
        assert_eq!(seen[1].2, sign(secret, &seen[1].1));
        assert_ne!(seen[1].2, sign("other", &seen[1].1));
        assert_eq!(
            read_cursor(engine.stores().0, &spec, id, &cursor_key(0))
                .await
                .unwrap(),
            second + 1
        );
        let _ = tx.send(true);
        task.abort();
    }

    #[tokio::test]
    async fn restart_resumes_at_the_persisted_cursor() {
        let harness = engine();
        let engine = &harness.engine;
        let id = repo(engine).await;
        let skipped = journal(engine, id, "RefMoved", r#"{"name":"main"}"#).await;
        let kept = journal(engine, id, "RefMoved", r#"{"name":"main"}"#).await;
        engine
            .stores()
            .0
            .commit(
                id,
                MetaTxn {
                    preconditions: vec![Precondition::DerivedIs {
                        kind: CURSOR_KIND.into(),
                        version: CURSOR_VERSION,
                        key: cursor_key(0),
                        value: None,
                    }],
                    writes: vec![Write::PutDerived {
                        kind: CURSOR_KIND.into(),
                        version: CURSOR_VERSION,
                        key: cursor_key(0),
                        value: kept.to_string().into_bytes(),
                    }],
                },
            )
            .await
            .unwrap();
        let inbox = Arc::new(Inbox::bind().await);
        let (_tx, rx) = watch::channel(false);
        let serving = inbox.clone();
        tokio::spawn(async move { serving.serve(rx).await });
        let client = reqwest::Client::new();
        let spec = spec(&inbox.url());
        let metric = counter();
        assert!(
            deliver_repo(engine.stores().0, &client, &spec, 0, id, "k", &Some(metric))
                .await
                .unwrap()
        );
        let seen = inbox.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert!(
            !seen[0]
                .1
                .windows(skipped.to_string().len())
                .any(|w| w == skipped.to_string().as_bytes())
                || seen[0]
                    .1
                    .windows(kept.to_string().len())
                    .any(|w| w == kept.to_string().as_bytes())
        );
        let body = String::from_utf8(seen[0].1.clone()).unwrap();
        assert!(body.contains(&kept.to_string()));
        assert!(!body.lines().nth(1).unwrap().contains(&skipped.to_string()) || kept == skipped);
        assert_eq!(body.lines().nth(1).unwrap(), kept.to_string());
        assert_ne!(body.lines().nth(1).unwrap(), skipped.to_string());
    }

    #[tokio::test]
    async fn filters_deliver_only_matching_kind_and_line() {
        let harness = engine();
        let engine = &harness.engine;
        let id = repo(engine).await;
        journal(engine, id, "ChangesetCreated", r#"{"line":"main"}"#).await;
        journal(engine, id, "LandAttempt", r#"{"line":"other"}"#).await;
        let kept = journal(engine, id, "LandAttempt", r#"{"line":"main"}"#).await;
        let inbox = Arc::new(Inbox::bind().await);
        let (_tx, rx) = watch::channel(false);
        let serving = inbox.clone();
        tokio::spawn(async move { serving.serve(rx).await });
        let mut spec = spec(&inbox.url());
        spec.kinds = vec!["LandAttempt".into()];
        spec.lines = vec!["main".into()];
        let client = reqwest::Client::new();
        let metric = counter();
        for _ in 0..3 {
            assert!(
                deliver_repo(
                    engine.stores().0,
                    &client,
                    &spec,
                    0,
                    id,
                    "k",
                    &Some(metric.clone())
                )
                .await
                .unwrap()
            );
        }
        let seen = inbox.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            String::from_utf8(seen[0].1.clone())
                .unwrap()
                .lines()
                .nth(1)
                .unwrap(),
            kept.to_string()
        );
    }

    #[tokio::test]
    async fn a_rejected_post_is_final_and_later_entries_still_deliver() {
        let harness = engine();
        let engine = &harness.engine;
        let id = repo(engine).await;
        let rejected = journal(engine, id, "WorkspaceOpened", r#"{"line":"main"}"#).await;
        let later = journal(engine, id, "WorkspaceOpened", r#"{"line":"main"}"#).await;
        let inbox = Arc::new(Inbox::bind().await);
        inbox.set_status(400);
        let (_tx, rx) = watch::channel(false);
        let serving = inbox.clone();
        tokio::spawn(async move { serving.serve(rx).await });
        let client = reqwest::Client::new();
        let spec = spec(&inbox.url());
        let metric = counter();
        assert!(
            deliver_repo(
                engine.stores().0,
                &client,
                &spec,
                0,
                id,
                "k",
                &Some(metric.clone())
            )
            .await
            .unwrap()
        );
        assert_eq!(value(&metric, "failed"), 1);
        assert_eq!(
            read_cursor(engine.stores().0, &spec, id, &cursor_key(0))
                .await
                .unwrap(),
            rejected.saturating_add(1)
        );
        inbox.set_status(200);
        assert!(
            deliver_repo(
                engine.stores().0,
                &client,
                &spec,
                0,
                id,
                "k",
                &Some(metric.clone())
            )
            .await
            .unwrap()
        );
        let seen = inbox.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, 400);
        assert_eq!(seen[1].0, 200);
        assert_eq!(
            String::from_utf8(seen[1].1.clone())
                .unwrap()
                .lines()
                .nth(1)
                .unwrap(),
            later.to_string()
        );
    }

    #[tokio::test]
    async fn a_new_hook_starts_at_the_tip() {
        let harness = engine();
        let engine = &harness.engine;
        let id = repo(engine).await;
        let before = journal(engine, id, "LandAttempt", r#"{"line":"main"}"#).await;
        let inbox = Arc::new(Inbox::bind().await);
        let (_tx, rx) = watch::channel(false);
        let serving = inbox.clone();
        tokio::spawn(async move { serving.serve(rx).await });
        let mut spec = spec(&inbox.url());
        spec.start = "tip".into();
        let client = reqwest::Client::new();
        let metric = counter();
        assert!(
            !deliver_repo(
                engine.stores().0,
                &client,
                &spec,
                0,
                id,
                "k",
                &Some(metric.clone())
            )
            .await
            .unwrap()
        );
        assert!(inbox.seen.lock().unwrap().is_empty());
        assert_eq!(
            read_cursor(engine.stores().0, &spec, id, &cursor_key(0))
                .await
                .unwrap(),
            before.saturating_add(1)
        );
        let after = journal(engine, id, "LandAttempt", r#"{"line":"main"}"#).await;
        assert!(
            deliver_repo(engine.stores().0, &client, &spec, 0, id, "k", &Some(metric))
                .await
                .unwrap()
        );
        let seen = inbox.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            String::from_utf8(seen[0].1.clone())
                .unwrap()
                .lines()
                .nth(1)
                .unwrap(),
            after.to_string()
        );
        assert_ne!(after, before);
    }

    /// A new hook on a long journal asks for the tip once and reads at most
    /// the next row. It does not page the history.
    #[tokio::test]
    async fn a_new_hook_on_a_long_journal_reads_the_tip_once() {
        let harness = engine();
        let engine = &harness.engine;
        let id = repo(engine).await;
        let batch = MetaTxn {
            preconditions: vec![],
            writes: (0..10_000)
                .map(|n| Write::Journal {
                    kind: "LandAttempt".into(),
                    body: format!(r#"{{"line":"main","n":{n}}}"#).into_bytes(),
                })
                .collect(),
        };
        engine.stores().0.commit(id, batch).await.unwrap();
        let counted = Counted {
            inner: harness.meta.clone(),
            journal: Arc::new(AtomicU64::new(0)),
            journal_tip: Arc::new(AtomicU64::new(0)),
        };
        let inbox = Arc::new(Inbox::bind().await);
        let (_tx, rx) = watch::channel(false);
        let serving = inbox.clone();
        tokio::spawn(async move { serving.serve(rx).await });
        let mut spec = spec(&inbox.url());
        spec.start = "tip".into();
        let client = reqwest::Client::new();
        assert!(
            !deliver_repo(&counted, &client, &spec, 0, id, "k", &None)
                .await
                .unwrap()
        );
        assert!(inbox.seen.lock().unwrap().is_empty());
        assert_eq!(counted.journal_tip.load(Ordering::Relaxed), 1);
        assert!(
            counted.journal.load(Ordering::Relaxed) <= 1,
            "journal reads {}",
            counted.journal.load(Ordering::Relaxed)
        );
        assert_eq!(
            read_cursor(&counted, &spec, id, &cursor_key(0))
                .await
                .unwrap(),
            10_001
        );
    }

    struct Counted {
        inner: Arc<SqliteMetaStore>,
        journal: Arc<AtomicU64>,
        journal_tip: Arc<AtomicU64>,
    }

    impl MetaStore for Counted {
        fn create_repo(&self, name: &str) -> impl Future<Output = StoreResult<RepoId>> + Send {
            self.inner.create_repo(name)
        }
        fn repo(&self, name: &str) -> impl Future<Output = StoreResult<Option<RepoId>>> + Send {
            self.inner.repo(name)
        }
        fn commit(
            &self,
            repo: RepoId,
            txn: MetaTxn,
        ) -> impl Future<Output = StoreResult<TxnOutcome>> + Send {
            self.inner.commit(repo, txn)
        }
        fn changeset(
            &self,
            repo: RepoId,
            id: graft_model::ChangesetId,
        ) -> impl Future<Output = StoreResult<Option<(Vec<u8>, graft_store::ChangesetRecord)>>> + Send
        {
            self.inner.changeset(repo, id)
        }
        fn get_ref(
            &self,
            repo: RepoId,
            name: &str,
        ) -> impl Future<Output = StoreResult<Option<graft_store::RefRecord>>> + Send {
            self.inner.get_ref(repo, name)
        }
        fn list_refs(
            &self,
            repo: RepoId,
            prefix: &str,
            limit: u32,
        ) -> impl Future<Output = StoreResult<Vec<graft_store::RefRecord>>> + Send {
            self.inner.list_refs(repo, prefix, limit)
        }
        fn workspace(
            &self,
            repo: RepoId,
            name: &str,
        ) -> impl Future<Output = StoreResult<Option<graft_store::WorkspaceRecord>>> + Send
        {
            self.inner.workspace(repo, name)
        }
        fn expired_workspaces(
            &self,
            repo: RepoId,
            before_ms: i64,
            limit: u32,
        ) -> impl Future<Output = StoreResult<Vec<graft_store::WorkspaceRecord>>> + Send {
            self.inner.expired_workspaces(repo, before_ms, limit)
        }
        fn list_workspaces(
            &self,
            repo: RepoId,
            owner: &str,
            after: &str,
            limit: u32,
        ) -> impl Future<Output = StoreResult<Vec<graft_store::WorkspaceRecord>>> + Send {
            self.inner.list_workspaces(repo, owner, after, limit)
        }
        fn count_records(
            &self,
            repo: RepoId,
            state: graft_store::RecordState,
        ) -> impl Future<Output = StoreResult<u64>> + Send {
            self.inner.count_records(repo, state)
        }
        fn count_workspaces(
            &self,
            repo: RepoId,
            before_ms: Option<i64>,
        ) -> impl Future<Output = StoreResult<u64>> + Send {
            self.inner.count_workspaces(repo, before_ms)
        }
        fn derived(
            &self,
            repo: RepoId,
            kind: &str,
            version: u32,
            key: &[u8],
        ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
            self.inner.derived(repo, kind, version, key)
        }
        fn line_pos(
            &self,
            repo: RepoId,
            line: &str,
            id: graft_model::ChangesetId,
        ) -> impl Future<Output = StoreResult<Option<u64>>> + Send {
            self.inner.line_pos(repo, line, id)
        }
        fn scan_line_path(
            &self,
            repo: RepoId,
            line: &str,
            path: &graft_model::RepoPath,
            before: u64,
            limit: u32,
        ) -> impl Future<Output = StoreResult<Vec<(u64, graft_model::ChangesetId)>>> + Send
        {
            self.inner.scan_line_path(repo, line, path, before, limit)
        }
        fn holders(
            &self,
            repo: RepoId,
            object: graft_model::ObjectId,
        ) -> impl Future<Output = StoreResult<u32>> + Send {
            self.inner.holders(repo, object)
        }
        fn land(
            &self,
            repo: RepoId,
            draft: graft_model::ChangesetId,
            line: &str,
        ) -> impl Future<Output = StoreResult<Option<graft_model::ChangesetId>>> + Send {
            self.inner.land(repo, draft, line)
        }
        fn journal(
            &self,
            repo: RepoId,
            from: Lsn,
            limit: u32,
        ) -> impl Future<Output = StoreResult<Vec<(Lsn, String, Vec<u8>)>>> + Send {
            self.journal.fetch_add(1, Ordering::Relaxed);
            self.inner.journal(repo, from, limit)
        }
        fn journal_tip(
            &self,
            repo: RepoId,
        ) -> impl Future<Output = StoreResult<Option<Lsn>>> + Send {
            self.journal_tip.fetch_add(1, Ordering::Relaxed);
            self.inner.journal_tip(repo)
        }
    }
}
