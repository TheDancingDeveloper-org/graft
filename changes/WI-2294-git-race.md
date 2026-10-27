- bench: the git baseline turns off background gc and accepts any want, so the concurrent-writers step no longer fails with "upload-pack: not our ref".
  The git baseline now runs without automatic gc, so label any comparison with results from before this change.
