//! Compatibility wrapper for the `graft serve` entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    graft_server::run(std::env::args().skip(1).collect())
}
