# How this project lands changes

graft is developed with graft. This page records the maintainers' own
delivery process. Outside contributors do not need it: send a pull request as
described in [CONTRIBUTING.md](../../CONTRIBUTING.md).

- **Trunk-based.** `main` is the only long-lived branch. Releases are `v*` tags
  on `main`.
- **Graft lines.** Maintainers and automation land changesets on the graft
  line `backlog-2` with `graft land`. (`backlog-1` is retired: a mistaken
  commit made its tip unsafe to batch, and graft cannot reset a line, so
  `backlog-2` was started from `main` instead. Never land on or batch
  `backlog-1`.)
- **Batch pull requests.** Landed work is delivered to GitHub as one batch
  pull request per backlog, cut with `graft repo batch cut graft --line
  backlog-2`. The mirror service claims the request and opens the pull
  request; each land keeps its own commit (D28). Do not run
  `graft-mirror pr-batch`, which is retired.
- **Review.** Every batch is reviewed before it merges. A reviewer other than
  the author reviews work an agent wrote. Blockers go back to the author.
- **Merge queue.** The queue rebases on the latest `main`, runs the required
  checks (`fmt`, `clippy`, `test`; see
  [.github/workflows/ci.yml](../../.github/workflows/ci.yml)) and merges.
  Pushes to `main` do not re-run CI the queue already passed.
- **Versions.** Versions are bumped in the release pull request, not in
  feature pull requests.
