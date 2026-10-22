//! `graft-server`: the authoritative graft server (`docs/design.md` §5.1).
//!
//! Work item G1.12 wires the gRPC services, auth, telemetry and the stores;
//! this skeleton only fixes the command surface.

mod config;

use std::process::ExitCode;

const USAGE: &str = "usage: graft-server <serve|admin> [args]
  serve --config <file>          run the server
  admin token mint ...           mint a token (operator bootstrap)
  admin repo create <name>       create a repository
  admin fsck <repo>              verify every referenced object
  admin backup <dir>             snapshot metadata
  admin gc <repo> [--dry-run]    collect orphans";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") | Some("admin") => {
            let config = config::ServerConfig::default();
            eprintln!(
                "graft-server: not implemented yet (see docs/plan.md, G1.12); defaults: {} objects={} meta={} min_free={}",
                config.listen,
                config.objects_dir.display(),
                config.meta_db.display(),
                config.min_free_fraction
            );
            ExitCode::from(2)
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(64)
        }
    }
}
