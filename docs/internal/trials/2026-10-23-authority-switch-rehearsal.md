# Authority switch rehearsal: A → B → A on rustTorrent (2026-10-23)

WI-2266, plan item G3.9. A full git-primary → graft-primary → git-primary
switch, run with the real binaries on a scratch copy of rustTorrent. A local
bare repository stood in for GitHub; no GitHub repository was created or
touched.

**Result: passed.** Nothing was lost, and both sides ended sha-identical.

## Setup

| Item | Value |
|---|---|
| Host | Node B workspace host, Linux 7.0.0-38, git 2.53.0 |
| graft | branch `wi-2266-authority` (off `origin/main` at `19a8e78`), debug build |
| Source | rustTorrent `origin/main` at `ae7fb6e55ec7` (296 commits reachable from `main`), copied into a scratch bare repository |
| "GitHub" | a bare clone of that copy, with `receive.denyNonFastForwards`, gated by the `HookGate` (`pre-receive` hook) |
| Server | `graft-server serve` on `127.0.0.1:17878`, fresh SQLite and object store, one service token (0600 file) |
| Mirror | `graft-mirror follow` / `export` daemons at `poll_interval_secs = 1`; GitHub `main` → graft line `trunk` |
| Command | `crates/graft-mirror/scripts/rehearse-authority-switch.sh $SCRATCH/rt-src.git $SCRATCH/rehearsal` |

The graft line is `trunk`, not `main`. D6 makes graft's `main` `attested`
(`ci/build`, `ci/test`), and the local run has no CI to attest. The first
attempt on `main` stopped at the first native land with
`rejected by attested: … has no passing ci/build attestation`. Every other
line is `direct`. The switch itself does not depend on the land policy.

## What the script did

1. **Mode A.** `follow` imported the 296 commits, which passed the fidelity
   check, and created the line. People pushed 3 commits, and follow imported
   them.
2. **A → B.** `freeze`: a person's push was refused by the gate. `verify
   --last 100`: level, 100 changesets re-exported and identical. `flip`:
   follow was stopped (it already refused the line), and export started.
   `unfreeze`: the gate is mirror-only.
3. **Mode B.** 3 native lands through `graft clone` / `commit` / `land`.
   Export pushed them on its lease. A person's push was refused (mirror-only).
4. **B → A.** `freeze`, then one more land went through, standing in for a
   land in flight at freeze time. Export pushed it, and the line levelled.
   `verify --last 100`, then `flip`: the gate is frozen to everyone, export
   was stopped, and follow started. `unfreeze`: the gate is open.
5. **Mode A again.** People pushed 2 commits on top of graft's exported
   history, and follow imported them as a fast-forward.
6. **Checks.**
   - The original `ae7fb6e5` is an ancestor of the final upstream `main`.
   - Exactly 9 first-parent commits were added: 5 by people and 4 native.
   - `git fsck` on the "GitHub" is clean.
   - A full `graft-mirror export --to` of graft's `trunk` names the same
     sha as upstream's `main` (`784e1cf3…`).
   - A final `authority verify --last 100` passes.

Logs: export pushed 2 times (the 3 mode-B lands in one push, and the
in-flight land in another). Follow moved the line 4 times (the initial
import, the 3 mode-A pushes, and the 2 pushes after the switch back).

## Transcript

Paths are shown as `$SCRATCH`. Full changeset ids and shas are as printed.

```text

== scratch copy of $SCRATCH/rt-src.git (main) in $SCRATCH/rehearsal
upstream main at ae7fb6e55ec78fd87c3a12d316b8b2bebf9fd459 (296 commits)

== graft-server on 127.0.0.1:17878

== mode A (git-primary): follow imports the history
line trunk
  authority: git-primary
  switch:    none
  gate:      open
  upstream:  765603256f936cf4d5c2d983acfa644ff4ceabc2
  graft:     c7c71134391676306d13a362d3c1cdda2f13ec0a7818beab3a04eed533550af48 (765603256f936cf4d5c2d983acfa644ff4ceabc2)
  level:     yes
  open bridge requests: 0

