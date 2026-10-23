# Line fork points: creating a line in O(1)

Status: implemented (WI-2316). The operator accepted option (a) on
2026-10-23. graft has no users, so it ships with no migration, no rollback
guard and no backfill: a plain schema bump (SQLite version 5) adds the
`line_forks` table. design.md and decisions.md are updated in their own PR.

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

## 3. Option (a): fork-point index (chosen)

A line created at `X` walks first parents from `X` while the records are not
landed. That walk is the unlanded stretch, the only changesets the create
lands. It stops at the first landed ancestor `a`, or at the root.

- At `a`: find a line `P` that holds `a` in its own rows, not through a
  fork (`MetaStore::line_holders`, one indexed read by changeset; SQLite has
  `line_pos_changeset` for it). Every landed changeset has such a line: it got
  its own row on the line it landed on. Forking from a direct holder keeps
  hop chains from growing with repeated creates. Write one
  `line_forks(repo, line, parent = P, pos = f)` row.
  The stretch is indexed at `f + 1, f + 2, …`.
- At the root: no fork row; the stretch is indexed from 0. That is a new
  history, so its whole length is new lands.

Nothing at positions `≤ f` is copied. Later lands index only themselves, as
before. Every line created on existing history forks; there is no second
path that writes full rows.

Reads fall through inside the store (`MetaStore::line_pos` and
`scan_line_path`), so `graft-index` and every caller are unchanged:

- `line_pos(L, id)`: `L`'s own row; on a miss, follow `L`'s fork `(P, f)` and
  accept a hit only at `≤ f` (the bound is the minimum over the hops).
- `scan_line_path(L, path, before, limit)`: scan `L`'s own rows, then, while
  the page is short, continue on `P` with `before = min(before, f + 1)`.

**Cost.** Create: the unlanded stretch (changeset reads and rows) plus one
indexed holder read, whatever the number of refs. Queries: their results plus one fork read per hop.
Log, path log, blame's version list, `is_ancestor`, `merge_base` and
rebase/sync onto a line position all go through those two reads.

**Correctness across the fork.** Positions are first-parent depth, so a
changeset has the same position on every line that holds it. The parent's
rows at `≤ f` are exactly the child's history below the fork, and the parent
never rewrites them (lines only fast-forward and can't be deleted). A fork of
a fork works the same way, one hop at a time.

**Parent moves.** The parent only moves forward; its later rows sit above
`f` and the bound filters them out.

**GC.** GC roots are ref targets and their chains (§8.6); it never reads
line rows, and the fork row holds no objects. If line deletion is ever added,
deleting a line with forked children must refuse, or copy their rows first.

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

## 6. Durable format

The stored line record is new and frozen by golden vectors:

- `Write::PutLineFork { line, parent, pos }` in `graft-store`.
- SQLite version 5: `line_forks(repo INTEGER, line TEXT, parent TEXT, pos
  INTEGER, PRIMARY KEY (repo, line)) WITHOUT ROWID`, plus the index
  `line_pos_changeset (repo, changeset, line)`. One fork per line: a second
  fork row fails the transaction in both stores. The DDL and a stored row
  are pinned by `line_fork_record_golden_vector` in
  `graft-store-sqlite/tests/migrations.rs`.

The meaning of `line_pos` and `line_paths` changed: a forked line's rows are
only its own lands. No encoding changed (ids, trees, changesets, frames,
tokens, derived values). With no users, there is no migration, no rollback
guard and no backfill.

## 7. Tests

- `graft-store` conformance `line_forks_fall_through_to_the_parent`, on
  memory and SQLite: positions and path scans through a fork and a fork of a
  fork, bounded at each fork, paging across the hop; direct holders; a
  duplicate fork row refused.
- `graft-store-sqlite/tests/migrations.rs`: a version 4 database opens at
  version 5 with its line rows intact.
- `graft-engine/tests/line_fork.rs`: log, path log, `is_ancestor` and blame
  across a fork of a fork on both stores; a log page straddling both hops;
  parents landing after the fork change none of the child's answers; and on a 300-deep history, creating
  a line at the tip writes one fork row and no position or path rows, while a
  line at a two-draft stack writes two; a second line at the tip forks from
  `main`, not from the first.
