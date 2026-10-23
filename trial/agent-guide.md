# Delivering a task through graft (trial)

**This overrides the repository `AGENTS.md` git workflow for the trial.**
Deliver through graft only. No `git push`. No GitHub PRs. No branches. Do not
create `g<phase>-…` or `wi-…` branches, do not run `git commit`, and do not
open or wait for a PR. Your change reaches the trial line `trial-1` only through `graft land`. You
cannot land on `main`; the operator merges the finished batch on GitHub.

Your token is `~/.config/graft/trial-N.token` (N is in your brief). Never print
it, never put it in a command's arguments, never copy it into the checkout.

## Setup (every shell)

```sh
export GRAFT_ENDPOINT=http://graft-dev:17888
g() { graft --token-file "$HOME/.config/graft/trial-N.token" "$@"; }   # N from your brief
```

`graft` is at `~/.local/bin/graft`. If it is missing, run `trial/install-cli.sh`.

## Steps

1. **Clone** into `Active/rt-trial-N` (the N from your brief), and work only there:
   ```sh
   g clone rusttorrent Active/rt-trial-N --line trial-1 && cd Active/rt-trial-N
   ```
   A working copy is also your workspace. `g workspace` shows it, and
   `g workspace renew` extends its lease if you are idle for long.
2. **Edit** the files with your normal tools. `g status` lists your changes;
   `g diff` shows them.
3. **Check** before you commit, with the commands the attester runs. They
   are rustTorrent CI's Rust job without the Tauri crate `rtbit-desktop` and
   with only `default-tls`. Cap parallelism on shared hosts:
   ```sh
   export CARGO_BUILD_JOBS=4
   cargo fmt --all -- --check                                                    # ci/fmt
   cargo clippy --workspace --exclude rtbit-desktop --all-targets \
     --no-default-features --features default-tls -- -D warnings                 # ci/clippy
   cargo test --workspace --exclude rtbit-desktop --locked \
     --no-default-features --features default-tls                                # ci/test
   ```
   For a quick loop, narrow to the crates you touched (`-p <crate>` instead of
   `--workspace --exclude rtbit-desktop`), but run the full commands once
   before you land: a failure there is attested and blocks the land.
4. **Commit** a draft: `g commit -m "<message>" --author-kind agent --meta vogt.work_item=WI-<n>`.
   More edits? Commit again (a stack of drafts), or `g commit --amend`.
   `g stack` lists the drafts; `g stack squash` makes them one.
5. **Sync** to pick up what others landed: `g sync`. Exit 0 means you are on
   the line tip (your stack is rebased). Exit 3 means conflicts, see below.
6. **Land**: `g land` (add `--squash` to land the stack as one changeset).
7. **Wait for the attestation**, see "Land results" for exit 5.

## Conflicts (exit 3 from `g sync` or `g land`)

The working copy moves onto the tip with your changes on top and the
conflicts written into the files:

- Text files get `<<<<<<<` / `=======` / `>>>>>>>` markers (graft's three-way
  line merge, merge3, could not merge them cleanly). Edit each file to the
  result you want and remove every marker.
- Other files (binary, or changed on both sides without text merge) get a
  `<path>.graft-conflict` sidecar. Decide the file's content, then delete the
  sidecar.
- `g status` lists what is still unresolved. Then re-run the checks, `g commit`
  (the message defaults to the stack's) and `g land`.
- `g land --sync` does the sync and writes the conflicts in one step.
- Exit 3 `dirty` means uncommitted local changes block the update: commit or
  revert them first.

## Land results (exit codes of `g land`)

| Exit | Meaning | What to do |
| --- | --- | --- |
| 0 | landed (`landed <id> on trial-1`) | done |
| 3 | conflict, or `dirty` | resolve as above, then commit and land |
| 4 | contended (another land won the race) | run `g land` again; `--wait 120` retries longer |
| 5 | rejected by the line policy | `trial-1` is `attested`: the stack head needs passing `ci/fmt`, `ci/clippy` and `ci/test`. The message names what is missing. Wait for the attester, then run `g land` again. Do not attest checks yourself |
| 6 | not authorized (token expired or missing scope) | stop and report blocked |
| 7 | unavailable / transport error | retry `g land` (it is idempotent); after 3 tries report blocked |
| 1, 2 | other failure / usage error | read the message; fix the command or report blocked |

### Waiting for the attestation

The attester on graft-dev attests the changeset you committed (it runs the
three checks above, one draft at a time, so allow tens of minutes). After your last `g commit`, retry
`g land` about every 60 seconds while it exits 5, for up to 60 minutes (three cargo
checks on a shared VM are slow):

```sh
for i in $(seq 60); do g land; rc=$?; [ $rc -ne 5 ] && break; sleep 60; done; echo "exit $rc"
```

If you amend, squash or re-commit after a sync conflict, the head changes and
the attester must check it again. If a check fails (not merely missing), fix the
code, commit, and land again. If it is still exit 5 after 60 minutes, report
blocked with the last message.

## Reading history

`g log`, `g show <rev>`, `g cat <path>`, `g ls [dir]`, `g blame <path>`.
Add `--json` for machine output. `g revert <rev>` adds a draft that undoes a
changeset.

## Finish

Report the landed changeset id (from `g land`, or `g log --limit 1`), then stop.
Leave the working copy in place; do not delete `Active/rt-trial-N`.
