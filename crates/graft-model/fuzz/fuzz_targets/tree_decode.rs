//! A decoded tree re-encodes to exactly the input: the encoding is canonical.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(tree) = graft_model::Tree::decode(data) {
        assert_eq!(tree.encode(), data);
    }
});
