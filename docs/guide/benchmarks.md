# Benchmarks

How the numbers in [bench.svg](bench.svg) were measured, what they do and do not show, and how to reproduce them.

The chart is generated from committed result files by `python3 bench/render.py`. It does not invent a figure.

## Method

Two result files back the chart. Both use seed 2294, five iterations per operation, and a fresh CLI process for every step (graft and git alike).

| Scale | File | Host | graft commit |
| --- | --- | --- | --- |
| 1,000 commits | `bench/results/2026-10-23-scales-10-1000/results.json` | host A: 24 CPUs, 62.6 GiB | `35c308759e8f` |
| 10,000 commits | `bench/results/2026-10-23-scale-10000/results.json` | host B: 4 vCPUs, 7.8 GiB | `2c32be2d39a3` |

Bars are p50 milliseconds; lower is faster. Teal is graft, grey is git. The operations are:

- full clone and sparse clone
- `log -50` and `log --path`
- blame
- status
- sync (5 behind)
- eight concurrent clean lands
- eight concurrent same-file lands (the harness `merge3` scenario)

The two hosts differ, and the 1,000-commit run predates later read-path work. Compare graft with git within one scale. Do not compare a 1,000-commit bar with a 10,000-commit bar as a speed change.

## What is measured

On the committed single-run files, graft is faster on full clone, sparse clone, `log --path` and blame at both recorded scales, and faster on concurrent clean lands at 10,000 commits (238 ms vs 365 ms). It is slower on `log -50`, status and, at 10,000 commits, sync.

### Real pull-request replay

`bench/results/2026-10-24-real-prs/results.json` (2026-10-24) replays 115 pull requests in five cases: an `rdpapp` split, an `rdpapp` stack, `fluent2` registration lists, a qBittorrent importer, and graft's own merge-backs. Clean merges and conflicts match git on every case: 58 clean, 47 conflicts, and no pull request merged cleanly in only one of graft and git. One case (`rdpapp-stack`) has no `agreement` object; its graft and git counts are equal (0 clean, 1 conflict). Round trips are 152 for graft and 246 for git.

`merged_clean` in that file differs from the summary's "clean" column for one case (`fluent2-registration`: the file says 7, while the agreement block and the summary say 18). The chart uses the agreement block.

### Branching strategies

`bench/results/2026-10-24-branching/results.json` (seed 2362) records round trips and conflict blocks for trunk, git flow and release-branch strategies, graft-tracked against git. This file predates merge points. A later run, with merge points, reports the same conflicts as git (18 of 18 blocks) and merges `release/1.1` into `main` cleanly (see [status](../status.md)). That later run is not in this file, so it is not on the chart; the chart shows the committed run only, until the file is regenerated.

## What is not measured

Some figures quoted in the project documentation have no committed `results.json` and are not drawn:

- The twelve-run read-path medians recorded on 2026-10-24. They are the no-regression floor. The older single-run p50s drawn on the chart are slower than these medians.

  | | 1k graft / git | 10k graft / git |
  | --- | ---: | ---: |
  | log -50 | 3.3 / 2.0 ms | 3.4 / 1.9 ms |
  | status | 4.1 / 3.2 ms | 13.3 / 6.5 ms |
  | sync, 5 behind | 54.6 / 23.4 ms | 70.6 / 29.1 ms |

- The scale-1,000 table in [status](../status.md) (clone 89 ms, status 5.6 ms, and the rest of that table).

These are recorded in the [engineering record](../internal/trials/) and [decisions](../decisions.md).

## How to read the speed

Read latency is a no-regression floor, not a phase-1 target (recorded 2026-10-25). No read path may be slower than the twelve-run medians above, and every operation stays O(change). Being 1.3x to 2.4x slower than git on log, status and sync in the single-run files is recorded, not a defect to close in phase 1. Being 2x faster than git is a later-phase target and is out of scope for phase 1.

## Reproduce

From a graft checkout, with the release CLI on `PATH` and a local server the harness can start:

```sh
python3 -m graftbench --scales 1000 --keep
python3 bench/render.py docs/guide/bench.svg
```

`--scales 10000` is the 10,000-commit run. Do not commit a partial file over the recorded results without saying which host and commit produced it. `bench/render.py` exits 1 if any of the four result files is missing.

## Continuous integration

Docs-only pull requests skip the Rust build. The `fmt` job still runs `python3 bench/render.py` and fails when a required results file is missing. The script writes the SVG and needs no network.

## See also

- [Safety](safety.md)
- [FAQ](faq.md#is-graft-faster-than-git)
- [Status](../status.md)
