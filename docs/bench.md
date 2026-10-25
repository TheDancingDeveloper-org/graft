# Bench methodology

How the numbers in [docs/guide/bench.svg](guide/bench.svg) were measured,
and how to read them. The chart is generated from committed result files
by `python3 bench/render.py`. It does not invent a figure.

## What is on the chart

Two result files, both seed 2294, five iterations per operation, a fresh
CLI process for every step (graft and git alike):

| Scale | File | Host | graft commit |
| --- | --- | --- | --- |
| 1,000 commits | `bench/results/2026-10-23-scales-10-1000/results.json` | vogt-prod, 24 CPUs, 62.6 GiB | `35c308759e8f` |
| 10,000 commits | `bench/results/2026-10-23-scale-10000/results.json` | graft-dev, 4 vCPU, 7.8 GiB | `2c32be2d39a3` |

Bars are p50 milliseconds. Lower is faster. Teal is graft, grey is git.
Operations: full clone, sparse clone, `log -50`, `log --path`, blame,
status, sync (5 behind), eight concurrent clean lands, eight concurrent
same-file lands (the harness `merge3` scenario).

The hosts differ, and the 1k run predates later read-path work. Compare
graft with git inside one scale. Do not compare a 1k bar with a 10k bar
as a speed change.

## What is not on the chart

The twelve-run read-path medians recorded by WI-2341 on 2026-10-24 are
not in any file under `bench/results/`. They are written in
[docs/status.md](status.md) and [docs/decisions.md](decisions.md) (D31):

| | 1k graft / git | 10k graft / git |
| --- | ---: | ---: |
| log -50 | 3.3 / 2.0 ms | 3.4 / 1.9 ms |
| status | 4.1 / 3.2 ms | 13.3 / 6.5 ms |
| sync, 5 behind | 54.6 / 23.4 ms | 70.6 / 29.1 ms |

Those cells are the floor. The single-run p50s drawn on the chart are
older and slower, and they are not the floor.

The scale-1,000 table in `docs/status.md` (clone 89 ms, status 5.6 ms,
and the rest of that table) also has no committed `results.json`. It is
not drawn.

## How to read the speed

graft is faster on full clone, sparse clone, `log --path` and blame at
both recorded scales, and faster on concurrent clean lands at 10k
(238 ms vs 365 ms). It is slower on `log -50`, status and, at 10k, sync.

Read latency is a no-regression floor, not a phase-1 target (D31,
2026-10-25). No read path may be slower than the WI-2341 medians above,
and every operation stays O(change). Being 1.3× to 2.4× slower than git
on log, status and sync in the single-run files is recorded, not a
defect to close in phase 1. 2× faster than git is a later-phase target.
WI-2400 is wont_do.

## Real-PR replay

`bench/results/2026-10-24-real-prs/results.json`, 2026-10-24, 115 PRs in
five cases (rdpapp split and stack, fluent2 registration, rustTorrent
qBittorrent, graft merge-back). Clean merges and conflicts match git on
every case: 58 clean, 47 conflicts, graft-only and git-only clean both
zero. One case (`rdpapp-stack`) has no `agreement` object; its graft and
git counts are equal (0 clean, 1 conflict). Round trips are graft 152,
git 246.

`merged_clean` in that file is not the summary's "clean" column for
fluent2 (the file says 7, the agreement block and the summary say 18).
The chart uses the agreement block.

## Branching strategies

`bench/results/2026-10-24-branching/results.json`, seed 2362. The chart
shows round trips and conflict blocks for trunk, git flow and release
branches, graft-tracked against git. This file predates merge points
(WI-2372). `docs/status.md` records the later hand-off: with merge points,
graft reports the same conflicts as git (18 of 18 blocks) and merges
`release/1.1` into `main` clean. That later run is not in this file, so
it is not on the chart. The file will be regenerated; until then the
chart shows the committed run only.

## Re-run

From a graft checkout, with the release CLI on `PATH` and a local server
the harness can start:

```sh
python3 -m graftbench --scales 1000 --keep
python3 bench/render.py docs/guide/bench.svg
```

`--scales 10000` is the 10k run. Do not commit a partial file over the
recorded results without saying which host and commit produced it.
`bench/render.py` exits 1 if any of the four result files is missing.

## CI

Docs-only pull requests skip the Rust build. The `fmt` job still runs
`python3 bench/render.py` and fails when a required results file is
missing. The script writes the SVG; the job does not need the network.
