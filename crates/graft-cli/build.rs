//! Embeds source revision metadata in the `graft` binary.

use std::process::Command;

#[path = "src/build_rev.rs"]
mod build_rev;

fn main() {
    let describe = build_rev::revision(std::env::var("GRAFT_BUILD_REV").ok(), git_describe());
    println!("cargo:rustc-env=GRAFT_GIT_DESCRIBE={describe}");
    println!("cargo:rerun-if-env-changed=GRAFT_BUILD_REV");
}

fn git_describe() -> Option<String> {
    if let Some(git_dir) = git_output(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }
    git_output(&["describe", "--always", "--dirty", "--abbrev=8"])
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
