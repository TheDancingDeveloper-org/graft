//! `graft-pilot`: runs the pilot harness by hand (work item G1.16).
//!
//! ```text
//! cargo build --release -p graft-server -p graft-mirror -p graft-cli -p graft-testkit
//! target/release/graft-pilot --git ~/src/rustTorrent --work /tmp/pilot --out pilot-results \
//!     --runs 3 --iterations 20 [--ref refs/heads/main] [--sparse-path P] [--hot-file P] \
//!     [--verify-limit N] [--crash N]
//! target/release/graft-pilot --fixture --work /tmp/pilot --out pilot-results
//! ```
//!
//! The binaries are found beside this one (or through `GRAFT_SERVER_BIN`,
//! `GRAFT_MIRROR_BIN`, `GRAFT_CLI_BIN`). Prints the report path and exits
//! non-zero if a fidelity, agreement or crash check failed.

use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    use graft_testkit::pilot::fixture::FixtureSpec;
    use graft_testkit::pilot::{PilotConfig, Source, run_pilot};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut source = None;
    let mut work = std::env::temp_dir().join("graft-pilot");
    let mut out = PathBuf::from("pilot-results");
    let mut cfg_edits: Vec<(String, String)> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned();
        match a.as_str() {
            "--fixture" => source = Some(Source::Fixture(FixtureSpec::default())),
            "--git" => source = value().map(Source::Git),
            "--work" => work = value().map(PathBuf::from).unwrap_or(work),
            "--out" => out = value().map(PathBuf::from).unwrap_or(out),
            "--ref" | "--repo" | "--runs" | "--iterations" | "--sparse-path" | "--hot-file"
            | "--verify-limit" | "--crash" => match value() {
                Some(v) => cfg_edits.push((a.clone(), v)),
                None => return usage(&format!("{a} needs a value")),
            },
            _ => return usage(&format!("unknown argument {a}")),
        }
    }
    let Some(source) = source else {
        return usage("pass --git <path-or-url> or --fixture");
    };
    let mut cfg = PilotConfig::new(source, &work, &out);
    for (k, v) in cfg_edits {
        let num = || {
            v.parse::<usize>()
                .map_err(|_| format!("{k} takes a number"))
        };
        let r = match k.as_str() {
            "--ref" => {
                cfg.git_ref = v.clone();
                Ok(())
            }
            "--repo" => {
                cfg.repo = v.clone();
                Ok(())
            }
            "--sparse-path" => {
                cfg.sparse_path = Some(v.clone());
                Ok(())
            }
            "--hot-file" => {
                cfg.hot_file = Some(v.clone());
                Ok(())
            }
            "--runs" => num().map(|n| cfg.runs = n),
            "--iterations" => num().map(|n| cfg.iterations = n),
            "--verify-limit" => num().map(|n| cfg.verify_limit = Some(n)),
            _ => num().map(|n| cfg.crash_restarts = n),
        };
        if let Err(e) = r {
            return usage(&e);
        }
    }
    match run_pilot(&cfg) {
        Ok(o) => {
            println!("results: {}", o.tsv.display());
            println!("report:  {}", o.report.display());
            let crash_ok = o.crash.as_ref().is_none_or(|c| c.passed());
            let ok = o.fidelity.failed == 0 && o.agreement.log && o.agreement.diff && crash_ok;
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("graft-pilot: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!(
        "graft-pilot: {msg}\nusage: graft-pilot (--git <path-or-url> | --fixture) [--work DIR] [--out DIR] [--ref REF] [--repo NAME] [--runs N] [--iterations N] [--sparse-path P] [--hot-file P] [--verify-limit N] [--crash N]"
    );
    ExitCode::from(64)
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    usage("the pilot harness runs on Unix only")
}
