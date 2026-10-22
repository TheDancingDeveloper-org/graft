//! `graft-server admin`: operator commands run on the server host
//! (`docs/design.md` §6.7, §12.1).
//!
//! `token mint` and `repo create` work while the server runs: the first
//! only signs, the second writes one row through SQLite. `fsck` opens the
//! object store, which one process owns at a time, so it runs with the
//! server stopped (as after a restore). `backup` snapshots the metadata
//! while the server runs.

use std::io::Read as _;
use std::sync::Arc;

use graft_auth::{NoDenylist, Token};
use graft_engine::{Engine, EngineConfig};
use graft_model::{ObjectId, SystemClock};
use graft_proto::{AdminApi as _, CallContext, Principal, PrincipalKind};
use graft_store::{ObjectStore, RepoId, StoreError, StoreResult, StoreStats};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};

use crate::config::ServerConfig;
use crate::denylist::Denylist;
use crate::mint::{MintRequest, mint, now_s};
use crate::{backup, fsck, keys, serve};

/// Default `token mint` TTL: 30 days, for operator and service tokens
/// that live in Infisical and are rotated there.
const DEFAULT_TTL_S: u64 = 30 * 24 * 3600;

/// A command's failure: the message, and the exit code.
pub(crate) struct Failure(pub(crate) String, pub(crate) u8);

impl Failure {
    fn usage(msg: impl Into<String>) -> Self {
        Self(msg.into(), 64)
    }

    fn failed(msg: impl Into<String>) -> Self {
        Self(msg.into(), 1)
    }
}

/// Runs `admin <args>`.
pub(crate) fn run(config: &ServerConfig, args: &[String]) -> Result<(), Failure> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["token", "mint", rest @ ..] => token_mint(config, rest),
        ["token", "revoke"] => token_revoke(config),
        ["repo", "create", name] => repo_create(config, name),
        ["fsck", repo] => fsck(config, repo),
        ["backup", dir, rest @ ..] => backup(config, dir, rest),
        ["gc", ..] => Err(Failure(
            "gc is not implemented in this release (docs/plan.md, G2.9)".into(),
            2,
        )),
        _ => Err(Failure::usage(crate::USAGE)),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::failed(format!("runtime: {e}")))
}

fn token_mint(config: &ServerConfig, args: &[&str]) -> Result<(), Failure> {
    let mut req = MintRequest {
        subject: String::new(),
        kind: PrincipalKind::Human,
        scopes: Vec::new(),
        ttl_s: DEFAULT_TTL_S,
        session: None,
        work_item: None,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .map(|v| v.to_string())
                .ok_or_else(|| Failure::usage(format!("{flag} needs a value")))
        };
        match *flag {
            "--subject" => req.subject = value()?,
            "--scope" => req.scopes.push(value()?),
            "--ttl" => {
                req.ttl_s = value()?
                    .parse()
                    .map_err(|_| Failure::usage("--ttl takes seconds"))?
            }
            "--kind" => {
                req.kind = match value()?.as_str() {
                    "human" => PrincipalKind::Human,
                    "agent" => PrincipalKind::Agent,
                    "ci" => PrincipalKind::Ci,
                    "service" => PrincipalKind::Service,
                    _ => return Err(Failure::usage("--kind is human, agent, ci or service")),
                }
            }
            "--session" => req.session = Some(value()?),
            "--work-item" => req.work_item = Some(value()?),
            other => return Err(Failure::usage(format!("unknown flag {other}"))),
        }
    }
    if req.subject.is_empty() || req.scopes.is_empty() {
        return Err(Failure::usage(
            "token mint needs --subject and at least one --scope",
        ));
    }
    let key = keys::load_or_create(&config.key_path())
        .map_err(|e| Failure::failed(format!("signing key: {e}")))?;
    let token = mint(&key, req, now_s()).map_err(|e| Failure::failed(e.to_string()))?;
    // The token goes to stdout only, for the operator to store in Infisical.
    println!("{}", token.expose());
    Ok(())
}

