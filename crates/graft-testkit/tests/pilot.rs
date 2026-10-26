//! The pilot harness end to end on the synthetic fixture, against the real
//! binaries (G1.16). The long crash suite is `#[ignore]`d: run it with
//! `cargo test -p graft-testkit --release --test pilot -- --ignored`
//! (`GRAFT_CRASH_RESTARTS` overrides its 100 restarts).

#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};

use graft_testkit::bench::Results;
use graft_testkit::crash::TempDir;
use graft_testkit::pilot::fixture::{FixtureSpec, HOT_FILE};
use graft_testkit::pilot::proc::{Bins, Site, Token, run_with};
use graft_testkit::pilot::{PilotConfig, PilotError, Source, crash::crash_suite, run_pilot};

#[test]
fn pilot_runs_end_to_end_on_the_synthetic_fixture() {
    let work = TempDir::new("pilot-work");
    let out = TempDir::new("pilot-out");
    let spec = FixtureSpec {
        commits: 12,
        files: 12,
        lines: 20,
        large_bytes: 2 << 20,
        seed: 16,
    };
    let mut cfg = PilotConfig::new(Source::Fixture(spec), work.path(), out.path());
    cfg.runs = 2;
    cfg.iterations = 3;
    cfg.crash_restarts = 3;
    let o = run_pilot(&cfg).unwrap_or_else(|e| panic!("pilot: {e}"));

    // 12 commits on main plus the two side-branch commits and the merge.
    assert_eq!(o.commits, 15);
    let f = &o.fidelity;
    assert_eq!(f.checked, o.commits, "every changeset is verified");
    assert_eq!(f.failed, 0, "fidelity problems: {:?}", f.problems);
    assert!(f.entries > 0);
    let a = &o.agreement;
    assert!(a.log && a.diff, "graft disagrees with git: {:?}", a.notes);
    assert!(a.blame_lines.1 > 0);
    assert_eq!(
        a.blame_lines.0, a.blame_lines.1,
        "blame disagrees: {:?}",
        a.notes
    );
    let crash = o.crash.as_ref().expect("crash suite ran");
    assert!(crash.passed(), "{}", crash.markdown());
    assert_eq!(crash.restarts, 3);
    assert!(crash.acked_changesets > 0);

    let tsv = std::fs::read_to_string(&o.tsv).expect("results.tsv");
    let results = Results::from_tsv(&tsv).expect("parses");
    for scenario in [
        "full clone",
        "sparse clone",
        "log --path",
        "diff",
        "blame cold",
        "blame cached",
    ] {
        for system in ["graft", "git"] {
            let row = results
                .get(scenario, system)
                .unwrap_or_else(|| panic!("no {scenario} row for {system}"));
            assert_eq!(row.summary.n, 6, "{scenario} {system}: 2 runs × 3 samples");
        }
    }
    let share = results
        .get("decompress share of full clone", "graft")
        .expect("D5 row");
    assert!((0.0..=100.0).contains(&share.summary.p50));
    assert!(results.get("restart to serving", "graft").is_some());
    assert!(out.path().join("results-0.tsv").is_file());
    assert!(out.path().join("results-1.tsv").is_file());

    let report = std::fs::read_to_string(&o.report).expect("report.md");
    assert!(report.contains(HOT_FILE));
    for section in [
        "## Fidelity",
        "## Agreement",
        "## Budgets",
        "## Reproducibility",
        "## Crash suite",
    ] {
        assert!(report.contains(section), "report lacks {section}");
    }
    assert!(
        !report.contains("Bearer"),
        "no token material in the report"
    );
}

#[test]
fn a_step_past_its_deadline_is_killed() {
    let start = Instant::now();
    let err = run_with(
        Command::new("sleep").arg("30"),
        Duration::from_millis(200),
        false,
    )
    .expect_err("times out");
    assert!(matches!(err, PilotError::Timeout(_)), "{err}");
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_dead_server_fails_a_cli_step_without_hanging() {
    let bins = Bins::locate().unwrap_or_else(|e| panic!("{e}"));
    let dir = TempDir::new("pilot-dead");
    let site = Site::new(&bins, dir.path()).expect("site");
    let token: Token = site.mint("t", &["repo:x:read"]).expect("mint");
    let server = site.start().expect("start");
    let url = server.url();
    server.kill9();
    let mut cmd = Command::new(&bins.cli);
    cmd.env("GRAFT_TOKEN", token.expose())
        .current_dir(dir.path())
        .args(["--endpoint", &url, "--repo", "x", "--json", "log"]);
    let ran = run_with(&mut cmd, Duration::from_secs(120), true).expect("bounded");
    assert!(!ran.status.success(), "a dead server is an error");
    let err = String::from_utf8_lossy(&ran.stderr);
    assert!(
        !err.trim().is_empty() || !ran.text().trim().is_empty(),
        "the failure names an outcome, not a clock"
    );
}

#[test]
#[ignore = "100 kill -9 restarts; run by hand or nightly"]
fn long_crash_suite_loses_no_acknowledged_write() {
    let restarts = std::env::var("GRAFT_CRASH_RESTARTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let bins = Bins::locate().unwrap_or_else(|e| panic!("{e}"));
    let dir = TempDir::new("pilot-crash");
    let o = crash_suite(&bins, dir.path(), restarts, 0x6716).unwrap_or_else(|e| panic!("{e}"));
    eprintln!("{}", o.markdown());
    assert_eq!(o.restarts, restarts);
    assert!(o.passed(), "{}", o.markdown());
}
