- graft-mirror `pr-batch`: a batch PR now carries one commit per land,
  oldest first, on GitHub's current `main` (D26). Each commit has the
  land's author and author time, its message, and `Graft-Changeset`,
  `Graft-Session`, `Graft-Work-Item`, `Graft-Checks` and `Graft-Batch`
  trailers; a dropped land's revert is its own commit. The chain is
  recorded (new `batch_commits` table) before the push. When the replay
  does not reproduce the integration tip, or a batch has more than 500
  lands, the batch is squashed as before and the PR body says why. The
  cut now caps every batch at 500 lands. Once land-pr rebase-merges the
  PR (D28), settling maps each merged commit back to its land by trailer;
  a merge commit maps to the original shas, and a squash-merge maps only
  the batch tip. graft runs no checks (D22).
