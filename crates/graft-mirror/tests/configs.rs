//! Every checked-in mirror config under `docker/` parses with the crate's real
//! parser, so a schema change cannot leave a dev stack config behind. A mirror
//! config is any `*.toml` with a `mirror` directory in its path (for example
//! `docker/dev/mirror/rusttorrent.toml`).

use std::fs;
use std::path::{Path, PathBuf};

use graft_mirror::config::FileConfig;

/// The repository's `docker/` directory, found from this crate's manifest.
fn docker_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docker")
        .canonicalize()
        .expect("docker/ exists at the repository root")
}

/// Collects `*.toml` files under `dir` whose path has a `mirror` component.
fn mirror_configs(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            mirror_configs(&path, found);
        } else if path.extension().is_some_and(|e| e == "toml")
            && path
                .parent()
                .into_iter()
                .flat_map(Path::components)
                .any(|c| c.as_os_str() == "mirror")
        {
            found.push(path);
        }
    }
}

#[test]
fn every_checked_in_mirror_config_parses() {
    let mut found = Vec::new();
    mirror_configs(&docker_dir(), &mut found);
    found.sort();
    assert!(
        !found.is_empty(),
        "no mirror configs found under docker/; the test would pass vacuously"
    );
    let refused: Vec<String> = found
        .iter()
        .filter_map(|path| {
            let text = fs::read_to_string(path).expect("readable config");
            FileConfig::parse(&text)
                .err()
                .map(|e| format!("{}: {e}", path.display()))
        })
        .collect();
    assert!(
        refused.is_empty(),
        "graft-mirror refuses these configs:\n{}",
        refused.join("\n")
    );
}

#[test]
fn the_old_source_section_is_refused() {
    // The schema this test guards against: `[source]` and `[graft]` sections
    // from before the follow config landed.
    let old = "[source]\nurl = \"https://example.invalid/r.git\"\nbranch = \"main\"\n\n\
               [graft]\nserver = \"http://graft-server:7878\"\nrepo = \"r\"\nline = \"main\"\n";
    assert!(FileConfig::parse(old).is_err());
}
