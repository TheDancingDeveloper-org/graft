//! `graft lsp`: owned by work item G3.6 (see docs/plan.md, wave 3.B note).
//!
//! Runs the LSP server (`graft-lsp`) over standard input and output, for the
//! working copy around the current directory. Standard output carries the
//! protocol, so failures before the session starts go to standard error.
//!
//! The global `--endpoint`, `--repo` and `--token-file` work before or
//! after `lsp` on the line, with the same fallbacks as every command:
//! `GRAFT_ENDPOINT`, the node socket and the working copy's server;
//! `GRAFT_TOKEN`.

use std::path::PathBuf;

use clap::Parser;
use graft_lsp::{Ended, LspServer};

use crate::Exit;
use crate::cli::{Cli, Global};
use crate::conn::{call_context, connect, runtime, working_copy};
use crate::error::Failure;

const USAGE: &str = "usage: graft lsp [--endpoint URL] [--repo NAME] [--token-file PATH]

Runs the graft LSP server over stdio. Start it from inside a working copy
(editors usually start it in the workspace folder); without one, pass
--repo, and only graft: documents get features.";

/// Runs the LSP server over stdio.
pub(crate) fn run(args: &[String]) -> Exit {
    let g = match parse(args, global()) {
        Ok(Some(g)) => g,
        Ok(None) => {
            println!("{USAGE}");
            return Exit::Ok;
        }
        Err(f) => return f.report(false),
    };
    match serve(&g) {
        Ok(Ended::Clean) => Exit::Ok,
        Ok(Ended::Unclean) => Exit::Failure,
        Err(f) => f.report(false),
    }
}

fn serve(g: &Global) -> Result<Ended, Failure> {
    let wc = working_copy()?;
    runtime()?.block_on(async {
        let conn = connect(g, wc.as_ref()).await?;
        let principal = call_context().principal;
        let server = LspServer::new(conn.remote, conn.repo, wc).with_principal(principal);
        Ok(server.serve_stdio().await?)
    })
}

/// The global options as clap read them: the dispatcher passes this
/// command only the arguments clap left over, and clap takes global options
/// wherever they appear.
fn global() -> Global {
    Cli::try_parse().map_or_else(|_| empty(), |cli| cli.global)
}

fn empty() -> Global {
    Global {
        json: false,
        endpoint: None,
        repo: None,
        token_file: None,
    }
}

/// The command's options over the global ones; `None` for `--help`.
fn parse(args: &[String], mut g: Global) -> Result<Option<Global>, Failure> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Failure::usage(format!("graft lsp: {name} needs a value")))
        };
        match name {
            "-h" | "--help" => return Ok(None),
            "--endpoint" => g.endpoint = Some(value()?),
            "--repo" => g.repo = Some(value()?),
            "--token-file" => g.token_file = Some(PathBuf::from(value()?)),
            // Editors pass `--stdio` by convention; stdio is the only transport.
            "--stdio" => {}
            other => {
                return Err(Failure::usage(format!(
                    "graft lsp: unknown argument {other:?}\n{USAGE}"
                )));
            }
        }
    }
    Ok(Some(g))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn options_parse_in_both_forms() {
        let g = parse(
            &args(&[
                "--endpoint",
                "http://h:1",
                "--repo=r",
                "--token-file",
                "/t",
                "--stdio",
            ]),
            empty(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(g.endpoint.as_deref(), Some("http://h:1"));
        assert_eq!(g.repo.as_deref(), Some("r"));
        assert_eq!(g.token_file, Some(PathBuf::from("/t")));
        assert!(!g.json);
        assert!(parse(&args(&["--help"]), empty()).unwrap().is_none());
        // Options clap already took stay unless given again.
        let base = Global {
            repo: Some("r".into()),
            ..empty()
        };
        let g = parse(&args(&["--stdio"]), base).unwrap().unwrap();
        assert_eq!(g.repo.as_deref(), Some("r"));
    }

    #[test]
    fn bad_options_are_usage_errors() {
        for bad in [&["--repo"][..], &["--repo="], &["--token"], &["serve"]] {
            let e = parse(&args(bad), empty()).expect_err("refused");
            assert_eq!(e.exit, Exit::Usage, "{bad:?}");
        }
    }
}
