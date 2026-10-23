# Runbook: switching a line's authority

This runbook hands one line between **git-primary** (mode A: GitHub's branch
is authoritative and graft follows it) and **graft-primary** (mode B: graft
lands natively and GitHub is an exported mirror). It covers both directions:
A → B for a cutover, and B → A for a rollback. The design is
[docs/design.md §11.3](../design.md#113-the-two-way-mirror-during-migration).

Each step is a `graft-mirror authority` command:

```sh
graft-mirror authority <status|freeze|verify|flip|unfreeze> \
  --config /etc/graft-mirror/<repo>.toml --line main [--last N]
```

`--config` is the mirror's usual config file, the one `follow` and `export`
run with. The token comes from its `token_file` or `GRAFT_TOKEN`, never from
the command line.

## What the tool does and does not do

- It records the switch in the mirror DB, so each step knows the stage it is
  at: `frozen`, `verified`, `flipped`. Every step is idempotent. If one fails
  half way, fix the cause and run it again.
- It sets the **gate** on the upstream branch:
  - With a `[github]` section, the gate is a repository ruleset named
    `graft-mirror-authority-<branch>`. It blocks updates, deletion and
    force-pushes. **Frozen**: nobody bypasses it. **Mirror-only**: only the
    mirror's GitHub App (`[github.github_app] app_id`) bypasses it
    (decision D8). **Open**: the ruleset is deleted, and the repository's own
    rules apply again (the merge queue). The credential needs the
    repository's *Administration: write* permission.
  - Without `[github]`, and with a local bare repository as `upstream`, the
    gate is a `pre-receive` hook plus a `graft-gate` state file in that
    repository. This is the stand-in used by the rehearsal and the tests.
- It moves the mirror DB's authority claim on the line by compare-and-swap.
  `follow` and the PR bridge write only git-primary lines. `export` writes
  only graft-primary lines, and a running follow or export re-reads the claim
  on every poll.
- It never forces a push and never moves a graft ref.
- **It does not change graft's own line policy.** graft-server has no API or
  config key for a line's authority yet. Until it has one, an operator stops
  and starts graft lands on the line by hand (step B1 below). The tool prints
  a reminder at each point where this applies.

## Before you start

- Schedule a window. A freeze blocks pushes or lands on the line until
  unfreeze.
- Check the line: `graft-mirror authority status`. You want `level: yes`, no
  `follow: stopped` or `export: stopped` line, and `open bridge requests: 0`.
  Resolve an export divergence with `graft-mirror reconcile` first.
- Rehearse on a scratch copy first (see [Rehearsal](#rehearsal)).

## A → B (git-primary to graft-primary)

1. **Freeze.** `authority freeze`. The gate closes, and GitHub's `main` takes
   no pushes and no merges. Let the GitHub merge queue drain: entries that
   cannot merge any more fail back to their authors. Keep `graft-mirror
   follow` running.
2. **Catch up.** Wait until `authority status` shows `level: yes`. Follow
   imports what reached GitHub before the freeze.
3. **Verify.** `authority verify --last 100`. It checks:
   - GitHub's branch and graft's line name the same commit.
   - Follow is not stopped, and no PR bridge request on the line is still
     open.
   - The last N changesets of the line re-export to the exact commits GitHub
     holds: the same tree changes, the same parents, the same sha.

   It records the verified pair of tips.
4. **Flip.** `authority flip`. It refuses if either side moved since
   verify. Otherwise the claim moves to graft-primary, export's record is
   reset so export starts level, and the gate becomes mirror-only. Then:
   - Stop `graft-mirror follow` (it already refuses the line) and the PR
     bridge for the line.
   - Set the line to graft-primary in graft's line policy (see "What the
     tool does and does not do").
   - Start `graft-mirror export --config ...`. Its first poll starts from the
     line tip, with nothing to push.
5. **Unfreeze.** `authority unfreeze`. The switch ends. The gate stays
   mirror-only: people and bots now land through graft, and GitHub PRs are
   imported as drafts if `[export] import_pulls = true`.

## B → A (graft-primary to git-primary, the rollback)

1. **Freeze.** `authority freeze`. The gate stays mirror-only, so export can
   push what graft has landed.
   - **B1:** stop graft lands on the line: set it to git-primary in graft's
     line policy, or hold the agents and people who land. A land that slips
     through before flip makes verify or flip refuse. It is never lost: let
     export push it, then verify again.
2. **Catch up.** Keep `graft-mirror export` running until `authority status`
   shows `level: yes`.
3. **Verify.** `authority verify --last 100`. It refuses while export is
   stopped on a divergence.
4. **Flip.** `authority flip`. The claim moves to git-primary, and follow's
   record restarts at GitHub's commit. The gate is now **frozen** to
   everyone, the mirror included: a still-running export stops writing on
   its next poll. Then:
   - Stop `graft-mirror export`.
   - Start `graft-mirror follow` (and the PR bridge, if used).
   - Restore the repository's own GitHub rules if any were removed for the
     cutover (the merge queue ruleset).
5. **Unfreeze.** `authority unfreeze`. The gate's ruleset is deleted, and
   GitHub's `main` is open to the merge queue again. Graft's history is
   already on GitHub, so nothing is lost.

## Aborting

Run `authority unfreeze` before `flip` to abort. The gate goes back to the
old authority's posture, the switch record is dropped, and the claim never
moved. After `flip`, there is no abort. Finish with `unfreeze`, then switch
back the other way.

## When a step refuses

| Message | Meaning | Do |
| --- | --- | --- |
| `no authority holds line …` | Neither follow nor export has run on the line | Start the current authority's service once, then freeze |
| `not level: upstream is at …` | One side is behind | Let follow or export catch up, then verify again |
| `graft's tip … has no git commit yet` | graft landed something export has not pushed | Keep export running, then verify again |
| `follow is stopped on upstream commit …` | Follow saw a non-fast-forward on GitHub | Resolve it (design §11.3, mode A), then verify |
| `… PR bridge request(s) … not final` | Bridged PRs are still in flight | Let them merge or be rejected, then verify |
| `export is stopped on a divergence` | Someone wrote to GitHub out of band | `graft-mirror reconcile`, then verify |
| `… re-exports to …, but upstream holds …` / `does not hold the changes of …` | The mirror map or GitHub's history does not match graft | **Stop. Do not flip.** Keep the old authority (`unfreeze` aborts) and investigate |
| `a side moved since verify` | A push or a land got in after verify | Catch up, then verify again (the switch is back at `frozen`) |
| `not verified: run verify first` | Flip came before verify | Verify |
| `the switch has flipped already` | Flip or freeze came after a flip | Run `unfreeze` to finish |
| `the upstream has a pre-receive hook of its own` | The local stand-in would replace someone's hook | Use a scratch repository |
| `a graft-primary branch is writable by the mirror's GitHub App alone` | `[github]` uses a `token_file` | Configure `[github.github_app]` (D8) |

## Rehearsal

`crates/graft-mirror/scripts/rehearse-authority-switch.sh <git-repo> [dir]`
runs a full A → B → A switch with the real binaries on a scratch copy of the
repository. It uses a local graft-server and a local bare repository
standing in for GitHub. The upstream branch maps to the graft line `trunk`
(`LINE=`), because D6 makes graft's `main` attested and the rehearsal has no
CI to attest. People push, graft lands, the gate refuses the pushes it
should refuse, and at the end:

- the original history is still under the branch, with every person's and
  every native commit on top;
- a full `graft-mirror export --to` of graft's `main` is sha-identical to
  the upstream branch.

Rehearsals are recorded in [docs/trials/](../trials/). A rehearsal against a
real scratch GitHub repository (the ruleset gate) needs the operator to
create that repository and the App installation.
