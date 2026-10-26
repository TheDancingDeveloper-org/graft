//! Generated command reference: `docs/guide/cli.md` must match live `--help`.
//!
//! `GRAFT_UPDATE_DOCS=1 cargo nextest run -p graft-cli --test cli_reference`
//! rewrites the page. Hidden commands stay out.

mod support;

use std::path::PathBuf;

use support::{graft, scratch};

const PAGE: &str = "docs/guide/cli.md";
const BANNER: &str =
    "<!-- generated from `graft --help`; do not edit. Regenerate with GRAFT_UPDATE_DOCS=1. -->";

/// One visible command, addressed by the words that follow `graft`.
struct Entry {
    words: Vec<String>,
    help: String,
}

#[test]
fn guide_cli_matches_help() {
    let entries = visible_entries();
    let rendered = render(&entries);
    let repo = repo_root();
    let path = repo.join(PAGE);
    if std::env::var_os("GRAFT_UPDATE_DOCS").is_some() {
        std::fs::write(&path, &rendered).expect("rewrite the command reference");
        return;
    }
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        current, rendered,
        "docs/guide/cli.md is stale. Regenerate it with GRAFT_UPDATE_DOCS=1."
    );
}

/// Walk every visible command and nested subcommand, skipping hidden ones.
fn visible_entries() -> Vec<Entry> {
    let mut entries = Vec::new();
    walk(&[], &mut entries);
    entries
}

fn walk(words: &[String], entries: &mut Vec<Entry>) {
    let Some(help) = help_of(words) else {
        return;
    };
    if !words.is_empty() {
        entries.push(Entry {
            words: words.to_vec(),
            help: help.clone(),
        });
    }
    for name in subcommands_of(&help) {
        let mut next = words.to_vec();
        next.push(name);
        walk(&next, entries);
    }
}

/// `Ok` is a real command. `None` is a positional clap listed under
/// `Commands:` (for example `config help`), which exits 2.
fn help_of(words: &[String]) -> Option<String> {
    let dir = scratch();
    let mut command = graft(dir.path());
    command.args(words).arg("--help");
    let output = command.output().expect("graft --help runs");
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).expect("help is utf-8"))
}

/// Names under a `Commands:` heading. A name that starts with `-` is an option
/// group clap prints there (for example stack's default), not a subcommand.
fn subcommands_of(help: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.is_empty() || (!line.starts_with(' ') && !line.starts_with('\t')) {
            break;
        }
        let trimmed = line.trim();
        let name = trimmed.split_whitespace().next().unwrap_or("");
        if name.is_empty() || name.starts_with('-') {
            continue;
        }
        names.push(name.to_owned());
    }
    names
}

fn render(entries: &[Entry]) -> String {
    let mut out = String::new();
    out.push_str(BANNER);
    out.push_str("\n\n# Command reference\n\n");
    out.push_str("Generated from `graft --help`. Hidden commands are left out.\n\n");
    out.push_str("## Index\n\n");
    for entry in entries {
        let heading = entry.words.join(" ");
        let anchor = heading.replace(' ', "-");
        out.push_str(&format!("- [`graft {heading}`](#{anchor})\n"));
    }
    out.push('\n');
    for entry in entries {
        let heading = entry.words.join(" ");
        out.push_str(&format!("## `graft {heading}`\n\n```text\n"));
        out.push_str(entry.help.trim_end());
        out.push_str("\n```\n\n");
    }
    out
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
