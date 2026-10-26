//! A local repository: the engine opened in-process over the same stores
//! the server uses (`docs/design.md` §5.4, decision D32).
//!
//! No HTTP, no node socket and no token. The process holds `repo.lock`
//! (`flock`, exclusive, non-blocking) for as long as [`LocalRepo`] lives; a
//! second opener gets [`Error::Busy`]. One process at a time is the whole
//! concurrency story: there are no leases across processes, and the local
//! principal is implicit.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use graft_engine::{Engine, EngineConfig};
use graft_model::SystemClock;
use graft_proto::api::AdminApi as _;
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind};
use graft_store_fs::{FsMode, FsObjectStore};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};

use crate::Error;

/// Metadata file inside the repository directory.
pub const META_FILE: &str = "meta.sqlite";

/// Object-tree directory inside the repository directory.
pub const OBJECTS_DIR: &str = "objects";

/// Exclusive lock file inside the repository directory.
pub const LOCK_FILE: &str = "repo.lock";

/// The local principal's subject. There is one, and it is not authenticated.
pub const LOCAL_SUBJECT: &str = "local";

/// The engine over the production stores, the type the server wires.
pub type LocalEngine = Engine<SqliteMetaStore, FsObjectStore>;

/// A repository directory opened in this process.
pub struct LocalRepo {
    dir: PathBuf,
    name: String,
    engine: Arc<LocalEngine>,
    _lock: File,
}

impl std::fmt::Debug for LocalRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRepo")
            .field("dir", &self.dir)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl LocalRepo {
    /// Creates `dir` as a repository named `name` and opens it.
    ///
    /// `dir` is the repository directory itself (`DIR/.graft/repo` in the
    /// design): `meta.sqlite`, `objects/` and `repo.lock`. Creating a name
    /// that already exists is a successful retry, as `create_repo` is.
    pub fn create(dir: impl Into<PathBuf>, name: &str) -> Result<Self, Error> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(Error::Io)?;
        let mut repo = Self::open(dir)?;
        repo.ensure_repo(name)?;
        Ok(repo)
    }

    /// Opens a repository directory, taking `repo.lock`.
    ///
    /// The directory is created if it is missing, so a caller can open
    /// before the first `create_repo`. The object store's own lock
    /// (`objects/.lock`) is held as well, for the life of the engine.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, Error> {
        let dir = dir.into();
        let lock = lock_repo(&dir)?;
        let meta =
            SqliteMetaStore::open(SqliteConfig::new(dir.join(META_FILE))).map_err(Error::Store)?;
        // The engine verifies frames as it decodes them, as the server does.
        let objects = FsObjectStore::open(dir.join(OBJECTS_DIR), FsMode::Authoritative)
            .map_err(Error::Store)?
            .verify_reads(false);
        let engine = Arc::new(Engine::new(
            Arc::new(meta),
            Arc::new(objects),
            Arc::new(SystemClock),
            EngineConfig::default(),
        ));
        Ok(Self {
            dir,
            name: String::new(),
            engine,
            _lock: lock,
        })
    }

    /// The repository directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The name passed to [`LocalRepo::create`], empty when only opened.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The in-process engine. Commands call the API traits on it.
    pub fn engine(&self) -> &LocalEngine {
        &self.engine
    }

    /// A shared handle, for a long-running session (`graft mcp` on stdio).
    pub fn engine_handle(&self) -> Arc<LocalEngine> {
        Arc::clone(&self.engine)
    }

    /// A call context for the implicit local principal on repository `name`:
    /// admin of that repository, with no token.
    pub fn context(name: &str, request_id: impl Into<String>) -> CallContext {
        CallContext {
            principal: Principal {
                subject: LOCAL_SUBJECT.to_owned(),
                kind: PrincipalKind::Human,
                session: None,
                work_item: None,
                scopes: vec![format!("repo:{name}:admin")],
            },
            request_id: request_id.into(),
            deadline_ms: None,
        }
    }

    fn ensure_repo(&mut self, name: &str) -> Result<(), Error> {
        let cx = Self::context(name, "local-init");
        let engine = Arc::clone(&self.engine);
        let name = name.to_owned();
        block_on(async move { engine.create_repo(&cx, name.clone()).await.map(|()| name) }).map(
            |name| {
                self.name = name;
            },
        )
    }
}

fn block_on<T>(fut: impl std::future::Future<Output = Result<T, ApiError>>) -> Result<T, Error> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Io)?;
    rt.block_on(fut).map_err(Error::Api)
}

fn lock_repo(dir: &Path) -> Result<File, Error> {
    fs::create_dir_all(dir).map_err(Error::Io)?;
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(Error::Io)?;
    #[cfg(unix)]
    {
        use rustix::fs::{FlockOperation, flock};
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(file),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(Error::Busy),
            Err(e) => Err(Error::Io(std::io::Error::from(e))),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        Err(Error::State(
            "local repositories need an exclusive file lock".to_owned(),
        ))
    }
}