== A -> B: freeze
line trunk: frozen for the switch git-primary -> graft-primary
a person's push was refused, as it should be

== A -> B: verify
line trunk: level at c7c71134391676306d13a362d3c1cdda2f13ec0a7818beab3a04eed533550af48 = 765603256f936cf4d5c2d983acfa644ff4ceabc2; 100 changeset(s) re-exported and identical

== A -> B: flip
line trunk: flipped to graft-primary; run unfreeze to open it
set line trunk to graft-primary in graft's line policy, and run export (not follow or bridge) for the line

== A -> B: unfreeze
line trunk: switch finished; graft-primary holds it
line trunk
  authority: graft-primary
  switch:    none
  gate:      mirror-only
  upstream:  765603256f936cf4d5c2d983acfa644ff4ceabc2
  graft:     c7c71134391676306d13a362d3c1cdda2f13ec0a7818beab3a04eed533550af48 (765603256f936cf4d5c2d983acfa644ff4ceabc2)
  level:     yes
  open bridge requests: 0

== mode B (graft-primary): graft lands, export pushes
a person's push was refused, as it should be
line trunk
  authority: graft-primary
  switch:    none
  gate:      mirror-only
  upstream:  402fffcc6460bce98f90fa00272cd690ed5c4036
  graft:     c7a37c08ca7624bd1ebc414208cd6812dcca8db4f6c331cbb9c175e20e85a34c9 (402fffcc6460bce98f90fa00272cd690ed5c4036)
  level:     yes
  open bridge requests: 0

== B -> A: freeze (one land still in flight)
line trunk: frozen for the switch graft-primary -> git-primary
graft still accepts lands on trunk: stop them on graft's side (see the runbook) before verify

== B -> A: verify
line trunk: level at c8e9506f0e23e336bfe1f15a30707c370345c65bcd6e4770e8d33a8d7a456bf11 = 1a7eb4f6cef5943df8a3d93d873c0825b4118842; 100 changeset(s) re-exported and identical

== B -> A: flip
line trunk: flipped to git-primary; run unfreeze to open it
set line trunk to git-primary in graft's line policy, and run follow (not export) for the line

== B -> A: unfreeze
line trunk: switch finished; git-primary holds it

== mode A again: people push on top of graft's history
line trunk
  authority: git-primary
  switch:    none
  gate:      open
  upstream:  784e1cf3d1ca31ab46bdd0d587e8d586c82c0216
  graft:     ca930e256a3841eed2683386664874ecb0a15208c62e41b299692e6312eaef20c (784e1cf3d1ca31ab46bdd0d587e8d586c82c0216)
  level:     yes
  open bridge requests: 0
line trunk: level at ca930e256a3841eed2683386664874ecb0a15208c62e41b299692e6312eaef20c = 784e1cf3d1ca31ab46bdd0d587e8d586c82c0216; 100 changeset(s) re-exported and identical

== checks
upstream kept all 296 original commits and gained 9 (5 by people, 4 native)
a full export of graft's trunk is sha-identical to upstream: 784e1cf3d1ca31ab46bdd0d587e8d586c82c0216

== REHEARSAL PASSED
exit 0
```

## Not covered

- **The ruleset gate on a real GitHub.** `RulesetGate` is tested against a
  fake API (wiremock) only. A run against a scratch GitHub repository needs
  the operator to create it and install the mirror's App with
  *Administration: write*.
- **graft's own line policy.** graft-server has no API or config key for a
  line's authority, so neither the tool nor this rehearsal set it. Native
  lands were stopped at the B → A freeze by not landing. The runbook gives
  the manual step.
- **The PR bridge and PR import.** The rehearsal ran neither, so no bridge
  request or GitHub PR was in flight at either switch. `verify` refuses while
  a bridge request is open (tested in `tests/authority.rs`).
