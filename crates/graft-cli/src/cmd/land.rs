//! `graft land`: lands the stack on the line (`docs/design.md` §8.2). On a
//! git-primary line the server hands it to the PR bridge and answers with
//! the pull request; `--wait` asks again (land is idempotent) until it
//! merges. Exit codes are as in §8.2; a pull request still open when the
//! wait runs out exits 4, like a contended land: run `graft land --wait`
//! again.

use std::time::Duration;

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{DEFAULT_CACHE_BYTES, LandOptions, LandOutcome};
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use serde_json::json;

use crate::cli::{Global, LandArgs};
use crate::conn::{begin, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// Lands the working copy's stack.
pub(crate) fn run(g: &Global, args: &LandArgs) -> Result<(), Failure> {
    let opts = LandOptions {
        squash: args.squash,
        wait: args.wait.map(Duration::from_secs),
        ..LandOptions::default()
    };
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
            s.wc.land(
                &s.conn.remote,
                &cache,
                &s.cx,
                &opts,
                &FetchOptions::default(),
            ),
        )
        .await?;
        let line = s.wc.state().line.clone();
        match out {
            LandOutcome::Landed {
                landed,
                rebased,
                update,
            } => {
                if g.json {
                    return print_json(&json!({
                        "outcome": "landed",
                        "line": line,
                        "landed": landed.to_string(),
                        "rebased": rebased,
                        "changed": update.changed,
                        "pr_url": null,
                        "recovered": s.recovered_json(),
                    }));
                }
                outln!("landed {} on {line}", short(landed));
            }
            LandOutcome::Queued { pr_url } => {
                if g.json {
                    return print_json(&json!({
                        "outcome": "queued",
                        "line": line,
                        "landed": null,
                        "rebased": null,
                        "changed": null,
                        "pr_url": pr_url,
                        "recovered": s.recovered_json(),
                    }));
                }
                outln!("queued for {line}: {pr_url}");
            }
            _ => {
                return Err(Failure::new(
                    crate::Exit::Failure,
                    "client",
                    "unknown land outcome",
                ));
            }
        }
        Ok(())
    })
}
