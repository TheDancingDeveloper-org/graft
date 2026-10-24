# Delivering a task through graft (trial)

**This overrides the repository `AGENTS.md` git workflow for the trial.**
Deliver through graft only. No `git push`. No GitHub PRs. No branches. Do not
create `g<phase>-…` or `wi-…` branches, do not run `git commit`, and do not
open or wait for a PR. Your change reaches the trial line `$LINE` only through `graft land`. You
cannot land on `main`. `graft-mirror pr-batch` exports the line as one GitHub
PR with one commit per land (D26); oversight reviews it and merges it with
`land-pr` (D28).

Your token is `~/.config/graft/trial-N.token` (N is in your brief). Never print
it, never put it in a command's arguments, never copy it into the checkout.

## Setup (every shell)

```sh
export GRAFT_ENDPOINT=http://graft-dev:17888
LINE=${GRAFT_TRIAL_LINE:?the trial line from your brief}
g() { graft --token-file "$HOME/.config/graft/trial-N.token" "$@"; }   # N from your brief
```

`graft` is at `~/.local/bin/graft`. If it is missing, run `trial/install-cli.sh`.

## Steps

1. **Clone** into `Active/rt-trial-N` (the N from your brief), and work only there:
   ```sh
   g clone rusttorrent Active/rt-trial-N --line "$LINE" && cd Active/rt-trial-N
   ```
   A working copy is also your workspace. `g workspace` shows it, and
   `g workspace renew` extends its lease if you are idle for long.
2. **Edit** the files with your normal tools. `g status` lists your changes;
   `g diff` shows them.
3. **Check** before you commit. `$LINE` lands directly (decision D22): no
   check runs when you land, and GitHub CI runs only on the batch PR, so a
   broken land breaks the whole batch. Run rustTorrent CI's Rust job
   yourself, without the Tauri crate `rtbit-desktop` and with only
   `default-tls`. Cap parallelism on shared hosts:
   ```sh
   export CARGO_BUILD_JOBS=4
   cargo fmt --all -- --check                                                    # ci/fmt
   cargo clippy --workspace --exclude rtbit-desktop \
     --no-default-features --features default-tls -- -D warnings                 # ci/clippy
   cargo test --workspace --exclude rtbit-desktop --locked \
     --no-default-features --features default-tls                                # ci/test
   ```
   For a quick loop, narrow to the crates you touched (`-p <crate>` instead of
   `--workspace --exclude rtbit-desktop`), but run the full commands once
   before you land.
4. **Commit** a draft: `g commit -m "<message>" --author-kind agent --meta vogt.work_item=WI-<n>`.
   With `VOGT_SESSION_ID` and `VOGT_WORK_ITEM` set (Vogt sets both), the
   draft records your session and work item and the author kind defaults to
   `agent` (WI-2337); the batch PR's commit for your land carries them as
   `Graft-Session` and `Graft-Work-Item` trailers.
   More edits? Commit again (a stack of drafts), or `g commit --amend`.
   `g stack` lists the drafts; `g stack squash` makes them one.
5. **Sync** to pick up what others landed: `g sync`. Exit 0 means you are on
   the line tip (your stack is rebased). Exit 3 means conflicts, see below.
6. **Land**: `g land` (add `--squash` to land the stack as one changeset).
   Landing is immediate: there is no attestation to wait for. `g land` first
   runs the pre-land check (D27): a dry-run rebase that stops the land on a
   real conflict before anything is sent; `--no-check` skips it. Exit 0
   means your change is on `$LINE`.

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
| 0 | landed (`landed <id> on $LINE`) | done |
| 3 | conflict, or `dirty` | resolve as above, then commit and land |
| 4 | contended (another land won the race) | run `g land` again; `--wait 120` retries longer |
| 5 | rejected by the line policy | not expected: `$LINE` is `direct`. Report blocked with the message. Do not attest checks yourself |
| 6 | not authorized (token expired or missing scope) | stop and report blocked |
| 7 | unavailable / transport error | retry `g land` (it is idempotent); after 3 tries report blocked |
| 1, 2 | other failure / usage error | read the message; fix the command or report blocked |

## Reading history

`g log`, `g show <rev>`, `g cat <path>`, `g ls [dir]`, `g blame <path>`.
Add `--json` for machine output. `g revert <rev>` adds a draft that undoes a
changeset.

## Finish

Report the landed changeset id (from `g land`, or `g log --limit 1`), then stop.
Leave the working copy in place; do not delete `Active/rt-trial-N`.
