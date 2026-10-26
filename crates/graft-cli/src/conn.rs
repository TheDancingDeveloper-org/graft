//! Connecting to a server or node, finding the working copy, and the call
//! context. Credentials resolve from explicit overrides, environment, then a
//! named user configuration. Tokens are never printed or put in an argument.

use std::env;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::GRAFT_DIR;
use graft_client::workcopy::{
    Begun, DEFAULT_CACHE_BYTES, Recovered, WcConfig, WcInfo, WcState, WorkingCopy,
};
use graft_client::workspace::DEFAULT_LEASE_MS;
use graft_client::{Backend, ENDPOINT_ENV, Endpoint, LocalRepo, NODE_SOCKET, Remote};
use graft_model::{ChangesetId, IdPrefix, ObjectKind};
use graft_proto::ApiError;
use graft_proto::api::RepoApi;
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use graft_proto::types::{ListRefs, Log, Page, ResolveRef, WorkspaceRef};

use crate::cli::{DEFAULT_LINE, Global};
use crate::config::UserConfig;
use crate::error::Failure;

/// The environment variable that carries the bearer token.
pub(crate) const TOKEN_ENV: &str = "GRAFT_TOKEN";

/// A connected session: the remote, the repository, and the working copy
/// when the current directory is inside one.
pub(crate) struct Conn {
    pub(crate) remote: Backend,
    pub(crate) repo: String,
    pub(crate) server: Option<String>,
    /// The line a short id is looked for on: the working copy's, else `main`.
    pub(crate) line: String,
    /// Changesets the working copy knows (base, head, an unfinished move),
    /// the first place a short id is looked for.
    pub(crate) known: Vec<ChangesetId>,
    /// Keeps a local repository's exclusive lock for the command lifetime.
    pub(crate) _local: Option<Arc<LocalRepo>>,
}

pub(crate) struct ResolvedSettings {
    pub(crate) server: Option<String>,
    pub(crate) endpoint: Endpoint,
    pub(crate) token: Option<String>,
}

/// How many refs, and how many changesets of the line's recent history, a
/// short id is matched against. Prefix resolution is bounded: an id older
/// than this needs its full form.
const PREFIX_REFS: u32 = 256;
const PREFIX_HISTORY: u32 = 512;

/// A single-threaded runtime: the CLI makes one call at a time.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// The working copy containing the current directory, if there is one.
/// A `.graft` directory that fails to load is an error, not "no working copy".
pub(crate) fn working_copy() -> Result<Option<WorkingCopy>, Failure> {
    let cwd = env::current_dir()?;
    if !cwd.ancestors().any(|d| d.join(GRAFT_DIR).is_dir()) {
        return Ok(None);
    }
    Ok(Some(WorkingCopy::open(&cwd)?))
}

/// The configuration and state of the working copy containing the current
/// directory, if there is one, without its index: for reads that need only
/// the repository, the endpoint and the base.
pub(crate) fn working_copy_info() -> Result<Option<WcInfo>, Failure> {
    let cwd = env::current_dir()?;
    if !cwd.ancestors().any(|d| d.join(GRAFT_DIR).is_dir()) {
        return Ok(None);
    }
    Ok(Some(WorkingCopy::open_info(&cwd)?))
}

/// What [`connect`] and [`default_rev`] read of a working copy.
pub(crate) trait WcView {
    fn config(&self) -> &WcConfig;
    fn server(&self) -> Option<&str>;
    fn state(&self) -> &WcState;
}

impl WcView for WorkingCopy {
    fn config(&self) -> &WcConfig {
        WorkingCopy::config(self)
    }
    fn server(&self) -> Option<&str> {
        WorkingCopy::server(self)
    }
    fn state(&self) -> &WcState {
        WorkingCopy::state(self)
    }
}

impl WcView for WcInfo {
    fn config(&self) -> &WcConfig {
        WcInfo::config(self)
    }
    fn server(&self) -> Option<&str> {
        WcInfo::server(self)
    }
    fn state(&self) -> &WcState {
        WcInfo::state(self)
    }
}

