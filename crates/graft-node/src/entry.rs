//! `graft-node`: one cache daemon per host (`docs/design.md` §5.1, work item
//! G2.3). Serves the object API from a bounded disk cache over a Unix socket,
//! single-flights identical fetches, and forwards repository calls upstream.
//!
//! The config file is `--config <file>`, else `$GRAFT_NODE_CONFIG`, else
//! `/etc/graft/node.toml`.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::{DEFAULT_CONFIG, NodeConfig};

const USAGE: &str = "usage: graft-node [--config <file>]

config: --config <file>, else $GRAFT_NODE_CONFIG, else /etc/graft/node.toml";

/// Runs the node daemon with arguments after the executable name.
pub fn run(args: Vec<String>) -> ExitCode {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let path = match args.as_slice() {
        [] => std::env::var_os("GRAFT_NODE_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG)),
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(64);
        }
    };
    let config = match NodeConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("graft-node: {e}");
            return ExitCode::from(78);
        }
    };
    match crate::daemon::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("graft-node: {e}");
            ExitCode::FAILURE
        }
    }
}
