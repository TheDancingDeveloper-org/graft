# Real-PR replay: 2026-10-24-real-prs

Started 2026-10-24T03:07:07+00:00, 443.9 s. graft graft 0.0.0 (protocol 0.1), git version 2.53.0.
Oracle: `cargo check --workspace --all-targets`, CARGO_BUILD_JOBS=4.

| case | mode | PRs | graft clean | graft conflicts | git clean | git conflicts | graft broken | git broken | graft rt | git rt |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| rdpapp-split | sequence | 9 | 1 | 8 | 1 | 8 | 0 | 0 | 17 | 19 |
| rdpapp-stack | stack | 11 | 0 | 1 | 0 | 1 | 0 | 0 | 2 | 2 |
| fluent2-registration | position | 34 | 18 | 16 | 18 | 16 | 0 | 0 | 50 | 64 |
| rusttorrent-qbit | position | 4 | 0 | 4 | 0 | 4 | 0 | 0 | 8 | 8 |
| graft-merge-back | position | 57 | 39 | 18 | 39 | 18 | 1 | 1 | 75 | 153 |

## rdpapp-split (sequence)

rdpapp `main.rs` split: the open PRs among #38-#50 that move gateway handlers out of main.rs.

9 PRs replayed, 0 skipped; graft import 12.2 s.

| | graft | git |
| --- | ---: | ---: |
| lands attempted | 9 | 9 |
| fast-forward (no merge needed) | 0 | 0 |
| merged clean | 1 | 1 |
| conflicts | 8 | 8 |
| clean but broken (`cargo check`) | 0 | 0 |
| merged, check passed | 1 | 1 |
| merged, check failed but an input failed too | 0 | 0 |
| errors | 0 | 0 |
| rebases | 9 | 9 |
| client round trips | 17 | 19 |
| integration wall time (s) | 1.849 | 2.295 |
| p50 per land (ms) | 195.8 | 223.7 |

Per PR: both clean 1 (same tree 1, different 0), graft only 0, git only 0, both conflict 8.

| PR | tool | conflicted paths |
| --- | --- | --- |
| 39 | graft | crates/rdpapp-gateway/src/main.rs |
| 39 | git | crates/rdpapp-gateway/src/main.rs |
| 40 | graft | crates/rdpapp-gateway/src/main.rs |
| 40 | git | crates/rdpapp-gateway/src/main.rs |
| 41 | graft | crates/rdpapp-gateway/src/main.rs |
| 41 | git | crates/rdpapp-gateway/src/main.rs |
| 46 | graft | crates/rdpapp-gateway/src/main.rs |
| 46 | git | crates/rdpapp-gateway/src/main.rs |
| 47 | graft | crates/rdpapp-gateway/src/main.rs |
| 47 | git | crates/rdpapp-gateway/src/main.rs |
| 48 | graft | crates/rdpapp-gateway/src/main.rs |
| 48 | git | crates/rdpapp-gateway/src/main.rs |
| 49 | graft | crates/rdpapp-gateway/src/main.rs |
| 49 | git | crates/rdpapp-gateway/src/main.rs |
| 50 | graft | crates/rdpapp-gateway/src/main.rs |
| 50 | git | crates/rdpapp-gateway/src/main.rs |

## rdpapp-stack (stack)

rdpapp 11-PR stack #28 -> #51 -> ... -> #59, plus #60 (merged into #59).

11 PRs replayed, 0 skipped; graft import 5.5 s.

| | graft | git |
| --- | ---: | ---: |
| lands attempted | 1 | 1 |
| fast-forward (no merge needed) | 0 | 0 |
| merged clean | 0 | 0 |
| conflicts | 1 | 1 |
| clean but broken (`cargo check`) | 0 | 0 |
| merged, check passed | 0 | 0 |
| merged, check failed but an input failed too | 0 | 0 |
| errors | 0 | 0 |
| rebases | 1 | 1 |
| client round trips | 2 | 2 |
| integration wall time (s) | 0.091 | 0.169 |
| p50 per land (ms) | 91.2 | 169.3 |

| PR | tool | conflicted paths |
| --- | --- | --- |
| stack | graft | plan.md |
| 28 | git | plan.md |

## fluent2-registration (position)

fluent2-egui PRs touching CHANGELOG.md or gallery/src/pages/mod.rs while the queue-ejected #58, #60, #64 and #87 were open.

34 PRs replayed, 1 skipped; graft import 2.0 s.

