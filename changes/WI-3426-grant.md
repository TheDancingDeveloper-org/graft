- The mirror service cuts batches with the mirror token alone (WI-3426). On a
  repository bound to a mirror, while a line's batch slot names a cut the
  service has claimed, the mirror kind may create, rebase and revert
  changesets and move that line, fenced by the claimer's lease epoch. Any
  other line, a finished cut, or an unbound repository is refused. The cut's
  line moves now carry the lease epoch.
