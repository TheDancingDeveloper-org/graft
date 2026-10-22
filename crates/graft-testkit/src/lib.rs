//! Test fixtures shared by every crate. Dev-dependency only.
//!
//! Each work item adds its own module file and one sorted `pub mod` line
//! below (docs/plan.md, "Crate ownership and parallelism").

pub mod clock;
pub mod crash;
pub mod fake_api;
pub mod synth;
