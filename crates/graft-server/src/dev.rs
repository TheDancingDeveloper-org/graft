//! Zero-configuration development server setup.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use graft_auth::{Claims, NoDenylist, Token};
use graft_engine::{Engine, EngineConfig};
use graft_model::{Changeset, SystemClock};
use graft_proto::types::{CreateChangeset, CreateLine};
use graft_proto::{Principal, PrincipalKind, RepoApi};
use graft_store::MetaStore;
use graft_store_fs::{FsMode, FsObjectStore};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};

use crate::config::ServerConfig;

/// Inputs for a local development server.
#[derive(Clone, Debug)]
pub struct DevOptions {
    /// Server data directory (objects and SQLite metadata).
    pub data_dir: PathBuf,
    /// Private state directory containing the signing key.
    pub state_dir: PathBuf,
    /// Main listener. Port zero asks the operating system for a free port.
    pub listen: SocketAddr,
    /// Repository to create and administer, when requested.
    pub repo: Option<String>,
    /// Permit a non-loopback listener. The caller must opt in explicitly.
    pub insecure_bind: bool,
    /// Previously stored token to reuse, if any.
    pub token: Option<String>,
}

impl DevOptions {
    /// Local defaults with an operating-system-selected port.
    pub fn new(data_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            state_dir: state_dir.into(),
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            repo: None,
            insecure_bind: false,
            token: None,
        }
    }
}

/// Prepared development server state. Its debug form never includes a token.
pub struct DevServer {
    config: ServerConfig,
    token: Option<Token>,
}

impl fmt::Debug for DevServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevServer")
            .field("listen", &self.config.listen)
            .field("objects_dir", &self.config.objects_dir)
            .field("meta_db", &self.config.meta_db)
            .field("key_file", &self.config.key_file)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl DevServer {
    /// Builds local server configuration, creates its key and optional
    /// repository, and mints or validates the repository admin token.
    pub fn prepare(options: DevOptions) -> Result<Self, String> {
        if !options.listen.ip().is_loopback() && !options.insecure_bind {
            return Err(format!(
                "refusing non-loopback development listener {}; pass --insecure-bind to allow it",
                options.listen
            ));
        }
        std::fs::create_dir_all(&options.data_dir)
            .map_err(|error| format!("create {}: {error}", options.data_dir.display()))?;
        create_private_dir(&options.state_dir)?;

        let mut config = ServerConfig {
            listen: options.listen.to_string(),
            objects_dir: options.data_dir.join("objects"),
            meta_db: options.data_dir.join("meta.db"),
            key_file: Some(options.state_dir.join("signing.key")),
            json_logs: false,
            min_free_fraction: 0.0,
            ..ServerConfig::default()
        };
        config.check().map_err(|error| error.to_string())?;
        let signing = crate::keys::load_or_create(&config.key_path())
            .map_err(|error| format!("signing key: {error}"))?;

        let token = match options.repo.as_deref() {
            Some(repo) => {
                crate::admin::repo_create_quiet(&config, repo)?;
                seed_main(&config, repo)?;
                Some(match options.token {
                    Some(encoded) => validate_token(&signing, encoded, repo)?,
                    None => mint_token(&signing, repo)?,
                })
            }
            None if options.token.is_some() => {
                return Err("a stored development token needs --repo".into());
            }
            None => None,
        };

        config.key_file = Some(config.key_path());
        Ok(Self { config, token })
    }

    /// Encoded admin token for the configured repository, if one was named.
    /// Treat the returned value as a secret.
    pub fn token(&self) -> Option<&str> {
        self.token.as_ref().map(Token::expose)
    }

    /// Listener requested by the caller. Port zero is resolved when serving.
    pub fn listen(&self) -> &str {
        &self.config.listen
    }

    /// Runs until shutdown and reports the bound address once ready.
    pub fn serve(
        self,
        ready: impl FnOnce(SocketAddr) -> Result<(), String> + Send + 'static,
    ) -> Result<(), String> {
        crate::serve::serve_ready(self.config, ready)
    }
}

