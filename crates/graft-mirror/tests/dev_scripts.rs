//! `scripts/dev/reload.sh` knows the binary for every graft service in the dev
//! `compose.yaml`, so a new service (such as another `graft-mirror-*` profile)
//! cannot fall outside the reload path and its loud mirror check.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// Service names under the top-level `services:` key of `compose.yaml`.
fn compose_services(text: &str) -> Vec<String> {
    let mut in_services = false;
    let mut names = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            in_services = line.trim_end() == "services:";
            continue;
        }
        if !in_services {
            continue;
        }
        if let Some(name) = line
            .strip_prefix("  ")
            .filter(|rest| !rest.starts_with(' ') && !rest.starts_with('#'))
            .and_then(|rest| rest.trim_end().strip_suffix(':'))
        {
            names.push(name.to_owned());
        }
    }
    names
}

fn reload_bin(root: &Path, service: &str) -> std::process::Output {
    Command::new("bash")
        .arg(root.join("scripts/dev/reload.sh"))
        .args(["--bin", service])
        .output()
        .expect("bash runs")
}

#[test]
fn reload_maps_every_graft_service_in_compose() {
    let root = repo_root();
    let text = fs::read_to_string(root.join("compose.yaml")).expect("compose.yaml");
    let graft: Vec<String> = compose_services(&text)
        .into_iter()
        .filter(|s| s.starts_with("graft-"))
        .collect();
    assert!(
        graft.iter().any(|s| s == "graft-mirror-rusttorrent"),
        "expected graft-mirror-rusttorrent among {graft:?}"
    );
    for svc in &graft {
        let out = reload_bin(&root, svc);
        assert!(
            out.status.success(),
            "reload.sh has no binary for {svc}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bin = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if svc.starts_with("graft-mirror") {
            assert_eq!(bin, "graft-mirror", "{svc}");
        }
    }
}

#[test]
fn reload_refuses_a_non_graft_service() {
    let out = reload_bin(&repo_root(), "prometheus");
    assert_eq!(out.status.code(), Some(64));
}
