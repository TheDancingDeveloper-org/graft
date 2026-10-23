# Known CI flakes

Each entry is quarantined, not fixed. Its fix removes the entry and the
quarantine together, under the same Vogt item.

## `test` / `nextest`: rustc OOM-killed, "could not compile" with no diagnostic

- **Vogt item:** WI-2314 (raised 2026-10-23).
- **Symptom:** many crates report `error: could not compile ...` at once,
  each with `Caused by: process didn't exit successfully: sccache rustc ...
  (exit status: 2)`, and no `error[E...]` or other compiler diagnostic.
  A re-run passes. Seen in run 39933165471 (PR #86) and run 39934199006
  (PR #88), attempt 1 of each.
- **Cause:** both failures ran on self-hosted runner
  `node-b-gha-public-publish-2` on Node B (winrarhost). That container's
  cgroup `memory.events` counted `oom_kill 17` since its start
  (2026-10-22 23:51Z), while host swap was 32G/32G full and `vogt-prod`
  (agent sessions that run cargo builds) used about 20 GiB with no
  memory limit.
- **Quarantine:** the `nextest` step retries once only when
  `.github/scripts/oom-flake-signature.sh` matches the log. Any other
  failure fails at once. Cargo parallelism stays uncapped.
- **Fix (host-side, operator):** a 16–20 GiB memory limit on `vogt-prod`,
  and look into Node B's full swap. Then remove the retry, the script and
  this entry.
