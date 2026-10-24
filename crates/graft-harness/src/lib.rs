//! The adversarial harness, part 1: racing lands under injected faults,
//! checked against a model (`docs/notes/adversarial-harness.md` §3, work
//! item WI-2345).
//!
//! A seeded [`scenario::Plan`] drives several clients landing on one line
//! of one engine. The engine runs over the memory store wrapped in
//! [`faulty::FaultyMeta`] and [`faulty::FaultyObjects`], which delay calls,
//! fail them, lose their acknowledgements, crash the process at a commit
//! and fill the disk. A single-threaded [`exec::Scheduler`] picks the
//! interleaving from the seed. Every operation and its answer go into a
//! history, and [`check`] compares the history and the final state with a
//! model of the line. A failure prints its seed and a one-line replay
//! command, and [`shrink`] cuts the plan down to a small failing case.
//!
//! The in-process run uses the memory store, not SQLite and the filesystem:
//! the SQLite store answers from a writer thread, which would put real
//! thread timing back into the interleaving and lose replay from the seed.
//! The out-of-process soak against a real `graft-server` (§3.1) is a later
//! step.
//!
//! This is a test tool. It is never a gate in the land path (D22).

pub mod check;
pub mod exec;
pub mod faulty;
pub mod probe;
pub mod rng;
pub mod scenario;
pub mod shrink;

pub use check::{Failure, Report, check};
pub use scenario::{Outcome, Plan, Shape, run};
