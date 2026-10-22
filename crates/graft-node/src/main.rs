//! `graft-node`: one cache daemon per host (`docs/design.md` §5.1, work item
//! G2.3). Serves the object API from a bounded disk cache over a Unix socket,
//! single-flights identical fetches, and forwards repository calls upstream.
//!
//! The config file is `--config <file>`, else `$GRAFT_NODE_CONFIG`, else
//! `/etc/graft/node.toml`.

use std::path::PathBuf;
use std::process::ExitCode;

use graft_node::config::{DEFAULT_CONFIG, NodeConfig};

const USAGE: &str = "usage: graft-node [--config <file>]

config: --config <file>, else $GRAFT_NODE_CONFIG, else /etc/graft/node.toml";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    match graft_node::daemon::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("graft-node: {e}");
            ExitCode::FAILURE
        }
    }
}
