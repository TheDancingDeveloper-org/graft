# Import group commit: one fsync per run, not per changeset

Status: proposal (WI-2323). Follows WI-2318 (#110). No code yet.

## 1. Problem

After WI-2318 pre-writes the trees of a run, importing 1,000 commits takes
about 6.2 s against a ~4.5 s target (git fast-import: about 1 s). What is left
is roughly 1,000 SQLite commits made one after another, each a WAL fsync under
`synchronous=FULL`.

## 2. Why the chained inserts don't group today

The SQLite writer already does group commit (`graft-store-sqlite/src/writer.rs`):
it takes every queued job, up to `MAX_BATCH` (256), runs each in its own
savepoint inside one `IMMEDIATE` transaction, and commits once. That only
helps when jobs are *waiting together* in the queue. Import never gives it a
second job:

1. `do_import_changesets` (graft-engine `write.rs`) calls `insert` for each
   changeset and awaits it before the next. `insert` ends with
   `meta.commit(repo, txn).await`, which returns only after the batch's
   `COMMIT` is durable.
2. The next changeset's `insert` *reads* its parent from the store before it
   can build its writes: `changeset_opt(parent)` (rule 1 and the pin), and
   `index.on_create`, which derives the generation and skip list and reads
   `line_pos` for the parent (graft-index `lib.rs`). Those reads go through
   reader connections, which see only committed data.
3. `SqliteStore::commit` is an `async fn`: it enqueues when first polled, not
   when called. So firing several commits without awaiting does not promise
   queue order, and a child could be applied before its parent.

So each batch the writer takes holds exactly one import job, and every
changeset pays a full fsync plus a writer round trip.

## 3. Options

All three keep `synchronous=FULL` for anything acknowledged, unless noted.
All three need the same engine-side piece: an **in-request overlay** so that
`insert` can build a child's writes from a parent that is planned but not yet
committed. The overlay holds, for changesets earlier in the same request,
the record and decoded changeset (for rule 1 and the pin list) and the
derived values graft-index needs (generation, skip list, line positions).
WI-2318's `prewrite_import` already keeps a `roots` map in the same spirit.
graft-index needs a way to derive from supplied parent values rather than
from the store (a small interface change in that crate).

### (a) A store write for a chained run (`commit_run`)

Add `MetaStore::commit_run(repo, Vec<MetaTxn>) -> Vec<TxnOutcome>`. The
SQLite writer runs the txns in order, each in its own savepoint (exactly as a
group of queued jobs runs today), and stops at the first one that is not
`Committed`: later ones are returned as not attempted. One `COMMIT` covers the
run; no caller is answered before it is durable. The memory store
implements it as a loop of `commit`.

- **Cost:** about one fsync per run (bounded by `MAX_BATCH` or the
  `import_prewrite_bytes` run the engine already cuts), instead of one per
  changeset. Expected to remove most of the ~1,000 fsyncs; to be confirmed on
  the bench import row at 1k.
- **Durability:** unchanged. Every acknowledged changeset is fsynced.
- **Failure:** a failed changeset rolls back to its savepoint; those before
  it stay (same contract as today's loop: "a failure keeps the ones before
  it"); those after it are not attempted, and the engine reports the first
  error. Preconditions (`ChangesetAbsent`, `RecordLive(parent)`) are checked
  inside the transaction, so they see the parent written earlier in the run.
- **Rule 3:** each changeset is still one `MetaTxn` with its own journal
  entry, applied atomically in its own savepoint. This is the same unit of
  atomicity the writer's existing group commit already relies on.
- **Size:** graft-store (trait + memory store), graft-store-sqlite (writer
  op), graft-index (derive from supplied parents), graft-engine (overlay and
  the import loop). Medium.

### (b) Pipeline dependent commits through the writer

The engine fires each changeset's commit without awaiting the previous one,
and the writer groups them as it does any queued jobs. As briefed, with
`synchronous=NORMAL` inside a batch and one fsync per group.

- **Cost:** similar fsync savings to (a).
- **Durability:** `synchronous=NORMAL` in WAL mode can lose the last
  transactions on power loss. If any of those were acknowledged, that breaks
  "every acknowledged write is durable". Keeping FULL and relying on grouping
  alone is safe, but then this is (a) without the ordering guarantee.
- **Ordering:** needs `commit` to enqueue synchronously at call time (point
  2.3), or the engine to serialize submission itself. Getting this wrong
  applies a child before its parent; the `RecordLive` precondition would
  catch it, but as a spurious failure.
- **Failure:** a parent's failure surfaces as a cascade of precondition
  failures in its children, which the engine must untangle.
- **Rule 3:** unchanged. But it makes the writer's correctness depend on
  queue order, which nothing states today.

### (c) Better: (a), sized by the existing run

Take (a) and submit exactly the run `prewrite_import` already planned: its
trees are durable before the run's metadata commits (rule 3's "objects are
durable before metadata references them" holds as now), so one `commit_run`
per pre-write run gives two syncs per run — objects, then metadata — however
many changesets it holds. Split a run longer than `MAX_BATCH` into chunks.
No `synchronous` change, no reliance on queue order.

A larger alternative, one `MetaTxn` (one journal entry) for the whole run,
would be faster still but does change rule 3 (one logical write per
changeset) and the journal's meaning; it is not recommended.

## 4. Recommendation

Option (c): (a)'s `commit_run`, one call per pre-write run.

**Operator decision on rule 3: not needed.** Each changeset stays one logical
write with its own journal entry, atomic in its own savepoint, and nothing
acknowledged is less durable. That is the interpretation the writer's group
commit already uses. If the operator wants the wording made explicit,
AGENTS.md rule 3 could read "one atomic unit (a transaction or savepoint) per
logical write"; that is a docs PR, not a precondition. Option (b) with
`synchronous=NORMAL` would need an operator decision (it weakens durability
of acknowledged writes) and is not recommended.

Note that (c) touches graft-index and graft-store as well as
graft-store-sqlite and graft-engine; with disjoint-crate workers, land the
graft-store and graft-index interface changes first as their own small PR.
