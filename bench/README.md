# graft bench: core source control, graft against git

A repeatable benchmark and correctness harness for graft as a git alternative
(phase 1, decision D22). It generates a synthetic Python project and a seeded
history, drives graft through the real `graft` CLI and git through `git`, and
writes JSON results plus a Markdown summary. Work item WI-2294.

Phase 1 scope only: every line is `direct`. There are no checks, attestation
or builds in the land path.

## What it does, per scale

A scale is a number of commits N (10, 1,000, 10,000 or 100,000) after an
initial import of the project.

1. **Generate.** `graftbench/gen.py` builds a pure-Python package
   (`src/synthapp/...`, 200 to 2,000 modules, N/50 clamped) and N commits from
   one `random.Random(seed)`. The commits mix: edit a function, add a function,
   multi-module refactors, add, delete and rename modules, the shared files
   (`registry.py`, package `__all__` lists, `CHANGELOG.md` with periodic
   releases), large CSV data files (0.2 to 1.2 MB, partly rewritten), and
   directories of many small fixture files. The same seed gives byte-identical
   history: the git commit id of the result is stable, and the stream sha256
   is recorded. The stream is written into a reference bare repository with
   `git fast-import`.
2. **Import.** `graft-mirror import` of the whole reference history into
   `<prefix>-<N>-import` (commits/s), and a check that the imported tree equals
   the model.
3. **Replay through the client.** `<prefix>-<N>` is seeded with commit 0 (an
   empty graft repository has no line), then commits 1..N go through a
   working copy: write the files, `graft commit`, `graft land`. git does the
   same: `git add -A`, `git commit`, `git push` to a bare repository.
4. **Verify the replay.** graft's tree and git's tree equal the model's
   replay byte for byte; git's replayed head equals the fast-import head (the
   history is byte-identical); graft's first-parent log is exactly the
   sequence of landed ids.
5. **Single-agent operations**, graft against git, `--iterations` samples
   each: full clone, sparse clone (one package), `status` and working-copy
   `diff` with 11 changed files, `diff` across the last 20 changesets,
   `log -50`, `log --path` on the hot shared file, `blame` on it, and `sync`
   of a working copy 5 lands behind (git: `pull --ff-only`).