/// The working copy the command needs, or a usage error saying so.
pub(crate) fn require_working_copy() -> Result<WorkingCopy, Failure> {
    working_copy()?.ok_or_else(|| Failure::usage("not inside a working copy (no .graft directory)"))
}

/// The call context for one request. Request ids are for logs and
/// idempotency, so they only need to be unique per process. The session and
/// work item come from `VOGT_SESSION_ID` and `VOGT_WORK_ITEM`.
pub(crate) fn call_context() -> CallContext {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let session = crate::identity::session();
    CallContext {
        principal: Principal {
            subject: env::var("USER").unwrap_or_else(|_| "graft-cli".to_owned()),
            kind: if session.is_some() {
                PrincipalKind::Agent
            } else {
                PrincipalKind::Human
            },
            session,
            work_item: crate::identity::work_item(),
            scopes: Vec::new(),
        },
        request_id: graft_client::request_id("cli", n),
        deadline_ms: None,
    }
}

/// A `--repo` value is a name, or `graft://` / `grafts://` carrying the endpoint.
fn split_repo_arg(repo: &str) -> Result<(String, Option<String>), Failure> {
    if repo.starts_with("graft://") || repo.starts_with("grafts://") {
        let url = graft_client::RepoUrl::parse(repo).map_err(Failure::usage)?;
        if url.line.is_some() {
            return Err(Failure::usage(
                "--repo graft URL does not take ?line=; pass the line to the command",
            ));
        }
        Ok((url.repo, Some(url.endpoint)))
    } else if repo.contains("://") {
        Err(Failure::usage(
            "expected a repository name, graft://host[:port]/repo, or grafts://host[:port]/repo",
        ))
    } else {
        Ok((repo.to_owned(), None))
    }
}

/// Connects. Named credentials are used only when their server owns the
/// resolved endpoint.
/// The repository directory of a `local:<path>` server value.
pub(crate) fn local_repo_path(server: &str) -> Option<&str> {
    let path = server.strip_prefix("local:")?;
    (!path.is_empty()).then_some(path)
}

/// True when `--endpoint` or a non-empty `GRAFT_ENDPOINT` is set.
pub(crate) fn endpoint_is_overridden(g: &Global) -> bool {
    g.endpoint.is_some()
        || env::var(ENDPOINT_ENV)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
}

pub(crate) async fn connect(g: &Global, wc: Option<&impl WcView>) -> Result<Conn, Failure> {
    let (repo, url_endpoint) = match (&g.repo, wc) {
        (Some(repo), _) => split_repo_arg(repo)?,
        (None, Some(wc)) => (wc.config().repo.clone(), None),
        (None, None) => {
            return Err(Failure::usage(
                "no repository: pass --repo, or run inside a working copy",
            ));
        }
    };
    let mut g = g.clone();
    if let Some(from_url) = &url_endpoint {
        match &g.endpoint {
            Some(flag) if Endpoint::parse(flag) != Endpoint::parse(from_url) => {
                return Err(Failure::usage(
                    "--endpoint does not match the graft URL; pass one of them",
                ));
            }
            None => g.endpoint = Some(from_url.clone()),
            Some(_) => {}
        }
    }
    let local_server = wc.and_then(WcView::server).and_then(local_repo_path);
    let endpoint_overridden = endpoint_is_overridden(&g);
    if local_server.is_some() && endpoint_overridden {
        return Err(Failure::usage(
            "this working copy is local:; --endpoint and GRAFT_ENDPOINT do not apply",
        ));
    }
    let (remote, server, local) = if let Some(path) = local_server {
        let local = Arc::new(LocalRepo::open(path)?);
        (
            Backend::local(local.engine_handle(), repo.clone()),
            wc.and_then(WcView::server).map(str::to_owned),
            Some(local),
        )
    } else {
        let user = crate::config::load()?;
        let settings = resolve_settings(&g, wc, &user)?;
        let remote = Remote::connect(settings.endpoint).await?;
        let remote = match settings.token {
            Some(token) => remote.with_token(token),
            None => remote,
        };
        (Backend::remote(remote), settings.server, None)
    };
    let (line, known) = match wc {
        Some(wc) => {
            let st = wc.state();
            let mut known = vec![st.base];
            known.extend(st.head);
            if let Some(m) = &st.moving {
                known.push(m.base);
                known.extend(m.head);
            }
            (st.line.clone(), known)
        }
        None => (DEFAULT_LINE.to_owned(), Vec::new()),
    };
    Ok(Conn {
        remote,
        repo,
        server,
        line,
        known,
        _local: local,
    })
}

