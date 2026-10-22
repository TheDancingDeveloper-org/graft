//! `graft api`: calls a read operation of the API with JSON in and JSON out
//! (`docs/design.md` §3, §7.2).
//!
//! [`OPS`] is the parity table: every operation of the four API traits, and
//! for each either the command that serves it or the reason it has none. The
//! test at the bottom keeps the table complete. The JSON a call prints is the
//! same shape the matching command prints with `--json`.

use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::Path;

use graft_model::{ChangesetId, RepoPath};
use graft_proto::CallContext;
use graft_proto::api::RepoApi;
use graft_proto::types::{
    Blame, Diff, GetChangesets, ListRefs, ListTree, Log, Page, Pair, PatchOptions, PathAt,
    ResolveRef, Watch,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::{ApiArgs, Global, PAGE_DEFAULT};
use crate::conn::{Conn, call_context, connect, repo_path, runtime, working_copy};
use crate::error::Failure;
use crate::render;

/// How the CLI surfaces one API operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// A command serves it, by this name.
    Command(&'static str),
    /// No command does, and this says why.
    Excluded(&'static str),
}

/// One operation of the API traits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Op {
    /// The method name, as in `graft-proto`.
    pub(crate) name: &'static str,
    /// The trait it belongs to.
    pub(crate) api: &'static str,
    /// How the CLI surfaces it.
    pub(crate) surface: Surface,
    /// Whether `graft api` can call it. Read operations only: a write needs
    /// its command, which carries the safeguards.
    pub(crate) raw: bool,
}

const fn op(name: &'static str, api: &'static str, surface: Surface, raw: bool) -> Op {
    Op {
        name,
        api,
        surface,
        raw,
    }
}

/// Every operation of `ObjectApi`, `RepoApi`, `WorkspaceApi` and `AdminApi`.
pub(crate) const OPS: &[Op] = &[
    op("get_objects", "ObjectApi", Surface::Command("clone"), false),
    op(
        "has_objects",
        "ObjectApi",
        Surface::Excluded("upload check for commit (phase 2)"),
        false,
    ),
    op(
        "put_objects",
        "ObjectApi",
        Surface::Excluded("blob upload for commit (phase 2)"),
        false,
    ),
    op(
        "create_changeset",
        "RepoApi",
        Surface::Excluded("commit creates a draft (phase 2)"),
        false,
    ),
    op("get_changesets", "RepoApi", Surface::Command("show"), true),
    op("resolve_ref", "RepoApi", Surface::Command("api"), true),
    op("list_refs", "RepoApi", Surface::Command("api"), true),
    op(
        "update_ref",
        "RepoApi",
        Surface::Excluded("moves admin and tool refs; served by graft-server admin (G1.12)"),
        false,
    ),
    op("stat", "RepoApi", Surface::Command("api"), true),
    op("list_tree", "RepoApi", Surface::Command("ls"), true),
    op("read_file", "RepoApi", Surface::Command("cat"), true),
    op("log", "RepoApi", Surface::Command("log"), true),
    op("is_ancestor", "RepoApi", Surface::Command("api"), true),
    op("merge_base", "RepoApi", Surface::Command("api"), true),
    op("diff", "RepoApi", Surface::Command("diff"), true),
    op("blame", "RepoApi", Surface::Command("blame"), true),
    op(
        "rebase",
        "RepoApi",
        Surface::Excluded("rebase is part of sync and land (phase 2 and 3)"),
        false,
    ),
    op(
        "revert",
        "RepoApi",
        Surface::Excluded("revert command (phase 2)"),
        false,
    ),
    op(
        "land",
        "RepoApi",
        Surface::Excluded("land command (phase 2)"),
        false,
    ),
    op(
        "attest",
        "RepoApi",
        Surface::Excluded("CI check results, attest (phase 3)"),
        false,
    ),
    op("watch", "RepoApi", Surface::Command("api"), true),
    op(
        "open_workspace",
        "WorkspaceApi",
        Surface::Excluded("workspace command (phase 2)"),
        false,
    ),
    op(
        "get_workspace",
        "WorkspaceApi",
        Surface::Excluded("workspace command (phase 2)"),
        false,
    ),
    op(
        "list_workspaces",
        "WorkspaceApi",
        Surface::Excluded("workspace command (phase 2)"),
        false,
    ),
    op(
        "renew_lease",
        "WorkspaceApi",
        Surface::Excluded("workspace command (phase 2)"),
        false,
    ),
    op(
        "set_head",
        "WorkspaceApi",
        Surface::Excluded("commit and stack move the head (phase 2)"),
        false,
    ),
    op(
        "sync",
        "WorkspaceApi",
        Surface::Excluded("sync command (phase 2)"),
        false,
    ),
    op(
        "stack",
        "WorkspaceApi",
        Surface::Excluded("stack command (phase 2 and 3)"),
        false,
    ),
    op(
        "close_workspace",
        "WorkspaceApi",
        Surface::Excluded("workspace command (phase 2)"),
        false,
    ),
    op(
        "create_repo",
        "AdminApi",
        Surface::Excluded("admin; graft-server admin subcommands (G1.12)"),
        false,
    ),
    op(
        "import_changesets",
        "AdminApi",
        Surface::Excluded("admin; graft-server admin subcommands (G1.12)"),
        false,
    ),
    op(
        "gc",
        "AdminApi",
        Surface::Excluded("admin; graft-server admin subcommands (G1.12)"),
        false,
    ),
    op(
        "mint_token",
        "AdminApi",
        Surface::Excluded("admin; graft-server admin subcommands (G1.12)"),
        false,
    ),
];

/// The table row for an operation name.
pub(crate) fn find(name: &str) -> Option<&'static Op> {
    OPS.iter().find(|op| op.name == name)
}

