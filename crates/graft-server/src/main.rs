//! `graft-server`: the authoritative graft server (`docs/design.md` §5.1).
//!
//! `serve` runs the gRPC services over the SQLite and filesystem stores,
//! with bearer-token auth, `/metrics`, `/healthz`, `/readyz`, the read-only
//! JSON facade and the MCP endpoint `/mcp` on the same listener. `admin`
//! holds the operator commands. The config file is `--config <file>`, else
//! `$GRAFT_SERVER_CONFIG`, else `/etc/graft/server.toml`.

mod admin;
mod admission;
mod app;
mod backup;
mod ci;
mod config;
mod denylist;
mod exports;
mod fsck;
mod grpc;
mod http;
mod keys;
mod mcp;
mod mint;
mod serve;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;

use config::ServerConfig;

const USAGE: &str = "usage: graft-server [--config <file>] <serve|admin> [args]
  serve                          run the server
  admin token mint --subject <s> --scope <scope>... [--ttl <secs>]
        [--kind human|agent|ci|service] [--session <ses_…>] [--work-item <WI-…>]
                                 mint a token (printed to stdout)
  admin token revoke             revoke the token read from stdin (server running)
  admin repo create <name>       create a repository
  admin fsck <repo>              verify every referenced object (server stopped)
  admin backup <dir> [--objects] snapshot metadata (and copy objects)
  admin gc <repo> [--dry-run]    collect orphans (not in this release)

config: --config <file>, else $GRAFT_SERVER_CONFIG, else /etc/graft/server.toml";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match take_config(&mut args) {
        Ok(path) => path,
        Err(msg) => return usage(&msg),
    };
    let (command, rest) = match args.split_first() {
        Some((c, rest)) => (c.as_str(), rest),
        None => return usage(USAGE),
    };
    if !matches!(command, "serve" | "admin") {
        return usage(USAGE);
    }
    if command == "serve" && !rest.is_empty() {
        return usage(USAGE);
    }
    let config = match ServerConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("graft-server: {e}");
            return ExitCode::from(78);
        }
    };
    match command {
        "serve" => match serve::serve(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("graft-server: {e}");
                ExitCode::FAILURE
            }
        },
        _ => match admin::run(&config, rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(admin::Failure(msg, code)) => {
                eprintln!("graft-server: {msg}");
                ExitCode::from(code)
            }
        },
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(64)
}

/// Removes `--config <file>` (or `--config=<file>`) from `args`, wherever it
/// is, and returns the config path to use.
fn take_config(args: &mut Vec<String>) -> Result<PathBuf, String> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config" {
            if i + 1 >= args.len() {
                return Err("--config needs a file".into());
            }
            found = Some(args.remove(i + 1));
            args.remove(i);
        } else if let Some(path) = args[i].strip_prefix("--config=") {
            found = Some(path.to_owned());
            args.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(found
        .or_else(|| std::env::var("GRAFT_SERVER_CONFIG").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(config::DEFAULT_CONFIG)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn config_flag_is_taken_from_anywhere() {
        let mut a = strs(&["admin", "--config", "/c.toml", "fsck", "r"]);
        assert_eq!(take_config(&mut a).expect("ok"), PathBuf::from("/c.toml"));
        assert_eq!(a, strs(&["admin", "fsck", "r"]));
        let mut a = strs(&["serve", "--config=/d.toml"]);
        assert_eq!(take_config(&mut a).expect("ok"), PathBuf::from("/d.toml"));
        assert_eq!(a, strs(&["serve"]));
        assert!(take_config(&mut strs(&["serve", "--config"])).is_err());
    }
}
