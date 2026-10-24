//! The planted-bugs hooks never reach a normal build (§3.4): `graft-server`
//! resolves `graft-engine` and `graft-gc` without the feature.

use std::process::Command;

#[test]
fn graft_server_is_built_without_planted_bugs() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(cargo)
        .args([
            "tree",
            "--offline",
            "-p",
            "graft-server",
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
    assert!(tree.contains("graft-engine"), "the tree names graft-engine");
    assert!(
        !tree.contains("planted-bugs"),
        "graft-server pulls in planted-bugs:\n{}",
        tree.lines()
            .filter(|l| l.contains("planted-bugs"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
