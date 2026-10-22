//! The port conformance suite (work item G1.2). Every store implementation
//! runs [`run_all`] from its own tests.

use crate::{MetaStore, ObjectStore};

/// Runs every conformance case against stores made by `factory`. Each case
/// gets fresh stores.
pub async fn run_all<O, M, F>(_factory: F)
where
    O: ObjectStore,
    M: MetaStore,
    F: Fn() -> (O, M),
{
    todo!("G1.2: conformance cases")
}
