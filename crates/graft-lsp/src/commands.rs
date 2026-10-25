//! `workspace/executeCommand` and the `graft/readFile` request.

use std::time::{SystemTime, UNIX_EPOCH};

use graft_client::Error as ClientError;
use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{
    CommitLimits, CommitOptions, DEFAULT_CACHE_BYTES, LandOptions, LandOutcome,
};
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use graft_model::{Author, AuthorKind, Change, Metadata, RepoPath, Timestamp};
use graft_proto::ApiError;
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{Blame, Diff, Page, PatchOptions, PathAt, WorkspaceRef};
use serde_json::{Value, json};

use crate::features::{PATCH_BYTES, api_error};
use crate::render::{kind_name, parse_kind, short};
use crate::rpc::RpcError;
use crate::{Conflict, GraftUri, LspServer, MAX_READ_BYTES, Target};

/// Files per `graft.diffBase` page.
const DIFF_PAGE: u32 = 100;

/// The most blame ranges `graft.blameFile` returns.
const MAX_BLAME_RANGES: usize = 10_000;

impl<A: ObjectApi + RepoApi + WorkspaceApi> LspServer<A> {
    pub(crate) async fn execute_command(&mut self, params: &Value) -> Result<Value, RpcError> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("no command"))?
            .to_owned();
        let arg = params
            .pointer("/arguments/0")
            .cloned()
            .unwrap_or(Value::Null);
        self.refresh_workcopy();
        let writes = matches!(
            command.as_str(),
            "graft.commit" | "graft.land" | "graft.sync"
        );
        let out = match command.as_str() {
            "graft.commit" => self.commit(&arg).await,
            "graft.land" => self.land(&arg).await,
            "graft.sync" => self.sync().await,
            "graft.diffBase" => self.diff_base(&arg).await,
            "graft.blameFile" => self.blame_file(&arg).await,
            other => Err(RpcError::invalid_params(format!("unknown command {other}"))),
        };
        if writes {
            self.restamp();
            self.publish_all(None).await;
            self.refresh_lenses();
        }
        out
    }

    async fn commit(&mut self, arg: &Value) -> Result<Value, RpcError> {
        let message = arg
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.trim().is_empty())
            .map(str::to_owned);
        let amend = flag(arg, "amend")?;
        if message.is_none() && !amend {
            return Err(RpcError::invalid_params("graft.commit needs a message"));
        }
        let opts = CommitOptions {
            message,
            author: self.commit_author(arg)?,
            authored_at: now(),
            metadata: metadata(arg)?,
            amend,
            limits: CommitLimits::default(),
        };
        let cx = self.cx();
        let wc = self.workcopy.as_mut().ok_or_else(no_working_copy)?;
        let begun = wc
            .begin(&self.api, &cx, DEFAULT_LEASE_MS)
            .await
            .map_err(client_error)?;
        let ws = WorkspaceRef {
            repo: self.repo.clone(),
            name: begun.workspace.name,
        };
        let done = with_heartbeat(
            &self.api,
            &cx,
            ws,
            DEFAULT_LEASE_MS,
            HEARTBEAT_EVERY,
            wc.commit(&self.api, &cx, &opts),
        )
        .await
        .map_err(client_error)?;
        self.show(
            3,
            &format!(
                "graft: committed {} in workspace {} ({} changes)",
                short(done.id),
                done.workspace,
                done.changes.len()
            ),
        );
        Ok(json!({
            "id": done.id.to_string(),
            "parent": done.parent.to_string(),
            "workspace": done.workspace,
            "superseded": done.superseded.map(|id| id.to_string()),
            "changes": done.changes.len(),
        }))
    }

    async fn sync(&mut self) -> Result<Value, RpcError> {
        let cx = self.cx();
        let wc = self.workcopy.as_mut().ok_or_else(no_working_copy)?;
        let begun = wc
            .begin(&self.api, &cx, DEFAULT_LEASE_MS)
            .await
            .map_err(client_error)?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES).map_err(client_error)?;
        let ws = WorkspaceRef {
            repo: self.repo.clone(),
            name: begun.workspace.name,
        };
        let out = with_heartbeat(
            &self.api,
            &cx,
            ws,
            DEFAULT_LEASE_MS,
            HEARTBEAT_EVERY,
            wc.sync(
                &self.api,
                &cache,
                &cx,
                DEFAULT_LEASE_MS,
                &FetchOptions::default(),
            ),
        )
        .await;
        let line = wc.state().line.clone();
        let out = match out {
            Ok(out) => out,
            Err(e) => return Err(self.write_failed(e)),
        };
        self.conflict = None;
        self.show(
            3,
            &if out.from == out.base {
                format!("graft: {line} is up to date at {}", short(out.base))
            } else {
                format!(
                    "graft: synced {line} to {} ({} files)",
                    short(out.base),
                    out.update.changed
                )
            },
        );
        Ok(json!({
            "line": line,
            "from": out.from.to_string(),
            "base": out.base.to_string(),
            "head": out.head.map(|h| h.to_string()),
            "changed": out.update.changed,
        }))
    }

    async fn land(&mut self, arg: &Value) -> Result<Value, RpcError> {
        let opts = LandOptions {
            squash: flag(arg, "squash")?,
            wait: None,
            ..LandOptions::default()
        };
        let cx = self.cx();
        let wc = self.workcopy.as_mut().ok_or_else(no_working_copy)?;
        let begun = wc
            .begin(&self.api, &cx, DEFAULT_LEASE_MS)
            .await
            .map_err(client_error)?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES).map_err(client_error)?;
        let ws = WorkspaceRef {
            repo: self.repo.clone(),
            name: begun.workspace.name,
        };
        let out = with_heartbeat(
            &self.api,
            &cx,
            ws,
            DEFAULT_LEASE_MS,
            HEARTBEAT_EVERY,
            wc.land(&self.api, &cache, &cx, &opts, &FetchOptions::default()),
        )
        .await;
        let line = wc.state().line.clone();
        let out = match out {
            Ok(out) => out,
            Err(e) => return Err(self.write_failed(e)),
        };
        self.conflict = None;
        match out {
            LandOutcome::Landed {
                landed, rebased, ..
            } => {
                self.show(3, &format!("graft: landed {} on {line}", short(landed)));
                Ok(json!({
                    "outcome": "landed",
                    "line": line,
                    "landed": landed.to_string(),
                    "rebased": rebased,
                }))
            }
            LandOutcome::Queued { pr_url } => {
                self.show(3, &format!("graft: queued for {line}: {pr_url}"));
                Ok(json!({"outcome": "queued", "line": line, "pr_url": pr_url}))
            }
            _ => Err(RpcError::failed("an unknown land outcome")),
        }
    }

    /// Remembers a conflict, so its regions are published, and turns the
    /// failure into a request error.
    fn write_failed(&mut self, e: ClientError) -> RpcError {
        // A stale base is the land-time form of a conflict (WI-2441): the
        // regions still have to be published, against the tip it names.
        let remembered = match &e {
            ClientError::Api(ApiError::Conflict { paths, onto }) => Some((
                *onto,
                paths
                    .iter()
                    .filter_map(|p| RepoPath::new(&p.path).ok())
                    .collect(),
            )),
            ClientError::Api(ApiError::StaleBase(detail)) => Some((
                detail.current_tip,
                detail
                    .paths
                    .iter()
                    .filter_map(|p| RepoPath::new(p).ok())
                    .collect(),
            )),
            _ => None,
        };
        if let Some((onto, paths)) = remembered {
            self.conflict = Some(Conflict { onto, paths });
        }
        client_error(e)
    }

    async fn diff_base(&mut self, arg: &Value) -> Result<Value, RpcError> {
        let wc = self.workcopy.as_ref().ok_or_else(no_working_copy)?;
        let (base, head) = (wc.state().base, wc.snapshot());
        let path = match uri_arg(arg) {
            None => None,
            Some(uri) => match self.target(uri) {
                Target::Local(p) => Some(p),
                _ => {
                    return Err(RpcError::invalid_params(
                        "graft.diffBase takes a working-copy file",
                    ));
                }
            },
        };
        let token = arg.get("page").and_then(Value::as_str).map(str::to_owned);
        let diff = self
            .api
            .diff(
                &self.cx(),
                Diff {
                    repo: self.repo.clone(),
                    from: base,
                    to: head,
                    paths: path.iter().cloned().collect(),
                    patch: Some(PatchOptions {
                        context: 3,
                        max_bytes_per_file: PATCH_BYTES,
                    }),
                    page: Page {
                        limit: DIFF_PAGE,
                        token,
                    },
                },
            )
            .await
            .map_err(api_error)?;
        let base_uri = path.as_ref().map(|p| {
            GraftUri {
                repo: self.repo.clone(),
                rev: base.to_string(),
                path: p.clone(),
            }
            .to_string()
        });
        let files: Vec<Value> = diff
            .files
            .iter()
            .map(|f| {
                json!({
                    "path": f.change.path().as_str(),
                    "change": match f.change {
                        Change::Delete { .. } => "delete",
                        _ => "put",
                    },
                    "patch": f.patch,
                    "truncated": f.truncated,
                })
            })
            .collect();
        Ok(json!({
            "base": base.to_string(),
            "head": head.to_string(),
            "base_uri": base_uri,
            "files": files,
            "next": diff.next,
        }))
    }

    async fn blame_file(&mut self, arg: &Value) -> Result<Value, RpcError> {
        let uri =
            uri_arg(arg).ok_or_else(|| RpcError::invalid_params("graft.blameFile needs a uri"))?;
        let Some((at, path, _)) = self.locate(uri).await? else {
            return Err(RpcError::invalid_params("not a graft document"));
        };
        let blame = self
            .api
            .blame(
                &self.cx(),
                Blame {
                    repo: self.repo.clone(),
                    at,
                    path: path.clone(),
                    lines: None,
                },
            )
            .await
            .map_err(api_error)?;
        let truncated = blame.ranges.len() > MAX_BLAME_RANGES;
        let ranges: Vec<Value> = blame
            .ranges
            .iter()
            .take(MAX_BLAME_RANGES)
            .map(|r| {
                json!({
                    "start": r.start,
                    "end": r.end,
                    "id": r.origin.id.to_string(),
                    "author": r.origin.author.name,
                    "kind": kind_name(r.origin.author.kind),
                    "subject": r.origin.subject,
                })
            })
            .collect();
        Ok(json!({
            "uri": uri,
            "at": at.to_string(),
            "path": path.as_str(),
            "ranges": ranges,
            "truncated": truncated,
        }))
    }

    /// `graft/readFile`: a file at a revision, as text.
    pub(crate) async fn read_file(&mut self, params: &Value) -> Result<Value, RpcError> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("no uri"))?;
        let g = GraftUri::parse(uri).map_err(RpcError::invalid_params)?;
        if g.repo != self.repo {
            return Err(RpcError::invalid_params(format!(
                "this server serves repository {:?}, not {:?}",
                self.repo, g.repo
            )));
        }
        let at = self.resolve(&g.rev).await?;
        let file = self
            .api
            .read_file(
                &self.cx(),
                PathAt {
                    repo: self.repo.clone(),
                    at,
                    path: g.path.clone(),
                },
            )
            .await
            .map_err(api_error)?;
        if file.data.len() > MAX_READ_BYTES {
            return Err(
                RpcError::failed(format!("{} is over {MAX_READ_BYTES} bytes", g.path))
                    .with_data(json!({"error": "limit_exceeded"})),
            );
        }
        let text = String::from_utf8(file.data).map_err(|_| {
            RpcError::failed(format!("{} is not UTF-8 text", g.path))
                .with_data(json!({"error": "binary"}))
        })?;
        Ok(json!({
            "uri": uri,
            "changeset": at.to_string(),
            "path": g.path.as_str(),
            "text": text,
        }))
    }

    /// The author of a commit: the argument's, else the configured one,
    /// else from the environment as the CLI takes it.
    fn commit_author(&self, arg: &Value) -> Result<Author, RpcError> {
        if let Some(a) = arg.get("author") {
            let field = |k: &str| {
                a.get(k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty() && !s.contains(['<', '>', '\n']))
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::invalid_params(format!("author.{k} is missing")))
            };
            let kind = match a.get("kind").and_then(Value::as_str) {
                None => AuthorKind::Human,
                Some(k) => parse_kind(k).ok_or_else(|| {
                    RpcError::invalid_params("author.kind is one of human, agent, bot, ci")
                })?,
            };
            return Ok(Author {
                name: field("name")?,
                email: field("email")?,
                kind,
            });
        }
        if let Some(a) = &self.author {
            return Ok(a.clone());
        }
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let user = env("USER").unwrap_or_else(|| "graft".to_owned());
        let kind = match env("GRAFT_AUTHOR_KIND") {
            None => AuthorKind::Human,
            Some(k) => parse_kind(&k).ok_or_else(|| {
                RpcError::invalid_params("GRAFT_AUTHOR_KIND is one of human, agent, bot, ci")
            })?,
        };
        Ok(Author {
            name: env("GRAFT_AUTHOR_NAME").unwrap_or_else(|| user.clone()),
            email: env("GRAFT_AUTHOR_EMAIL").unwrap_or_else(|| format!("{user}@localhost")),
            kind,
        })
    }
}

