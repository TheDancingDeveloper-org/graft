- `Rebase { dry_run: true }` runs the same line merge and reports the same
  result or conflicts, but writes no changeset, object, pin or journal entry,
  and needs only `read` (D27, design §8.8). gRPC and `/mcp` admit a dry run
  as a read.
