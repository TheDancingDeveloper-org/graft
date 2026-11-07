//! `graft repo`: creates repositories (`CreateRepo`, D41). Creating one
//! needs `repos:admin`, after which the creator administers it, or the
//! repository's own admin. A repository that exists is a successful retry.
//!
//! `graft repo mirror` and `graft repo batch` drive the server's mirror
//! settings and batch requests (D41 §3.2, §3.5). Binding, removing and
//! purging need `mirror:bind`, and export lines `mirror:export`; both are
//! operator-only grants. A credential is read from a file or standard input,
//! never an argument, and no output carries it. A request that creates
//! something gets one `client_request_id` per invocation, reused on retry.

use std::io::Read as _;
use std::path::Path;

use graft_mcp::{
    MirrorAdmin, changeset_bytes, join_lines, mirror_json, new_client_request_id, parse_lines,
    request_json, with_retry,
};
use graft_proto::api::RepoApi;
use graft_proto::types::{RefKind, UpdateRef, followed_ref};
use graft_proto::{ApiError, pb};
use serde_json::{Value, json};

use crate::cli::{BatchAction, Global, MirrorAction, RepoAction, RepoArgs};
use crate::conn::{call_context, connect, resolve_settings, runtime, working_copy_info};
use crate::error::Failure;
use crate::mirror_client::MirrorClient;
use crate::render::{print_json, short};
use graft_client::workcopy::WorkingCopy;

/// Largest credential accepted, in bytes.
const MAX_CREDENTIAL: u64 = 16 * 1024;

/// Runs a repo action.
pub(crate) fn run(g: &Global, args: &RepoArgs) -> Result<(), Failure> {
    match &args.action {
        RepoAction::Create { name } => runtime()?.block_on(async {
            // The name (or a graft URL) is the repository to connect for.
            let mut g = g.clone();
            g.repo = Some(name.clone());
            let conn = connect(&g, None::<&WorkingCopy>).await?;
            let cx = call_context();
            conn.remote.create_repo(&cx, conn.repo.clone()).await?;
            if g.json {
                return print_json(&json!({ "repo": conn.repo }));
            }
            outln!("created repository {}", conn.repo);
            Ok(())
        }),
        RepoAction::Mirror { action } => {
            // Read the credential before connecting, so a bad source fails
            // without a round trip.
            let credential = match action {
                MirrorAction::Credential {
                    from_file, stdin, ..
                } => Some(read_credential(from_file.as_deref(), *stdin)?),
                _ => None,
            };
            runtime()?.block_on(async {
                let client = mirror_client(g).await?;
                mirror(g, &client, action, credential).await
            })
        }
        RepoAction::Batch { action } => runtime()?.block_on(async {
            let client = mirror_client(g).await?;
            batch(g, &client, action).await
        }),
    }
}

/// Connects to the mirror service of the server the settings name.
async fn mirror_client(g: &Global) -> Result<MirrorClient, Failure> {
    let user = crate::config::load()?;
    let wc = working_copy_info()?;
    let settings = resolve_settings(g, wc.as_ref(), &user)?;
    MirrorClient::connect(&settings.endpoint, settings.token).await
}

/// Reads a credential from `path` or standard input. Trailing line breaks
/// are dropped; nothing of it is ever echoed.
fn read_credential(path: Option<&Path>, stdin: bool) -> Result<Vec<u8>, Failure> {
    let mut raw = Vec::new();
    let read = match (path, stdin) {
        (Some(path), _) => {
            std::fs::File::open(path).and_then(|f| f.take(MAX_CREDENTIAL + 1).read_to_end(&mut raw))
        }
        (None, true) => std::io::stdin()
            .lock()
            .take(MAX_CREDENTIAL + 1)
            .read_to_end(&mut raw),
        (None, false) => return Err(Failure::usage("pass --from-file PATH or --stdin")),
    };
    read.map_err(|e| Failure::usage(format!("cannot read the credential: {}", e.kind())))?;
    if raw.len() as u64 > MAX_CREDENTIAL {
        return Err(Failure::usage(format!(
            "the credential is longer than {MAX_CREDENTIAL} bytes"
        )));
    }
    while matches!(raw.last(), Some(b'\n' | b'\r')) {
        raw.pop();
    }
    if raw.is_empty() {
        return Err(Failure::usage("the credential is empty"));
    }
    Ok(raw)
}

