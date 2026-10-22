//! `graft`: the command-line client (`docs/design.md` §3 verbs, §9).
//!
//! Every command takes `--json`; exit codes follow design §8.2 ([`Exit`]).

mod cmd;

use std::process::ExitCode;

/// Exit codes shared by every command (`docs/design.md` §8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum Exit {
    /// Success (or landed).
    Ok = 0,
    /// Usage error.
    Usage = 2,
    /// Land or sync conflict.
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

const COMMANDS: &[(&str, &str)] = &[
    (
        "clone",
        "open a workspace and lay out a sparse working copy",
    ),
    ("status", "working-copy changes against the head"),
    ("diff", "diff the working copy or two changesets"),
    ("log", "first-parent history"),
    ("show", "a changeset and its patch"),
    ("blame", "line origins"),
    ("cat", "print a file at a changeset"),
    ("ls", "list a directory at a changeset"),
    ("sparse", "add or remove slice prefixes"),
    ("commit", "create a draft on the server"),
    ("land", "land the stack on a line"),
    ("sync", "move the base to the line tip"),
    ("stack", "list, reorder, squash, split or drop drafts"),
    ("revert", "create a draft inverting a changeset"),
    ("workspace", "list, open, renew or close workspaces"),
    ("api", "call any RPC with JSON in and out"),
    ("mcp", "run the MCP server over stdio"),
    ("lsp", "run the LSP server over stdio"),
    ("version", "print versions"),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("version") => {
            println!(
                "graft {} (protocol {}.{})",
                env!("CARGO_PKG_VERSION"),
                graft_proto::PROTOCOL_VERSION.major,
                graft_proto::PROTOCOL_VERSION.minor
            );
            Exit::Ok
        }
        Some("lsp") => cmd::lsp::run(&args[1..]),
        Some(c) if COMMANDS.iter().any(|(n, _)| *n == c) => {
            eprintln!("graft {c}: not implemented yet (see docs/plan.md)");
            Exit::Usage
        }
        _ => {
            eprintln!("usage: graft <command> [--json] [args]\n");
            for (name, help) in COMMANDS {
                eprintln!("  {name:<10} {help}");
            }
            Exit::Usage
        }
    };
    ExitCode::from(code as u8)
}
