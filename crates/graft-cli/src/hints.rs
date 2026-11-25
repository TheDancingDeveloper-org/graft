//! "Did you mean" hints for flags that other tools (git above all) have and
//! graft spells differently, or does not have. Agents and people type them by
//! habit; a usage error that names the graft form saves a round trip.

use clap::error::{ContextKind, ContextValue, ErrorKind};

/// What to say for a flag: the graft equivalent, or why there is none.
/// `(command, flag, hint)`; a flag is the text clap reports, `-n` or `--check`.
const HINTS: &[(&str, &str, &str)] = &[
    ("log", "--max-count", "use `--limit N` (or `-n N`)"),
    (
        "log",
        "--oneline",
        "`graft log` already prints one line per changeset",
    ),
    (
        "diff",
        "--check",
        "graft has no whitespace check; `graft diff --stat` counts lines per file",
    ),
    (
        "diff",
        "--cached",
        "graft has no staging area; `graft diff` compares the working copy with its base",
    ),
    (
        "diff",
        "--staged",
        "graft has no staging area; `graft diff` compares the working copy with its base",
    ),
    (
        "diff",
        "--name-status",
        "use `--name-only` for paths, or `--json` for each file's status",
    ),
    (
        "diff",
        "--numstat",
        "use `--stat` for added and removed line counts per file",
    ),
    (
        "diff",
        "--shortstat",
        "use `--stat` for added and removed line counts per file",
    ),
    (
        "status",
        "--short",
        "`graft status` is already compact; add `--json` for a machine-readable list",
    ),
    (
        "status",
        "-s",
        "`graft status` is already compact; add `--json` for a machine-readable list",
    ),
    (
        "status",
        "--porcelain",
        "`graft status` is already compact; add `--json` for a machine-readable list",
    ),
    (
        "status",
        "--workspace",
        "status takes no workspace flag; run it in the working copy, or use `-C <dir>`",
    ),
    (
        "land",
        "--line",
        "land goes to the working copy's own line; to merge into another line use `graft line merge <src> --into <line>`",
    ),
    (
        "land",
        "--no-verify",
        "use `--no-check` to skip the pre-land check",
    ),
    (
        "land",
        "--force",
        "graft land never overwrites; on a conflict use `--sync` to write the conflicts into the files",
    ),
    ("cat", "-r", "use `--at <REV>`"),
    ("cat", "--rev", "use `--at <REV>`"),
    ("cat", "--revision", "use `--at <REV>`"),
];

/// The hint for a clap usage error, when it is an unknown flag of one of the
/// commands in [`HINTS`]. `args` are the command line without the program name.
pub(crate) fn for_error(e: &clap::Error, args: &[String]) -> Option<String> {
    if e.kind() != ErrorKind::UnknownArgument {
        return None;
    }
    let flag = match e.get(ContextKind::InvalidArg)? {
        ContextValue::String(text) => text.split([' ', '=']).next()?.to_owned(),
        _ => return None,
    };
    let command = command_of(args)?;
    lookup(command, &flag).map(|h| format!("did you mean: {h}"))
}

fn lookup(command: &str, flag: &str) -> Option<&'static str> {
    HINTS
        .iter()
        .find(|(c, f, _)| *c == command && *f == flag)
        .map(|(_, _, hint)| *hint)
}

/// The subcommand on the line: the first word that is not a global flag or
/// that flag's value.
fn command_of(args: &[String]) -> Option<&str> {
    const WITH_VALUE: &[&str] = &[
        "--endpoint",
        "--repo",
        "--token-file",
        "--server",
        "-C",
        "--directory",
    ];
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if WITH_VALUE.contains(&arg.as_str()) {
            it.next();
        } else if !arg.starts_with('-') {
            return Some(arg.as_str());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn command_skips_global_flags_and_their_values() {
        assert_eq!(
            command_of(&words("--endpoint http://x --json diff --check")),
            Some("diff")
        );
        assert_eq!(command_of(&words("--json")), None);
    }

    #[test]
    fn lookup_is_per_command() {
        assert!(lookup("cat", "-r").is_some());
        assert!(lookup("log", "-r").is_none());
    }
}
