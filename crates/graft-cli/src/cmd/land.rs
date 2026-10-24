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
//!
//! A file both sides changed is merged line by line on the server when it
//! is text (decision D16); only overlapping edits conflict. `--merge file`
//! asks for the stricter rebase, where any file changed on both sides
//! conflicts.
//!
//! Before it sends `Land`, the pre-land check (§8.8, decision D27) reads a
//! dry-run rebase of the stack onto the tip and the draft overlap on its
//! paths. A real conflict exits 3 with the paths and sends nothing (with
//! `--sync` the land goes ahead, so the conflicts are written into the
//! files); files that merge cleanly and other workspaces' drafts are only
//! reported. `--no-check` skips it. It builds and tests nothing, and the
//! server's land still decides. JSON output carries it under `preflight`.
//!
//! `--merged-from <line>@<rev>` lands the resolution of a conflicted
//! `graft line merge` (D29): the land records a merge point from that
//! source changeset, which must be on the source line.

use std::time::Duration;

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{DEFAULT_CACHE_BYTES, LandOptions, LandOutcome};
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use graft_mcp::preflight::{self, PreflightRequest};
use graft_proto::types::{MergeMode, MergedFrom};
use serde_json::json;

use crate::cli::{Global, LandArgs, LandMerge};
use crate::conn::{begin, runtime};
use crate::error::Failure;
use crate::render::{print_json, short};

/// Lands the working copy's stack.
pub(crate) fn run(g: &Global, args: &LandArgs) -> Result<(), Failure> {
    let merge = match args.merge {
        LandMerge::Line => MergeMode::Line,
        LandMerge::File => MergeMode::File,
    };
    let mut opts = LandOptions {
        squash: args.squash,
        sync: args.sync,
        wait: args.wait.map(Duration::from_secs),
        merge,
        ..LandOptions::default()
    };
    runtime()?.block_on(async {
        let mut s = begin(g).await?;
        s.note_recovery();
        if let Some(text) = &args.merged_from {
            let (line, rev) = text
                .rsplit_once('@')
                .filter(|(l, r)| !l.is_empty() && !r.is_empty())
                .ok_or_else(|| Failure::usage("--merged-from takes <line>@<rev>"))?;
            opts.merged_from = Some(MergedFrom {
                line: line.to_owned(),
                changeset: s.conn.resolve(&s.cx, rev).await?,
            });
        }
        let state = s.wc.state();
        let preflight = match (args.no_check, state.head) {
            (true, _) | (_, None) => preflight::skipped_json(),
            (false, Some(head)) => {
                let req = PreflightRequest {
                    repo: s.conn.repo.clone(),
                    line: state.line.clone(),
                    base: state.base,
                    head,
                    merge,
                };
                let found = preflight::preflight(&s.conn.remote, &s.cx, &req).await?;
                let value = found.json();
                if !g.json {
                    for line in found.lines() {
                        outln!("{line}");
                    }
                }
                if let Some(conflict) = found.conflict.clone().filter(|_| !args.sync) {
                    return Err(Failure::from(conflict)
                        .with_detail("preflight", value)
                        .with_note(
                            "nothing was sent: the pre-land check found a real conflict; run `graft sync` (or `graft land --sync`) to write it into the files, or `--no-check` to send the land anyway",
                        ));
                }
                value
            }
        };
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
            failure.with_note(note).with_detail("preflight", preflight.clone())
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
                        "preflight": preflight,
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
                        "preflight": preflight,
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
