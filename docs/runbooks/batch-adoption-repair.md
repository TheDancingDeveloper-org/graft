# Repair a batch that adopted a leftover PR (WI-2585)

Before WI-2585, `graft-mirror pr-batch` looked up a batch's PR by head
branch name across all PR states. On a repo whose GitHub side still had a
`graft/batch-<n>` PR from earlier (a trial, a re-onboarded repo, a reset
mirror DB), a new batch with the same number adopted that PR. When the old PR
was merged, the batch was settled as `merged` against the old merge commit:
its lands were recorded as delivered but never reached GitHub `main`.

The fix makes `pr-batch` skip every number whose branch or PR already exists
on GitHub, and never adopt a PR it did not open at its own head. The DB rows
written by the bad run still need this one-off repair. **Deploy the fixed
image first**, or the next run repeats the fault.

## The graft-dev case

Repo `graft`, line `backlog-1`, mirror container `graft-dev-mirror-graft`,
DB `/var/lib/graft/mirror/graft.sqlite` in volume `graft-dev-mirror-graft`.
Batch 2 adopted PR #143 (merged 2026-10-24 as `98f6abb`) and was settled as
merged. Batch 1 is `rejected` and changed nothing. Batch 2 was the line's
first batch that was not rejected on a fresh DB, so before it settled the line
record held:

- `start` = batch 2's `start` (the fork point; nothing had merged since);
- `start_base` = the same changeset (a fresh line starts with both at the fork);
- `sync_from` = `synced` (every sync sets both to the base tip; only a merged
  batch moves `sync_from` away from it);
- `sync_batch` = NULL.

`synced` itself is untouched by the bad settle and stays.

## Procedure

1. Stop the mirror so no `pr-batch` or follow runs during the repair:
   `docker stop graft-dev-mirror-graft`.
2. Back up the DB:
   ```sh
   docker run --rm -v graft-dev-mirror-graft:/m alpine \
     cp /m/graft.sqlite /m/graft.sqlite.pre-wi-2585
   ```
3. Open it with any SQLite 3 shell on the volume (for example
   `docker run --rm -it -v graft-dev-mirror-graft:/m keinos/sqlite3 sqlite3 /m/graft.sqlite`)
   and check you are repairing what you expect:
   ```sql
   SELECT number, line, state, pr_number, lands, hex(start), hex(tip), hex(base)
     FROM batches ORDER BY number;
   -- expect: 1 backlog-1 rejected ...; 2 backlog-1 merged 143 2 ...
   SELECT line, hex(synced), hex(sync_from), hex(start), hex(start_base), sync_batch
     FROM batch_lines WHERE line = 'backlog-1';
   -- expect: start = sync_from = batch 2's tip (the bad settle wrote both)
   ```
   Stop if batch 2 is not `merged` with `pr_number` 143, or if a batch newer
   than 2 exists: the state has moved on and this procedure no longer fits.
4. Undo the settle, in one transaction:
   ```sql
   BEGIN;
   UPDATE batches
      SET state = 'rejected',
          detail = 'WI-2585: adopted leftover PR #143 by head name; not delivered',
          merge_sha = NULL,
          merged = NULL
    WHERE number = 2 AND line = 'backlog-1' AND state = 'merged';
   UPDATE batch_commits SET merged_sha = NULL WHERE batch = 2;
   UPDATE batch_lines
      SET start      = (SELECT start FROM batches WHERE number = 2),
          start_base = (SELECT start FROM batches WHERE number = 2),
          sync_from  = synced,
          sync_batch = NULL
    WHERE line = 'backlog-1';
   COMMIT;
   ```
   Re-run the two queries from step 3: batch 2 is `rejected`, and the line's
   `start` equals batch 2's `start`.
5. Start the mirror (`docker start graft-dev-mirror-graft`) and run the batch
   with the fixed image:
   ```sh
   docker exec graft-dev-mirror-graft graft-mirror pr-batch backlog-1 \
     --base main --config /etc/graft/deploy/mirror.toml
   ```
   Expected: `batch 3 (open): 2 land(s)` (or a higher number: `graft/batch-2`
   now has both a branch and PR #143, so it is skipped) with a **new** PR
   listing both lands (c5277958105f2 for WI-2548, and the WI-2542 land).
   PR #143 is not touched.
6. Optional tidy-up on GitHub: the branch `graft/batch-2` now holds the bad
   run's commits under the merged PR #143. Delete it once the new PR is open;
   nothing references it.

## Other repos

The same shape applies to any line whose batch settled against a PR it did
not open: check `batches.pr_number` against the PRs the mirror created (the PR
body carries `Graft-Batch: <n>` and the land ids). If the line had a merged
batch before the bad one, `start`, `start_base` and `sync_from` must come from
that earlier batch's settle (`start` = its `tip`, `start_base` = its `base`),
not from the fork; work it out from `batches` before writing anything.
