//! Column encodings and error mapping.

use graft_model::{ChangesetId, ObjectId};
use graft_store::{RecordState, RefKind, StoreError, StoreResult};
use rusqlite::ErrorCode;

/// Maps a SQLite failure to a store error. The message names the failure,
/// never the bytes being written.
pub(crate) fn sql_err(err: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(code, _) = &err {
        match code.code {
            ErrorCode::DiskFull => return StoreError::Full,
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                return StoreError::Corrupt(code.to_string());
            }
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                return StoreError::Busy(crate::DEFAULT_RETRY_MS);
            }
            _ => {}
        }
    }
    StoreError::Backend(format!("sqlite: {err}"))
}

/// A `u64` as a SQLite integer.
pub(crate) fn int(v: u64) -> StoreResult<i64> {
    i64::try_from(v).map_err(|_| StoreError::Invalid(format!("{v} is out of range")))
}

/// A SQLite integer read back as a `u64`.
pub(crate) fn uint(v: i64) -> StoreResult<u64> {
    u64::try_from(v).map_err(|_| StoreError::Corrupt(format!("negative value {v}")))
}

/// A 32-byte id read from a blob column.
pub(crate) fn object_id(bytes: &[u8]) -> StoreResult<ObjectId> {
    <[u8; 32]>::try_from(bytes)
        .map(ObjectId)
        .map_err(|_| StoreError::Corrupt(format!("id of {} bytes", bytes.len())))
}

/// A changeset id read from a blob column.
pub(crate) fn changeset_id(bytes: &[u8]) -> StoreResult<ChangesetId> {
    object_id(bytes).map(ChangesetId)
}

pub(crate) fn ref_kind_code(kind: RefKind) -> i64 {
    match kind {
        RefKind::Line => 0,
        RefKind::Tag => 1,
        RefKind::Tool => 2,
    }
}

pub(crate) fn ref_kind(code: i64) -> StoreResult<RefKind> {
    match code {
        0 => Ok(RefKind::Line),
        1 => Ok(RefKind::Tag),
        2 => Ok(RefKind::Tool),
        _ => Err(StoreError::Corrupt(format!("ref kind {code}"))),
    }
}

pub(crate) fn state_code(state: RecordState) -> i64 {
    match state {
        RecordState::Draft => 0,
        RecordState::Landed => 1,
        RecordState::Superseded => 2,
        RecordState::Abandoned => 3,
        RecordState::Collected => 4,
    }
}

pub(crate) fn state(code: i64) -> StoreResult<RecordState> {
    match code {
        0 => Ok(RecordState::Draft),
        1 => Ok(RecordState::Landed),
        2 => Ok(RecordState::Superseded),
        3 => Ok(RecordState::Abandoned),
        4 => Ok(RecordState::Collected),
        _ => Err(StoreError::Corrupt(format!("record state {code}"))),
    }
}
