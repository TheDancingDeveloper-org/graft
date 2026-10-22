//! Every accepted id text form prints back to exactly the input.
#![no_main]

use graft_model::{BlobId, ChangesetId, IdPrefix, ObjectId, TreeId};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    if let Ok(id) = ObjectId::from_hex(s) {
        assert_eq!(id.to_hex(), s);
    }
    if let Ok(id) = BlobId::parse(s) {
        assert_eq!(id.to_string(), s);
    }
    if let Ok(id) = TreeId::parse(s) {
        assert_eq!(id.to_string(), s);
    }
    if let Ok(id) = ChangesetId::parse(s) {
        assert_eq!(id.to_string(), s);
    }
    if let Ok(p) = IdPrefix::parse(s) {
        assert_eq!(p.to_string(), s);
    }
});