6. **Concurrent writers.** `--writers` clones per system land at once,
   `--rounds` times per scenario:
   - `clean`: each writer appends to a different module;
   - `merge3`: each writer edits its own region of one shared file, so later
     lands need a rebase with a three-way line merge (D16);
   - `conflict`: every writer edits the same line. The conflict comes back to
     the client (graft: exit 3 and markers in the file; git: a failed
     rebase), which resolves it by rule (the line becomes the sorted union of
     all writers' values) and lands again.

   graft writers run `graft land`; on exit 3 from the server's file-level
   rebase they run `graft land --sync` (the client syncs, merging by line,
   and lands); exit 4 is retried. git writers `push`, and on a rejection
   `fetch`, `rebase` and push again. The summary reports p50/p95/p99 land
   latency, lands/s, contention, refusals, conflicts surfaced, and the time
   to the first conflict.
7. **Verify the end state.** The final graft tree and the final git tree
   both equal the expected tree (model + harness edits) byte for byte, and
   each other. Every land is on graft's line exactly once. The line never
   moved backwards: replay lands are in order, and every tip a poller saw
   during the concurrent lands is on the final history, in the order seen.
   `blame` of the shared files covers each file exactly with origins on the
   line, and its per-line agreement with `git blame` is reported.

For a local server the harness also samples the server's CPU seconds and RSS
(from `/proc`) and its storage size after each phase. Against a remote server
it reads `process_*` metrics from `/metrics` if the server serves them.

## Running

Python 3.10 or later, standard library only. Build the release binaries
first, then run from `bench/`:

```sh
CARGO_BUILD_JOBS=4 cargo build --release -p graft-cli -p graft-server -p graft-mirror
cd bench
python3 -m graftbench --scales 10,1000
```

The harness starts its own `graft-server` in a temp directory on an ephemeral
loopback port, creates the repositories, mints a token into a 0600 file, and
removes it all afterwards (`--keep` keeps the scratch directory, `--work DIR`
puts it somewhere else). Results go to `results/<UTC time>/` (or `--out DIR`):
`results.json` and `summary.md`. The exit status is 0 when every correctness
check passes, 1 when one fails, 2 on a harness error.

### Phase-1 features (WI-2349 step 8a)

```sh
python3 -m graftbench.phase1 --bin-dir ../target/release --out results/<dir>
```

Pre-land check latency in `graft land` against `--no-check` (up to date, one
land behind, real conflict), N lands → `pr-batch` → N commits with trailers
and export time (`--batches 10,50,200,500,501`), and identity binding. The
batch PR goes to a fake GitHub on loopback and a local bare upstream.
`--container graft-dev-server --line <test-line> --prefix <repo-prefix>` runs
it against a deployed server, in new repositories only.

### Commands per scale

| Scale | Command | Time on the reference host |
| --- | --- | --- |
| 10 | `python3 -m graftbench --scales 10` | about 10 s |
| 1,000 | `python3 -m graftbench --scales 1000` | about 6 min (with scale 10; import 1 min, replays 2 min) |
| 10,000 | `python3 -m graftbench --scales 10000` | tens of minutes (not run yet) |
| 100,000 | `python3 -m graftbench --scales 100000` | hours (not run yet) |

`--scales 10,1000,10000,100000` runs them all in one go, one fresh pair of
repositories per scale. The replays are one CLI process per commit and per
land, so time grows linearly with N. The 100k stream also carries a few GB of
data-file versions.

Useful options:

| Option | Default | What |
| --- | --- | --- |
| `--seed` | 2294 | generator seed |
| `--writers` | 8 | concurrent writers |
| `--rounds` | 5 | rounds per concurrent scenario (latency samples = writers x rounds) |
| `--iterations` | 5 | samples per single-agent operation |
| `--graft-bin`, `--server-bin`, `--mirror-bin` | `target/release/*` | binaries under test |
| `--no-import` | | skip the `graft-mirror import` step and its repository |
| `--label` | the output dir name | title of the summary |

### Batch delivery (`--batch`, WI-2352)

`python3 -m graftbench --batch [--batch-agents 5] [--batch-rounds 2]` runs
the batch scenario on a local server: agents land at once on an integration
line (pre-land check on), `graft-mirror pr-batch` exports one commit per land
to a local bare repository behind a fake GitHub API, one planted land breaks
the build (`compileall`), `pr-batch --exclude` drops it, and a simulated
rebase-merge lands the batch. It reports authorship per commit, `git bisect`
and `git blame` on the export, good lands held by the bad one, drop and
recut time, and land to `main` p50/p90. `graftbench.batchlive` is the live
counterpart against graft-dev and GitHub (docs/trials/2026-10-24-batch-delivery.md).

### Against a running server

```sh
python3 -m graftbench --scales 1000 \
  --server-url http://graft-dev:17888 --token-file ~/.config/graft/bench.token \
  --repo-prefix bench-$(date +%m%d)
```

The token goes only by `--token-file` (never in argv), and is never
printed. The repositories `<prefix>-<N>` and `<prefix>-<N>-import` must exist
and be empty, and the token needs `read`, `write`, `land:main` and `admin` on
each. On the server host:

```sh
graft-server admin repo create bench-1023-1000
graft-server admin repo create bench-1023-1000-import
graft-server admin token mint --subject graft-bench --kind agent --ttl 86400 \
  --scope repo:bench-1023-1000:read --scope repo:bench-1023-1000:write \
  --scope repo:bench-1023-1000:land:main --scope repo:bench-1023-1000:admin \
  --scope repo:bench-1023-1000-import:read --scope repo:bench-1023-1000-import:write \
  --scope repo:bench-1023-1000-import:land:main --scope repo:bench-1023-1000-import:admin \
  > bench.token   # store it 0600; don't print it
```

The `main` line must be `direct` on that server (`[[lines]]` or
`[default_line]` in `server.toml`). Server CPU and storage aren't
sampled remotely beyond `/metrics`.

## Many working copies (WI-2313)

```sh
python3 -m graftbench --scales 10 --many-copies 1,8,32
```

This runs instead of the full bench. For each N and shape (full, and sparse
on one package) it starts N `graft clone`s at once and compares them with one
git clone plus N-1 `git worktree add`. It reports wall time, per-clone
latency, and bytes on disk split into files, cache and other state. It gates
only on the counts: every copy exists and matches the model. Times are
reported, not gated. graft-node is not started. See
`docs/notes/many-working-copies.md`.

## Live awareness (WI-2311)

`--live-awareness` adds a deterministic scenario that measures whether
checking the server before you finish cuts conflicts and rework when many
agents edit similar code (`graftbench/livea.py`). It runs after the scales,
on its own small module set:

```sh
python3 -m graftbench --scales 10 --live-awareness --overlap mid
python3 -m graftbench --scales 10 --live-awareness --overlap low,mid,high --racing
```

- **Script.** From `--seed`, `--la-writers` (4) x `--la-steps` (6) edits.
  Each step adds a distinct marker to one key: the writer's own file, its own
  region of a shared file (a clean line merge), or a hot key of a shared
  file (a true conflict). `--overlap low|mid|high` sets the shares. Conflicts
  are resolved by the sorted union of every value in the block, so every
  policy must end at the same tree.
- **Scheduler.** One coordinator performs one action (edit, commit, sync,
  land) for one writer per tick, round-robin with a seeded jitter. No client
  calls overlap, so the same seed gives the same counts; each graft and git
  policy runs twice and the counts must match.
- **Policies.** graft `late` and `sync-early`; `preview` and `drafts` switch
  on when `graft status --help` offers `--upstream` / `--drafts` (WI-2309,
  WI-2310) and are reported as skipped until then. git `late` and
  `fetch-rebase-early`.
- **Oracle.** A pure-Python replay of the same schedule with `git merge-file`
  as the three-way reference predicts the conflicting paths of every sync and
  land, the counts and the final tree, and records each writer's open-draft
  paths per tick. graft must match it event for event.
- **Gates.** Same counts twice; graft equals the oracle; every final tree
  equals the expected tree and graft equals git; no lost or duplicated
  lands; the line never moves backwards; every path a refused land names
  did conflict. Timings (probe p50/p95, wall) are reported only.
  `--racing` reruns each graft policy with real threads, for correctness.
- **MCP.** Two writers conflict on one key, over `/mcp` (urllib, the token
  from the token file) and through the CLI; both must report the same
  conflict and land the same file. It uses `graft_conflicts` and
  `graft_resolve` when the server lists them; otherwise it reads the three
  sides with `graft_read`, resolves locally and lands from a new workspace,
  and says so.

A refused `graft land` can name fewer paths than the `land --sync` that
follows marks as conflicted: the summary counts those refusals
("named fewer paths").

## Tests

```sh
cd bench
python3 -m unittest discover -s tests     # or: python3 -m pytest tests
```

They check that the stream is deterministic, that every commit changes the
tree and fits it, that the model, disk and git replays agree, and that the
conflict rule and the merge3 regions behave as the scenarios assume.

## Layout

| Path | What |
| --- | --- |
| `graftbench/gen.py` | the project and commit-stream generator |
| `graftbench/model.py` | model replay, disk replay, tree comparison |
| `graftbench/tools.py` | `graft`, `graft-mirror` and `git` drivers, `fast-import` |
| `graftbench/site.py` | the local server (temp dir, ephemeral port) or a remote one |
| `graftbench/bench.py` | one scale: generate, import, replay, operations, verify |
| `graftbench/conc.py` | the concurrent-writer scenarios and the tip poller |
| `graftbench/copies.py` | the many-working-copies scenario (`--many-copies`) |
| `graftbench/livea.py` | the live-awareness script, scheduler, policies and oracle |
| `graftbench/mcp.py` | a minimal MCP client for `/mcp` |
| `graftbench/report.py` | machine info and the Markdown summary |
| `results/` | committed example runs |

## Reading the numbers

- Each graft step is a fresh CLI process (connect, read the working copy,
  call, exit), and so is each git step. The comparison is of the two
  command lines as agents use them, not of the servers alone.
- git's "server" is a local bare repository reached over `file://`. graft's
  is a real server over loopback HTTP/2. git has no network hop.
- The `merge3` latency for graft includes the refused `land` and the
  `land --sync` retry. See "Known gaps".
- The tip poller (`log --at main --limit 1` every 50 ms) runs during the
  graft concurrent rounds and adds a little load.

## Known gaps

- `graft land` sends `MergeMode::File`
  (`crates/graft-client/src/workcopy/session.rs`), so the server's land
  rebase refuses any path both sides changed, even when a line merge (D16)
  would be clean. The `merge3` scenario lands only through the
  `land --sync` fallback, and the summary counts the refusals ("land
  refused"). When the client sends `MergeMode::Line`, the refusals should drop
  to zero with no harness change (WI-2296).
- `pr-batch` (the squashed batch PR, D21) is not exercised. It needs a
  GitHub-side config.

## Real overlapping PRs (WI-2338)

`graftbench/realprs.py` replays real PR diffs from the estate's repositories
(the cases in `docs/notes/graft-fit-assessment.md`, gap 4) through graft and
git. Each PR lands from the fork point it really had: once with `graft land`
(and `land --sync` when the server's rebase refuses), and once with git
(`git push`; on a rejection `fetch`, `git merge-tree` to classify, `git rebase`
and `push`). The PR's change is what the agent had before it first had to
integrate: the oldest head before a force push, or the head before the first
merge of `main` back in.

```sh
python3 -m graftbench.realprs fetch   # gh GraphQL + bare clones -> realprs/cases.json
python3 -m graftbench.realprs run     # -> results/<date>-real-prs/
```

`fetch` needs `gh` signed in (rdpapp is private). `run` needs only the bare
clones (`--cache`, default `~/.cache/graft-bench/realprs`) and the release
binaries. Modes: `position` (each PR on its own line, at the `main` it really
integrated with), `sequence` (open PRs one after another onto today's `main`)
and `stack` (a PR chain: graft lands it as drafts in one call, git lands and
rebases PR by PR). A PR stops at its first conflict; nothing is resolved.

The oracle for "clean but broken" is `cargo check --workspace --all-targets`
(CARGO_BUILD_JOBS=4) on each merged tree; a merge is broken when it fails and
both its inputs pass. `--no-oracle` skips it. This is measurement, never a
land check (D22).
