//! Object frames: one format at rest and on the wire (`docs/design.md` §6.3).
//!
//! `frame = "GRF1" ‖ u8 kind ‖ u8 codec ‖ u64 uncompressed_len ‖ body`, the
//! length big-endian. The kind byte is the id's kind letter (`b`, `t`, `c`).
//! Transport compression is gzip (operator decision, 2026-10-22).

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use graft_model::{ObjectId, ObjectKind};

/// Frame magic bytes.
pub const MAGIC: &[u8; 4] = b"GRF1";

/// Bytes before the body: magic, kind, codec and length.
pub const HEADER_LEN: usize = 4 + 1 + 1 + 8;

/// Bodies smaller than this are stored raw.
pub const GZIP_MIN_BYTES: usize = 512;

/// gzip is kept only when it saves at least this percentage.
pub const GZIP_MIN_SAVING_PERCENT: usize = 10;

/// The gzip level used for frame bodies.
pub const GZIP_LEVEL: u32 = 6;

/// How a frame body is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Uncompressed.
    Raw = 0,
    /// gzip (RFC 1952).
    Gzip = 1,
}

impl Codec {
    fn from_byte(b: u8) -> Option<Codec> {
        match b {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Gzip),
            _ => None,
        }
    }
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
    /// Wrong magic bytes, or fewer bytes than a header.
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

fn kind_byte(kind: ObjectKind) -> u8 {
    kind.letter() as u8
}

fn kind_from_byte(b: u8) -> Option<ObjectKind> {
    match b {
        b'b' => Some(ObjectKind::Blob),
        b't' => Some(ObjectKind::Tree),
        b'c' => Some(ObjectKind::Changeset),
        _ => None,
    }
}

impl Frame {
    /// Encodes `payload`, choosing raw or gzip by the documented thresholds:
    /// gzip (level 6) for payloads of at least [`GZIP_MIN_BYTES`] when it
    /// saves at least [`GZIP_MIN_SAVING_PERCENT`], raw otherwise.
    pub fn encode(kind: ObjectKind, payload: &[u8]) -> Frame {
        let uncompressed_len = payload.len() as u64;
        if payload.len() >= GZIP_MIN_BYTES {
            let gz = gzip(payload);
            if gz.len() * 100 <= payload.len() * (100 - GZIP_MIN_SAVING_PERCENT) {
                return Frame {
                    kind,
                    codec: Codec::Gzip,
                    uncompressed_len,
                    body: gz,
                };
            }
        }
        Frame {
            kind,
            codec: Codec::Raw,
            uncompressed_len,
            body: payload.to_vec(),
        }
    }

    /// Serializes the frame to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.body.len());
        out.extend_from_slice(MAGIC);
        out.push(kind_byte(self.kind));
        out.push(self.codec as u8);
        out.extend_from_slice(&self.uncompressed_len.to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }

    /// Parses frame bytes without decompressing. A raw body must already be
    /// exactly `uncompressed_len` bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Frame, FrameError> {
        if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC {
            return Err(FrameError::BadMagic);
        }
        let kind = kind_from_byte(bytes[4]).ok_or(FrameError::Unknown(bytes[4]))?;
        let codec = Codec::from_byte(bytes[5]).ok_or(FrameError::Unknown(bytes[5]))?;
        let mut len = [0u8; 8];
        len.copy_from_slice(&bytes[6..HEADER_LEN]);
        let uncompressed_len = u64::from_be_bytes(len);
        let body = &bytes[HEADER_LEN..];
        if codec == Codec::Raw && body.len() as u64 != uncompressed_len {
            return Err(FrameError::LengthMismatch);
        }
        Ok(Frame {
            kind,
            codec,
            uncompressed_len,
            body: body.to_vec(),
        })
    }

    /// Decompresses the payload and checks its length against the header.
    /// Never reads more than `uncompressed_len + 1` bytes from a gzip body.
    pub fn decode(&self) -> Result<Vec<u8>, FrameError> {
        let payload = match self.codec {
            Codec::Raw => self.body.clone(),
            Codec::Gzip => {
                // Don't trust the header for the allocation; grow as read.
                let cap = self.uncompressed_len.min(self.body.len() as u64 * 4) as usize;
                let mut out = Vec::with_capacity(cap);
                GzDecoder::new(self.body.as_slice())
                    .take(self.uncompressed_len.saturating_add(1))
                    .read_to_end(&mut out)
                    .map_err(|e| FrameError::Codec(e.kind().to_string()))?;
                out
            }
        };
        if payload.len() as u64 != self.uncompressed_len {
            return Err(FrameError::LengthMismatch);
        }
        Ok(payload)
    }

    /// Decompresses and checks the payload against `id`.
    pub fn decode_verified(&self, id: ObjectId) -> Result<Vec<u8>, FrameError> {
        let payload = self.decode()?;
        if ObjectId::hash(self.kind, &payload) != id {
            return Err(FrameError::HashMismatch);
        }
        Ok(payload)
    }
}

