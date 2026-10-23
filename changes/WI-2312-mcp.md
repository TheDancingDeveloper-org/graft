- Conflict resolution over MCP (WI-2312, option (a), step 2c). New tools
  `graft_conflicts` (read-only: per conflicting path the base, ours and
  theirs ids and kinds, `text_mergeable`, a `reason`, and diff3 markers
  clipped to 16 KiB; the failing draft; paged by path with `next`) and
  `graft_resolve` (rebases the workspace stack onto `onto` with per-path
  `put`/`delete` resolutions under `graft_commit`'s inline limits, CAS on
  `expected_head`; returns the new head, resolved and merged paths).
  `graft_revert` is now served. The `conflict` tool error gains an additive
  `sides` list with each path's base, ours and theirs ids. The server's
  `/mcp` wires `GetConflicts` and `Resolve` to the engine with the same
  owner check as gRPC. A pure-MCP agent now completes a conflicted land
  (same-line, modify/delete and binary cases in
  `crates/graft-server/tests/mcp_conflict.rs`).