/// A failed client call as a request failure.
fn client_error(e: ClientError) -> RpcError {
    let kind = match &e {
        ClientError::Api(api) => return api_error(api.clone()),
        ClientError::NothingToCommit => "nothing_to_commit",
        ClientError::NoDraft => "no_draft",
        ClientError::Diverged { .. } => "diverged",
        ClientError::LandedDirty { .. } => "landed_dirty",
        ClientError::Dirty(_) => "dirty",
        _ => "client",
    };
    RpcError::failed(e.to_string()).with_data(json!({"error": kind}))
}

fn no_working_copy() -> RpcError {
    RpcError::failed("no working copy: start the server inside one")
        .with_data(json!({"error": "no_working_copy"}))
}

fn flag(arg: &Value, name: &str) -> Result<bool, RpcError> {
    match arg.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(RpcError::invalid_params(format!("{name} is a boolean"))),
    }
}

/// A command's URI: the argument itself, or its `uri` field.
fn uri_arg(arg: &Value) -> Option<&str> {
    arg.as_str()
        .or_else(|| arg.get("uri").and_then(Value::as_str))
}

/// Extra commit metadata: an object of strings, no key under `graft.`.
fn metadata(arg: &Value) -> Result<Metadata, RpcError> {
    let mut out = Metadata::new();
    let Some(map) = arg.get("metadata") else {
        return Ok(out);
    };
    let map = map
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("metadata is an object of strings"))?;
    for (k, v) in map {
        let v = v
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("metadata is an object of strings"))?;
        if k.is_empty() || k.starts_with("graft.") {
            return Err(RpcError::invalid_params(format!(
                "metadata key {k:?}: keys must be non-empty and not under graft."
            )));
        }
        out.insert(k.clone(), v.to_owned());
    }
    Ok(out)
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    Timestamp { ms, offset_min: 0 }
}
