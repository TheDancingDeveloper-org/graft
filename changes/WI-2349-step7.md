- `graft land` and the MCP `graft_land` tool run the pre-land check first
  (design §8.8, D27): a dry-run rebase of the stack onto the tip
  (`Rebase { dry_run: true }`) and the draft-overlap read. A real conflict
  stops the land with exit 3 (MCP: a `conflict` error) and its paths,
  sending nothing; `graft land --sync` still sends it so the conflicts are
  written into the files. Files that merge cleanly and other workspaces'
  drafts are only reported. Without the `repo:<r>:drafts` scope, or against
  a server that does not serve a read, that part is skipped with a note.
  `--no-check` (MCP `check: false`) skips it all. Both clients share
  `graft_mcp::preflight` and return the same `preflight` object, on success
  and on a failed land. Advisory only: the server's land still decides, and
  no build or test runs (D22).
