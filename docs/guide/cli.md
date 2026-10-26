<!-- generated from `graft --help`; do not edit. Regenerate with GRAFT_UPDATE_DOCS=1. -->

# Command reference

Generated from `graft --help`. Hidden commands are left out.

## Index

- [`graft login`](#login)
- [`graft logout`](#logout)
- [`graft config`](#config)
- [`graft config show`](#config-show)
- [`graft doctor`](#doctor)
- [`graft init`](#init)
- [`graft clone`](#clone)
- [`graft status`](#status)
- [`graft diff`](#diff)
- [`graft log`](#log)
- [`graft show`](#show)
- [`graft blame`](#blame)
- [`graft cat`](#cat)
- [`graft ls`](#ls)
- [`graft sparse`](#sparse)
- [`graft sparse list`](#sparse-list)
- [`graft sparse add`](#sparse-add)
- [`graft sparse remove`](#sparse-remove)
- [`graft commit`](#commit)
- [`graft workspace`](#workspace)
- [`graft workspace show`](#workspace-show)
- [`graft workspace list`](#workspace-list)
- [`graft workspace open`](#workspace-open)
- [`graft workspace renew`](#workspace-renew)
- [`graft workspace close`](#workspace-close)
- [`graft line`](#line)
- [`graft line create`](#line-create)
- [`graft line merge`](#line-merge)
- [`graft line merges`](#line-merges)
- [`graft line list`](#line-list)
- [`graft tag`](#tag)
- [`graft tag create`](#tag-create)
- [`graft tag list`](#tag-list)
- [`graft stack`](#stack)
- [`graft stack list`](#stack-list)
- [`graft stack squash`](#stack-squash)
- [`graft stack reorder`](#stack-reorder)
- [`graft stack split`](#stack-split)
- [`graft stack drop`](#stack-drop)
- [`graft sync`](#sync)
- [`graft land`](#land)
- [`graft revert`](#revert)
- [`graft pick`](#pick)
- [`graft api`](#api)
- [`graft version`](#version)
- [`graft mcp`](#mcp)
- [`graft lsp`](#lsp)
- [`graft completions`](#completions)

## `graft login`

```text
Store credentials for a named server, or list configured servers

Usage: graft login [OPTIONS] [NAME]

Arguments:
  [NAME]  Server name to store

Options:
      --json               Print stable JSON; its schema is published with the release
      --list               List configured servers without changing them
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft logout`

```text
Remove a named server and its stored token

Usage: graft logout [OPTIONS] <NAME>

Arguments:
  <NAME>  Server name to remove

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft config`

```text
Inspect user configuration

Usage: graft config [OPTIONS] <COMMAND>

Commands:
  show  Print named servers and the effective selection; tokens are redacted
  help  Print this message or the help of the given subcommand(s)

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft config show`

```text
Print named servers and the effective selection; tokens are redacted

Usage: graft config show [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft doctor`

```text
Check config, server, endpoint, protocol and token

Exit status:

* 0  every check passes * 1  any check fails or is skipped * 2  a usage error, or the working copy cannot be read, before the checks run

Usage: graft doctor [OPTIONS]

Options:
      --json
          Print stable JSON; its schema is published with the release

      --endpoint <URL>
          Where to send requests: `http://host:port`, `https://…`, or `unix:/path`

      --repo <NAME>
          Repository name (default: the working copy's)

      --token-file <PATH>
          File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument

      --server <NAME>
          Named server in the user config. Its endpoint and token file are used; the token is never an argument

  -C, --directory <DIR>
          Run as if started in this directory, like `git -C`

  -h, --help
          Print help (see a summary with '-h')
```

## `graft init`

```text
Create a local repository and working copy

Usage: graft init [OPTIONS] [DIR]

Arguments:
  [DIR]  Directory to create (default: the current directory)

Options:
      --json               Print stable JSON; its schema is published with the release
      --repo <NAME>        Repository name (default: the directory name)
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft clone`

```text
Open a workspace and lay out a sparse working copy

Usage: graft clone [OPTIONS] <REPO> [DIR]

Arguments:
  <REPO>  Repository to clone
  [DIR]   Directory to create (default: the repository name's last segment)

Options:
      --json               Print stable JSON; its schema is published with the release
      --line <LINE>        Line whose tip to clone, unless `--at` is given [default: main]
      --at <REV>           Snapshot to clone: a changeset id or a ref name
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --path <PREFIX>      Include only this path prefix (repeatable; default: the whole snapshot)
      --root-files         With `--path`, also include the files at the repository root
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft status`

```text
Working-copy changes against the head draft, else the base (no server needed)

With --upstream it also asks the server, without moving anything, how far the line tip is ahead of the base, which files changed there, and whether the files you changed (uncommitted edits and the stack) would merge onto the tip or conflict, with the conflicting lines.

With --drafts it asks which other agents' live workspaces have open drafts on the files you changed (or on PATHS), before anyone lands. That needs the repo:<r>:drafts scope.

Usage: graft status [OPTIONS] [PATHS]...

Arguments:
  [PATHS]...
          With --drafts: ask about these files or directories instead

Options:
      --json
          Print stable JSON; its schema is published with the release

      --upstream
          Also compare with the line tip (read-only; contacts the server)

      --endpoint <URL>
          Where to send requests: `http://host:port`, `https://…`, or `unix:/path`

      --limit <LIMIT>
          With --upstream: the most entries in each list
          
          [default: 100]

      --cursor <TOKEN>
          With --upstream: resume token for the line's changed files

      --repo <NAME>
          Repository name (default: the working copy's)

      --drafts
          Also list other live workspaces with open drafts on your changed files (read-only; contacts the server)

      --token-file <PATH>
          File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument

      --ignored
          Also list the untracked entries that `.gitignore` and `.graftignore` ignore (an ignored directory is one entry)

      --server <NAME>
          Named server in the user config. Its endpoint and token file are used; the token is never an argument

  -C, --directory <DIR>
          Run as if started in this directory, like `git -C`

  -h, --help
          Print help (see a summary with '-h')
```

## `graft diff`

```text
Diff the working copy, or two changesets

Usage: graft diff [OPTIONS] [FROM] [TO]

Arguments:
  [FROM]  The earlier changeset or ref
  [TO]    The later changeset or ref. With neither, diff the working copy

Options:
      --json               Print stable JSON; its schema is published with the release
      --path <PATH>        Restrict to this path or directory (repeatable)
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --limit <LIMIT>      Files per page [default: 100]
      --cursor <TOKEN>     Resume token from a previous page
      --repo <NAME>        Repository name (default: the working copy's)
      --stat               Per-file added and removed line counts, and a summary
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft log`

```text
First-parent history

Usage: graft log [OPTIONS]

Options:
      --at <REV>           Start here: a changeset id or ref (default: the working copy's base, else `main`)
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --path <PATH>        Only changesets that touch this path or directory (repeatable)
      --limit <LIMIT>      Changesets per page [default: 50]
      --repo <NAME>        Repository name (default: the working copy's)
      --cursor <TOKEN>     Resume token from a previous page
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft show`

```text
A changeset with its patch

Usage: graft show [OPTIONS] <REV>

Arguments:
  <REV>  The changeset id or ref

Options:
      --json               Print stable JSON; its schema is published with the release
      --limit <LIMIT>      Patch files per page [default: 100]
      --cursor <TOKEN>     Resume token from a previous page of files
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft blame`

```text
Line origins for a file

Usage: graft blame [OPTIONS] <PATH>

Arguments:
  <PATH>  The file

Options:
      --at <REV>           The snapshot (default: the working copy's base, else `main`)
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --lines <START:END>  Only lines `START:END` (0-based, END exclusive)
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft cat`

```text
Print a file at a changeset

Usage: graft cat [OPTIONS] <PATH>

Arguments:
  <PATH>  The file

Options:
      --at <REV>           The snapshot (default: the working copy's base, else `main`)
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft ls`

```text
List a directory at a changeset

Usage: graft ls [OPTIONS] [PATH]

Arguments:
  [PATH]  The directory (default: the root)

Options:
      --at <REV>           The snapshot (default: the working copy's base, else `main`)
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
  -R, --recursive          List every descendant, not only the direct children
      --limit <LIMIT>      Entries per page [default: 100]
      --repo <NAME>        Repository name (default: the working copy's)
      --cursor <TOKEN>     Resume token from a previous page
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft sparse`

```text
List, add or remove the slice's path prefixes

Usage: graft sparse [OPTIONS] [COMMAND]

Commands:
  list    List the prefixes the working copy holds
  add     Add prefixes and fetch them
  remove  Remove prefixes; their files stay on disk, untouched
  help    Print this message or the help of the given subcommand(s)

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft sparse list`

```text
List the prefixes the working copy holds

Usage: graft sparse list [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft sparse add`

```text
Add prefixes and fetch them

Usage: graft sparse add [OPTIONS] <PREFIXES>...

Arguments:
  <PREFIXES>...  Path prefixes to add

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft sparse remove`

```text
Remove prefixes; their files stay on disk, untouched

Usage: graft sparse remove [OPTIONS] <PREFIXES>...

Arguments:
  <PREFIXES>...  Path prefixes to remove

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft commit`

```text
Upload the slice's changes and create a draft on the server

Usage: graft commit [OPTIONS]

Options:
      --json                Print stable JSON; its schema is published with the release
  -m, --message <MESSAGE>   The message (required, unless --amend keeps the old one)
      --amend               Replace the head draft instead of adding one; the old head is superseded
      --endpoint <URL>      Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --author <AUTHOR>     The author as `Name <email>` (default: GRAFT_AUTHOR_NAME and GRAFT_AUTHOR_EMAIL, else $USER with no email)
      --repo <NAME>         Repository name (default: the working copy's)
      --author-kind <KIND>  What kind of author this is: human, agent, bot or ci (default: GRAFT_AUTHOR_KIND, else human)
      --token-file <PATH>   File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --meta <KEY=VALUE>    Metadata `KEY=VALUE` for tools (repeatable)
      --server <NAME>       Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>     Run as if started in this directory, like `git -C`
  -h, --help                Print help
```

## `graft workspace`

```text
Show, list, open, renew or close workspaces

Usage: graft workspace [OPTIONS] [COMMAND]

Commands:
  show   Show a workspace (default: the working copy's)
  list   List your workspaces
  open   Open a workspace: for the working copy at its base, or on a line's tip
  renew  Renew a workspace's lease
  close  Close a workspace; its drafts become abandoned
  help   Print this message or the help of the given subcommand(s)

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft workspace show`

```text
Show a workspace (default: the working copy's)

Usage: graft workspace show [OPTIONS] [NAME]

Arguments:
  [NAME]  The workspace

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft workspace list`

```text
List your workspaces

Usage: graft workspace list [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --limit <LIMIT>      Workspaces per page [default: 100]
      --cursor <TOKEN>     Resume token from a previous page
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft workspace open`

```text
Open a workspace: for the working copy at its base, or on a line's tip

Usage: graft workspace open [OPTIONS] [NAME]

Arguments:
  [NAME]  The name (default: one the server picks)

Options:
      --json               Print stable JSON; its schema is published with the release
      --line <LINE>        Outside a working copy, the line to track [default: main]
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft workspace renew`

```text
Renew a workspace's lease

Usage: graft workspace renew [OPTIONS] [NAME]

Arguments:
  [NAME]  The workspace (default: the working copy's)

Options:
      --json               Print stable JSON; its schema is published with the release
      --lease <SECONDS>    Lease length in seconds (default: 2 hours)
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft workspace close`

```text
Close a workspace; its drafts become abandoned

Usage: graft workspace close [OPTIONS] [NAME]

Arguments:
  [NAME]  The workspace (default: the working copy's)

Options:
      --json               Print stable JSON; its schema is published with the release
      --keep-drafts        Ask the server to keep the drafts
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft line`

```text
Create or list lines

Usage: graft line [OPTIONS] <COMMAND>

Commands:
  create  Fork a new line at a changeset; an existing line is never overwritten
  merge   Merge another line into a line; only the target moves
  merges  List a line's merge points, newest first
  list    List the lines, in name order
  help    Print this message or the help of the given subcommand(s)

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft line create`

```text
Fork a new line at a changeset; an existing line is never overwritten

Usage: graft line create [OPTIONS] --from <REV> <NAME>

Arguments:
  <NAME>  The new line's name

Options:
      --from <REV>         Where it starts: a changeset id or ref
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft line merge`

```text
Merge another line into a line; only the target moves

Usage: graft line merge [OPTIONS] <SRC>

Arguments:
  <SRC>
          The source line, only read

Options:
      --into <LINE>
          The target line (default: the working copy's line)

      --json
          Print stable JSON; its schema is published with the release

      --at <REV>
          The source changeset to merge: an id or ref on the source line (default: its tip)

      --endpoint <URL>
          Where to send requests: `http://host:port`, `https://…`, or `unix:/path`

  -m, --message <MSG>
          Message of the merge landing

      --repo <NAME>
          Repository name (default: the working copy's)

      --merge <MODE>
          How a file both sides changed is merged: `line` or `file`

          Possible values:
          - line: Three-way line merge for text files under 1 MiB
          - file: Any file changed on both sides conflicts
          
          [default: line]

      --token-file <PATH>
          File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument

      --server <NAME>
          Named server in the user config. Its endpoint and token file are used; the token is never an argument

      --sync
          On a conflict, sync this working copy (on the target line) to the tip and write the merge into the files

  -C, --directory <DIR>
          Run as if started in this directory, like `git -C`

  -h, --help
          Print help (see a summary with '-h')
```

## `graft line merges`

```text
List a line's merge points, newest first

Usage: graft line merges [OPTIONS] [DST]

Arguments:
  [DST]  The target line (default: the working copy's line)

Options:
      --from <LINE>        Only merges from this line
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --limit <LIMIT>      Merge points per page [default: 100]
      --cursor <TOKEN>     Resume token from a previous page
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft line list`

```text
List the lines, in name order

Usage: graft line list [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --prefix <PREFIX>    Only lines whose name starts with this
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --limit <LIMIT>      Lines per page [default: 100]
      --cursor <TOKEN>     Resume token from a previous page
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft tag`

```text
Create or list immutable tags

Usage: graft tag [OPTIONS] <COMMAND>

Commands:
  create  Tag a changeset as `tags/<name>`; a tag never moves and is never re-created
  list    List the tags, in name order
  help    Print this message or the help of the given subcommand(s)

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft tag create`

```text
Tag a changeset as `tags/<name>`; a tag never moves and is never re-created

Usage: graft tag create [OPTIONS] <NAME>

Arguments:
  <NAME>  The tag's name (`tags/` is added when missing)

Options:
      --at <REV>           What to tag: a changeset id or ref [default: main]
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft tag list`

```text
List the tags, in name order

Usage: graft tag list [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --prefix <PREFIX>    Only tags whose name (after `tags/`) starts with this
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --limit <LIMIT>      Refs per page [default: 100]
      --cursor <TOKEN>     Resume token from a previous page
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft stack`

```text
List, squash, reorder, split or drop the drafts between the base and the head

Usage: graft stack [OPTIONS] [COMMAND]

Commands:
  list     List the drafts, oldest first
  squash   Squash adjacent drafts into one (default: the whole stack)
  reorder  Put the drafts in a new order, oldest first
  split    Split a draft into one draft per part; paths no part names stay in a last draft
  drop     Drop a draft from the stack
  help     Print this message or the help of the given subcommand(s)

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft stack list`

```text
List the drafts, oldest first

Usage: graft stack list [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft stack squash`

```text
Squash adjacent drafts into one (default: the whole stack)

Usage: graft stack squash [OPTIONS] [DRAFTS]...

Arguments:
  [DRAFTS]...  The drafts, by id or unique id prefix

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft stack reorder`

```text
Put the drafts in a new order, oldest first

Usage: graft stack reorder [OPTIONS] <DRAFTS>...

Arguments:
  <DRAFTS>...  Every draft, by id or unique id prefix, oldest first

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft stack split`

```text
Split a draft into one draft per part; paths no part names stay in a last draft

Usage: graft stack split [OPTIONS] --part <PATHS> <DRAFT>

Arguments:
  <DRAFT>  The draft, by id or unique id prefix

Options:
      --json               Print stable JSON; its schema is published with the release
      --part <PATHS>       The paths (or directories) of one part, comma-separated (repeatable)
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft stack drop`

```text
Drop a draft from the stack

Usage: graft stack drop [OPTIONS] <DRAFT>

Arguments:
  <DRAFT>  The draft, by id or unique id prefix

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft sync`

```text
Move the base to the line tip (the server rebases a stack; conflicts are written into the files)

Usage: graft sync [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft land`

```text
Land the stack on the line

Usage: graft land [OPTIONS]

Options:
      --json
          Print stable JSON; its schema is published with the release

      --wait <SECONDS>
          Wait up to this many seconds for a contended land (default: 60)

      --endpoint <URL>
          Where to send requests: `http://host:port`, `https://…`, or `unix:/path`

      --squash
          Land the stack as one changeset

      --repo <NAME>
          Repository name (default: the working copy's)

      --sync
          On a conflict, sync to the tip and write the conflicts into the files (still exits 3)

      --merge <MODE>
          How the server's rebase treats a file both sides changed: `line` merges text files line by line, `file` refuses any file changed on both sides

          Possible values:
          - line: Three-way line merge for text files under 1 MiB
          - file: Any file changed on both sides conflicts
          
          [default: line]

      --token-file <PATH>
          File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument

      --no-check
          Skip the pre-land check (the dry-run rebase that stops a land on a real conflict, and the draft-overlap report)

      --server <NAME>
          Named server in the user config. Its endpoint and token file are used; the token is never an argument

  -C, --directory <DIR>
          Run as if started in this directory, like `git -C`

      --merged-from <LINE@REV>
          Land a merge resolution: record a merge point from `<line>@<rev>`

  -h, --help
          Print help (see a summary with '-h')
```

## `graft revert`

```text
Add a draft that inverts a changeset

Usage: graft revert [OPTIONS] <REV>

Arguments:
  <REV>  The changeset to invert: an id or a ref

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft pick`

```text
Add a draft that re-applies a landed changeset (a cherry-pick; conflicts are written into the files)

Usage: graft pick [OPTIONS] <REV>

Arguments:
  <REV>
          The landed changeset to re-apply: an id or a ref

Options:
      --json
          Print stable JSON; its schema is published with the release

      --merge <MODE>
          How a file both sides changed is merged

          Possible values:
          - line: Three-way line merge for text files under 1 MiB
          - file: Any file changed on both sides conflicts
          
          [default: line]

      --endpoint <URL>
          Where to send requests: `http://host:port`, `https://…`, or `unix:/path`

      --repo <NAME>
          Repository name (default: the working copy's)

      --token-file <PATH>
          File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument

      --server <NAME>
          Named server in the user config. Its endpoint and token file are used; the token is never an argument

  -C, --directory <DIR>
          Run as if started in this directory, like `git -C`

  -h, --help
          Print help (see a summary with '-h')
```

## `graft api`

```text
Call a read operation of the API, with JSON in and out

Usage: graft api [OPTIONS] [OP]

Arguments:
  [OP]  The operation, as named by `--list` (for example `list_tree`)

Options:
      --json               Print stable JSON; its schema is published with the release
      --list               List every operation and how the CLI surfaces it
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --input <FILE>       JSON request from a file, or `-` for standard input (default: standard input, when it is not a terminal)
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft version`

```text
Print versions

Usage: graft version [OPTIONS]

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft mcp`

```text
Run the MCP server over stdio, for agents

Serves graft's MCP tools against the remote API, at the endpoint the other commands use. Inside a working copy it adds the working-copy tools: graft_status, graft_commit with from_working_copy, and graft_land and graft_sync that move the working copy. Standard output is the MCP channel; messages go to standard error. The token comes from the working copy's named server, `--server`, `--token-file`, or `GRAFT_TOKEN`, and is never printed.

Usage: graft mcp [OPTIONS]

Options:
      --json
          Print stable JSON; its schema is published with the release

      --print-config <CLIENT>
          Print the registration snippet for this client and exit. Never prints a token

          Possible values:
          - claude: `claude mcp add`
          - codex:  A Codex `config.toml` block
          - cursor: A Cursor `.cursor/mcp.json` block

      --endpoint <URL>
          Where to send requests: `http://host:port`, `https://…`, or `unix:/path`

      --repo <NAME>
          Repository name (default: the working copy's)

      --token-file <PATH>
          File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument

      --server <NAME>
          Named server in the user config. Its endpoint and token file are used; the token is never an argument

  -C, --directory <DIR>
          Run as if started in this directory, like `git -C`

  -h, --help
          Print help (see a summary with '-h')
```

## `graft lsp`

```text
Run the LSP server over stdio

Usage: graft lsp [OPTIONS] [ARGS]...

Arguments:
  [ARGS]...  Arguments passed to the LSP server

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

## `graft completions`

```text
Print a shell completion script

Usage: graft completions [OPTIONS] <SHELL>

Arguments:
  <SHELL>  Shell to generate completions for [possible values: bash, elvish, fish, powershell, zsh]

Options:
      --json               Print stable JSON; its schema is published with the release
      --endpoint <URL>     Where to send requests: `http://host:port`, `https://…`, or `unix:/path`
      --repo <NAME>        Repository name (default: the working copy's)
      --token-file <PATH>  File holding the bearer token. `GRAFT_TOKEN` is the other source. A token is never taken from an argument
      --server <NAME>      Named server in the user config. Its endpoint and token file are used; the token is never an argument
  -C, --directory <DIR>    Run as if started in this directory, like `git -C`
  -h, --help               Print help
```

