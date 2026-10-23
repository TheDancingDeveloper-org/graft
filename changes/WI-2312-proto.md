- graft-proto: interface for conflict resolution without a working copy
  (WI-2312, option (a)). New `WorkspaceApi::get_conflicts` (`GetConflicts` →
  `Conflicts`: per-path base/ours/theirs ids and kinds, merge3 markers capped
  at `MAX_CONFLICT_MARKER_BYTES`, paged) and `WorkspaceApi::resolve`
  (`Resolve` → `Resolved`: one transaction, CAS on `expected_head`,
  deterministic ids, `graft.rebased-from` plus `graft.resolved-paths`, D16 for
  unresolved paths). `ApiError::unimplemented` helper; every implementer has a
  stub returning it. No wire or golden changes; the gRPC mapping is step 2b.
