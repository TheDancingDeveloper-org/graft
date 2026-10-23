//! Versioned schema migrations (`docs/design.md` §6.5).
//!
//! The schema version lives in SQLite's `user_version`. Migration `i` (from
//! zero) takes the database from version `i` to `i + 1`, and runs in the same
//! transaction as the version bump, so a crash leaves the database at one
//! version or the next, never between them. Migrations are append-only: a
//! released one is never edited.

use rusqlite::{Connection, TransactionBehavior};

use crate::codec::sql_err;
use graft_store::{StoreError, StoreResult};

/// The migrations, in order. `MIGRATIONS[i]` moves version `i` to `i + 1`.
pub const MIGRATIONS: &[&str] = &[V1, V2, V3, V4];

/// The schema version this build writes.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Version 1: the schema sketched in design §6.5.
///
/// Ids are 32-byte blobs; LSNs, positions and times are integers. Every
/// table is keyed by repository first, so each query is an index range.
const V1: &str = "
CREATE TABLE repos (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE changesets (
    repo          INTEGER NOT NULL REFERENCES repos(id),
    id            BLOB NOT NULL,
    content       BLOB NOT NULL,
    lsn           INTEGER NOT NULL,
    received_at   INTEGER NOT NULL,
    state         INTEGER NOT NULL,
    holder        TEXT,
    superseded_by BLOB,
    PRIMARY KEY (repo, id)
);

CREATE TABLE refs (
    repo    INTEGER NOT NULL REFERENCES repos(id),
    name    TEXT NOT NULL,
    target  BLOB NOT NULL,
    version INTEGER NOT NULL,
    kind    INTEGER NOT NULL,
    PRIMARY KEY (repo, name)
) WITHOUT ROWID;

CREATE TABLE journal (
    repo INTEGER NOT NULL REFERENCES repos(id),
    lsn  INTEGER NOT NULL,
    kind TEXT NOT NULL,
    body BLOB NOT NULL,
    PRIMARY KEY (repo, lsn)
);

CREATE TABLE workspaces (
    repo        INTEGER NOT NULL REFERENCES repos(id),
    name        TEXT NOT NULL,
    owner       TEXT NOT NULL,
    session     TEXT,
    line        TEXT NOT NULL,
    base        BLOB NOT NULL,
    head        BLOB,
    lease_until INTEGER NOT NULL,
    PRIMARY KEY (repo, name)
) WITHOUT ROWID;
CREATE INDEX workspaces_lease ON workspaces (repo, lease_until, name);

CREATE TABLE derived (
    repo  INTEGER NOT NULL REFERENCES repos(id),
    kind  TEXT NOT NULL,
    ver   INTEGER NOT NULL,
    key   BLOB NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (repo, kind, ver, key)
);

CREATE TABLE line_pos (
    repo      INTEGER NOT NULL REFERENCES repos(id),
    line      TEXT NOT NULL,
    changeset BLOB NOT NULL,
    pos       INTEGER NOT NULL,
    PRIMARY KEY (repo, line, changeset)
) WITHOUT ROWID;

CREATE TABLE line_paths (
    repo      INTEGER NOT NULL REFERENCES repos(id),
    line      TEXT NOT NULL,
    path      TEXT NOT NULL,
    pos       INTEGER NOT NULL,
    changeset BLOB NOT NULL,
    PRIMARY KEY (repo, line, path, pos, changeset)
) WITHOUT ROWID;

CREATE TABLE holders (
    repo   INTEGER NOT NULL REFERENCES repos(id),
    object BLOB NOT NULL,
    holder TEXT NOT NULL,
    PRIMARY KEY (repo, object, holder)
) WITHOUT ROWID;

CREATE TABLE uploads (
    repo    INTEGER NOT NULL REFERENCES repos(id),
    object  BLOB NOT NULL,
    owner   TEXT NOT NULL,
    expires INTEGER NOT NULL,
    PRIMARY KEY (repo, object, owner)
) WITHOUT ROWID;

CREATE TABLE doomed (
    repo   INTEGER NOT NULL REFERENCES repos(id),
    object BLOB NOT NULL,
    since  INTEGER NOT NULL,
    PRIMARY KEY (repo, object)
) WITHOUT ROWID;

CREATE TABLE lands (
    repo   INTEGER NOT NULL REFERENCES repos(id),
    draft  BLOB NOT NULL,
    line   TEXT NOT NULL,
    landed BLOB NOT NULL,
    PRIMARY KEY (repo, draft, line)
) WITHOUT ROWID;

CREATE TABLE tombstones (
    repo   INTEGER NOT NULL REFERENCES repos(id),
    id     BLOB NOT NULL,
    at     INTEGER NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (repo, id)
) WITHOUT ROWID;
";

/// Version 2 (G2.1): `ListWorkspaces` by owner, and orphan gauges that count
/// one record state without scanning the others.
const V2: &str = "
CREATE INDEX workspaces_owner ON workspaces (repo, owner, name);
CREATE INDEX changesets_state ON changesets (repo, state);
";

/// Version 3 (G2.9): `list_records` pages one record state in id order
/// without sorting.
const V3: &str = "
CREATE INDEX changesets_state_id ON changesets (repo, state, id);
";

/// Version 4 (G3.11): GC protection records (design §8.6).
///
/// Records get parent `pins` and `expires_at`, with an index over exactly
/// the records a collection may take (no pins, an expiry). Holder rows get
/// an index by holder and an optional expiry (backfilled from the upload
/// lease each `upload:<ms>:<who>` row names) with its own index. Doomed
/// marks get `deleting` and an index by time. Repositories get the
/// `gc_protection` marker, off for every repository that exists now: their
/// pins and expiries are backfilled by the engine (`Engine::protect_repo`),
/// which sets it. Repositories created from now on start with it on.
const V4: &str = "
ALTER TABLE repos ADD COLUMN gc_protection INTEGER NOT NULL DEFAULT 0;

ALTER TABLE changesets ADD COLUMN pins INTEGER NOT NULL DEFAULT 0;
ALTER TABLE changesets ADD COLUMN expires_at INTEGER;
CREATE INDEX changesets_expiry ON changesets (repo, expires_at, id)
    WHERE pins = 0 AND expires_at IS NOT NULL;

ALTER TABLE holders ADD COLUMN expires INTEGER;
UPDATE holders
    SET expires = CAST(substr(holder, 8, instr(substr(holder, 8), ':') - 1) AS INTEGER)
    WHERE substr(holder, 1, 7) = 'upload:' AND instr(substr(holder, 8), ':') > 1;
CREATE INDEX holders_holder ON holders (repo, holder, object);
CREATE INDEX holders_expiry ON holders (repo, expires, object, holder)
    WHERE expires IS NOT NULL;

ALTER TABLE doomed ADD COLUMN deleting INTEGER NOT NULL DEFAULT 0;
CREATE INDEX doomed_since ON doomed (repo, since, object);
";

/// The database's schema version.
pub fn version(conn: &Connection) -> StoreResult<u32> {
    let v: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_err)?;
    u32::try_from(v).map_err(|_| StoreError::Corrupt(format!("schema version {v}")))
}

/// Brings the database up to `target`, one migration per transaction. A
/// database newer than this build is refused rather than touched.
pub fn migrate_to(conn: &mut Connection, target: u32) -> StoreResult<()> {
    let target = target.min(SCHEMA_VERSION);
    loop {
        let current = version(conn)?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::Invalid(format!(
                "database schema version {current} is newer than this build ({SCHEMA_VERSION})"
            )));
        }
        if current >= target {
            return Ok(());
        }
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        // Re-read under the write lock: another opener may have migrated.
        if version(&tx)? == current {
            tx.execute_batch(MIGRATIONS[current as usize])
                .map_err(sql_err)?;
            tx.pragma_update(None, "user_version", current + 1)
                .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
    }
}

/// Brings the database up to [`SCHEMA_VERSION`].
pub fn migrate(conn: &mut Connection) -> StoreResult<()> {
    migrate_to(conn, SCHEMA_VERSION)
}
