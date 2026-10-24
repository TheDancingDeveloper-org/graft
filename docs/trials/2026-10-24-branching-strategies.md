# Branching strategies through graft and git (WI-2362, 2026-10-24)

Trunk-based development, git flow and "release branches only", each run as
one scripted history through graft and through git. Measurement only (D22):
nothing built, tested or checked code. Harness:
[`bench/graftbench/branching.py`](../../bench/graftbench/branching.py). Raw
results:
[`bench/results/2026-10-24-branching`](../../bench/results/2026-10-24-branching/).

## git concepts in graft

Answered from the code at `98f6abb` (CLI `crates/graft-cli/src/cli.rs`, engine
`crates/graft-engine/src/{rebase,revert,refs}.rs`, design §4.6, §7.2, §8).

| git | graft | Notes |
| --- | --- | --- |
| branch | **line** | A named ref that moves only forward, by CAS. `graft line create/list`. |
| feature branch | **workspace + draft stack** | `graft clone --line L` opens a workspace on L; `graft commit` adds drafts; `graft land` rebases the stack onto L's tip (line merge, D16) and moves L. No branch name is needed. |
| merge (into the same branch, e.g. `pull`) | **rebase with line merge** (G3.7) | G3.7's "line merge" is `merge = line` inside `Rebase`/`Sync`/`Land`: a three-way **per-file, line-level** merge used when a stack moves onto a newer tip. It is not a merge between two lines. |
| merge one branch into another | **none** | No command, no MCP tool, and no merge record: the CLI only makes one-parent changesets. A client can carry the work across as a squash (below). |
| release branch | **`graft line create release/x --from develop`** | Fork point, one row (D23). Lands go to it like any line. |
| tag | **`tags/<name>` ref, but no command to make one** | The engine has an immutable `Tag` ref kind (`UpdateRef`, `tag-immutable`), and `graft-mirror follow` imports GitHub tags as `tags/<name>` (WI-2342). `update_ref` is excluded from the CLI. The bench uses `graft line create tags/<name> --from <line>`, which is a mutable, landable line. |
| cherry-pick | **none in the CLI or MCP; the `Rebase` API can** | `Rebase { changeset: X, onto }` of a landed X moves only X (its chain stops at the first landed parent), so it is a server-side cherry-pick with line merge. Nothing exposes it. `graft revert` makes the inverse, not the change. Today a client re-applies the diff itself. |
| revert | **`graft revert <rev>`** | Inverse of a changeset relative to its first parent, on the working copy's head. File-level: a path changed since is a conflict (exit 3), no line merge. |
| hotfix back-merge | **none; a carry** | Land the fix on `main` (a workspace on main), then carry that changeset onto `develop` (a cherry-pick by client). |
| stacked branches / PR chains | **draft stack** | `graft stack list/squash/reorder/split/drop`; lands in one call. |

The "carry" the bench uses for every git merge or cherry-pick:
`graft diff BASE SRC` (paths), `graft cat` of each path at BASE and SRC, a
local three-way merge into a working copy on the target line, `graft commit`,
`graft land`. BASE is the changeset's parent for a cherry-pick, and for a
merge either the last source changeset this client carried (`tracked`) or
what `graft api merge_base` answers (`mergebase`).

## Method

- **Content.** 8 modules of 6 `key = "..."` lines, 4 padding lines apart.
  Every edit adds a unique marker to one key; 30% of edits go to three hot
  keys, so overlaps happen. Conflicts are resolved by one rule in both
  systems: each key becomes `ours | (theirs - base)`. With that rule a merge
  is the union of both sides and a cherry-pick adds exactly its own markers,
  so a model predicts every line's and tag's tree.
- **Writers.** 3 writers per batch start features from the same tip, then
  land in order, so later lands need a rebase. graft: one workspace per
  writer and line, `commit` per commit, `land` (then `land --sync` and a
  resolve on a conflict). git: a clone per writer; trunk and release
  strategies rebase and push, git flow merges `feature/*` with `--no-ff`.
- **Scripts** (seed 2362):
  - *trunk*: 6 batches of 3 features on `main`, a tag every 2 batches.
  - *git flow*: `develop` cut from `main`. Twice: 3 batches on develop; cut
    `release/1.x`; develop moves on; 2 release fixes; interim back-merge to
    develop; another develop batch and release fix; release → main, tag
    `v1.x.0`, release → develop; a develop batch; a hotfix on main, tag
    `v1.x.1`, hotfix → develop.
  - *release branches*: twice: 3 batches on main; cut `release/2.x`, tag
    `v2.x.0`; a main batch; a fix on main cherry-picked to the release; a
    release-only fix; tag `v2.x.1`. Then a late fix picked into both
    releases and tagged on each.
- **Round trips** count commands that reach a server or remote: every graft
  command (each is a fresh process that calls the server), and git `clone`,
  `fetch` and `push`.
- Local graft-server and a bare git repository on the graft-dev VM, release
  build of `main` at `98f6abb`. Two runs at seed 2362 gave identical
  `results.json`.

