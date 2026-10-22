//! An accepted path is unchanged and every component is itself valid.
#![no_main]

use graft_model::RepoPath;
use graft_model::path::check_component;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(p) = RepoPath::from_bytes(data) {
        assert_eq!(p.as_str().as_bytes(), data);
        assert!(p.components().all(|c| check_component(c).is_ok()));
        let _ = p.check_portable();
    }
});