/// A refusal, with a note when it is for an operator-only grant.
fn refused(e: ApiError) -> Failure {
    let operator = matches!(
        &e,
        ApiError::PermissionDenied { scope, .. } if scope.starts_with("mirror:")
    );
    let failure = Failure::from(e);
    if operator {
        failure.with_detail("operator_only", true.into()).with_note(
            "mirror:bind and mirror:export are operator-only grants; an operator runs this command",
        )
    } else {
        failure
    }
}

fn lines(follow: &[String], export: &[String]) -> Result<Vec<pb::MirrorLine>, Failure> {
    Ok(join_lines(
        parse_lines(follow, "follow").map_err(Failure::usage)?,
        parse_lines(export, "export").map_err(Failure::usage)?,
    ))
}

async fn mirror(
    g: &Global,
    client: &MirrorClient,
    action: &MirrorAction,
    credential: Option<Vec<u8>>,
) -> Result<(), Failure> {
    let cx = call_context();
    let key = |name: &str, upstream: &str| pb::MirrorKey {
        repo: name.to_owned(),
        upstream: upstream.to_owned(),
    };
    match action {
        MirrorAction::Add {
            name,
            upstream,
            follow,
            export,
            interval,
            disabled,
        } => {
            let lines = lines(follow, export)?;
            if lines.is_empty() {
                return Err(Failure::usage(
                    "name at least one --follow or --export BRANCH=LINE",
                ));
            }
            let row = client
                .set_mirror(
                    &cx,
                    pb::SetMirrorRequest {
                        repo: name.clone(),
                        upstream: upstream.clone(),
                        lines,
                        interval_s: interval.unwrap_or(0),
                        enabled: !disabled,
                        export: None,
                        expected_version: None,
                    },
                )
                .await
                .map_err(refused)?;
            print_mirror(g, "added", &row)
        }
        MirrorAction::Set {
            name,
            upstream,
            enable,
            disable,
            follow,
            export,
            interval,
        } => {
            let old = client
                .get_mirror(&cx, key(name, upstream))
                .await
                .map_err(refused)?;
            let new_lines = if follow.is_empty() && export.is_empty() {
                old.lines.clone()
            } else {
                lines(follow, export)?
            };
            let enabled = match (enable, disable) {
                (true, _) => true,
                (_, true) => false,
                _ => old.enabled,
            };
            let row = client
                .set_mirror(
                    &cx,
                    pb::SetMirrorRequest {
                        repo: name.clone(),
                        upstream: upstream.clone(),
                        lines: new_lines,
                        interval_s: interval.unwrap_or(old.interval_s),
                        enabled,
                        export: old.export,
                        expected_version: Some(old.version),
                    },
                )
                .await
                .map_err(refused)?;
            print_mirror(g, "updated", &row)
        }
        MirrorAction::Credential { name, upstream, .. } => {
            let credential = credential.expect("read before connecting");
            let old = client
                .get_mirror(&cx, key(name, upstream))
                .await
                .map_err(refused)?;
            let row = client
                .set_mirror_credential(
                    &cx,
                    pb::SetMirrorCredentialRequest {
                        repo: name.clone(),
                        upstream: upstream.clone(),
                        credential,
                        expected_version: old.version,
                    },
                )
                .await
                .map_err(refused)?;
            print_mirror(g, "sealed a credential into", &row)
        }
        MirrorAction::Remove {
            name,
            upstream,
            clear_followed,
        } => {
            let out = client
                .remove_mirror(
                    &cx,
                    pb::RemoveMirrorRequest {
                        repo: name.clone(),
                        upstream: upstream.clone(),
                        expected_version: None,
                        clear_followed: *clear_followed,
                    },
                )
                .await
                .map_err(refused)?;
            if g.json {
                return print_json(&json!({
                    "repo": name,
                    "upstream": upstream,
                    "cleared": out.cleared,
                }));
            }
            outln!("removed mirror {name} {upstream}");
            for line in &out.cleared {
                outln!("cleared followed marker on {line}");
            }
            Ok(())
        }
        MirrorAction::Purge {
            name,
            upstream,
            request_id,
        } => {
            let req = pb::PurgeMirrorRequest {
                repo: name.clone(),
                upstream: upstream.clone(),
                client_request_id: request_id
                    .clone()
                    .unwrap_or_else(|| new_client_request_id("cli")),
            };
            let r = with_retry(|| client.purge_mirror(&cx, req.clone()))
                .await
                .map_err(|e| retryable(refused(e), &req.client_request_id))?;
            print_request(g, "purge requested", &r)
        }
        MirrorAction::MarkFollowed {
            name,
            line,
            changeset,
            force,
        } => {
            if force.is_some() && changeset.is_none() {
                return Err(Failure::usage("--force requires --changeset"));
            }
            let old = force
                .as_deref()
                .map(graft_model::ChangesetId::parse)
                .transpose()
                .map_err(|_| Failure::usage("--force is not a changeset id"))?;
            let mut target_global = g.clone();
            target_global.repo = Some(name.clone());
            let conn = connect(&target_global, None::<&WorkingCopy>).await?;
            let target = match changeset {
                Some(id) => conn.resolve(&cx, id).await?,
                None => {
                    let page = client
                        .list_mirrors(
                            &cx,
                            pb::ListMirrorsRequest {
                                repo: name.clone(),
                                enabled: None,
                                limit: 100,
                                page_token: String::new(),
                            },
                        )
                        .await
                        .map_err(refused)?;
                    let status = page.mirrors.iter().find_map(|mirror| {
                        mirror.lines.iter().any(|l| l.line == *line && l.direction == "follow")
                            .then(|| mirror.status.as_ref().and_then(|s| s.last_changeset.clone()))
                            .flatten()
                    }).ok_or_else(|| Failure::usage(
                        "--changeset is required when mirror status has no last imported changeset",
                    ))?;
                    let bytes: [u8; 32] = status.try_into().map_err(|_| {
                        Failure::usage("mirror status has an invalid last changeset")
                    })?;
                    graft_model::ChangesetId(graft_model::ObjectId(bytes))
                }
            };
            let marker_name = followed_ref(line);
            let current = conn
                .remote
                .resolve_ref(
                    &cx,
                    graft_proto::types::ResolveRef {
                        repo: name.clone(),
                        name: marker_name.clone(),
                    },
                )
                .await;
            if current.is_ok() && force.is_none() {
                return Err(Failure::usage(format!(
                    "followed marker {marker_name} already exists; pass --force <old-id> to replace it"
                )));
            }
            let view = conn
                .remote
                .update_ref(
                    &cx,
                    UpdateRef {
                        repo: name.clone(),
                        name: marker_name,
                        expected: old,
                        new: target,
                        kind: RefKind::Tool,
                        lease_epoch: None,
                    },
                )
                .await?;
            if g.json {
                return print_json(&json!({"repo": name, "line": line, "ref": view.name,
                    "target": view.target.to_string()}));
            }
            outln!("marked {name} {line} followed at {}", short(view.target));
            Ok(())
        }
        MirrorAction::Status {
            name,
            upstream,
            limit,
            cursor,
        } => {
            if let (Some(name), Some(upstream)) = (name, upstream) {
                let row = client
                    .get_mirror(&cx, key(name, upstream))
                    .await
                    .map_err(refused)?;
                return print_mirror(g, "mirror", &row);
            }
            let page = client
                .list_mirrors(
                    &cx,
                    pb::ListMirrorsRequest {
                        repo: name.clone().unwrap_or_default(),
                        enabled: None,
                        limit: *limit,
                        page_token: cursor.clone().unwrap_or_default(),
                    },
                )
                .await
                .map_err(refused)?;
            let next = (!page.next_page_token.is_empty()).then_some(page.next_page_token);
            if g.json {
                return print_json(&json!({
                    "mirrors": page.mirrors.iter().map(mirror_json).collect::<Vec<_>>(),
                    "next_page_token": next,
                }));
            }
            if page.mirrors.is_empty() {
                outln!("no mirrors");
            }
            for m in &page.mirrors {
                outln!("{}", mirror_line(m));
            }
            if let Some(next) = next {
                outln!("more: --cursor {next}");
            }
            Ok(())
        }
    }
}