## Results

**Correctness.** Every line and every tag is byte-identical between graft
(both variants) and git, and equals the model, in all three strategies.

| strategy | system | commands | round trips | ops that conflicted | conflict blocks | refused lands → `--sync` | line-merged paths at land |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| trunk | git | 156 | 60 | 3 | 5 | — | — |
| trunk | graft | 85 | 85 | 3 | 4 | 3 | 6 |
| git flow | git | 436 | 154 | 14 | 18 | — | — |
| git flow | graft tracked | 301 | 293 | 15 | 29 | 8 | 11 |
| git flow | graft mergebase | 311 | 303 | 15 | 32 | 8 | 11 |
| release | git | 272 | 106 | 9 | 11 | — | — |
| release | graft | 177 | 173 | 9 | 11 | 7 | 4 |

(The two graft variants differ only on repeated merges, so trunk and release
show one row.)

**Trunk-based fits graft.** Same conflicts as git (3), about half the
commands (85 against 156), and 6 paths merged by line on the server during
land with no client step. git counts 5 conflict blocks against graft's 4
because its rebase replays commit by commit and meets one overlap twice.

**Moving work between lines** (conflict blocks, round trips per operation):

| git flow op | git | graft tracked | graft mergebase |
| --- | --- | --- | --- |
| release/1.0 → develop (interim) | 1 conflict, 2 rt | 1, 10 rt | 1, 10 rt |
| release/1.0 → main | clean, 2 rt | clean, 22 rt | clean, 22 rt |
| release/1.0 → develop (final) | 2, 2 rt | 2, 9 rt | **3, 14 rt** |
| hotfix 1.0.1 → develop | 1, 2 rt | 1, 7 rt | 1, 7 rt |
| release/1.1 → develop (interim) | 2, 2 rt | 2, 10 rt | 2, 10 rt |
| release/1.1 → main | **clean**, 2 rt | **11 blocks**, 22 rt | **11 blocks**, 22 rt |
| release/1.1 → develop (final) | 1, 2 rt | 1, 7 rt | **3, 12 rt** |
| hotfix 1.1.1 → develop | 1, 2 rt | 1, 7 rt | 1, 7 rt |

| release-branches op | git | graft |
| --- | --- | --- |
| pick main fix → release/2.0 | 2 blocks, 2 rt | 2, 9 rt |
| pick main fix → release/2.1 | 1, 2 rt | 1, 9 rt |
| pick late fix → release/2.0 | clean, 2 rt | clean, 7 rt |
| pick late fix → release/2.1 | clean, 2 rt | clean, 7 rt |

- **Cherry-picks match git exactly** in outcome (same conflicts, same
  trees), at 7 to 9 round trips against git's fetch + push, because the
  client fetches both versions of every path.
- **Merges between lines are where graft falls behind.** A carry is a
  squash with no record of what it merged. In git the second release's
  merge into `main` is clean: `main`'s merges of release/1.0 and the hotfix
  are ancestors of release/1.1. In graft `main` holds squashes, the only
  common ancestor is the seed, and the same carry meets **11 conflict
  blocks** that git never sees. Remembering the last carried point
  (`tracked`) helps only for the same pair of lines.
- **`merge_base` never advances** between lines that exchange carries, so a
  client that trusts it re-merges old work: 3 blocks against 2 and 1 on the
  final back-merges, and more round trips.
- **Tags** work only as a stand-in: `line create tags/v1.0.0 --from main`
  gives the right tree at the right point, but the ref is a line that can
  still be landed on.
- **History shape.** git flow in git leaves 34 merge commits on `main` and
  45 on `develop`; graft's lines are linear, one changeset per land or
  carry. graft's history is simpler to read, but it can't say "release/1.0
  is in main".

## Gaps (raised as Vogt items)

1. **No merge between lines and no merge record** (WI-2363): git flow's
   release → main and back-merges become client-side squashes. Repeated
   merges between different line pairs conflict where git doesn't (11 blocks
   against 0), and `merge_base` can't advance. This needs a design decision
   (a second parent on a landed changeset, or a recorded merge point), so it
   goes to the operator.
2. **No cherry-pick command** (WI-2364): the engine's `Rebase` already does a
   server-side pick with line merge, but neither the CLI nor MCP exposes it.
   A client pick costs 7 to 9 round trips and a local merger, against 2 for
   git.
3. **No tag command** (WI-2365): an immutable `Tag` ref exists in the engine, but only
   `graft-mirror follow` can make one. Release tags on a graft-primary line
   need a CLI and MCP command.

Not a gap here, but noted: `graft revert` is file-level (a path changed
since the reverted changeset is a conflict), unlike the line-merging land.

## Reproduce

```sh
CARGO_BUILD_JOBS=4 cargo build --release -p graft-cli -p graft-server -p graft-mirror
cd bench
python3 -m graftbench.branching --out results/<dir>     # seed 2362, about 1 minute
python3 -m unittest tests/test_branching.py
```
