# Line fork points: creating a line in O(1)

Status: design note for WI-2316. This note doesn't change
[design.md](../design.md) or [decisions.md](../decisions.md). The recommended
option changes what stored line-index rows mean, so it needs an operator
decision before any code lands (see §6).

## 1. Problem

`do_create_line` and `UpdateRef` creating a line both call
`line_rows(old = None, new)` in `graft-engine/src/refs.rs`. With no `old`, the
added stretch is `depth(new) + 1` changesets: every first-parent ancestor back
to the root. For each one the transaction writes a `line_pos` row, its
`line_paths` rows (one per changed path and ancestor directory), and reads its
changeset and record. One commit therefore carries O(history × paths) rows.
On a 100k-changeset repository, creating an integration line (D21) is slow,
holds the single writer for the whole time, and duplicates the parent line's
index. AGENTS.md rule 2 forbids this.

Import does the same, but import also writes those changesets, so its index
cost follows the change. Only creating a line *on existing history* is the bug.

## 2. Facts the options rely on

- A line moves only by first-parent fast-forward (§4.6), and lines can't be
  deleted (only tags can, by admin). A row written for a line stays true for
  ever.
- `pos` is the first-parent depth: the root is 0 and a land at depth `d` gets
  `pos = d`. Two lines that share a first-parent prefix give each shared
  changeset the **same** position. So "below the fork, ask the parent" needs
  no renumbering.
- Every query that uses the index is `line_pos(line, id)` or a range scan of
  `line_pos` / `line_paths` from one position down to another (log, file
  history, blame's version list, `is_ancestor` on a line, `merge_base`,
  rebase/sync onto a line position).

## 3. Option (a): fork-point index (recommended)

When a line is created at `X`, find the nearest existing line `P` that has `X`
on it: `line_pos(P, X) = Some(px)`. Store one fork record
`line_forks(repo, line, parent = P, pos = px)` and write **no** `line_pos` or
`line_paths` rows for positions `≤ px`. Later lands index only themselves, as
today.

Queries fall through:

- `line_pos(L, id)`: look up `L`'s own rows; on a miss, if `L` has a fork
  `(P, f)`, answer `line_pos(P, id)` only when the result is `≤ f`. Recursing
  through a chain of forks costs one read per hop.
- Range scans (`log`, path history, blame): scan `L`'s own rows from the top
  down to `f + 1`, then continue on `P` from `f` downward. The result set is
  unchanged, so the cost stays O(results) plus one read per fork hop.
- Ancestry and merge base: unchanged algorithms over the fall-through
  `line_pos`.

**Cost.** Create: one `line_pos` read per candidate parent line (a handful
per repository, §4.6) plus one row. With no line holding `X` (a fresh root
ref, or an `X` only on drafts), fork from the nearest landed ancestor: walk
first parents from `X` until `line_pos(P, a)` hits, which costs the unlanded
stretch, and index only that stretch. That is O(new lands). Queries add one
hop per fork depth. Fork chains are short (integration lines fork from `main`),
and the hop count can be capped by flattening: a new fork records the
*root-most* parent line that holds the fork point.

**Correctness across the fork.** The parent's rows at positions `≤ f` are the
child's history exactly, because both lines' first-parent chains agree below
`X`, and the parent never rewrites them. Log, blame and ancestry see the same
rows they see today.

**Parent moves.** The parent only moves forward. Its later rows have positions
`> f` and are filtered out by the `≤ f` bound. No interaction.

**GC.** GC roots are ref targets and their first-parent chains (§8.6). It
doesn't read line rows to decide liveness. The fork record holds no objects.
No interaction, as long as lines stay non-deletable. If line deletion is ever
added, deleting a line with forked children must either refuse or materialise
the children's rows first.

**Migration.** Existing lines keep their full rows and have no fork record, so
they read exactly as today. No backfill is needed. Compacting old lines' rows
into forks would be optional and separate.

## 4. Option (b): shared-prefix rows

Split each line's index into chain segments `(segment, pos) → id`, with lines
pointing to a list of segments. Creating a line at a point inside a segment
splits it (or references a prefix of it).

Cost per create is O(1) plus a segment split, and queries need a segment
lookup per range. But every existing `line_pos` and `line_paths` row changes
key shape (`line` becomes `segment`), every reader and writer changes, and
existing rows need a full rewrite migration. It solves the same problem as (a)
with far more durable-format change. Not recommended.

## 5. Option (c, for comparison): do nothing, chunk the write

Keep the full copy but write it in bounded batches. That fixes the single
huge transaction but not the O(history) cost or the duplicated rows, and it
breaks "one transaction per logical write" (rule 3). Rejected.

## 6. Does it change a durable format?

Option (a) changes no **encoding**: no id, tree, changeset, frame or token
bytes, no derived-kind value encoding, and no change to the shape of a
`line_pos` or `line_paths` row. It does change stored state in two ways:

1. **A new stored record and table**: `line_forks(repo, line, parent, pos)`, a
   new `Write::PutLineFork` and a `MetaStore::line_fork` read, a SQLite
   migration, and the memory store.
2. **The meaning of existing tables changes.** Today a line's `line_pos` and
   `line_paths` rows are its *complete* history. Under (a), a forked line's
   rows are only its own lands. A reader that doesn't know about forks does
   not ignore the change safely: it answers `line_pos(L, id) = None` for
   anything below the fork, so `is_ancestor` says false, `log` stops at the
   fork, and blame loses versions. Rolling the server back past this change
   would silently give wrong answers for every forked line.

So (a) is not the "new rows old readers ignore" case. Per the WI-2316 brief
and AGENTS.md rule 1, it needs an operator decision before implementation.

**Decision asked.** Accept option (a), with:

- the `line_forks` table and the fall-through semantics recorded in
  design.md §4.9 / §6.5 and a decisions.md entry;
- a one-way schema bump: a store at the new schema version refuses to open
  under an older binary, so a rollback fails loudly instead of answering
  wrongly;
- no backfill for existing lines.

## 7. Implementation outline (after the decision)

- `graft-store`: `Write::PutLineFork { line, parent, pos }`,
  `MetaStore::line_fork(repo, line)`, memory store, conformance tests.
- `graft-store-sqlite`: migration adding `line_forks` and the version bump.
- `graft-index`: a `line_pos` wrapper and range-scan helpers that follow
  forks; route `history.rs`, `ancestry.rs` and `blame.rs` through them.
- `graft-engine`: `line_rows(old = None)` finds the fork line and writes the
  fork record plus only the unlanded stretch. `workspace.rs` and `rebase.rs`
  call the index wrapper instead of `meta.line_pos` directly.
- Tests: a read-count test that creating a line at depth 10k and 100k writes
  O(1) rows and does O(lines) reads; log, blame, `is_ancestor` and merge base
  equal across a fork and an equivalent fully indexed line; existing
  unforked lines unchanged. A bench assertion at WI-2304 scale.
