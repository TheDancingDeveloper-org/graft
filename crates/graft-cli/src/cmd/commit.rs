//! `graft commit [--amend] [-m]`: uploads the slice's changed files the
//! server lacks and creates a draft on top of the workspace head
//! (`docs/design.md` §3, §7.4, §8.5). Nothing is pushed: the draft is on the
//! server, visible at once.

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use graft_client::workcopy::{CommitLimits, CommitOptions};
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use graft_model::{Author, AuthorKind, Metadata, Timestamp};
use serde_json::json;

use crate::cli::{CommitArgs, Global};
use crate::conn::{begin, runtime};
use crate::error::Failure;
use crate::render::{self, print_json, short};

/// Commits the working copy's changes.
pub(crate) fn run(g: &Global, args: &CommitArgs) -> Result<(), Failure> {
    if args.message.is_none() && !args.amend {
        return Err(Failure::usage("a commit needs a message: pass -m"));
    }
    let author = author(args)?;
    let metadata = metadata(&args.meta)?;
    let opts = CommitOptions {
        message: args.message.clone(),
        author,
        authored_at: authored_at()?,
        metadata,
        amend: args.amend,
        limits: CommitLimits::default(),
    };
    runtime()?.block_on(async {
        let mut s = begin(g).await?;
        s.note_recovery();
        let ws = s.workspace_ref();
        let done = with_heartbeat(
            &s.conn.remote,
            &s.cx,
            ws,
            DEFAULT_LEASE_MS,
            HEARTBEAT_EVERY,
            s.wc.commit(&s.conn.remote, &s.cx, &opts),
        )
        .await?;
        if g.json {
            return print_json(&json!({
                "id": done.id.to_string(),
                "parent": done.parent.to_string(),
                "workspace": done.workspace,
                "superseded": done.superseded.map(|id| id.to_string()),
                "changes": done.changes.iter().map(render::change).collect::<Vec<_>>(),
                "upload": {
                    "blobs": done.upload.blobs,
                    "uploaded": done.upload.uploaded,
                    "bytes": done.upload.bytes,
                    "requests": done.upload.requests,
                },
                "recovered": s.recovered_json(),
            }));
        }
        outln!(
            "committed {} in workspace {} ({} changes, {} of {} blobs uploaded)",
            short(done.id),
            done.workspace,
            done.changes.len(),
            done.upload.uploaded,
            done.upload.blobs
        );
        if let Some(old) = done.superseded {
            outln!("superseded {}", short(old));
        }
        Ok(())
    })
}

/// The author: `--author`, else `GRAFT_AUTHOR_NAME` and `GRAFT_AUTHOR_EMAIL`,
/// else the user name.
fn author(args: &CommitArgs) -> Result<Author, Failure> {
    let kind_text = args
        .author_kind
        .clone()
        .or_else(|| env::var("GRAFT_AUTHOR_KIND").ok())
        .unwrap_or_else(|| "human".to_owned());
    let kind = match kind_text.as_str() {
        "human" => AuthorKind::Human,
        "agent" => AuthorKind::Agent,
        "bot" => AuthorKind::Bot,
        "ci" => AuthorKind::Ci,
        other => {
            return Err(Failure::usage(format!(
                "unknown author kind {other:?}: use human, agent, bot or ci"
            )));
        }
    };
    let (name, email) = match &args.author {
        Some(text) => parse_author(text)?,
        None => {
            let user = env::var("USER").unwrap_or_else(|_| "graft".to_owned());
            let name = env::var("GRAFT_AUTHOR_NAME").unwrap_or_else(|_| user.clone());
            let email =
                env::var("GRAFT_AUTHOR_EMAIL").unwrap_or_else(|_| format!("{user}@localhost"));
            (name, email)
        }
    };
    Ok(Author { name, email, kind })
}

/// `Name <email>`.
fn parse_author(text: &str) -> Result<(String, String), Failure> {
    let bad = || Failure::usage(format!("--author {text:?}: expected `Name <email>`"));
    let (name, rest) = text.split_once('<').ok_or_else(bad)?;
    let email = rest.strip_suffix('>').ok_or_else(bad)?;
    let name = name.trim();
    if name.is_empty() || email.is_empty() || email.contains(['<', '>']) {
        return Err(bad());
    }
    Ok((name.to_owned(), email.to_owned()))
}

/// `--meta KEY=VALUE` pairs. Keys under `graft.` are the server's.
fn metadata(pairs: &[String]) -> Result<Metadata, Failure> {
    let mut out = Metadata::new();
    for pair in pairs {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| Failure::usage(format!("--meta {pair:?}: expected KEY=VALUE")))?;
        if k.is_empty() || k.starts_with("graft.") {
            return Err(Failure::usage(format!(
                "--meta {k:?}: keys must be non-empty and not under graft."
            )));
        }
        out.insert(k.to_owned(), v.to_owned());
    }
    Ok(out)
}

/// The authoring time: `GRAFT_AUTHOR_DATE` (ms since the epoch, for
/// reproducible commits), else now, in UTC.
fn authored_at() -> Result<Timestamp, Failure> {
    let ms = match env::var("GRAFT_AUTHOR_DATE") {
        Ok(text) => text.trim().parse().map_err(|_| {
            Failure::usage("GRAFT_AUTHOR_DATE must be milliseconds since the epoch")
        })?,
        Err(_) => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64),
    };
    Ok(Timestamp { ms, offset_min: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authors_and_metadata_parse() {
        assert_eq!(
            parse_author("A Person <a@example.com>").unwrap(),
            ("A Person".to_owned(), "a@example.com".to_owned())
        );
        assert!(parse_author("nobody").is_err());
        assert!(parse_author("<a@b>").is_err());
        let m = metadata(&["vogt.work_item=WI-1".to_owned()]).unwrap();
        assert_eq!(m["vogt.work_item"], "WI-1");
        assert!(metadata(&["graft.rebased-from=x".to_owned()]).is_err());
        assert!(metadata(&["novalue".to_owned()]).is_err());
    }
}
