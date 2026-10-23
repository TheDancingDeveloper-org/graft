# Quiet-host baseline: graft-pilot on rustTorrent (2026-10-23)

WI-2248. The same `graft-pilot` trial as the 2026-10-22 Node B baseline (WI-2247),
run on the dedicated graft-dev VM, which has nothing else running on it.

## Setup

| Item | Value |
|---|---|
| Host | graft-dev VM (Proxmox guest on trigproxmox), 4 vCPU (AMD Ryzen 7 7735HS), 8 GB RAM, Ubuntu 26.04, kernel 7.0.0-30 |
| graft | `origin/main` at `7e82d62` (includes WI-2247), `--release` |
| Source | `TheDancingDeveloper-org/rustTorrent`, bare clone, `refs/heads/main` at `ae7fb6e55ec7` (323 commits, 673 files) |
| Command | `graft-pilot --git ~/bench/rustTorrent.git --work ~/bench/work --out ~/bench/out --runs 3 --iterations 20` |
| Window | 2026-10-23 01:22:58Z to 01:28:14Z (5 min 16 s) |
| Defaults | hot file `Cargo.lock`, sparse path `crates/`, diff `main~20..main` |

## Host load

- Before the run (1-minute load): 0.03. The check made just before it, with the
  build finished and the idle graft-dev stack up (server and mirror containers,
  healthy), read 0.17. The brief's threshold was below 1.
- During the run, sampled every 10 s from `/proc/loadavg`: the 1-minute load
  rose from 0.03 to a peak of 3.22 and settled near 2.2. That load is the pilot's
  own work (import, server, clones), not other tenants. Nothing else ran on the
  VM, and no deploy or CI job was active on its runner.
- An earlier attempt failed within seconds because the pilot has to be started
  from the graft workspace (it runs `cargo build` for the binaries). It is
  not part of these numbers.

## Results (all runs pooled, 60 samples per scenario)

| Scenario | System | n | p50 | p95 | mean | graft ÷ git (p50) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| import | graft | 3 | 19550.38 ms | 20026.59 ms | 19626.78 ms | |
| full clone | graft | 60 | 332.63 ms | 398.65 ms | 331.23 ms | 1.18 |
| full clone | git | 60 | 282.86 ms | 295.68 ms | 283.57 ms | |
| sparse clone | graft | 60 | 171.98 ms | 231.62 ms | 180.16 ms | 0.96 |
| sparse clone | git | 60 | 179.86 ms | 198.24 ms | 182.80 ms | |
| log --path | graft | 60 | 4.71 ms | 5.60 ms | 4.81 ms | 1.26 |
| log --path | git | 60 | 3.75 ms | 4.30 ms | 3.77 ms | |
| diff (with patches) | graft | 60 | 13.10 ms | 16.77 ms | 13.74 ms | 1.02 |
| diff (with patches) | git | 60 | 12.89 ms | 15.50 ms | 13.28 ms | |
| blame cold | graft | 60 | 8.98 ms | 16.42 ms | 10.85 ms | 1.77 |
| blame cold | git | 60 | 5.08 ms | 9.60 ms | 5.56 ms | |
| blame cached | graft | 60 | 26.91 ms | 30.98 ms | 27.80 ms | 0.55 |
| blame cached | git | 60 | 48.70 ms | 55.68 ms | 50.32 ms | |
| decompress frames | graft | 60 | 12.88 ms | 13.56 ms | 13.03 ms | |
| decompress share of full clone | graft | 60 | 3.88 % | 5.38 % | 4.06 % | |
| restart to serving (kill -9) | graft | 9 | 15.47 ms | 16.59 ms | 15.43 ms | |

Fidelity: 323 of 323 changesets match `git checkout` exactly. Agreement: `log
--path` and `diff` agree; blame agrees on 12001 of 12001 lines (623 as
equivalent tie-breaks). The pilot exited 0.

## Reproducibility (p50 per run, spread)

Acceptance is a spread within 10%. Most scenarios miss it, at 10 to 15%, with
three runs of 20 samples on millisecond-scale commands.

| Scenario | System | Run 1 | Run 2 | Run 3 | Spread | Within 10% |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| import | graft | 19550.38 | 20026.59 | 19303.38 | 3.7% | yes |
| full clone | graft | 350.18 | 324.65 | 323.89 | 8.1% | yes |
| full clone | git | 284.68 | 278.54 | 279.70 | 2.2% | yes |
| sparse clone | graft | 188.01 | 162.41 | 172.23 | 14.9% | no |
| sparse clone | git | 191.82 | 177.82 | 176.32 | 8.7% | yes |
| log --path | graft | 5.11 | 4.46 | 4.61 | 14.0% | no |
| log --path | git | 3.95 | 3.57 | 3.56 | 11.0% | no |
| diff | graft | 14.69 | 12.92 | 12.91 | 13.8% | no |
| diff | git | 13.67 | 12.41 | 12.78 | 9.9% | yes |
| blame cold | graft | 8.46 | 9.31 | 8.93 | 9.5% | yes |
| blame cold | git | 5.24 | 4.83 | 4.65 | 12.2% | no |
| blame cached | graft | 29.75 | 26.49 | 26.43 | 12.5% | no |
| blame cached | git | 53.13 | 48.40 | 48.17 | 10.3% | no |
| restart to serving | graft | 16.06 | 14.95 | 14.56 | 10.0% | no |

In every scenario the first run is the slowest, by up to 8% on full clone and
diff, which suggests cache warm-up. Runs 2 and 3 agree within about 2%
(sparse clone excepted, at 6%).

## Against Node B (graft p50)

| Scenario | Node B (after WI-2247) | graft-dev VM | Change |
|---|---:|---:|---:|
| full clone | 266 ms | 332.6 ms | +25% |
| sparse clone | 156 ms | 172.0 ms | +10% |
| log --path | 3.9 ms | 4.71 ms | +21% |
| diff | 12.9 ms | 13.1 ms | +2% |

Only graft's numbers were supplied for Node B, so there is no git comparison
for that host in this note.

## Reading

- The quiet host did not make graft faster. All four figures are the same as or
  slower than Node B's. Node B is faster per core (or the VM costs more in
  virtualised I/O and process start), and its load was not the dominant noise.
  The VM is the better baseline for stability, not for speed.
- Against git on the same host, the ratios hold up: diff is level (1.02), sparse
  clone is level (0.96), blame cached is 1.8x faster than git, and full clone
  (1.18), log --path (1.26) and blame cold (1.77) are slower.
- Budgets still missed on this host: sparse clone (172 ms vs 50 ms), full clone
  (333 ms vs 150 ms) and blame cached (27 ms vs 10 ms). `log --path` is within
  its 10 ms budget.
- The 2× faster-than-git kill criterion is not met in any of the four budgeted
  scenarios.
- This is one 3 × 20 sample set. Spreads above 10% mean differences smaller
  than about 15% between hosts are not conclusive.

## Reproducing

```sh
tailscale ssh sprooty@graft-dev
git clone https://github.com/TheDancingDeveloper-org/graft ~/bench/graft && cd ~/bench/graft
git clone --bare https://github.com/TheDancingDeveloper-org/rustTorrent ~/bench/rustTorrent.git
cargo build --release -p graft-testkit --bin graft-pilot
cat /proc/loadavg        # start when the 1-minute load is below 1
target/release/graft-pilot --git ~/bench/rustTorrent.git --work ~/bench/work \
    --out ~/bench/out --runs 3 --iterations 20
```

Setting the VM up needed `build-essential` and `pkg-config` (there was no
linker) and rustup (minimal profile, stable). Both were installed for this run.
