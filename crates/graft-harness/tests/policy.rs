//! The planted-bugs hooks never reach a normal build (§3.4): no workspace
//! crate but the harness resolves `graft-engine` or `graft-gc` with the
//! feature, on its normal (non-dev) dependency edges.

use std::process::Command;

#[test]
fn no_shipped_crate_is_built_with_planted_bugs() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(cargo)
        .args([
            "tree",
            "--workspace",
            "--exclude",
            "graft-harness",
            "-e",
            "normal,features",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8_lossy(&out.stdout);
    for name in [
        "graft-server",
        "graft-cli",
        "graft-node",
        "graft-engine",
        "graft-gc",
    ] {
        assert!(tree.contains(name), "the tree names {name}");
    }
    assert!(
        !tree.contains("planted-bugs"),
        "a shipped crate pulls in planted-bugs:\n{}",
        tree.lines()
            .filter(|l| l.contains("planted-bugs"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
