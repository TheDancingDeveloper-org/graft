//! Object frames: one format at rest and on the wire (`docs/design.md` §6.3).
//!
//! `frame = "GRF1" ‖ u8 kind ‖ u8 codec ‖ u64 uncompressed_len ‖ body`.
//! Transport compression is gzip (operator decision, 2026-10-22).

use graft_model::{ObjectId, ObjectKind};

/// Frame magic bytes.
pub const MAGIC: &[u8; 4] = b"GRF1";

/// Bodies smaller than this are stored raw.
pub const GZIP_MIN_BYTES: usize = 512;

/// How a frame body is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Uncompressed.
    Raw = 0,
    /// gzip (RFC 1952).
    Gzip = 1,
}

/// A stored or transmitted object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Blob or tree (changesets travel as typed messages).
    pub kind: ObjectKind,
    /// Body codec.
    pub codec: Codec,
    /// Length of the payload after decoding.
    pub uncompressed_len: u64,
    /// The encoded body.
    pub body: Vec<u8>,
}

/// Why a frame could not be decoded or verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Wrong magic bytes.
    BadMagic,
    /// Unknown kind or codec byte.
    Unknown(u8),
    /// The decoded length does not match the header.
    LengthMismatch,
    /// The payload does not hash to the expected id.
    HashMismatch,
    /// The body failed to decompress.
    Codec(String),
}

impl Frame {
    /// Encodes `payload`, choosing raw or gzip by the documented thresholds.
    pub fn encode(_kind: ObjectKind, _payload: &[u8]) -> Frame {
        todo!("G1.3: frame encoding with gzip")
    }

    /// Serializes the frame to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        todo!("G1.3: frame serialization")
    }

    /// Parses frame bytes without decompressing.
    pub fn from_bytes(_bytes: &[u8]) -> Result<Frame, FrameError> {
        todo!("G1.3: frame parsing")
    }

    /// Decompresses and checks the payload against `id`.
    pub fn decode_verified(&self, _id: ObjectId) -> Result<Vec<u8>, FrameError> {
        todo!("G1.3: decode and verify")
    }
}