/// The request fields `graft api` reads. Each operation takes the ones it
/// needs and ignores the rest; unknown fields are refused, so a typo fails.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Input {
    name: Option<String>,
    ids: Vec<String>,
    at: Option<String>,
    path: Option<String>,
    from: Option<String>,
    to: Option<String>,
    a: Option<String>,
    b: Option<String>,
    until: Option<String>,
    paths: Vec<String>,
    prefix: Option<String>,
    recursive: bool,
    patch: bool,
    lines: Option<[u32; 2]>,
    from_lsn: u64,
    kinds: Vec<String>,
    wait_ms: Option<u32>,
    limit: Option<u32>,
    cursor: Option<String>,
}

/// A required field, or a usage error naming it.
fn need(field: Option<String>, name: &str) -> Result<String, Failure> {
    field.ok_or_else(|| Failure::usage(format!("this operation needs \"{name}\"")))
}

/// The page request from the optional `limit` and `cursor` fields.
fn page(input: &Input) -> Page {
    Page {
        limit: input.limit.unwrap_or(PAGE_DEFAULT),
        token: input.cursor.clone(),
    }
}

/// Lists the operations, or calls one.
pub(crate) fn run(g: &Global, args: &ApiArgs) -> Result<(), Failure> {
    if args.list {
        return render::print_json(&list_json());
    }
    let name = args
        .op
        .as_deref()
        .ok_or_else(|| Failure::usage("name an operation, or pass --list"))?;
    let op = find(name).ok_or_else(|| {
        Failure::usage(format!(
            "unknown operation {name:?} (see `graft api --list`)"
        ))
    })?;
    if !op.raw {
        let why = match op.surface {
            Surface::Command(cmd) => format!("use `graft {cmd}`"),
            Surface::Excluded(reason) => reason.to_owned(),
        };
        return Err(Failure::usage(format!(
            "`graft api {name}` is not available: {why}"
        )));
    }
    let input = read_input(args.input.as_deref())?;
    runtime()?.block_on(async {
        let wc = working_copy()?;
        let conn = connect(g, wc.as_ref()).await?;
        let cx = call_context();
        let out = call(&conn, &cx, op.name, input).await?;
        render::print_json(&out)
    })
}

/// The operation table as JSON.
fn list_json() -> Value {
    Value::Array(
        OPS.iter()
            .map(|op| {
                let (command, excluded) = match op.surface {
                    Surface::Command(c) => (Some(c), None),
                    Surface::Excluded(reason) => (None, Some(reason)),
                };
                json!({
                    "op": op.name,
                    "api": op.api,
                    "raw": op.raw,
                    "command": command,
                    "excluded": excluded,
                })
            })
            .collect(),
    )
}

