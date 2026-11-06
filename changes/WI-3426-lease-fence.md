- A claimed batch cut grants the mirror service changeset writes and its
  line move only while the mirror lease is held under the claim's epoch
  (WI-3426). A claim outlived by its lease grants nothing, even before it is
  settled back to pending.
