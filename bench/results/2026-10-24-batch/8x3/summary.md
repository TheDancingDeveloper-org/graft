## Batch delivery (WI-2352)

8 agents x 3 lands = 24 lands on `int`, concurrent, pre-land check on; 0 lands needed `--sync`, 24 land attempts; land p50 233.2 ms, p90 381.0 ms.

| Measure | Result |
| --- | --- |
| Batch 1 commits | 24 for 24 lands (one per land: True); CI fail (planted) |
| Authorship fully correct | 24/24 (100%): author 24, session 24, work item 24, kind agent 24 |
| `git bisect` lands on the bad land's commit | True |
| `git blame` of good lands' lines | 23/23 on the right land's commit |
| Commits on `main` | 25 for 24 lands (1.042 per land; the bad land and its revert are both kept: True) |
| Good lands held by the bad one | 23 |
| Drop and recut (`pr-batch --exclude`) | 0.221 s; PR 1 closed True with comment True; batch 2 25 commits, CI pass |
| Cut batch 1 | 0.444 s (PR open 0.529 s after the last land) |
| Land to `main` (immediate rebase-merge (fast-forward) after the recut; no review wait) | p50 2.044 s, p90 2.708 s; settle run 0.218 s |

Result: pass.