/// The request: from `--input FILE`, or standard input. Standard input is
/// read only when it is not a terminal, so nothing waits on a prompt.
fn read_input(file: Option<&Path>) -> Result<Value, Failure> {
    let text = match file {
        Some(path) if path.as_os_str() != "-" => fs::read_to_string(path)?,
        _ => {
            let stdin = io::stdin();
            if stdin.is_terminal() {
                return Err(Failure::usage(
                    "no request: pass --input FILE, or pipe JSON to standard input",
                ));
            }
            let mut text = String::new();
            stdin.lock().read_to_string(&mut text)?;
            text
        }
    };
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text)
        .map_err(|e| Failure::usage(format!("the request is not valid JSON: {e}")))
}

/// Decodes the request fields.
fn decode(input: Value) -> Result<Input, Failure> {
    serde_json::from_value(input).map_err(|e| Failure::usage(format!("bad request: {e}")))
}

/// Parses a list of changeset ids or ref names, resolving each.
async fn resolve_all(
    conn: &Conn,
    cx: &CallContext,
    revs: &[String],
) -> Result<Vec<ChangesetId>, Failure> {
    let mut out = Vec::with_capacity(revs.len());
    for rev in revs {
        out.push(conn.resolve(cx, rev).await?);
    }
    Ok(out)
}

/// Parses a list of repository paths.
fn paths_of(texts: &[String]) -> Result<Vec<RepoPath>, Failure> {
    texts.iter().map(|p| repo_path(p)).collect()
}