pub(crate) fn resolve_settings(
    g: &Global,
    wc: Option<&impl WcView>,
    user: &UserConfig,
) -> Result<ResolvedSettings, Failure> {
    let endpoint_override = g.endpoint.as_deref().map(Endpoint::parse).or_else(|| {
        env::var(ENDPOINT_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| Endpoint::parse(&value))
    });
    let token_override = read_token(g)?;
    resolve_settings_from_sources(
        wc.map(|view| (view.config(), view.server())),
        user,
        g.server.as_deref(),
        endpoint_override,
        token_override,
    )
}

fn resolve_settings_from_sources(
    wc: Option<(&WcConfig, Option<&str>)>,
    user: &UserConfig,
    server_override: Option<&str>,
    endpoint_override: Option<Endpoint>,
    token_override: Option<String>,
) -> Result<ResolvedSettings, Failure> {
    // `--server` names a server in the user config. It wins over the working
    // copy's recorded server and the user default.
    let selected_server = server_override.or_else(|| {
        wc.and_then(|(_, server)| server)
            .or(user.default_server.as_deref())
    });
    let named = selected_server.and_then(|name| user.servers.get(name));
    let legacy_endpoint = wc
        .filter(|(_, server)| server.is_none())
        .and_then(|(config, _)| config.endpoint.as_deref())
        .map(Endpoint::parse);
    if endpoint_override.is_none()
        && legacy_endpoint.is_none()
        && selected_server.is_some()
        && named.is_none()
    {
        let name = selected_server.unwrap_or_default();
        return Err(Failure::new(
            crate::Exit::Failure,
            "config",
            format!("server {name} is not configured; run `graft login {name} --endpoint URL`"),
        ));
    }
    let named_endpoint = named.map(|server| Endpoint::parse(&server.endpoint));
    let endpoint = endpoint_override
        .clone()
        .or_else(|| {
            wc.and_then(|(_, server)| server)
                .and(named_endpoint.clone())
        })
        .or_else(|| {
            let socket = Path::new(NODE_SOCKET);
            socket.exists().then(|| Endpoint::NodeSocket(socket.to_owned()))
        })
        .or(legacy_endpoint)
        .or(named_endpoint.clone())
        .ok_or_else(|| {
            Failure::usage(
                "no endpoint: pass --endpoint, set GRAFT_ENDPOINT, run `graft login`, or use a configured working copy",
            )
        })?;
    let server = named_endpoint
        .as_ref()
        .filter(|named_endpoint| **named_endpoint == endpoint)
        .and(selected_server)
        .map(str::to_owned);
    let token = match token_override {
        Some(token) => Some(token),
        None => server
            .as_ref()
            .and(named)
            .map(|server| token_from_file(&server.token_file))
            .transpose()?,
    };
    Ok(ResolvedSettings {
        server,
        endpoint,
        token,
    })
}

/// The bearer token from `--token-file`, else `GRAFT_TOKEN`. Surrounding
/// whitespace is dropped; the value is never echoed.
pub(crate) fn read_token(g: &Global) -> Result<Option<String>, Failure> {
    if let Some(path) = &g.token_file {
        return token_from_file(path).map(Some);
    }
    Ok(env::var(TOKEN_ENV)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty()))
}

