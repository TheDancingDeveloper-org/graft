## Batch delivery (WI-2352)

5 agents x 2 lands = 10 lands on `int`, concurrent, pre-land check on; 0 lands needed `--sync`, 10 land attempts; land p50 338.6 ms, p90 460.3 ms.

| Measure | Result |
| --- | --- |
| Batch 1 commits | 10 for 10 lands (one per land: True); CI fail (planted) |
| Authorship fully correct | 10/10 (100%): author 10, session 10, work item 10, kind agent 10 |
| `git bisect` lands on the bad land's commit | True |
| `git blame` of good lands' lines | 9/9 on the right land's commit |
| Commits on `main` | 11 for 10 lands (1.1 per land; the bad land and its revert are both kept: True) |
| Good lands held by the bad one | 9 |
| Drop and recut (`pr-batch --exclude`) | 0.26 s; PR 1 closed True with comment True; batch 2 11 commits, CI pass |
| Cut batch 1 | 0.311 s (PR open 0.35 s after the last land) |
| Land to `main` (immediate rebase-merge (fast-forward) after the recut; no review wait) | p50 1.995 s, p90 2.34 s; settle run 0.273 s |

Result: pass.