/// Calls one read operation. `name` is a raw operation, checked by the caller.
async fn call(conn: &Conn, cx: &CallContext, name: &str, input: Value) -> Result<Value, Failure> {
    let i = decode(input)?;
    let repo = conn.repo.clone();
    let api = &conn.remote;
    match name {
        "resolve_ref" => {
            let name = need(i.name.clone(), "name")?;
            let view = api.resolve_ref(cx, ResolveRef { repo, name }).await?;
            Ok(render::ref_view(&view))
        }
        "list_refs" => {
            let refs = api
                .list_refs(
                    cx,
                    ListRefs {
                        repo,
                        prefix: i.prefix.clone(),
                        page: page(&i),
                    },
                )
                .await?;
            Ok(json!({"refs": refs.iter().map(render::ref_view).collect::<Vec<_>>()}))
        }
        "get_changesets" => {
            let ids = resolve_all(conn, cx, &i.ids).await?;
            let got = api.get_changesets(cx, GetChangesets { repo, ids }).await?;
            Ok(json!({
                "found": got.found.iter().map(render::changeset_view).collect::<Vec<_>>(),
                "missing": got.missing.iter().map(ChangesetId::to_string).collect::<Vec<_>>(),
                "collected": got.collected.iter().map(ChangesetId::to_string).collect::<Vec<_>>(),
            }))
        }
        "stat" => {
            let at = conn.resolve(cx, &need(i.at.clone(), "at")?).await?;
            let path = repo_path(&need(i.path.clone(), "path")?)?;
            let entry = api.stat(cx, PathAt { repo, at, path }).await?;
            Ok(render::entry(&entry))
        }
        "list_tree" => {
            let at = conn.resolve(cx, &need(i.at.clone(), "at")?).await?;
            let path = i.path.as_deref().map(repo_path).transpose()?;
            let entries = api
                .list_tree(
                    cx,
                    ListTree {
                        repo,
                        at,
                        path,
                        recursive: i.recursive,
                        page: page(&i),
                    },
                )
                .await?;
            Ok(render::entries(&entries))
        }
        "read_file" => {
            let at = conn.resolve(cx, &need(i.at.clone(), "at")?).await?;
            let text = need(i.path.clone(), "path")?;
            let path = repo_path(&text)?;
            let content = api.read_file(cx, PathAt { repo, at, path }).await?;
            Ok(render::file_content(at, &text, &content))
        }
        "log" => {
            let from = conn.resolve(cx, &need(i.from.clone(), "from")?).await?;
            let until = match &i.until {
                Some(rev) => Some(conn.resolve(cx, rev).await?),
                None => None,
            };
            let history = api
                .log(
                    cx,
                    Log {
                        repo,
                        from,
                        until,
                        paths: paths_of(&i.paths)?,
                        page: page(&i),
                    },
                )
                .await?;
            Ok(render::history(&history))
        }
        "is_ancestor" | "merge_base" => {
            let a = conn.resolve(cx, &need(i.a.clone(), "a")?).await?;
            let b = conn.resolve(cx, &need(i.b.clone(), "b")?).await?;
            if name == "is_ancestor" {
                let result = api.is_ancestor(cx, Pair { repo, a, b }).await?;
                Ok(json!({"a": a.to_string(), "b": b.to_string(), "result": result}))
            } else {
                let base = api.merge_base(cx, Pair { repo, a, b }).await?;
                Ok(json!({
                    "a": a.to_string(),
                    "b": b.to_string(),
                    "merge_base": base.map(|id| id.to_string()),
                }))
            }
        }
        "diff" => {
            let from = conn.resolve(cx, &need(i.from.clone(), "from")?).await?;
            let to = conn.resolve(cx, &need(i.to.clone(), "to")?).await?;
            let patch = i.patch.then_some(PatchOptions {
                context: 3,
                max_bytes_per_file: 64 * 1024,
            });
            let result = api
                .diff(
                    cx,
                    Diff {
                        repo,
                        from,
                        to,
                        paths: paths_of(&i.paths)?,
                        patch,
                        page: page(&i),
                    },
                )
                .await?;
            Ok(render::diff(from, to, &result))
        }
        "blame" => {
            let at = conn.resolve(cx, &need(i.at.clone(), "at")?).await?;
            let text = need(i.path.clone(), "path")?;
            let path = repo_path(&text)?;
            let lines = i.lines.map(|[start, end]| (start, end));
            let result = api
                .blame(
                    cx,
                    Blame {
                        repo,
                        at,
                        path,
                        lines,
                    },
                )
                .await?;
            Ok(render::blame(at, &text, &result))
        }
        "watch" => {
            let page = api
                .watch(
                    cx,
                    Watch {
                        repo,
                        from_lsn: i.from_lsn,
                        kinds: i.kinds.clone(),
                        limit: i.limit.unwrap_or(PAGE_DEFAULT),
                        wait_ms: i.wait_ms.unwrap_or(0),
                    },
                )
                .await?;
            Ok(render::journal(&page))
        }
        other => Err(Failure::usage(format!("no raw call for {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::CommandFactory;

    use super::*;
    use crate::cli::Cli;

    /// Every operation of the four traits, as `graft-proto` declares them.
    const TRAIT_OPS: &[&str] = &[
        // ObjectApi
        "get_objects",
        "has_objects",
        "put_objects",
        // RepoApi
        "create_changeset",
        "get_changesets",
        "resolve_ref",
        "list_refs",
        "update_ref",
        "stat",
        "list_tree",
        "read_file",
        "log",
        "is_ancestor",
        "merge_base",
        "diff",
        "blame",
        "rebase",
        "revert",
        "land",
        "attest",
        "watch",
        // WorkspaceApi
        "open_workspace",
        "get_workspace",
        "list_workspaces",
        "renew_lease",
        "set_head",
        "sync",
        "stack",
        "close_workspace",
        // AdminApi
        "create_repo",
        "import_changesets",
        "gc",
        "mint_token",
    ];

    #[test]
    fn every_trait_operation_has_one_row() {
        let rows: Vec<&str> = OPS.iter().map(|op| op.name).collect();
        let unique: BTreeSet<&str> = rows.iter().copied().collect();
        assert_eq!(unique.len(), rows.len(), "an operation has two rows");
        let want: BTreeSet<&str> = TRAIT_OPS.iter().copied().collect();
        assert_eq!(unique, want, "the table and the traits disagree");
    }

    #[test]
    fn every_command_named_in_the_table_exists() {
        let commands: BTreeSet<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        for op in OPS {
            if let Surface::Command(cmd) = op.surface {
                assert!(
                    commands.contains(cmd),
                    "{} names unknown command {cmd}",
                    op.name
                );
            }
        }
    }

    #[test]
    fn exclusions_say_why_and_raw_calls_are_reads() {
        for op in OPS {
            match op.surface {
                Surface::Excluded(reason) => assert!(!reason.is_empty(), "{}", op.name),
                Surface::Command(_) => {}
            }
            if op.raw {
                assert!(
                    !op.name.starts_with("create") && !op.name.starts_with("update"),
                    "{} is a write but marked raw",
                    op.name
                );
            }
        }
    }

    #[test]
    fn list_has_one_entry_per_operation() {
        let list = list_json();
        assert_eq!(list.as_array().map(Vec::len), Some(OPS.len()));
    }
}