pub(crate) fn token_from_file(path: &Path) -> Result<String, Failure> {
    let text = fs::read_to_string(path).map_err(|e| {
        Failure::new(
            crate::Exit::Failure,
            "token",
            format!(
                "cannot read the token file {}: {}",
                path.display(),
                e.kind()
            ),
        )
    })?;
    let token = text.trim().to_owned();
    if token.is_empty() {
        return Err(Failure::new(
            crate::Exit::Failure,
            "token",
            format!("the token file {} is empty", path.display()),
        ));
    }
    Ok(token)
}

impl Conn {
    /// Resolves a changeset id, a short id (as the CLI prints them) or a
    /// ref name to a changeset id.
    pub(crate) async fn resolve(
        &self,
        cx: &CallContext,
        rev: &str,
    ) -> Result<ChangesetId, Failure> {
        if let Ok(id) = ChangesetId::parse(rev) {
            return Ok(id);
        }
        let found = self
            .remote
            .resolve_ref(
                cx,
                ResolveRef {
                    repo: self.repo.clone(),
                    name: rev.to_owned(),
                },
            )
            .await;
        match (found, IdPrefix::parse(rev)) {
            (Ok(view), _) => Ok(view.target),
            (Err(ApiError::NotFound { .. }), Ok(prefix))
                if prefix.kind() == ObjectKind::Changeset =>
            {
                self.resolve_prefix(cx, &prefix).await
            }
            (Err(e), _) => Err(e.into()),
        }
    }

    /// Resolves a short changeset id against what the working copy knows,
    /// the refs' targets, and the recent history of the line: a bounded
    /// set, never the whole history.
    async fn resolve_prefix(
        &self,
        cx: &CallContext,
        prefix: &IdPrefix,
    ) -> Result<ChangesetId, Failure> {
        let mut candidates = self.known.clone();
        let refs = self
            .remote
            .list_refs(
                cx,
                ListRefs {
                    repo: self.repo.clone(),
                    prefix: None,
                    page: Page {
                        limit: PREFIX_REFS,
                        token: None,
                    },
                },
            )
            .await?;
        let head = refs.iter().find(|r| r.name == self.line).map(|r| r.target);
        candidates.extend(refs.iter().map(|r| r.target));
        if let Some(from) = head {
            let history = self
                .remote
                .log(
                    cx,
                    Log {
                        repo: self.repo.clone(),
                        from,
                        until: None,
                        paths: Vec::new(),
                        page: Page {
                            limit: PREFIX_HISTORY,
                            token: None,
                        },
                    },
                )
                .await?;
            candidates.extend(history.changesets.iter().map(|c| c.id));
        }
        candidates.sort();
        candidates.dedup();
        let mut matches = candidates.into_iter().filter(|id| prefix.matches(&id.0));
        match (matches.next(), matches.next()) {
            (Some(id), None) => Ok(id),
            (Some(_), Some(_)) => Err(Failure::usage(format!(
                "the short id {prefix} is ambiguous; give more of it"
            ))),
            (None, _) => Err(ApiError::NotFound {
                what: format!("changeset {prefix}"),
            }
            .into()),
        }
    }
}

/// A write command's session: the working copy, connected, its workspace
/// renewed (or opened) and any interrupted commit finished
/// (`WorkingCopy::begin`).
pub(crate) struct Session {
    pub(crate) wc: WorkingCopy,
    pub(crate) conn: Conn,
    pub(crate) cx: CallContext,
    pub(crate) begun: Begun,
}

/// Opens the write session for the working copy around the current
/// directory.
pub(crate) async fn begin(g: &Global) -> Result<Session, Failure> {
    let mut wc = require_working_copy()?;
    let conn = connect(g, Some(&wc)).await?;
    let cx = call_context();
    let begun = wc.begin(&conn.remote, &cx, DEFAULT_LEASE_MS).await?;
    Ok(Session {
        wc,
        conn,
        cx,
        begun,
    })
}

