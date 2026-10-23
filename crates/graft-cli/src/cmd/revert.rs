//! `graft revert <rev>`: adds a draft on top of the head that inverts a
//! changeset (`docs/design.md` §3, §7.2). The server makes the inverse,
//! relative to the changeset's first parent, on the working copy's head;
//! a path changed since is a conflict (exit 3). The files the inverse
//! touches must not have local changes. `graft land` lands it like any
//! other draft.

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::DEFAULT_CACHE_BYTES;
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use serde_json::json;

use crate::cli::{Global, RevertArgs};
use crate::conn::{begin, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// Reverts a changeset.
pub(crate) fn run(g: &Global, args: &RevertArgs) -> Result<(), Failure> {
    runtime()?.block_on(async {
        let mut s = begin(g).await?;
        s.note_recovery();
        s.finish_unfinished().await?;
        let target = s.conn.resolve(&s.cx, &args.rev).await?;
        let cache = s.wc.open_cache(DEFAULT_CACHE_BYTES)?;
        let ws = s.workspace_ref();
        let done = with_heartbeat(
            &s.conn.remote,
            &s.cx,
            ws,
            DEFAULT_LEASE_MS,
            HEARTBEAT_EVERY,
            s.wc.revert(
                &s.conn.remote,
                &cache,
                &s.cx,
                target,
                DEFAULT_LEASE_MS,
                &FetchOptions::default(),
            ),
        )
        .await?;
        if g.json {
            return print_json(&json!({
                "reverted": done.reverted.to_string(),
                "id": done.id.to_string(),
                "workspace": s.begun.workspace.name,
                "changed": done.update.changed,
                "recovered": s.recovered_json(),
            }));
        }
        outln!(
            "reverted {} as draft {} ({} files)",
            short(done.reverted),
            short(done.id),
            done.update.changed
        );
        Ok(())
    })
}
