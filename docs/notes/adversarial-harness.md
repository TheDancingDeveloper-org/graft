# Adversarial harness: attacking graft on purpose

Status: design note (operator request, 2026-10-24). The work items are:
- WI-2345: racing lands under faults (first, p1);
- WI-2346: merge3 against git;
- WI-2347: hostile git repositories;
- WI-2348: hostile API clients.

This note doesn't change any design rule or durable format.

## 1. Why

graft's correctness evidence today comes from friendly runs:
- property tests in seven crates;
- the `kill -9` crash suite (`graft-testkit/src/crash.rs`, design §6.6);
- the pilot harness;
- the bench (WI-2294). It compares graft, git and a model replay byte for
  byte, with eight concurrent writers in three fixed scenarios
  (`bench/graftbench/conc.py`).

None of them *chooses* the worst moment:
- a crash between the object write and the CAS;
- a retry that arrives after the first attempt has already landed;
- a lease that runs out mid-land;
- a GC sweep running alongside a land;
- an acknowledgement lost after a commit that succeeded.

Those are the cases a source-control server gets wrong. The fit assessment
(docs/notes/graft-fit-assessment.md, gap 4) found the claims untested on
hostile or realistic concurrency.

The harness tests phase-1 properties: storage, history, merge and land
correctness. It is a test tool, never a gate in the land path
([D22](../decisions.md#d22-phase-1-is-a-git-alternative)).

## 2. Shape

Four parts share one approach:

- a seeded adversary picks the inputs, the interleaving and the faults;
- a recorder writes every operation, with its arguments and result, to a
  history;
- a checker compares the history and the final state against a model.

Every failure prints its seed and a one-line replay command. The history is
shrunk to a minimal failing case (proptest-style, by dropping operations
and faults until the failure goes away).

| Part | Adversary controls | Oracle |
| --- | --- | --- |
| 1. Racing lands under faults (WI-2345) | interleaving of N clients, process kills, store faults, lease and GC timing | a line model and the invariants in §3 |
| 2. merge3 against git (WI-2346) | three-way merge inputs | `git merge-file`, plus `cargo check` on clean merges of real files |
| 3. Hostile git repositories (WI-2347) | the shape of the imported history | sha-identical round trip, or a typed refusal with nothing partial |
| 4. Hostile clients (WI-2348) | requests, tokens, sizes | typed `ApiError`, no partial effect, no canary in any output |

## 3. Part 1: racing lands under faults

### 3.1 The adversary

It runs at two levels.

**In-process (fast, deterministic, in CI).**
- The engine is generic over its stores and clock (`Engine<M, O>` with
  `Arc<dyn Clock>`). The harness wraps a real `graft-store-sqlite` and
  `graft-store-fs` pair in `Faulty<M>` and `Faulty<O>`, and uses the
  existing `FakeClock`.
- The wrappers decide, per call and from the seed, to do one of these:
  - pass through;
  - delay (yield N times, so other tasks run first);
  - fail before the write (`StoreError`, nothing written);
  - **fail after the write**: the write commits but the caller sees an
    error. This is the lost-ack case;
  - stall until the adversary releases it.
- A single-threaded runtime with a seeded task order makes the
  interleaving reproducible. The adversary also:
  - moves the fake clock past lease ends, including the 10-minute
    land-attempt lease;
  - runs the GC sweep at chosen points;
  - fills the object store's quota (`ENOSPC`).

**Out of process (slower, real, on demand).** These are soak runs against
a real `graft-server`:
- `kill -9` and restart the server at seeded points, reusing the
  `crash.rs` parent and child pattern;
- duplicate, delay and reorder client retries at the transport;
- drive real CLI working copies, so "never stranded" is checked end to
  end.

Long soaks run on the graft-dev VM through `bench/`, as an `adversary`
mode next to `conc` and `livea`.

### 3.2 Client behaviour

Each simulated client loops: open or reuse a workspace, commit drafts over
a chosen path set, then `land`. It retries on `Contended` and `Unavailable`.
On `Conflict` it resolves by a fixed rule (the sorted union of values, as
in `conc.py`), so the final content doesn't depend on order.

Path sets are drawn to force every case:
- disjoint paths;
- one shared file in separate regions (line merge);
- the same lines (conflict);
- adjacent regions (`EAGER` conflict);
- delete against edit;
- binary files.

### 3.3 Properties the checker enforces

These are the invariants of design §4.10 and AGENTS.md rules 3 and 4,
stated over the history:

1. **No lost or duplicated land.** What counts as "the same land" is
   defined below. Every land acknowledged with `Landed` is on the line
   exactly once. A land that returned an error, or whose ack was lost, is
   on the line at most once. A blind retry of the same `(head, line)`
   returns the same `Landed` (§7.9).

   **Land identity.** A land's identity is its *root draft*: the draft the
   client created with `CreateChangeset`. The recorder logs every id the
   client is handed back, so it knows the roots.
   - Every rebase (on land, and on `sync`, which runs the same code, §8.4)
     writes a new changeset whose `graft.rebased-from` metadata names the
     changeset it was rebased from (`graft-engine/src/rebase.rs`). That is
     the immediate source, overwritten on each rebase.
   - The checker follows `graft.rebased-from` from each landed changeset,
     hop by hop, until it reaches a changeset the recorder saw the client
     create. That is the land's root.
   - An amended draft is a new root. The superseded one must never land.

   **Retry after a lost ack.** A client whose land ack was lost does one of
   two things, and the model allows exactly these outcomes:
   - **Blind retry** (the same head). It must get back the original
     `Landed`, through the `lands[(head, line)]` record.
   - **Sync, then land again.** The sync rebases the client's stack onto a
     tip that may already hold the first land, so the re-land has a new
     head, and the `(head, line)` key no longer matches. The model accepts
     either:
     - a typed answer that the draft is already landed;
     - a land whose changes against its first parent are empty.

     A second land with a non-empty change from the same root is a
     duplicate and fails the check. The first worker step pins down which
     of the two outcomes graft gives today and records it here. If it is
     neither, that is a finding, not a harness change.
2. **Lines only move forward.** Each line's sequence of tips forms one
   first-parent chain. Every move is in the journal, in the same
   transaction as its index rows.
3. **CAS only.** Two checks, neither of which assumes the journal records
   the expected version:
   - **Expected versions come from the transaction, not the journal.**
     `Faulty<M>` sees every `MetaTxn` before passing it on. A ref write
     (`Write::SetRef`) must come with a `Precondition::RefIs { name,
     expected }` on the same ref, and the wrapper records that
     `expected: Option<(target, version)>`. No two committed transactions
     may carry the same `expected` for one ref. No transaction may hold
     two `SetRef` writes, so nothing moves two refs at once.
   - **The journal chain must be gap-free.** The `RefMoved` body is
     `{name, old, new, version, actor}` (`graft-engine/src/journal.rs`),
     with `version` the version after the move. Per ref, in journal
     order:
     - versions run 1, 2, 3, … with no gap and no repeat;
     - each entry's `old` equals the previous entry's `new` (`null` first);
     - the final `refs` row's `(target, version)` equals the last entry.
4. **Conflicts are never silent.** The model is independent of graft: it
   never calls `graft-model`'s merge3 or any other diff3.
   - **Region layout.** The generator lays each shared file out as
     numbered regions with known gaps of unchanged lines between them, and
     every client write replaces whole regions. Two writes conflict in the
     model when they touch the same region, or adjacent regions with a gap
     of zero lines (`EAGER`'s adjacency rule, restated over the layout).
     Writes with a gap of at least `REGION_GAP` don't conflict.
   - **Expected content.** On a conflict, clients resolve by the sorted
     union of values (§3.2). That result doesn't depend on order, so the
     model computes each region's expected final value directly: the
     sorted union of the values of every acknowledged land on it, plus
     those of lost-ack lands that the history shows on the line. No merge
     algorithm is needed.
   - **The checks:**
     - Two writes that conflict in the model can't both land unless a
       resolution draft followed.
     - The final content of every region equals the model's value.
     - A conflict graft reports where the model expects none is counted
       as a spurious conflict and reported. It is not a safety failure.
5. **Objects before metadata.** After any fault and restart, every object a
   landed changeset or line index row refers to is present and hashes to
   its id.
6. **GC is safe and complete.** GC never removes an object that is held,
   landed or inside a live lease. After the leases and retention run out,
   no unreferenced object is left unreported (§8.6).
7. **Never stranded.** After any fault, each client's `status` reports a
   base, head and state, and `sync` moves it forward.
8. **The derived index matches history.** `log --path` and blame at the
   end agree with a full recomputation from the line's changesets. This is
   the only place the harness deliberately walks history, as an oracle.

### 3.4 Proving the harness bites

A harness that never fails proves nothing. Plant bugs one at a time, and
require the harness to catch each within the CI seed budget.

Some bugs can be made by mutating what crosses the store ports, so they
live entirely in `graft-harness`'s `Faulty<M>` and `Faulty<O>` wrappers.
The rest are engine or GC logic and need test-only hooks in the crate that
owns the logic:

| Planted bug | Where | Needs a production-crate hook |
| --- | --- | --- |
| A ref write that skips the CAS | `Faulty<M>` drops the `Precondition::RefIs` from a `MetaTxn` (its fields are public) | No |
| An object the store acknowledged but didn't keep: the port-level form of "metadata before objects" | `Faulty<O>` returns `Ok` from a put and discards the bytes | No |
| A land that commits metadata before awaiting its object writes (the real ordering bug) | `graft-engine` land path | Yes: `graft-engine` |
| The `lands[(head, line)]` idempotency check skipped | `graft-engine` land path | Yes: `graft-engine` |
| A lease end compared with `<` instead of `<=` | lease checks in `graft-engine` and the sweep in `graft-gc` | Yes: `graft-engine`, `graft-gc` |
| GC ignores the land-attempt holder | `graft-gc` sweep | Yes: `graft-gc` |

The hooks:

- **A cargo feature.** `cfg(test)` alone can't be seen from another crate,
  so the hooks use a named feature, `planted-bugs`, on `graft-engine` and
  `graft-gc`. It is off by default.
- **Where the feature is enabled.** Only in `graft-harness`'s
  `[dev-dependencies]`. The workspace uses resolver 3, which doesn't unify
  features from dev-dependencies into normal builds, so `cargo build -p
  graft-server` and every release image are built without it.
- **Off at runtime too.** With the feature compiled in, each hook is still
  a no-op until the harness switches it on (a test-only setter on a
  `PlantedBug` value). So a `cargo test --workspace` build that unifies the
  feature still behaves normally.
- **A policy check.** CI asserts that `cargo tree -p graft-server -e
  features` (normal edges) doesn't contain `planted-bugs`.
- **An interface PR first.** The hooks land as their own small PR,
  touching `graft-engine` and `graft-gc` only: the feature, the
  `PlantedBug` type and the hook call sites, with no behaviour change
  while the hooks are off. This follows AGENTS.md's rule for changing
  another crate's API. It is the only change part 1 makes outside
  `graft-harness` and `bench`.

WI-2345's "done when" (a planted bug is caught and replayed from its seed)
can be met with the two wrapper-level bugs before the interface PR lands.

### 3.5 Budgets

- **CI (merge queue):** fixed seeds, about 2 minutes on the in-process
  adversary. CI caps build parallelism nowhere, and no run waits on the
  network.
- **On demand:** random seeds for hours on graft-dev. Failing seeds are
  saved under `bench/results/adversary/` and become fixed seeds in CI.

## 4. Parts 2–4, in brief

- **merge3 against git (WI-2346).**
  - Generate and mutate inputs from the `graft-model` merge3 corpus and
    real estate files.
  - Compare against `git merge-file`. Every disagreement is either an
    accepted difference, listed in the corpus with its reason, or a bug.
  - After a clean merge of real Rust sources, run `cargo check`, and count
    the clean-but-wrong merges (the fit assessment's gap 1).
  - This is shared with the replay bench (WI-2338).
- **Hostile git repositories (WI-2347).**
  - Generator scripts, not binary fixtures, build repositories with
    octopus merges, several roots, empty and signed commits, odd tags,
    gitlinks, symlinks and modes.
  - Paths: not valid UTF-8, colliding when case is ignored, or reserved
    names.
  - Blobs at the 64 MiB limit, deep trees, and upstream force-pushes while
    follow runs.
  - Each must round-trip with identical shas, or be refused with a typed
    error and no partial import.
- **Hostile clients (WI-2348).** Every surface (gRPC, JSON, `/mcp`, node)
  gets:
  - scope escalation, including drafts without `repo:<r>:drafts` (D24);
  - expired, revoked, replayed and tampered tokens;
  - oversized and malformed requests, and requests for unbounded answers.

  A canary token and canary file contents are planted, then every log
  line, error, `Debug` output, metric label and process argument is
  scanned for them (rules 7 and 8).

## 5. Where it lives

| Part | Code | Crates touched |
| --- | --- | --- |
| 1 | A new crate, `graft-harness`: `Faulty<M>`/`Faulty<O>`, the scheduler, recorder, model and checkers, and the wrapper-level planted bugs, with the runs as its integration tests. `bench/graftbench/adversary.py` runs the out-of-process soak | `graft-harness` (new), `bench`, and an interface PR first that adds the `planted-bugs` feature to `graft-engine` and `graft-gc` (§3.4). No other engine change |
| 2 | `graft-model` tests, plus a `bench` scenario for `cargo check` | `graft-model`, `bench` |
| 3 | `graft-mirror` and `graft-git` integration tests, with generator scripts under `tests/hostile/` | `graft-git`, `graft-mirror` |
| 4 | `graft-server` integration tests, with a canary scanner in `graft-testkit` | `graft-server`, `graft-testkit` |

**Why a new crate, not `graft-testkit` or integration tests only.**
- **`graft-testkit` would create a cycle.** It depends only on
  `graft-model`, `graft-proto` and `graft-store`, and `graft-engine`,
  `graft-gc` and nine other crates dev-depend on it. Part 1 needs the
  engine, GC and both real stores as normal dependencies. Putting that
  code in `graft-testkit` would make it depend on `graft-engine` while
  `graft-engine` dev-depends on it. That is a cycle, and the engine's own
  unit tests would see two copies of the engine crate.
- **Integration tests alone would spread the code.** Integration tests
  inside `graft-engine` would need `graft-gc`, `graft-store-sqlite` and
  `graft-store-fs` added as dev-dependencies there, and the scheduler,
  recorder and checkers couldn't be shared with the soak driver.
- **So `graft-harness` is a leaf.** It depends normally on
  `graft-engine`, `graft-gc`, `graft-store` (with its `testkit` feature),
  `graft-store-sqlite`, `graft-store-fs`, `graft-proto`, `graft-model` and
  `graft-testkit` (for `crash.rs` and `FakeClock`). Its
  `[dev-dependencies]` repeat `graft-engine` and `graft-gc` with
  `features = ["planted-bugs"]`. Nothing depends on `graft-harness`, so
  there is no cycle, and no existing crate's `Cargo.toml` changes, except
  the feature declarations in the interface PR.
- **Cargo changes.** Add `crates/graft-harness` to the workspace
  `members` (one entry per line, in sorted order) and to
  `[workspace.dependencies]`, with `publish = false`. The out-of-process
  mode runs the built `graft-server` binary as a child process, so it
  needs no dependency on `graft-server`.

The parts touch disjoint crates, apart from `bench` in parts 1 and 2
(separate files: `adversary.py`, and a new merge scenario). So two workers
can run at once (WI-2345 with WI-2346 or WI-2347).

## 6. Out of scope

- CI checks on lands, attestation, and phase-2 policies (D22).
- Performance. The adversary measures correctness, and the bench (WI-2294)
  measures speed.
- Byzantine servers. The server is trusted, and the adversary attacks it
  from outside and through its storage.
