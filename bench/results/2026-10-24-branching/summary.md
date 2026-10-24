# Branching strategies through graft and git (2026-10-24)

Seed 2362, graft graft 0.0.0 (protocol 0.1), git version 2.53.0. Measurement only (D22).

## trunk

Ops: feature 18, tag 3, feature_commits 37

| system | model ok | = git | commands | round_trips | conflicts | conflict_blocks | land_retries | line_merged_paths | carries | carry_conflicted |
|---|---|---|---|---|---|---|---|---|---|---|
| git | True | — | 156 | 60 | 3 | 5 | 0 | 0 | 0 | 0 |
| graft-tracked | True | True | 85 | 85 | 3 | 4 | 3 | 6 | 0 | 0 |
| graft-mergebase | True | True | 85 | 85 | 3 | 4 | 3 | 6 | 0 | 0 |

git history: main 38 commits (0 merges)

## gitflow

Ops: cut 3, feature 36, fix 8, merge 6, tag 4, pick 2, feature_commits 72

| system | model ok | = git | commands | round_trips | conflicts | conflict_blocks | land_retries | line_merged_paths | carries | carry_conflicted |
|---|---|---|---|---|---|---|---|---|---|---|
| git | True | — | 436 | 154 | 14 | 18 | 0 | 0 | 0 | 0 |
| graft-tracked | True | True | 301 | 293 | 15 | 29 | 8 | 11 | 8 | 7 |
| graft-mergebase | True | True | 311 | 303 | 15 | 32 | 8 | 11 | 8 | 7 |

Between lines (conflict blocks, round trips):

| op | git | graft-tracked | graft-mergebase |
|---|---|---|---|
| merge release/1.0 → develop | conflict 1b, 2 rt | conflict 1b, 10 rt | conflict 1b, 10 rt |
| merge release/1.0 → main | clean 0b, 2 rt | clean 0b, 22 rt | clean 0b, 22 rt |
| merge release/1.0 → develop | conflict 2b, 2 rt | conflict 2b, 9 rt | conflict 3b, 14 rt |
| pick hf0 → develop | conflict 1b, 2 rt | conflict 1b, 7 rt | conflict 1b, 7 rt |
| merge release/1.1 → develop | conflict 2b, 2 rt | conflict 2b, 10 rt | conflict 2b, 10 rt |
| merge release/1.1 → main | clean 0b, 2 rt | conflict 11b, 22 rt | conflict 11b, 22 rt |
| merge release/1.1 → develop | conflict 1b, 2 rt | conflict 1b, 7 rt | conflict 3b, 12 rt |
| pick hf1 → develop | conflict 1b, 2 rt | conflict 1b, 7 rt | conflict 1b, 7 rt |

git history: main 95 commits (34 merges), develop 126 commits (45 merges), release/1.0 30 commits (9 merges), release/1.1 91 commits (31 merges)

## release

Ops: feature 24, cut 2, tag 6, fix 5, pick 4, feature_commits 49

| system | model ok | = git | commands | round_trips | conflicts | conflict_blocks | land_retries | line_merged_paths | carries | carry_conflicted |
|---|---|---|---|---|---|---|---|---|---|---|
| git | True | — | 272 | 106 | 9 | 11 | 0 | 0 | 0 | 0 |
| graft-tracked | True | True | 177 | 173 | 9 | 11 | 7 | 4 | 4 | 2 |
| graft-mergebase | True | True | 177 | 173 | 9 | 11 | 7 | 4 | 4 | 2 |

Between lines (conflict blocks, round trips):

| op | git | graft-tracked | graft-mergebase |
|---|---|---|---|
| pick mf0 → release/2.0 | conflict 2b, 2 rt | conflict 2b, 9 rt | conflict 2b, 9 rt |
| pick mf1 → release/2.1 | conflict 1b, 2 rt | conflict 1b, 9 rt | conflict 1b, 9 rt |
| pick late → release/2.0 | clean 0b, 2 rt | clean 0b, 7 rt | clean 0b, 7 rt |
| pick late → release/2.1 | clean 0b, 2 rt | clean 0b, 7 rt | clean 0b, 7 rt |

git history: main 53 commits (0 merges), release/2.0 22 commits (0 merges), release/2.1 51 commits (0 merges)

