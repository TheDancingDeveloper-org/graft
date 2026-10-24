//! `graft pick <rev>`: adds a draft on top of the head that re-applies a
//! landed changeset's own changes, relative to its first parent (a
//! cherry-pick, WI-2364). The server rebases the changeset onto the working
//! copy's head with a line merge, which records `graft.rebased-from`. When
//! it conflicts, the changes are written into the files (markers for text,
//! a `.graft-conflict` sidecar otherwise) and the command exits 3: resolve
//! them, then `graft commit` and `graft land`. The files the pick touches
//! must not have local changes.

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::DEFAULT_CACHE_BYTES;
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use graft_proto::types::MergeMode;
use serde_json::json;

use crate::cli::{Global, LandMerge, PickArgs};
use crate::conn::{begin, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// Picks a changeset.
pub(crate) fn run(g: &Global, args: &PickArgs) -> Result<(), Failure> {
    let merge = match args.merge {
        LandMerge::Line => MergeMode::Line,
        LandMerge::File => MergeMode::File,
    };
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
            s.wc.pick(
                &s.conn.remote,
                &cache,
                &s.cx,
                target,
                merge,
                DEFAULT_LEASE_MS,
                &FetchOptions::default(),
            ),
        )
        .await?;
        if g.json {
            return print_json(&json!({
                "picked": done.picked.to_string(),
                "id": done.id.to_string(),
                "workspace": s.begun.workspace.name,
                "changed": done.update.changed,
                "recovered": s.recovered_json(),
            }));
        }
        outln!(
            "picked {} as draft {} ({} files)",
            short(done.picked),
            short(done.id),
            done.update.changed
        );
        Ok(())
    })
}