| | graft | git |
| --- | ---: | ---: |
| lands attempted | 34 | 34 |
| fast-forward (no merge needed) | 11 | 11 |
| merged clean | 7 | 7 |
| conflicts | 16 | 16 |
| clean but broken (`cargo check`) | 0 | 0 |
| merged, check passed | 7 | 7 |
| merged, check failed but an input failed too | 0 | 0 |
| errors | 0 | 0 |
| rebases | 23 | 23 |
| client round trips | 50 | 64 |
| integration wall time (s) | 1.77 | 1.959 |
| p50 per land (ms) | 50.0 | 58.3 |

Per PR: both clean 18 (same tree 18, different 0), graft only 0, git only 0, both conflict 16.

| PR | tool | conflicted paths |
| --- | --- | --- |
| 56 | graft | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 56 | git | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 58 | graft | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 58 | git | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 55 | graft | CHANGELOG.md |
| 55 | git | CHANGELOG.md |
| 57 | graft | CHANGELOG.md, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/src/surfaces/mod.rs, crates/fluent2-egui/src/surfaces/tab_strip.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 57 | git | CHANGELOG.md, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/src/surfaces/mod.rs, crates/fluent2-egui/src/surfaces/tab_strip.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 60 | graft | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 60 | git | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 59 | graft | CHANGELOG.md, Cargo.lock, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/src/data/mod.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 59 | git | CHANGELOG.md, Cargo.lock, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/src/data/mod.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 61 | graft | CHANGELOG.md, crates/fluent2-egui/src/data/mod.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs, upstream.toml |
| 61 | git | CHANGELOG.md, crates/fluent2-egui/src/data/mod.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs, upstream.toml |
| 65 | graft | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 65 | git | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 62 | graft | CHANGELOG.md, NOTICE, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/src/data/mod.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 62 | git | CHANGELOG.md, NOTICE, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/src/data/mod.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 64 | graft | CHANGELOG.md, crates/fluent2-egui/src/controls/mod.rs, crates/gallery/src/pages/mod.rs |
| 64 | git | CHANGELOG.md, crates/fluent2-egui/src/controls/mod.rs, crates/gallery/src/pages/mod.rs |
| 72 | graft | CHANGELOG.md |
| 72 | git | CHANGELOG.md |
| 75 | graft | CHANGELOG.md |
| 75 | git | CHANGELOG.md |
| 83 | graft | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 83 | git | CHANGELOG.md, crates/gallery/src/pages/mod.rs |
| 85 | graft | CHANGELOG.md |
| 85 | git | CHANGELOG.md |
| 87 | graft | CHANGELOG.md, README.md, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/tests/readme_index.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 87 | git | CHANGELOG.md, README.md, crates/fluent2-egui/Cargo.toml, crates/fluent2-egui/tests/readme_index.rs, crates/gallery/Cargo.toml, crates/gallery/src/pages/mod.rs |
| 89 | graft | CHANGELOG.md |
| 89 | git | CHANGELOG.md |

## rusttorrent-qbit (position)

rustTorrent qbit cluster #138, #139, #141 against #150.

4 PRs replayed, 0 skipped; graft import 2.1 s.

| | graft | git |
| --- | ---: | ---: |
| lands attempted | 4 | 4 |
| fast-forward (no merge needed) | 0 | 0 |
| merged clean | 0 | 0 |
| conflicts | 4 | 4 |
| clean but broken (`cargo check`) | 0 | 0 |
| merged, check passed | 0 | 0 |
| merged, check failed but an input failed too | 0 | 0 |
| errors | 0 | 0 |
| rebases | 4 | 4 |
| client round trips | 8 | 8 |
| integration wall time (s) | 0.712 | 0.48 |
| p50 per land (ms) | 84.8 | 69.6 |

Per PR: both clean 0 (same tree 0, different 0), graft only 0, git only 0, both conflict 4.

| PR | tool | conflicted paths |
| --- | --- | --- |
| 138 | graft | CHANGELOG.md, crates/librtbit/src/http_api/handlers/qbit_compat.rs, crates/librtbit/src/http_api/handlers/qbit_parity_spec.json, docs/QBIT-API-PARITY.md |
| 138 | git | CHANGELOG.md, crates/librtbit/src/http_api/handlers/qbit_compat.rs, crates/librtbit/src/http_api/handlers/qbit_parity_spec.json, docs/QBIT-API-PARITY.md |
| 139 | graft | crates/librtbit/src/http_api/handlers/qbit_compat.rs, crates/librtbit/src/session/scheduler.rs, crates/librtbit/src/session_persistence/postgres.rs |
| 139 | git | crates/librtbit/src/http_api/handlers/qbit_compat.rs, crates/librtbit/src/session/scheduler.rs, crates/librtbit/src/session_persistence/postgres.rs |
| 141 | graft | CHANGELOG.md, crates/librtbit/src/http_api/handlers/qbit_compat.rs, crates/librtbit/src/http_api/handlers/qbit_parity_spec.json |
| 141 | git | CHANGELOG.md, crates/librtbit/src/http_api/handlers/qbit_compat.rs, crates/librtbit/src/http_api/handlers/qbit_parity_spec.json |
| 150 | graft | crates/librtbit/src/http_api/handlers/qbit_parity.rs |
| 150 | git | crates/librtbit/src/http_api/handlers/qbit_parity.rs |

