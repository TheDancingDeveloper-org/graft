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

1. **No lost or duplicated land.** Every land acknowledged with `Landed` is
   on the line exactly once. A land that returned an error, or a lost ack,
   is on the line at most once. A retry of the same `(head, line)` returns
   the same `Landed` (§7.9).
2. **Lines only move forward.** Each line's sequence of tips forms one
   first-parent chain. Every move is in the journal, in the same
   transaction as its index rows.
3. **CAS only.** No two journal entries move a ref from the same expected
   version. Nothing moves two refs at once.
4. **Conflicts are never silent.** Two drafts that the merge rules (`EAGER`)
   say conflict can't both land unless a resolution draft followed. The
   final content of every path equals the model's.
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

A harness that never fails proves nothing. Behind a test-only cargo
feature, plant bugs one at a time and require the harness to catch each
within the CI seed budget:
- a ref write that skips the CAS;
- a land that writes metadata before its objects;
- an idempotency check that is skipped;
- a lease end compared with `<` instead of `<=`;
- GC that ignores the land-attempt holder.

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
| 1 | `graft-testkit` (`adversary/`: `Faulty<M>`/`Faulty<O>`, scheduler, recorder, checker, planted bugs behind a test-only feature); `bench/graftbench/adversary.py` for the out-of-process soak | `graft-testkit`, `bench` (no engine API change; if one is needed, it goes in its own interface PR first) |
| 2 | `graft-model` tests, plus a `bench` scenario for `cargo check` | `graft-model`, `bench` |
| 3 | `graft-mirror` and `graft-git` integration tests, with generator scripts under `tests/hostile/` | `graft-git`, `graft-mirror` |
| 4 | `graft-server` integration tests, with a canary scanner in `graft-testkit` | `graft-server`, `graft-testkit` |

The parts touch disjoint crates, apart from `graft-testkit` in parts 1 and
4, so two workers can run at once (WI-2345 with WI-2346 or WI-2347).

## 6. Out of scope

- CI checks on lands, attestation, and phase-2 policies (D22).
- Performance. The adversary measures correctness, and the bench (WI-2294)
  measures speed.
- Byzantine servers. The server is trusted, and the adversary attacks it
  from outside and through its storage.
