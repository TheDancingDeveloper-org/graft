//! Column encodings and error mapping.

use graft_model::{ChangesetId, ObjectId};
use graft_store::{
    AttestKind, AttestStatus, LandCheckState, RecordState, RefKind, StoreError, StoreResult,
};
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

pub(crate) fn attest_kind_code(kind: AttestKind) -> i64 {
    match kind {
        AttestKind::Check => 0,
        AttestKind::Review => 1,
        AttestKind::Approval => 2,
    }
}

pub(crate) fn attest_kind(code: i64) -> StoreResult<AttestKind> {
    match code {
        0 => Ok(AttestKind::Check),
        1 => Ok(AttestKind::Review),
        2 => Ok(AttestKind::Approval),
        _ => Err(StoreError::Corrupt(format!("attestation kind {code}"))),
    }
}

pub(crate) fn attest_status_code(status: AttestStatus) -> i64 {
    match status {
        AttestStatus::Pass => 0,
        AttestStatus::Fail => 1,
        AttestStatus::Pending => 2,
        AttestStatus::Error => 3,
    }
}

pub(crate) fn attest_status(code: i64) -> StoreResult<AttestStatus> {
    match code {
        0 => Ok(AttestStatus::Pass),
        1 => Ok(AttestStatus::Fail),
        2 => Ok(AttestStatus::Pending),
        3 => Ok(AttestStatus::Error),
        _ => Err(StoreError::Corrupt(format!("attestation status {code}"))),
    }
}

pub(crate) fn land_check_code(state: LandCheckState) -> i64 {
    match state {
        LandCheckState::Pending => 0,
        LandCheckState::Good => 1,
        LandCheckState::Bad => 2,
        LandCheckState::Reverted => 3,
    }
}

pub(crate) fn land_check(code: i64) -> StoreResult<LandCheckState> {
    match code {
        0 => Ok(LandCheckState::Pending),
        1 => Ok(LandCheckState::Good),
        2 => Ok(LandCheckState::Bad),
        3 => Ok(LandCheckState::Reverted),
        _ => Err(StoreError::Corrupt(format!("land check state {code}"))),
    }
}
