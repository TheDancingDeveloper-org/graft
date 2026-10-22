//! In-memory `ObjectStore` and `MetaStore`, for tests only (work item G1.2).

/// An in-memory store implementing both ports.
#[derive(Debug, Default)]
pub struct MemoryStore {
    _private: (),
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}
