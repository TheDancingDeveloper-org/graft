# Migrate a Git repository gradually

Graft can sit beside an existing GitHub repository without changing where the
repository is authoritative. In the recommended setup for this release, Git
remains the authority: maintainers continue to protect and merge the default
branch in GitHub, while `graft-mirror` follows that branch into a graft line.
Work may land on a separate graft integration line and leave graft as a batch
pull request.

The examples below use these placeholders:

- `/srv/git/project.git` is a local bare clone of the GitHub repository.
- `project` is the graft repository name.
- `main` is both the GitHub default branch and its followed graft line.
- `integration` is the graft line that accepts native lands.
- `/etc/graft/mirror.toml` is the mirror configuration file.
- `/run/secrets/graft-mirror-token` is a file containing the graft token.

## Import once

Start with a bare Git repository that has the branches and tags you want to
preserve. Import its default branch into the corresponding graft line:

```sh
graft-mirror import /srv/git/project.git \
  --repo project \
  --server https://graft.example.com \
  --db /var/lib/graft-mirror/project.sqlite \
  --token-file /run/secrets/graft-mirror-token \
  --line refs/heads/main=main
```

Import preserves the reachable commit graph, tags, commit headers, modes, and
the mapping between Git commit SHAs and graft changesets. It performs a
fidelity check for every commit: exporting an imported commit must reproduce
the original SHA. A commit with a path that graft cannot represent, such as a
non-UTF-8 or non-portable path, stops the import and reports the offenders. If
an operator deliberately accepts a known exception, repeat `--allow-lossy
<sha>` for those commits; otherwise do not bypass the fidelity check.

## Follow the GitHub default branch

Create a mirror configuration. Keep credentials in token files, not in this
file or on the command line:

```toml
server = "https://graft.example.com"
repo = "project"
upstream = "https://github.com/example/project.git"
mirror_dir = "/var/lib/graft-mirror/project.git"
db = "/var/lib/graft-mirror/project.sqlite"
token_file = "/run/secrets/graft-mirror-token"
poll_interval_secs = 15

[github]
owner = "example"
repo = "project"
token_file = "/run/secrets/github-token"

[export]
default_author = "Graft Export <graft@example.com>"

[[lines]]
branch = "main"
line = "main"

[[lines]]
branch = "graft/integration"
line = "integration"
```

Run the follower as a long-lived service. It fetches GitHub and advances the
configured graft lines when their Git branches move:

```sh
graft-mirror follow --config /etc/graft/mirror.toml
```

The followed `main` line remains Git-primary. Do not land directly on it;
GitHub remains the place where its protected default branch moves.

## Deliver lands as batch pull requests

Land work on the graft `integration` line. To publish its pending lands, sync
them onto the followed GitHub base and create or update one batch pull request:

```sh
graft-mirror pr-batch integration \
  --base main \
  --config /etc/graft/mirror.toml
```

Run the command again when more lands are ready. Each land is exported as one
commit in the batch. `graft-mirror` creates or updates the pull request but
never merges it. Review and merge that pull request with your normal process;
afterward, `follow` imports the resulting GitHub history and settles the batch.

To avoid many small batches, set cut thresholds in `mirror.toml`:

```toml
[batch]
min_lands = 6          # default 6
min_age_minutes = 60   # default 60
import_size = 256      # optional; replaces a top-level `batch = 256`
```

With the `[batch]` table (or the `--min-lands N` / `--min-age MINUTES`
flags, which override it), `pr-batch` cuts a batch only when at least
`min_lands` lands are waiting, or the previous batch merged or was closed at
least `min_age_minutes` ago, or the line has no previous batch. Otherwise it
opens nothing, exits 0 and prints
`batch deferred: <k> land(s) waiting, next cut at <time> or <N> lands`. The
age counts from the run that saw the previous batch merged or closed. A
release blocker can skip the wait with `--force`; the pull request body then
says the batch was forced. Without the table or flags, `pr-batch` cuts
whenever there is something to batch. A top-level `batch = N` (the import
size) and a `[batch]` table cannot both appear; move the size into
`import_size`.

Graft runs no build, test, check, checker, or CI job when work lands. Required
checks run in the user's forge on the batch pull request, and the user's merge
process decides when the pull request may enter the protected branch.

## Re-land an outside contributor's pull request

A pull request opened on GitHub by someone outside graft cannot be merged
there while the line lands through graft. Re-land it through graft instead,
with the contributor credited as co-author:

```sh
cd <graft working copy of the line>
graft-mirror import-pr <owner>/<repo>#<n> \
  --github-token-file <file> --message-out /tmp/pr.msg
graft diff                       # review the change
graft commit -m "$(cat /tmp/pr.msg)"
graft land
```

`import-pr` reads the pull request through the GitHub API, fetches its head
and base read-only into a scratch repository, and applies its net change
(merge base to head) to the working copy's files. Nothing from the pull
request runs: no hooks, diff drivers or textconv, and a change to a `.git` or
`.graft` path is refused. A closed pull request, or one whose head moves
while it is read, is refused too.

The message is the pull request's title, then one trailer block:

```text
Fix the frobnicator

Source-PR: https://github.com/<owner>/<repo>/pull/<n>
Co-authored-by: <login> <<id>+<login>@users.noreply.github.com>
```

The project identity stays the commit author; the contributor is a
co-author, and GitHub links the noreply address to their account. When the
pull request author's id or login is missing, the import stops rather than
land without credit. The batch commit's `Graft-*` trailers join this block,
so GitHub still parses `Co-authored-by`.

GitHub cannot mark the original pull request merged, because its commits are
re-created. When the batch holding the land merges, `pr-batch` closes the
original pull request with a comment: "Landed on `<base>` as `<sha>` via
#`<batch>`, credited as co-author — thanks!", where `<base>` is the branch the
batch merged into. Only pull requests of the batch's own repository are
closed, and never a graft batch PR; a failure to comment is logged and leaves the pull
request open for you.

## Export the complete repository

A full export is the exit path. It writes every line, tag, and landed
changeset that graft can reach into a bare Git repository:

```sh
graft-mirror export \
  --to /srv/git/project-export.git \
  --repo project \
  --db /var/lib/graft-mirror/project.sqlite \
  --server https://graft.example.com \
  --token-file /run/secrets/graft-mirror-token
```

Keep the mirror database with the graft backup while migration is in progress:
it contains the imported Git headers and SHA mapping used for exact round trips.
The exported bare repository can be checked with ordinary Git tools and pushed
to a forge if you decide to leave graft.

## Advanced: switch authority

Switching a line from Git-primary to graft-primary is an advanced operation and
is not recommended in this release. It changes which system may accept writes,
so schedule it as an operator-controlled migration rather than a routine part
of onboarding.

Inspect the current state, freeze writes, verify recent history, flip the
authority, and unfreeze only after the new authority is ready:

```sh
graft-mirror authority status --config /etc/graft/mirror.toml --line main
graft-mirror authority freeze --config /etc/graft/mirror.toml --line main
graft-mirror authority verify --config /etc/graft/mirror.toml --line main --last 100
graft-mirror authority flip --config /etc/graft/mirror.toml --line main
graft-mirror authority unfreeze --config /etc/graft/mirror.toml --line main
```

After a successful switch, the continuous exporter is the graft-primary bridge
back to GitHub:

```sh
graft-mirror export --config /etc/graft/mirror.toml
```

Do not skip or reorder the authority steps. If verification finds divergence,
leave the line frozen and resolve it explicitly before flipping authority.