/// `token revoke`: reads a token from stdin (never argv) and refuses it
/// from now on, for writes. Only its `jti` and expiry are stored. The token
/// must still be live and signed by this server's key, since that is the
/// only way its `jti` is known.
fn token_revoke(config: &ServerConfig) -> Result<(), Failure> {
    let mut text = String::new();
    std::io::stdin()
        .take(graft_auth::MAX_TOKEN_LEN as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| Failure::failed(format!("read stdin: {e}")))?;
    let key = keys::load_or_create(&config.key_path())
        .map_err(|e| Failure::failed(format!("signing key: {e}")))?;
    let now = now_s();
    let claims = graft_auth::verify(
        &key.verifying_key(),
        &Token::from_encoded(text.trim().to_owned()),
        now,
        &NoDenylist,
    )
    .map_err(|_| {
        Failure::failed(
            "not a live token of this server (expired, malformed, or signed by another key)",
        )
    })?;
    if !config.meta_db.exists() {
        return Err(Failure::failed(format!(
            "{} does not exist; start the server once first",
            config.meta_db.display()
        )));
    }
    let deny = Denylist::open(&config.meta_db).map_err(Failure::failed)?;
    deny.revoke(&claims.jti, claims.exp, now)
        .map_err(Failure::failed)?;
    println!("revoked {} until {}", claims.jti, claims.exp);
    Ok(())
}

fn repo_create(config: &ServerConfig, name: &str) -> Result<(), Failure> {
    if let Some(dir) = config.meta_db.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Failure::failed(format!("create {}: {e}", dir.display())))?;
    }
    let meta = SqliteMetaStore::open(SqliteConfig::new(&config.meta_db))
        .map_err(|e| Failure::failed(format!("open {}: {e:?}", config.meta_db.display())))?;
    // Only metadata changes; the object store stays with the server.
    let engine = Engine::new(
        Arc::new(meta),
        Arc::new(NoObjects),
        Arc::new(SystemClock),
        EngineConfig::default(),
    );
    let cx = operator(&format!("repo:{name}:admin"));
    runtime()?
        .block_on(engine.create_repo(&cx, name.to_owned()))
        .map_err(|e| Failure::failed(e.to_string()))?;
    println!("repository {name} exists");
    Ok(())
}

fn fsck(config: &ServerConfig, repo: &str) -> Result<(), Failure> {
    let (meta, objects) = serve::open_stores(config)
        .map_err(|e| Failure::failed(format!("{e} (fsck needs the server stopped)")))?;
    let report = runtime()?
        .block_on(fsck::fsck(&meta, &objects, repo))
        .map_err(Failure::failed)?;
    for line in &report.details {
        println!("problem: {line}");
    }
    println!(
        "fsck {repo}: {} changesets ({} collected), {} trees, {} blobs, {} bytes, {} refs, {} problems",
        report.changesets,
        report.collected,
        report.trees,
        report.blobs,
        report.bytes,
        report.refs,
        report.problems
    );
    if report.is_clean() {
        Ok(())
    } else {
        Err(Failure::failed(format!("fsck {repo}: not clean")))
    }
}

fn backup(config: &ServerConfig, dir: &str, args: &[&str]) -> Result<(), Failure> {
    let objects = match args {
        [] => None,
        ["--objects"] => Some(config.objects_dir.as_path()),
        _ => return Err(Failure::usage("backup <dir> [--objects]")),
    };
    let s = backup::backup(&config.meta_db, objects, std::path::Path::new(dir))
        .map_err(Failure::failed)?;
    println!("metadata snapshot: {}", s.meta.display());
    if objects.is_some() {
        println!(
            "objects: {} copied ({} bytes), {} already present",
            s.objects_copied, s.bytes_copied, s.objects_present
        );
    }
    Ok(())
}

/// The operator acting on the host, holding exactly `scope`.
fn operator(scope: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "operator".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec![scope.to_owned()],
        },
        request_id: "admin".into(),
        deadline_ms: None,
    }
}

/// An object store that holds nothing, for admin commands that touch only
/// metadata.
struct NoObjects;

impl ObjectStore for NoObjects {
    async fn put(&self, _repo: RepoId, _id: ObjectId, _frame: Vec<u8>) -> StoreResult<()> {
        Err(StoreError::Backend(
            "no object store in this command".into(),
        ))
    }
    async fn get(&self, _repo: RepoId, _id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        Ok(None)
    }
    async fn has(&self, _repo: RepoId, _ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        Ok(Vec::new())
    }
    async fn delete(&self, _repo: RepoId, _id: ObjectId) -> StoreResult<()> {
        Ok(())
    }
    async fn stats(&self) -> StoreResult<StoreStats> {
        Ok(StoreStats::default())
    }
}
