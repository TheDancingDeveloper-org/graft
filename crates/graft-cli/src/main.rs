//! `graft`: the command-line client (`docs/design.md` §3 verbs, §9).
//!
//! Every command takes `--json`, whose shape is the JSON Schema saved
//! under `crates/graft-cli/schemas/`. Exit codes follow design §8.2 ([`Exit`]).

#[macro_use]
mod out;

mod api;
mod cli;
mod cmd;
mod conn;
mod error;
mod identity;
mod render;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};
use error::Failure;

/// Exit codes (`docs/design.md` §8.2). `Failure` is for everything §8.2 does
/// not name (not found, a damaged working copy, an I/O error); it is 1, the
/// conventional general failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Exit {
    /// Success.
    Ok = 0,
    /// Any other failure.
    Failure = 1,
    /// Usage error.
    Usage = 2,
    /// Land or sync conflict, or local changes that would be overwritten.
    Conflict = 3,
    /// Contended; retryable.
    Contended = 4,
    /// A land policy refused.
    Rejected = 5,
    /// Not authorized.
    Denied = 6,
    /// Unavailable or transport error; retryable.
    Unavailable = 7,
}

/// Commands that are designed but not built yet (`docs/plan.md`).
const PLANNED: &[&str] = &[];

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return parse_failure(e),
    };
    let json = cli.global.json;
    let result = dispatch(&cli);
    let code = match result {
        Ok(()) => Exit::Ok,
        Err(failure) => failure.report(json),
    };
    ExitCode::from(code as u8)
}

/// A command line clap refused. Help and version print as usual. A usage
/// error prints as the JSON error object when `--json` is on the line.
fn parse_failure(e: clap::Error) -> ExitCode {
    if e.exit_code() == 0 {
        let _ = e.print();
        return ExitCode::SUCCESS;
    }
    if std::env::args_os().any(|a| a == "--json") {
        let first = e.to_string();
        let message = first.lines().next().unwrap_or("usage error");
        let message = message.strip_prefix("error: ").unwrap_or(message);
        let code = Failure::usage(message.to_owned()).report(true);
        return ExitCode::from(code as u8);
    }
    e.exit()
}

fn dispatch(cli: &Cli) -> Result<(), Failure> {
    let g = &cli.global;
    match &cli.command {
        Command::Clone(args) => cmd::clone::run(g, args),
        Command::Status(args) => cmd::status::run(g, args),
        Command::Diff(args) => cmd::diff::run(g, args),
        Command::Log(args) => cmd::log::run(g, args),
        Command::Show(args) => cmd::show::run(g, args),
        Command::Blame(args) => cmd::blame::run(g, args),
        Command::Cat(args) => cmd::cat::run(g, args),
        Command::Ls(args) => cmd::ls::run(g, args),
        Command::Sparse(args) => cmd::sparse::run(g, args),
        Command::Commit(args) => cmd::commit::run(g, args),
        Command::Workspace(args) => cmd::workspace::run(g, args),
        Command::Line(args) => cmd::line::run(g, args),
        Command::Tag(args) => cmd::tag::run(g, args),
        Command::Stack(args) => cmd::stack::run(g, args),
        Command::Sync => cmd::sync::run(g),
        Command::Land(args) => cmd::land::run(g, args),
        Command::Revert(args) => cmd::revert::run(g, args),
        Command::Pick(args) => cmd::pick::run(g, args),
        Command::Attest(args) => cmd::attest::run(g, args),
        Command::Ci(args) => cmd::ci::run(g, args),
        Command::Api(args) => api::run(g, args),
        Command::Version => cmd::version::run(g),
        Command::Mcp => cmd::mcp::run(g),
        Command::Lsp { args } => match cmd::lsp::run(args) {
            Exit::Ok => Ok(()),
            code => Err(Failure::silent(code)),
        },
        Command::Planned(args) => {
            let name = args.first().map_or("", String::as_str);
            if PLANNED.contains(&name) {
                Err(Failure::usage(format!(
                    "graft {name}: not implemented yet (see docs/plan.md)"
                )))
            } else {
                Err(Failure::usage(format!("unknown command: {name}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_design() {
        assert_eq!(Exit::Ok as u8, 0);
        assert_eq!(Exit::Usage as u8, 2);
        assert_eq!(Exit::Conflict as u8, 3);
        assert_eq!(Exit::Contended as u8, 4);
        assert_eq!(Exit::Rejected as u8, 5);
        assert_eq!(Exit::Denied as u8, 6);
        assert_eq!(Exit::Unavailable as u8, 7);
    }
}