fn gzip(payload: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(
        Vec::with_capacity(payload.len() / 2),
        Compression::new(GZIP_LEVEL),
    );
    // Writing to a Vec cannot fail.
    enc.write_all(payload).expect("gzip into memory");
    enc.finish().expect("gzip into memory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn compressible(n: usize) -> Vec<u8> {
        b"graft frames ".iter().copied().cycle().take(n).collect()
    }

    /// Bytes that gzip cannot shrink (a xorshift stream).
    fn incompressible(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn small_payloads_stay_raw() {
        let f = Frame::encode(ObjectKind::Blob, &compressible(GZIP_MIN_BYTES - 1));
        assert_eq!(f.codec, Codec::Raw);
    }

    #[test]
    fn compressible_payloads_at_threshold_use_gzip() {
        let f = Frame::encode(ObjectKind::Blob, &compressible(GZIP_MIN_BYTES));
        assert_eq!(f.codec, Codec::Gzip);
        assert_eq!(f.decode().unwrap(), compressible(GZIP_MIN_BYTES));
    }

    #[test]
    fn incompressible_payloads_stay_raw() {
        let f = Frame::encode(ObjectKind::Blob, &incompressible(4096));
        assert_eq!(f.codec, Codec::Raw);
    }

    #[test]
    fn gzip_kept_only_when_it_saves_ten_percent() {
        // Half compressible, half random: check the rule against the gzip
        // size itself, whichever side of the threshold it lands.
        for split in [0, 256, 512, 1024, 2048, 3072, 4096] {
            let mut p = compressible(split);
            p.extend(incompressible(4096 - split));
            let f = Frame::encode(ObjectKind::Blob, &p);
            let saves = gzip(&p).len() * 10 <= p.len() * 9;
            assert_eq!(f.codec == Codec::Gzip, saves, "split {split}");
            assert_eq!(f.decode().unwrap(), p);
        }
    }

    // Golden frames: the byte layout is a durable format.
    const GOLDEN_RAW: &[u8] = b"GRF1b\x00\x00\x00\x00\x00\x00\x00\x00\x05hello";

    #[test]
    fn golden_raw_frame() {
        let f = Frame::encode(ObjectKind::Blob, b"hello");
        assert_eq!(f.to_bytes(), GOLDEN_RAW);
        assert_eq!(Frame::from_bytes(GOLDEN_RAW).unwrap(), f);
        assert_eq!(f.decode().unwrap(), b"hello");
    }

    #[test]
    fn golden_empty_tree_frame() {
        let bytes = b"GRF1t\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let f = Frame::from_bytes(bytes).unwrap();
        assert_eq!(f.kind, ObjectKind::Tree);
        assert_eq!(f.decode().unwrap(), b"");
        assert_eq!(Frame::encode(ObjectKind::Tree, b"").to_bytes(), bytes);
    }

    #[test]
    fn golden_gzip_frame() {
        // 600 × 'a', gzip level 6 as written by flate2 (mtime 0, OS 255).
        let mut bytes = b"GRF1b\x01\x00\x00\x00\x00\x00\x00\x02\x58".to_vec();
        bytes.extend_from_slice(&[
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xed, 0xc0, 0xb1, 0x00,
            0x00, 0x00, 0x00, 0x80, 0x30, 0xd6, 0xfc, 0x25, 0x92, 0xe8, 0x6c, 0xb0, 0xf5, 0x04,
            0x2d, 0x6c, 0x70, 0xfa, 0x58, 0x02, 0x00, 0x00,
        ]);
        let f = Frame::from_bytes(&bytes).unwrap();
        assert_eq!(f.codec, Codec::Gzip);
        assert_eq!(f.decode().unwrap(), vec![b'a'; 600]);
        assert_eq!(
            Frame::encode(ObjectKind::Blob, &[b'a'; 600]).to_bytes(),
            bytes
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = GOLDEN_RAW.to_vec();
        b[3] = b'2';
        assert_eq!(Frame::from_bytes(&b), Err(FrameError::BadMagic));
        assert_eq!(Frame::from_bytes(b"GRF1b"), Err(FrameError::BadMagic));
        assert_eq!(Frame::from_bytes(b""), Err(FrameError::BadMagic));
    }

    #[test]
    fn rejects_unknown_kind_and_codec() {
        let mut b = GOLDEN_RAW.to_vec();
        b[4] = b'z';
        assert_eq!(Frame::from_bytes(&b), Err(FrameError::Unknown(b'z')));
        let mut b = GOLDEN_RAW.to_vec();
        b[5] = 2;
        assert_eq!(Frame::from_bytes(&b), Err(FrameError::Unknown(2)));
    }

    #[test]
    fn rejects_length_mismatch() {
        let mut b = GOLDEN_RAW.to_vec();
        b.push(b'!');
        assert_eq!(Frame::from_bytes(&b), Err(FrameError::LengthMismatch));
        b.truncate(b.len() - 2);
        assert_eq!(Frame::from_bytes(&b), Err(FrameError::LengthMismatch));

        let mut gz = Frame::encode(ObjectKind::Blob, &compressible(1000));
        gz.uncompressed_len = 999;
        assert_eq!(gz.decode(), Err(FrameError::LengthMismatch));
        gz.uncompressed_len = 1001;
        assert_eq!(gz.decode(), Err(FrameError::LengthMismatch));
        gz.uncompressed_len = u64::MAX;
        assert_eq!(gz.decode(), Err(FrameError::LengthMismatch));
    }

    #[test]
    fn rejects_corrupt_gzip() {
        let mut gz = Frame::encode(ObjectKind::Blob, &compressible(1000));
        let n = gz.body.len();
        gz.body[n / 2] ^= 0xff;
        gz.body.truncate(n - 4);
        assert!(matches!(gz.decode(), Err(FrameError::Codec(_))));
    }

    proptest! {
        #[test]
        fn round_trips(payload in prop::collection::vec(any::<u8>(), 0..4096), tree in any::<bool>()) {
            let kind = if tree { ObjectKind::Tree } else { ObjectKind::Blob };
            let f = Frame::encode(kind, &payload);
            let parsed = Frame::from_bytes(&f.to_bytes()).unwrap();
            prop_assert_eq!(&parsed, &f);
            prop_assert_eq!(parsed.decode().unwrap(), payload);
        }

        #[test]
        fn decoder_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            if let Ok(f) = Frame::from_bytes(&bytes) {
                let _ = f.decode();
            }
        }
    }
}
