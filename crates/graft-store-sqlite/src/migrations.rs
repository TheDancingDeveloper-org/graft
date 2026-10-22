//! Versioned schema migrations (`docs/design.md` §6.5).

/// The schema version this build writes. Zero until G1.6 lands the first
/// migration.
pub const SCHEMA_VERSION: u32 = 0;
