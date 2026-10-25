//! Compatibility wrapper for the `graft node` entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    graft_node::run(std::env::args().skip(1).collect())
}
