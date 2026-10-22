//! Fuzzes the frame decoder: parsing and decoding arbitrary bytes must
//! return an error or a payload of exactly the declared length, never panic.
//!
//! Run with `cargo +nightly fuzz run frame_decode` from `crates/graft-proto`.

#![no_main]

use graft_proto::Frame;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(frame) = Frame::from_bytes(data) {
        if let Ok(payload) = frame.decode() {
            assert_eq!(payload.len() as u64, frame.uncompressed_len);
        }
    }
});