/// The line a development repository starts on.
const DEV_LINE: &str = "main";

/// Writes the canonical empty root and points `main` at it, when the
/// repository has no line yet. The same engine calls `graft init` uses, so
/// the id, the record and the journal come from one path.
fn seed_main(config: &ServerConfig, repo: &str) -> Result<(), String> {
    let meta = Arc::new(
        SqliteMetaStore::open(SqliteConfig::new(&config.meta_db))
            .map_err(|error| format!("open {}: {error:?}", config.meta_db.display()))?,
    );
    let objects = Arc::new(
        FsObjectStore::open(&config.objects_dir, FsMode::Authoritative)
            .map_err(|error| format!("open {}: {error:?}", config.objects_dir.display()))?,
    );
    let engine = Engine::new(
        meta.clone(),
        objects,
        Arc::new(SystemClock),
        EngineConfig::default(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("runtime: {error}"))?;
    runtime.block_on(seed_main_async(&engine, meta.as_ref(), repo))
}

async fn seed_main_async(
    engine: &Engine<SqliteMetaStore, FsObjectStore>,
    meta: &SqliteMetaStore,
    repo: &str,
) -> Result<(), String> {
    let repo_id = meta
        .repo(repo)
        .await
        .map_err(|error| format!("repository {repo}: {error:?}"))?
        .ok_or_else(|| format!("repository {repo} was not created"))?;
    if meta
        .get_ref(repo_id, DEV_LINE)
        .await
        .map_err(|error| format!("read {DEV_LINE}: {error:?}"))?
        .is_some()
    {
        return Ok(());
    }
    let cx = operator(repo);
    let root_cs = Changeset::canonical_empty_root();
    let created = engine
        .create_changeset(
            &cx,
            CreateChangeset {
                repo: repo.to_owned(),
                parents: root_cs.parents.clone(),
                changes: root_cs.changes.clone(),
                author: root_cs.author.clone(),
                authored_at: root_cs.authored_at,
                message: root_cs.message.clone(),
                metadata: root_cs.metadata.clone(),
                inline_blobs: Vec::new(),
                workspace: None,
            },
        )
        .await
        .map_err(|error| format!("seed root: {error}"))?;
    engine
        .create_line(
            &cx,
            CreateLine {
                repo: repo.to_owned(),
                name: DEV_LINE.to_owned(),
                from: created.id,
            },
        )
        .await
        .map_err(|error| format!("seed {DEV_LINE}: {error}"))?;
    Ok(())
}

/// The operator acting on the host, holding the repository's admin scope.
fn operator(repo: &str) -> graft_proto::CallContext {
    graft_proto::CallContext {
        principal: Principal {
            subject: "operator".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{repo}:admin")],
        },
        request_id: "dev-seed".into(),
        deadline_ms: None,
    }
}

fn create_private_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("secure {}: {error}", path.display()))?;
    }
    Ok(())
}

fn validate_token(
    signing: &graft_auth::SigningKey,
    encoded: String,
    repo: &str,
) -> Result<Token, String> {
    let token = Token::from_encoded(encoded);
    let claims = graft_auth::verify(
        &signing.verifying_key(),
        &token,
        crate::mint::now_s(),
        &NoDenylist,
    )
    .map_err(|error| format!("stored development token: {error}"))?;
    let scope = format!("repo:{repo}:admin");
    if claims.principal.scopes.iter().any(|held| held == &scope) {
        Ok(token)
    } else {
        Err(format!("stored development token lacks {scope}"))
    }
}

