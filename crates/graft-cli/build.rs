//! Embeds source revision metadata in the `graft` binary.

use std::process::Command;

fn main() {
    if let Some(git_dir) = git_output(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }

    let describe = git_output(&["describe", "--always", "--dirty", "--abbrev=8"])
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=GRAFT_GIT_DESCRIBE={describe}");
}

fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_owned())
        .filter(|output| !output.is_empty())
}