impl Session {
    /// The workspace, for a heartbeat.
    pub(crate) fn workspace_ref(&self) -> WorkspaceRef {
        WorkspaceRef {
            repo: self.conn.repo.clone(),
            name: self.begun.workspace.name.clone(),
        }
    }

    /// How an interrupted commit ended, for JSON: `"finished"`,
    /// `"rolled_back"`, or null.
    pub(crate) fn recovered_json(&self) -> serde_json::Value {
        match self.begun.recovered {
            None => serde_json::Value::Null,
            Some(Recovered::Finished(_)) => "finished".into(),
            Some(Recovered::RolledBack) => "rolled_back".into(),
        }
    }

    /// Finishes an update an earlier command left half done (a land, sync,
    /// revert or stack operation whose files had not moved yet), before a
    /// command that needs the working copy settled. `sync` and `land` do
    /// this themselves.
    pub(crate) async fn finish_unfinished(&mut self) -> Result<(), Failure> {
        if !self.begun.unfinished {
            return Ok(());
        }
        let cache = self.wc.open_cache(DEFAULT_CACHE_BYTES)?;
        let moved = self
            .wc
            .finish(
                &self.conn.remote,
                &cache,
                &self.cx,
                DEFAULT_LEASE_MS,
                &FetchOptions::default(),
            )
            .await?;
        self.begun.unfinished = false;
        if let Some(moved) = moved {
            if let Some(conflicts) = moved.conflicts {
                return Err(graft_client::Error::Conflicted(Box::new(conflicts)).into());
            }
            eprintln!(
                "graft: finished an interrupted update ({} files)",
                moved.update.changed
            );
        }
        Ok(())
    }

    /// Tells people on standard error what happened to an interrupted
    /// commit (the JSON carries it as `recovered`).
    pub(crate) fn note_recovery(&self) {
        match self.begun.recovered {
            None => {}
            Some(Recovered::Finished(id)) => {
                eprintln!("graft: finished an interrupted commit: {id}")
            }
            Some(Recovered::RolledBack) => eprintln!(
                "graft: dropped an interrupted commit that could not complete; its changes are uncommitted"
            ),
        }
    }
}

/// The snapshot a read defaults to: the working copy's base, else the
/// `main` line. Resolved by [`Conn::resolve`].
pub(crate) fn default_rev(wc: Option<&impl WcView>) -> String {
    match wc {
        Some(wc) => wc.state().base.to_string(),
        None => DEFAULT_LINE.to_owned(),
    }
}