fn mint_token(signing: &graft_auth::SigningKey, repo: &str) -> Result<Token, String> {
    let claims = Claims {
        principal: Principal {
            subject: "dev-admin".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{repo}:admin")],
        },
        exp: i64::MAX,
        jti: graft_auth::new_jti().map_err(|error| format!("token id: {error}"))?,
    };
    graft_auth::mint(signing, &claims).map_err(|error| format!("mint token: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_store::MetaStore;
    use std::fs;

    fn paths(name: &str) -> (graft_testkit::crash::TempDir, PathBuf, PathBuf) {
        let temp = graft_testkit::crash::TempDir::new(name);
        let data = temp.path().join("data");
        let state = temp.path().join("state");
        (temp, data, state)
    }

    #[test]
    fn prepares_private_reusable_credentials() {
        let (_temp, data, state) = paths("server-dev-setup");
        let mut options = DevOptions::new(&data, &state);
        options.repo = Some("demo".into());
        let first = DevServer::prepare(options.clone()).expect("first setup");
        let encoded = first.token().expect("admin token").to_owned();

        options.token = Some(encoded.clone());
        let second = DevServer::prepare(options).expect("second setup");
        assert_eq!(second.token(), Some(encoded.as_str()));
        assert!(!format!("{second:?}").contains(&encoded));
        assert!(data.join("meta.db").is_file());
        assert!(state.join("signing.key").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(state.join("signing.key"))
                .expect("stat key")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let state_mode = fs::metadata(&state)
                .expect("stat state")
                .permissions()
                .mode();
            assert_eq!(state_mode & 0o777, 0o700);
        }
    }

    #[test]
    fn refuses_non_loopback_without_explicit_opt_in() {
        let (_temp, data, state) = paths("server-dev-bind");
        let mut options = DevOptions::new(data, state);
        options.listen = "0.0.0.0:0".parse().expect("address");
        assert!(DevServer::prepare(options.clone()).is_err());
        options.insecure_bind = true;
        assert!(DevServer::prepare(options).is_ok());
    }

    #[test]
    fn seeded_root_matches_the_canonical_id_and_journal() {
        let (_temp, data, state) = paths("server-dev-seed");
        let mut options = DevOptions::new(&data, &state);
        options.repo = Some("demo".into());
        DevServer::prepare(options).expect("setup");

        let meta = SqliteMetaStore::open(SqliteConfig::new(data.join("meta.db"))).expect("meta");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let repo = meta.repo("demo").await.expect("repo").expect("created");
            let line = meta
                .get_ref(repo, DEV_LINE)
                .await
                .expect("ref")
                .expect("main");
            let canonical = Changeset::canonical_empty_root();
            assert_eq!(line.target, canonical.id());
            let (content, record) = meta
                .changeset(repo, canonical.id())
                .await
                .expect("load")
                .expect("stored");
            let stored = Changeset::decode(&content).expect("decode");
            assert!(stored.is_canonical_empty_root());
            assert_eq!(record.state, graft_store::RecordState::Landed);
            let journal = meta
                .journal(repo, graft_store::Lsn::default(), 8)
                .await
                .expect("journal");
            let created = journal
                .iter()
                .find(|(_, kind, _)| kind == "ChangesetCreated")
                .expect("ChangesetCreated");
            let body = String::from_utf8(created.2.clone()).expect("utf8");
            // The create entry is the engine's own: the same body `graft init`
            // writes. The line lands the record afterwards.
            assert_eq!(
                body,
                format!(r#"{{"id":"{}","state":"abandoned"}}"#, canonical.id())
            );
        });
    }

    #[test]
    fn rejects_a_stored_token_for_another_repository() {
        let (_temp, data, state) = paths("server-dev-token-scope");
        let mut first = DevOptions::new(&data, &state);
        first.repo = Some("one".into());
        let token = DevServer::prepare(first)
            .expect("first setup")
            .token()
            .expect("token")
            .to_owned();
        let mut second = DevOptions::new(data, state);
        second.repo = Some("two".into());
        second.token = Some(token);
        assert!(DevServer::prepare(second).is_err());
    }
}