async fn batch(g: &Global, client: &MirrorClient, action: &BatchAction) -> Result<(), Failure> {
    let cx = call_context();
    match action {
        BatchAction::Cut {
            name,
            line,
            base,
            exclude,
            request_id,
        } => {
            let base = base
                .as_deref()
                .map(|b| changeset_bytes("--base", b))
                .transpose()
                .map_err(Failure::usage)?;
            let excludes = exclude
                .iter()
                .map(|e| changeset_bytes("--exclude", e))
                .collect::<Result<Vec<_>, _>>()
                .map_err(Failure::usage)?;
            let req = pb::CutBatchRequest {
                repo: name.clone(),
                line: line.clone(),
                base,
                excludes,
                client_request_id: request_id
                    .clone()
                    .unwrap_or_else(|| new_client_request_id("cli")),
            };
            let r = with_retry(|| client.cut_batch(&cx, req.clone()))
                .await
                .map_err(|e| retryable(refused(e), &req.client_request_id))?;
            print_request(g, "batch requested", &r)
        }
        BatchAction::Status { name, line } => {
            let view = client
                .get_batch(
                    &cx,
                    pb::BatchKey {
                        repo: name.clone(),
                        line: line.clone(),
                    },
                )
                .await
                .map_err(refused)?;
            if g.json {
                return print_json(&json!({ "batch": view.batch.as_ref().map(request_json) }));
            }
            match &view.batch {
                Some(r) => outln!("{}", request_line(r)),
                None => outln!("no open batch on {line}"),
            }
            Ok(())
        }
        BatchAction::Cancel { name, id } => {
            let r = client
                .cancel_request(
                    &cx,
                    pb::CancelRequestRequest {
                        repo: name.clone(),
                        id: *id,
                    },
                )
                .await
                .map_err(refused)?;
            print_request(g, "request cancelled", &r)
        }
        BatchAction::List {
            name,
            line,
            limit,
            cursor,
        } => {
            let page = client
                .list_batches(
                    &cx,
                    pb::ListBatchesRequest {
                        repo: name.clone(),
                        line: line.clone(),
                        page_token: cursor.clone().unwrap_or_default(),
                        limit: *limit,
                    },
                )
                .await
                .map_err(refused)?;
            let next = (!page.next_page_token.is_empty()).then_some(page.next_page_token);
            if g.json {
                return print_json(&json!({
                    "batches": page.batches.iter().map(request_json).collect::<Vec<_>>(),
                    "next_page_token": next,
                }));
            }
            if page.batches.is_empty() {
                outln!("no batches");
            }
            for r in &page.batches {
                outln!("{}", request_line(r));
            }
            if let Some(next) = next {
                outln!("more: --cursor {next}");
            }
            Ok(())
        }
    }
}

