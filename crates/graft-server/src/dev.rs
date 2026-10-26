//! Zero-configuration development server setup.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use graft_auth::{Claims, NoDenylist, Token};
use graft_model::{Author, AuthorKind, Changeset, ObjectKind, Timestamp, Tree};
use graft_proto::{Frame, Principal, PrincipalKind};
use graft_store::{
    ChangesetRecord, Lsn, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState, RefKind,
    RefRecord, TxnOutcome, Write,
};
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
/// repository has no line yet. A second start finds the ref and writes
/// nothing. The root's content is fixed (D34): no parents, no changes,
/// empty author, bot, epoch, empty message and metadata, empty tree.
fn seed_main(config: &ServerConfig, repo: &str) -> Result<(), String> {
    let meta = SqliteMetaStore::open(SqliteConfig::new(&config.meta_db))
        .map_err(|error| format!("open {}: {error:?}", config.meta_db.display()))?;
    let objects = FsObjectStore::open(&config.objects_dir, FsMode::Authoritative)
        .map_err(|error| format!("open {}: {error:?}", config.objects_dir.display()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("runtime: {error}"))?;
    runtime.block_on(seed_main_async(&meta, &objects, repo))
}

async fn seed_main_async(
    meta: &SqliteMetaStore,
    objects: &FsObjectStore,
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

    let tree = Tree::default();
    let changeset = Changeset {
        parents: Vec::new(),
        root: tree.id(),
        changes: Vec::new(),
        author: Author {
            name: String::new(),
            email: String::new(),
            kind: AuthorKind::Bot,
        },
        authored_at: Timestamp {
            ms: 0,
            offset_min: 0,
        },
        message: String::new(),
        metadata: Default::default(),
    };
    let id = changeset.id();
    let tree_frame = Frame::encode(ObjectKind::Tree, &tree.encode()).to_bytes();
    objects
        .put(repo_id, tree.id().0, tree_frame)
        .await
        .map_err(|error| format!("store empty tree: {error:?}"))?;

    let txn = MetaTxn {
        preconditions: vec![
            Precondition::ChangesetAbsent(id),
            Precondition::RefIs {
                name: DEV_LINE.to_owned(),
                expected: None,
            },
        ],
        writes: vec![
            Write::PutChangeset {
                id,
                content: changeset.encode(),
                record: ChangesetRecord {
                    lsn: Lsn::default(),
                    received_at_ms: 0,
                    state: RecordState::Landed,
                    holder: None,
                    superseded_by: None,
                },
            },
            Write::Journal {
                kind: "ChangesetCreated".into(),
                body: format!(r#"{{"id":"{id}","state":"abandoned"}}"#).into_bytes(),
            },
            Write::SetRef(RefRecord {
                name: DEV_LINE.to_owned(),
                target: id,
                version: 1,
                kind: RefKind::Line,
            }),
            Write::PutLinePos {
                line: DEV_LINE.to_owned(),
                changeset: id,
                pos: 1,
            },
            Write::Journal {
                kind: "RefMoved".into(),
                body: format!(
                    r#"{{"name":"{DEV_LINE}","old":null,"new":"{id}","version":1,"actor":"dev-admin"}}"#
                )
                .into_bytes(),
            },
        ],
    };
    match meta
        .commit(repo_id, txn)
        .await
        .map_err(|error| format!("seed {DEV_LINE}: {error:?}"))?
    {
        TxnOutcome::Committed(_) => Ok(()),
        TxnOutcome::PreconditionFailed(index) => {
            if index == 1
                && meta
                    .get_ref(repo_id, DEV_LINE)
                    .await
                    .map_err(|error| format!("read {DEV_LINE}: {error:?}"))?
                    .is_some()
            {
                Ok(())
            } else {
                Err(format!("seed {DEV_LINE}: precondition {index} failed"))
            }
        }
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
