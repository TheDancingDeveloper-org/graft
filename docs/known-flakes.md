# Known flakes

Quarantined tests (`#[ignore]`d) that fail intermittently under CI load. Each
has a Vogt item to fix it; remove the entry and the `#[ignore]` together.

| Test | Symptom | Item | Quarantined |
| --- | --- | --- | --- |
| `graft-mirror` `tests/export.rs` `a_thousand_random_lands_export_in_order_with_deterministic_shas` | Final poll returns `Held { upstream: Some(..) }` instead of `UpToDate` | WI-2262 | 2026-10-23 |
| `graft-mirror` `tests/export_crash.rs` `kill_9_at_any_point_resumes_with_no_duplicate_and_no_gap` | Fails the merge queue under full-workspace load | WI-2262 | 2026-10-23 |
