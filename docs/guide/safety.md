# Safety

Each row is a claim and the test or result file that shows it. A claim that is not in this table is not one graft makes.

| Claim | What shows it |
| --- | --- |
| Every write that was acknowledged survives `kill -9`. | The crash harness in [crates/graft-testkit/src/pilot/crash.rs](../../crates/graft-testkit/src/pilot/crash.rs) restarts the server and checks that every acknowledged blob and changeset is still there and that each line points at its last acknowledged target. The harness itself is [crates/graft-testkit/src/crash.rs](../../crates/graft-testkit/src/crash.rs). The server end-to-end case is [crates/graft-server/tests/e2e.rs](../../crates/graft-server/tests/e2e.rs). The local-mode case is [crates/graft-cli/tests/local_mode.rs](../../crates/graft-cli/tests/local_mode.rs). |
| No land is lost or duplicated, and retries agree. | Property 1 in [crates/graft-harness/src/check.rs](../../crates/graft-harness/src/check.rs). |
| A line never moves backwards. | Property 2 ("lines move forward") in the same [check.rs](../../crates/graft-harness/src/check.rs): a line move must land on a first-parent descendant of the previous target. |
| Every object a landed changeset refers to is present and hashes to its id. | Property 5 in [crates/graft-harness/src/check.rs](../../crates/graft-harness/src/check.rs). |
| Nothing that has landed is collected. | Property 6 in [crates/graft-harness/src/check.rs](../../crates/graft-harness/src/check.rs): the sweep is failed if it collects a landed record, a pinned record, or a record still inside its lease or expiry. |
| Every conflict reaches the client. | [crates/graft-harness/tests/probes.rs](../../crates/graft-harness/tests/probes.rs) (`racing_merges_land_cleanly_or_conflict`): a racing land merges cleanly or conflicts, and does not land silently. |
| Replaying real pull requests, graft and git land the same clean merges and the same conflicts. | [bench/results/2026-10-24-real-prs/summary.md](../../bench/results/2026-10-24-real-prs/summary.md) and the matching [results.json](../../bench/results/2026-10-24-real-prs/results.json). That file records one replay; it is not a proof for every repository. |

The harness checks outcomes after the fault, not timings. See [Stability](stability.md) for the encodings and the protocol rule those checks sit on.
