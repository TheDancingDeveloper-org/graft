//! Generated shell completion and manual-page acceptance tests.

mod support;

use std::fs;

use support::{assert_no_internal_references, graft, scratch};

const HIDDEN_COMMANDS: &[&str] = &["attest", "ci", "serve", "node", "mirror", "man"];
const HIDDEN_DESCRIPTIONS: &[&str] = &[
    "Record a check result for a changeset",
    "CI helpers",
    "Run the authoritative server",
    "Run the host cache daemon",
    "Import, follow, batch or export git repositories",
    "Generate manual pages",
    "Commands designed but not built yet",
];

#[test]
fn completions_cover_public_commands_and_options() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let dir = scratch();
        let output = graft(dir.path())
            .args(["completions", shell])
            .output()
            .expect("completion command runs");
        assert!(output.status.success(), "{shell}: {output:?}");
        let script = String::from_utf8(output.stdout).expect("completion script is text");
        assert!(!script.is_empty(), "{shell} completion is empty");
        for expected in ["land", "sync"] {
            assert!(
                script.contains(expected),
                "{shell} completion lacks {expected}"
            );
        }
        let json_option = if shell == "fish" { "-l json" } else { "--json" };
        assert!(
            script.contains(json_option),
            "{shell} completion lacks --json"
        );
        for hidden in HIDDEN_DESCRIPTIONS {
            assert!(
                !script.contains(hidden),
                "{shell} completion exposed hidden command {hidden}"
            );
        }
        assert_no_internal_references(&["completions", shell], &script);
    }
}

#[test]
fn man_writes_root_and_every_visible_subcommand() {
    let dir = scratch();
    let out = dir.path().join("man");
    graft(dir.path())
        .args(["man", "--out"])
        .arg(&out)
        .assert()
        .success();

    let root = fs::read_to_string(out.join("graft.1")).expect("root man page");
    let land = fs::read_to_string(out.join("graft-land.1")).expect("land man page");
    assert!(!root.is_empty());
    assert!(!land.is_empty());
    assert_no_internal_references(&["man"], &root);
    assert_no_internal_references(&["man", "land"], &land);

    for hidden in HIDDEN_COMMANDS {
        assert!(
            !out.join(format!("graft-{hidden}.1")).exists(),
            "wrote a page for hidden command {hidden}"
        );
        assert!(
            !root.contains(&format!("graft\\-{hidden}(1)")),
            "root page exposed hidden command {hidden}"
        );
    }

    for visible in public_subcommands() {
        let page = out.join(format!("graft-{visible}.1"));
        let contents =
            fs::read_to_string(&page).unwrap_or_else(|error| panic!("{}: {error}", page.display()));
        assert!(!contents.is_empty(), "{} is empty", page.display());
        assert_no_internal_references(&["man", &visible], &contents);
    }
}

fn public_subcommands() -> Vec<String> {
    let dir = scratch();
    let output = graft(dir.path())
        .arg("--help")
        .output()
        .expect("help command runs");
    let help = String::from_utf8(output.stdout).expect("help is text");
    help.lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect()
}
