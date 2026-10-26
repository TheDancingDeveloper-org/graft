# Stability

What graft promises not to change, and what a pre-1.0 release may still change.

## Frozen encodings

These byte formats are frozen by golden tests. A change to one of them needs a migration and an operator decision; a test failure is not a reason to rewrite the vector.

| Encoding | Golden test |
| --- | --- |
| Object ids (blobs, trees, changesets) | [crates/graft-model/tests/golden.rs](../../crates/graft-model/tests/golden.rs) |
| Tree bytes | [crates/graft-model/tests/golden.rs](../../crates/graft-model/tests/golden.rs) |
| Changeset bytes | [crates/graft-model/tests/golden.rs](../../crates/graft-model/tests/golden.rs) |
| Content frames (raw and compressed) | [crates/graft-proto/src/frame.rs](../../crates/graft-proto/src/frame.rs) |
| Capability tokens | [crates/graft-auth/tests/tokens.rs](../../crates/graft-auth/tests/tokens.rs) |

## Storage schema

The SQLite schema version is `user_version`. It is the length of the migration list in [crates/graft-store-sqlite/src/migrations.rs](../../crates/graft-store-sqlite/src/migrations.rs) (`SCHEMA_VERSION`). Each migration runs in the same transaction as the version bump, so a crash leaves the database at one version or the next, never between them. Released migrations are append-only: a migration that has shipped is never edited. This build refuses a database whose version is newer than its own. The migration tests are [crates/graft-store-sqlite/tests/migrations.rs](../../crates/graft-store-sqlite/tests/migrations.rs).

## Protocol

`graft_proto::PROTOCOL_VERSION` is major 0, minor 1 ([crates/graft-proto/src/version.rs](../../crates/graft-proto/src/version.rs)). A client sends `major.minor`. The server accepts it when the majors match and the server's minor is at least the client's. Anything else, a different major, a newer minor than the server, or a missing or malformed header, is refused with `IncompatibleProtocol`, which names the server's version. A minor bump stays compatible for older clients on the same major; a major bump does not.

## Releases before 1.0

A 0.x release may change behaviour that is not one of the frozen encodings above. It may not change those encodings without a migration. Each user-visible change is announced in the changelog: one fragment under [changes/](../../changes/README.md) per change, folded into `CHANGELOG.md` by the release.

## You can always leave

A full export writes every line, tag and landed changeset into an ordinary Git repository. That export is the guarantee that a repository in graft is never a trap. The steps are in [Git migration](git-migration.md).

## Platforms

The CLI, the node and the server are built and shipped for:

- Linux x86_64, musl (`x86_64-unknown-linux-musl`)
- Linux aarch64, musl (`aarch64-unknown-linux-musl`)
- macOS aarch64 (`aarch64-apple-darwin`)

Those are the three archive jobs in [.github/workflows/release.yml](../../.github/workflows/release.yml). Windows is not built.

Continuous integration runs the required checks on the repository's self-hosted runner. Release builds use the hosted runners named in that same workflow (`ubuntu-24.04`, `ubuntu-24.04-arm`, `macos-14`). The required-check jobs are in [.github/workflows/ci.yml](../../.github/workflows/ci.yml).