/// A failure that may have been recorded: it names the key a retry reuses.
fn retryable(failure: Failure, client_request_id: &str) -> Failure {
    failure
        .with_detail("client_request_id", client_request_id.into())
        .with_note(format!(
            "to retry the same request, pass --request-id {client_request_id}"
        ))
}

fn print_mirror(g: &Global, verb: &str, m: &pb::Mirror) -> Result<(), Failure> {
    if g.json {
        return print_json(&json!({ "mirror": mirror_json(m) }));
    }
    outln!("{verb} {}", mirror_line(m));
    Ok(())
}

fn print_request(g: &Global, verb: &str, r: &pb::MirrorRequest) -> Result<(), Failure> {
    if g.json {
        return print_json(&json!({ "request": request_json(r) }));
    }
    outln!("{verb}: {}", request_line(r));
    Ok(())
}

/// One line for a mirror row, from its JSON form so both outputs agree.
fn mirror_line(m: &pb::Mirror) -> String {
    let v = mirror_json(m);
    let lines: Vec<String> = m
        .lines
        .iter()
        .map(|l| format!("{}={}({})", l.branch, l.line, l.direction))
        .collect();
    let status = match &v["status"]["error_code"] {
        Value::String(code) => code.clone(),
        _ if v["status"].is_null() => "not polled".to_owned(),
        _ => "ok".to_owned(),
    };
    format!(
        "{} {} {} v{} credential:{} lines:{} status:{}",
        m.repo,
        m.upstream,
        if m.enabled { "enabled" } else { "disabled" },
        m.version,
        if m.credential_set { "set" } else { "none" },
        if lines.is_empty() {
            "-".to_owned()
        } else {
            lines.join(",")
        },
        status
    )
}

/// One line for a request, from its JSON form.
fn request_line(r: &pb::MirrorRequest) -> String {
    let v = request_json(r);
    let mut out = format!(
        "#{} {} {} {} state:{}",
        r.id,
        r.repo,
        if r.line.is_empty() { "-" } else { &r.line },
        v["kind"].as_str().unwrap_or("?"),
        v["state"].as_str().unwrap_or("?"),
    );
    if let Some(b) = v["batch_state"].as_str() {
        out.push_str(&format!(" batch:{b}"));
    }
    if let Some(url) = v["result"]["pr_url"].as_str() {
        out.push_str(&format!(" pr:{url}"));
    }
    if let Some(code) = v["result"]["error_code"].as_str() {
        out.push_str(&format!(" error:{code}"));
    }
    out.push_str(&format!(" request-id:{}", r.client_request_id));
    out
}
