//! A decoded changeset re-encodes to exactly the input: the encoding is
//! canonical.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(cs) = graft_model::Changeset::decode(data) {
        assert_eq!(cs.encode(), data);
    }
});
