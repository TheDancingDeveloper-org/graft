//! `graft land`: lands the stack on the line (`docs/design.md` §8.2). One
//! `Land` call does the work: the server rebases the stack onto the tip and
//! moves the line by compare-and-swap, retrying there. Then the working copy
//! moves to the landed changeset, bringing in the line's other changes
//! within the slice. Exit codes are as in §8.2:
//!
//! | Result | Exit | What the working copy does |
//! | --- | --- | --- |
//! | landed | 0 | moves to the landed changeset, no stack |
//! | conflict | 3 | nothing; with `--sync` it moves to the tip and the conflicts are written into the files |
//! | contended | 4 | nothing, after retrying with backoff for up to `--wait` (60 s) |
//! | rejected | 5 | nothing (a policy said no: the detail names it) |
//! | denied | 6 | nothing |
//! | unavailable | 7 | nothing, or a half-done move the next command finishes; land is idempotent |
//!
//! On a git-primary line the server hands the stack to the PR bridge and
//! answers with the pull request; `--wait` asks again until it merges, and a
//! pull request still open when the wait runs out exits 4.

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
        sync: args.sync,
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
        .await
        .map_err(|e| {
            let failure = Failure::from(e);
            let note = match failure.kind() {
                "conflict" => {
                    "nothing changed: run `graft sync` (or `graft land --sync`) to write the conflicts into the files"
                }
                "contended" => "retryable: run `graft land` again, or wait longer with --wait",
                "unavailable" => "retryable: land is idempotent, so run `graft land` again",
                _ => return failure,
            };
            failure.with_note(note)
        })?;
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
                if rebased {
                    let files = match update.changed {
                        1 => "1 file".to_owned(),
                        n => format!("{n} files"),
                    };
                    outln!(
                        "landed {} on {line} (rebased onto the tip; {files} updated)",
                        short(landed)
                    );
                } else {
                    outln!("landed {} on {line}", short(landed));
                }
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
