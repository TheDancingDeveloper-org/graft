//! `graft-mirror`: the only service that speaks git (`docs/design.md` §11).
//!
//! Work items G1.14 (import, follow), G2.6 (PR bridge), G3.3 (export),
//! G3.9 (authority switch).

use std::process::ExitCode;

const USAGE: &str = "usage: graft-mirror <command> [args]
  import <git-dir> --repo <name>        lossless import with fidelity check
  follow --config <file>                git-primary: follow GitHub main
  bridge --config <file>                git-primary: land drafts as GitHub PRs
  export --config <file>                graft-primary: export lines to GitHub
  export --to <bare-repo> --repo <name> full export (exit path, drill)
  verify --repo <name> [--last N]       re-export and compare shas
  reconcile --repo <name> --line <l>    resolve a divergence by hand
  authority <status|freeze|verify|flip|unfreeze> --line <l>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("import" | "follow" | "bridge" | "export" | "verify" | "reconcile" | "authority") => {
            eprintln!("graft-mirror: not implemented yet (see docs/plan.md)");
            ExitCode::from(2)
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(64)
        }
    }
}
