//! Compatibility wrapper for the `graft mirror` entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    graft_mirror::run(std::env::args().skip(1).collect())
}
