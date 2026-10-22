//! Byte-level helpers shared by the canonical encodings. Integers are
//! big-endian; `str` is a `u32` length followed by UTF-8.

use crate::changeset::DecodeError;
use crate::id::ObjectId;

pub(crate) fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_len(out: &mut Vec<u8>, n: usize) {
    put_u32(out, u32::try_from(n).expect("length fits in u32"));
}

pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
    put_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

/// A cursor over a payload that fails with [`DecodeError::Truncated`] when it
/// runs out.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.buf.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    pub(crate) fn i16(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    pub(crate) fn id(&mut self) -> Result<ObjectId, DecodeError> {
        Ok(ObjectId(self.take(32)?.try_into().expect("32 bytes")))
    }

    /// A count of items that each take at least `min_item` bytes. Rejecting
    /// a count the remaining bytes cannot hold keeps allocation bounded by
    /// the payload size.
    pub(crate) fn count(&mut self, min_item: usize) -> Result<usize, DecodeError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item) > self.remaining() {
            return Err(DecodeError::Truncated);
        }
        Ok(n)
    }

    pub(crate) fn str(&mut self) -> Result<&'a str, DecodeError> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.take(n)?).map_err(|_| DecodeError::NotCanonical("utf8"))
    }
}
