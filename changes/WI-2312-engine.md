- Conflict resolution without a working copy (WI-2312, option (a), step 2b).
  graft-engine implements `GetConflicts` (plans the stack's sync onto the
  line tip, or `onto`, in memory and returns, for the first draft that
  conflicts, each path's base/ours/theirs entries and merge3 markers capped
  at `MAX_CONFLICT_MARKER_BYTES`; binary, oversized, kind-changing and
  add/delete paths get ids and a reason; paged by path) and `Resolve` (the
  sync with caller resolutions for conflicting paths, the D16 line merge for
  the rest, `Conflict` if anything still conflicts, a CAS on
  `expected_head`, deterministic ids, idempotent retry). Rebased drafts keep
  `graft.rebased-from`; the draft that took resolutions records the new
  `graft.resolved-paths` (`graft_engine::RESOLVED_PATHS`). New gRPC
  `WorkspaceService.GetConflicts` and `.Resolve` (additive messages),
  served by graft-server, forwarded by graft-node, and implemented by
  graft-client's `Remote` and the testkit server.