/// Parses a repository path, a usage error when it is not one.
pub(crate) fn repo_path(text: &str) -> Result<graft_model::RepoPath, Failure> {
    graft_model::RepoPath::new(text)
        .map_err(|e| Failure::usage(format!("invalid path {text:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    fn user_with_servers(dir: &Path) -> UserConfig {
        let default_token = dir.join("default-token");
        let working_token = dir.join("working-token");
        fs::write(&default_token, "default-secret\n").unwrap();
        fs::write(&working_token, "working-secret\n").unwrap();
        UserConfig {
            default_server: Some("default".to_owned()),
            servers: [
                (
                    "default".to_owned(),
                    ServerConfig {
                        endpoint: "http://default.example".to_owned(),
                        token_file: default_token,
                    },
                ),
                (
                    "working".to_owned(),
                    ServerConfig {
                        endpoint: "http://working.example".to_owned(),
                        token_file: working_token,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn token_file_is_trimmed_and_never_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        fs::write(&file, "  abc123\n").unwrap();
        assert_eq!(token_from_file(&file).unwrap(), "abc123");
        fs::write(&file, " \n").unwrap();
        assert!(token_from_file(&file).is_err());
    }

    #[test]
    fn default_rev_is_main_outside_a_working_copy() {
        assert_eq!(default_rev(None::<&WcInfo>), "main");
    }

    #[test]
    fn working_copy_server_precedes_the_user_default() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_with_servers(dir.path());
        let wc = WcConfig::default();

        let settings =
            resolve_settings_from_sources(Some((&wc, Some("working"))), &user, None, None, None)
                .unwrap();
        assert_eq!(settings.server.as_deref(), Some("working"));
        assert_eq!(settings.endpoint, Endpoint::parse("http://working.example"));
        assert_eq!(settings.token.as_deref(), Some("working-secret"));
    }

    #[test]
    fn user_default_is_used_outside_a_working_copy() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_with_servers(dir.path());

        let settings = resolve_settings_from_sources(None, &user, None, None, None).unwrap();
        assert_eq!(settings.server.as_deref(), Some("default"));
        assert_eq!(settings.endpoint, Endpoint::parse("http://default.example"));
        assert_eq!(settings.token.as_deref(), Some("default-secret"));
    }

    #[test]
    fn endpoint_override_does_not_reuse_an_unrelated_named_token() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_with_servers(dir.path());
        let endpoint = Endpoint::parse("http://override.example");

        let endpoint_only =
            resolve_settings_from_sources(None, &user, None, Some(endpoint.clone()), None).unwrap();
        assert_eq!(endpoint_only.endpoint, endpoint);
        assert_eq!(endpoint_only.server, None);
        assert_eq!(endpoint_only.token, None);

        let token_only = resolve_settings_from_sources(
            None,
            &user,
            None,
            None,
            Some("override-secret".to_owned()),
        )
        .unwrap();
        assert_eq!(
            token_only.endpoint,
            Endpoint::parse("http://default.example")
        );
        assert_eq!(token_only.token.as_deref(), Some("override-secret"));
    }

    #[test]
    fn endpoint_override_reuses_a_named_token_only_when_it_matches() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_with_servers(dir.path());

        let settings = resolve_settings_from_sources(
            None,
            &user,
            None,
            Some(Endpoint::parse("http://default.example")),
            None,
        )
        .unwrap();

        assert_eq!(settings.server.as_deref(), Some("default"));
        assert_eq!(settings.token.as_deref(), Some("default-secret"));
    }

    #[test]
    fn legacy_working_copy_endpoint_precedes_the_user_default() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_with_servers(dir.path());
        let wc = WcConfig {
            endpoint: Some("http://legacy.example".to_owned()),
            ..WcConfig::default()
        };

        let settings =
            resolve_settings_from_sources(Some((&wc, None)), &user, None, None, None).unwrap();

        assert_eq!(settings.endpoint, Endpoint::parse("http://legacy.example"));
        assert_eq!(settings.server, None);
        assert_eq!(settings.token, None);
    }

    #[test]
    fn full_overrides_bypass_a_missing_working_copy_server() {
        let user = UserConfig::default();
        let wc = WcConfig::default();

        let settings = resolve_settings_from_sources(
            Some((&wc, Some("removed"))),
            &user,
            None,
            Some(Endpoint::parse("http://override.example")),
            Some("override-secret".to_owned()),
        )
        .unwrap();
        assert_eq!(settings.server, None);
        assert_eq!(
            settings.endpoint,
            Endpoint::parse("http://override.example")
        );
        assert_eq!(settings.token.as_deref(), Some("override-secret"));
    }

    #[test]
    fn server_override_precedes_the_working_copy_server() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_with_servers(dir.path());
        let wc = WcConfig::default();

        let settings = resolve_settings_from_sources(
            Some((&wc, Some("working"))),
            &user,
            Some("default"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(settings.server.as_deref(), Some("default"));
        assert_eq!(settings.endpoint, Endpoint::parse("http://default.example"));
        assert_eq!(settings.token.as_deref(), Some("default-secret"));
    }

    #[test]
    fn server_override_outside_a_working_copy_names_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_with_servers(dir.path());

        let settings =
            resolve_settings_from_sources(None, &user, Some("working"), None, None).unwrap();
        assert_eq!(settings.server.as_deref(), Some("working"));
        assert_eq!(settings.endpoint, Endpoint::parse("http://working.example"));
        assert_eq!(settings.token.as_deref(), Some("working-secret"));
    }
}
