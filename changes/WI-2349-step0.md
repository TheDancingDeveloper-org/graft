- `Rebase { dry_run: true }` runs the same line merge and reports the same
  result or conflicts, but writes no changeset, object, pin or journal entry
  (D27, design §8.8). It checks path scopes as a real rebase does, so its
  conflicts never name a path the caller may not write. gRPC and `/mcp`
  admit a dry run as a read.
