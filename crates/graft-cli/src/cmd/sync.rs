//! `graft sync`: moves the working copy and its workspace to the line tip
//! (`docs/design.md` §8.4). With no stack this fast-forwards the files that
//! changed; with a stack the server rebases it, and a conflict changes
//! nothing (exit 3).

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::DEFAULT_CACHE_BYTES;
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use serde_json::json;

use crate::cli::Global;
use crate::conn::{begin, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// Syncs the working copy.
pub(crate) fn run(g: &Global) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let mut s = begin(g).await?;
        s.note_recovery();
        let cache = s.wc.open_cache(DEFAULT_CACHE_BYTES)?;
        let ws = s.workspace_ref();
        let out = with_heartbeat(
            &s.conn.remote,
            &s.cx,
            ws,
            DEFAULT_LEASE_MS,
            HEARTBEAT_EVERY,
            s.wc.sync(
                &s.conn.remote,
                &cache,
                &s.cx,
                DEFAULT_LEASE_MS,
                &FetchOptions::default(),
            ),
        )
        .await?;
        let line = s.wc.state().line.clone();
        if g.json {
            return print_json(&json!({
                "line": line,
                "from": out.from.to_string(),
                "base": out.base.to_string(),
                "head": out.head.map(|h| h.to_string()),
                "changed": out.update.changed,
                "fetch": {
                    "blobs": out.update.fetch.blobs,
                    "bytes": out.update.fetch.bytes,
                    "requests": out.update.fetch.requests,
                },
                "recovered": s.recovered_json(),
            }));
        }
        if out.from == out.base {
            outln!("{line} is up to date at {}", short(out.base));
        } else {
            outln!(
                "synced {line}: {} -> {} ({} files)",
                short(out.from),
                short(out.base),
                out.update.changed
            );
        }
        if let Some(head) = out.head {
            outln!("head {}", short(head));
        }
        Ok(())
    })
}