## graft-merge-back (position)

graft PRs that merged `main` back in, landed at the `main` they merged.

57 PRs replayed, 1 skipped; graft import 2.3 s.

| | graft | git |
| --- | ---: | ---: |
| lands attempted | 57 | 57 |
| fast-forward (no merge needed) | 0 | 0 |
| merged clean | 39 | 39 |
| conflicts | 18 | 18 |
| clean but broken (`cargo check`) | 1 | 1 |
| merged, check passed | 38 | 38 |
| merged, check failed but an input failed too | 0 | 0 |
| errors | 0 | 0 |
| rebases | 57 | 57 |
| client round trips | 75 | 153 |
| integration wall time (s) | 3.376 | 4.29 |
| p50 per land (ms) | 46.0 | 73.3 |

Per PR: both clean 39 (same tree 39, different 0), graft only 0, git only 0, both conflict 18.

| PR | tool | conflicted paths |
| --- | --- | --- |
| 12 | graft | Cargo.lock, Cargo.toml |
| 12 | git | Cargo.lock, Cargo.toml |
| 10 | graft | Cargo.toml, crates/graft-testkit/src/lib.rs |
| 10 | git | Cargo.toml, crates/graft-testkit/src/lib.rs |
| 14 | graft | Cargo.lock, Cargo.toml |
| 14 | git | Cargo.lock, Cargo.toml |
| 17 | graft | Cargo.toml |
| 17 | git | Cargo.toml |
| 20 | graft | crates/graft-testkit/src/lib.rs |
| 20 | git | crates/graft-testkit/src/lib.rs |
| 22 | graft | crates/graft-testkit/src/lib.rs |
| 22 | git | crates/graft-testkit/src/lib.rs |
| 21 | graft | crates/graft-testkit/src/lib.rs |
| 21 | git | crates/graft-testkit/src/lib.rs |
| 23 | graft | crates/graft-testkit/src/lib.rs |
| 23 | git | crates/graft-testkit/src/lib.rs |
| 24 | graft | crates/graft-testkit/src/lib.rs |
| 24 | git | crates/graft-testkit/src/lib.rs |
| 26 | graft | crates/graft-testkit/src/lib.rs |
| 26 | git | crates/graft-testkit/src/lib.rs |
| 89 | graft | deploy/README.md, deploy/server.toml, docs/decisions.md, docs/dev-stack.md, docs/plan.md |
| 89 | git | deploy/README.md, deploy/server.toml, docs/decisions.md, docs/dev-stack.md, docs/plan.md |
| 90 | graft | crates/graft-client/src/remote.rs, crates/graft-engine/src/lib.rs |
| 90 | git | crates/graft-client/src/remote.rs, crates/graft-engine/src/lib.rs |
| 93 | graft | crates/graft-client/src/remote.rs, crates/graft-engine/src/lib.rs |
| 93 | git | crates/graft-client/src/remote.rs, crates/graft-engine/src/lib.rs |
| 97 | graft | deploy/README.md |
| 97 | git | deploy/README.md |
| 98 | graft | crates/graft-mcp/src/lib.rs, crates/graft-mcp/src/write.rs, crates/graft-mcp/tests/http.rs, crates/graft-server/tests/mcp_conflict.rs |
| 98 | git | crates/graft-mcp/src/lib.rs, crates/graft-mcp/src/write.rs, crates/graft-mcp/tests/http.rs, crates/graft-server/tests/mcp_conflict.rs |
| 99 | graft | crates/graft-server/tests/mcp_conflict.rs |
| 99 | git | crates/graft-server/tests/mcp_conflict.rs |
| 102 | graft | bench/README.md, bench/graftbench/__main__.py, bench/graftbench/report.py |
| 102 | git | bench/README.md, bench/graftbench/__main__.py, bench/graftbench/report.py |
| 112 | graft | docs/decisions.md |
| 112 | git | docs/decisions.md |

Clean but broken: #35 (graft), #35 (git).

