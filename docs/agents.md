# Using graft from an agent

graft is a server-centric source control system. A **repository** holds history as changesets on a **line** (the usual one is `main`). A **workspace** is one agent's place to work: a lease on the server plus a stack of **drafts**. A draft is a changeset that is not on the line yet. **Land** rebases that stack onto the line tip and moves the line by compare-and-swap. There is no branch per change and no pull request between agents: concurrent work lands on the same line, and a conflict comes back in the files.

`graft login` and cloning by URL are coming in v0.1.0. Until then, point the client at the server with `GRAFT_ENDPOINT` and a token file.

## Setup

```sh
export GRAFT_ENDPOINT=http://HOST:PORT          # or https://…, or unix:/path
export GRAFT_AUTHOR_NAME="Name" GRAFT_AUTHOR_EMAIL="name@example"
g() { graft --token-file "$HOME/.config/graft/token" "$@"; }
```

The token is a bearer credential. Never print it, never pass it as an argument, never copy it into the checkout or a prompt. `GRAFT_TOKEN` is the other accepted source; a command argument is not.

`graft serve --dev` (a local server with a dev token) is also coming in v0.1.0.

## The loop

1. **Clone.** `g clone REPO DIR --line main` then `cd DIR`. A working copy is also a workspace. `g workspace` shows it; `g workspace renew` extends the lease.
2. **Edit** with ordinary tools. `g status` lists changes; `g diff` shows them. `g status --upstream` asks, without moving anything, how far the line tip is ahead and whether your files would merge or conflict. `g status --drafts` lists other live workspaces with open drafts on those files (needs the `repo:<r>:drafts` scope).
3. **Commit** a draft: `g commit -m "message"`. More edits become another draft, or replace the head with `g commit --amend`. `g stack` lists the drafts; `g stack squash` makes them one. Author defaults to `GRAFT_AUTHOR_NAME` / `GRAFT_AUTHOR_EMAIL`. Kind defaults to `GRAFT_AUTHOR_KIND`, else `agent` when `VOGT_SESSION_ID` is set, else `human`.
4. **Sync** to pick up other lands: `g sync`. Exit 0 means the working copy is on the tip (a stack is rebased). Exit 3 means conflicts; see below.
5. **Land:** `g land` (add `--squash` to land the stack as one changeset). Before it sends anything, land runs a pre-land check: a dry-run rebase that stops on a real conflict, plus a report of overlapping drafts. `--no-check` skips it. The dry run needs only what a land needs. The overlap part needs `repo:<r>:drafts`; without that scope only that part is skipped (`pre-land: draft overlap not checked` on stderr) and the conflict stop still works. The check builds and tests nothing.

When a line is exported to a git host as a batch pull request, a land that arrives while that batch is still open is appended to it as one more commit. Earlier commits keep their identities. A sealed batch (merge already started) takes no more commits; those lands wait for the next batch. graft itself runs no build, test or checker.

## Conflicts (exit 3)

The working copy moves onto the tip with your changes on top and the conflicts written into the files:

- Text files get `<<<<<<<` / `=======` / `>>>>>>>` markers. Edit each file to the result you want and remove every marker.
- Other files get a `<path>.graft-conflict` sidecar. Decide the file's content, then delete the sidecar.
- `g status` lists what is still unresolved. Then `g commit` and `g land`.
- `g land --sync` syncs and writes the conflicts in one step (still exits 3).
- Exit 3 `dirty` means uncommitted local changes block the update. Commit or discard them first.

## Exit codes of `graft land` and `graft sync`

| Exit | Meaning | What to do |
| --- | --- | --- |
| 0 | landed, or already on the tip | done |
| 3 | conflict, or `dirty` | resolve as above, then commit and land |
| 4 | contended (another land won) | run `g land` again; `--wait 120` waits longer |
| 5 | rejected by the line's policy | read the message; do not invent a bypass |
| 6 | not authorized (token or scope) | stop; the token is wrong, expired or missing a scope |
| 7 | unavailable or transport error | retry; `g land` is idempotent. After a few tries, stop |
| 1, 2 | other failure, or usage | read the message; fix the command |

## Reading history

`g log`, `g show REV`, `g cat PATH`, `g ls [DIR]`, `g blame PATH`. Add `--json` for machine output. `g diff A B` diffs two changesets. `g revert REV` adds a draft that undoes a changeset; `g pick REV` adds one that re-applies it.

## What never to do

- Do not `git push`, `git commit` or otherwise use git inside a graft working copy. History moves only through `graft`.
- Do not print, log or paste the token.
- Do not land with `--no-check` to skip a conflict you have not looked at.
- Do not walk around an exit 5 or exit 6. Those need a person or a new token.

## Copy into AGENTS.md or CLAUDE.md

Paste the block below into a repository's agent instructions. Fill in the endpoint and the repository name. The token file path is the one from Setup.

```text
This repo is on graft, not git. Do not run git commit, git push or git pull.

Setup (once per shell):
  export GRAFT_ENDPOINT=http://HOST:PORT
  g() { graft --token-file "$HOME/.config/graft/token" "$@"; }

Loop:
  g clone REPO DIR --line main && cd DIR
  # edit files
  g status            # g diff to see the patch; g status --upstream before landing
  g commit -m "what and why"
  g sync              # exit 3: conflicts, see below
  g land              # exit 0: on the line. --squash lands the stack as one changeset

Conflicts (exit 3 from sync or land): markers <<<<<<< ======= >>>>>>> in text
files; a PATH.graft-conflict sidecar otherwise. Edit to the result, delete
markers and sidecars, then: g commit -m "resolve" && g land
`dirty` on exit 3: commit or discard uncommitted edits first.
Exit 4: run g land again. Exit 6 or 5: stop and report the message.
Exit 7: retry g land a few times, then stop.

Never print the token. Never pass it as an argument.
```

## Checkout-less agents (`/mcp`)

An agent with no working copy talks to the server's `/mcp` endpoint (streamable HTTP) instead of the CLI. The same tools exist over stdio as `graft mcp`, which adds the working-copy tools when started inside a checkout.

Open a workspace (`graft_workspace`), then:

- Read: `graft_log`, `graft_show`, `graft_diff`, `graft_blame`, `graft_read`, `graft_ls`.
- See others: `graft_upstream` (would your paths merge onto the tip?) and `graft_draft_overlap` (other workspaces' drafts on those paths; needs `repo:<r>:drafts`).
- Write: `graft_commit` with inline file content, `graft_sync` to move the base to the tip, `graft_land` to land the stack.
- Conflicts: `graft_conflicts` returns the paths and markers; `graft_resolve` rebases with the contents you supply. `graft_revert` and `graft_pick` add drafts.

Results are capped. A token sent to `/mcp` follows the same rule: it is a credential, not something to print.
