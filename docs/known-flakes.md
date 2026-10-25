# Known CI flakes

Each entry is quarantined, not fixed. Its fix removes the entry and the
quarantine together, under the same Vogt item.

## `test` / `nextest`: build OOM-killed (rustc, cargo or the linker)

- **Vogt item:** WI-2314 (raised 2026-10-23).
- **Symptom:** the nextest build fails with no `error[E...]` or other
  compiler diagnostic and no test failure, in one of three forms. A
  re-run passes.
  - **rustc killed:** many crates report `error: could not compile ...`
    at once, each `Caused by: process didn't exit successfully: sccache
    rustc ... (exit status: 2)`. Runs 39933165471 (PR #86) and
    39934199006 (PR #88), attempt 1 of each.
  - **cargo killed:** only `Compiling` lines, then `error: command cargo
    ... test --no-run ... exited` (no exit code, or code 101) and no
    `could not compile`. Run 39937737599 (PR #91), attempt 2.
  - **linker killed:** `error: linking with cc failed`, each with a
    `collect2: fatal error: ld terminated with signal 9 [Killed]` (or
    `signal: 9, SIGKILL`) note, plus `could not compile ... due to 1
    previous error` summaries. Merge-queue run 39938324480 (PR #90).
- **Cause:** both failures ran on self-hosted runner
  `node-b-gha-public-publish-2` on Node B (winrarhost). That container's
  cgroup `memory.events` counted `oom_kill 17` since its start
  (2026-10-22 23:51Z), while host swap was 32G/32G full and `vogt-prod`
  (agent sessions that run cargo builds) used about 20 GiB with no
  memory limit.
- **Quarantine:** the `nextest` step retries once only when
  `.github/scripts/oom-flake-signature.sh` matches the log, on
  pull_request and merge_group runs alike. Any other failure fails at
  once, and the job stays required. Cargo parallelism stays uncapped.
  Sample logs and the check: `.github/scripts/tests/oom-flake-signature/run.sh`.
- **Fix (host-side, operator):** a 16–20 GiB memory limit on `vogt-prod`,
  and look into Node B's full swap. Then remove the retry, the script and
  this entry.

## `test` / `import_kill9_restart_fetch_fsck`

- **Vogt item:** WI-2469 (quarantined 2026-10-25 under WI-2467).
- **Symptom:** the ~75s graft-server kill-9 import, restart, fetch and fsck
  test failed on merge-queue run for PR #196 (attempt 1) and passed on retry.
  No assertion or panic was stable enough to fix the same day.
- **Quarantine:** the test is `#[ignore]`d, so the required `test` job's
  nextest run skips it. A separate `slow` job in `.github/workflows/ci.yml`
  runs only that test, only on `merge_group`, with `continue-on-error: true`.
  It is not a required check, so a flake there does not fail the merge queue.
- **Fix:** WI-2469. Find the kill-9 race, then drop the `#[ignore]`, the
  `slow` job and this entry together.
