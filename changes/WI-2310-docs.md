# WI-2310: draft overlap decision (docs)

- `docs/decisions.md`: D24, draft overlap visibility: the `repo:<r>:drafts` scope (granted only by `admin`), paths and principals but never contents, head ids or tokens, live leased workspaces only, index-order paging with a 4 × limit scan budget, no data migration.
- `docs/design.md`: `OverlappingDrafts` in §7.2; `attest` and `drafts` scopes in §12.1.
- `docs/notes/draft-overlap.md`: points at D24; duplicate migration sentence removed.
