//! Every API wrapper in graft-mirror forwards every method of the trait it
//! implements (WI-2630). A wrapper that leaves a method to the trait default
//! answers `unimplemented` (or `Unavailable`) even when the server behind it
//! serves the RPC, as `GetLineConfig` once did. The traits' defaults stop the
//! compiler catching that, so this reads the sources: the trait methods from
//! graft-proto's `api.rs`, and each `impl <Trait> for <Wrapper>` block in
//! this crate's `src/`, counting `fn name(` and macro lines `name(Req) -> Out;`.
//!
//! A `RepoApi` impl that uses `graft_proto::forward_repo_api!` (or a
//! `WorkspaceApi` impl with `forward_workspace_api!`) forwards every
//! method by construction (WI-2638, WI-2639): leaving one out fails to compile, and
//! graft-proto's own test pins the macro's list to the trait. Those impls
//! still count as seen here; the other traits are still read from source.

use std::collections::BTreeSet;
use std::path::Path;

const TRAITS: [&str; 4] = ["ObjectApi", "RepoApi", "WorkspaceApi", "AdminApi"];

/// The block that opens at the first `{` at or after `start`, without its
/// braces. Braces in these sources balance outside strings and comments.
fn block(src: &str, start: usize) -> &str {
    let open = start + src[start..].find('{').expect("block opens");
    let mut depth = 0usize;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced block at {start}");
}

/// The leading identifier of `s` (letters, digits, `_`).
fn ident(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    &s[..end]
}

/// Method names declared or forwarded at the top level of `body`.
fn methods(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in body.lines().map(str::trim) {
        let rest = line
            .strip_prefix("async fn ")
            .or_else(|| line.strip_prefix("fn "));
        if let Some(rest) = rest {
            out.insert(ident(rest).to_owned());
            continue;
        }
        // A forwarding macro's line: `name(Req) -> Out;`.
        let name = ident(line);
        if !name.is_empty()
            && line[name.len()..].starts_with('(')
            && line.contains(") ->")
            && line.ends_with(';')
        {
            out.insert(name.to_owned());
        }
    }
    out
}

fn trait_methods(api: &str, name: &str) -> BTreeSet<String> {
    let at = api
        .find(&format!("pub trait {name}:"))
        .unwrap_or_else(|| panic!("trait {name} in api.rs"));
    let found = methods(block(api, at));
    assert!(!found.is_empty(), "no methods read for {name}");
    found
}

fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let src = std::fs::read_to_string(&path).unwrap();
            // Test doubles in a `mod tests` may leave methods to defaults.
            let src = match src.find("#[cfg(test)]\nmod tests") {
                Some(at) => src[..at].to_owned(),
                None => src,
            };
            out.push((path.display().to_string(), src));
        }
    }
}

#[test]
fn every_wrapper_forwards_every_api_method() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let api = std::fs::read_to_string(crate_dir.join("../graft-proto/src/api.rs")).unwrap();
    let mut files = Vec::new();
    sources(&crate_dir.join("src"), &mut files);

    let mut seen = BTreeSet::new();
    let mut missing = Vec::new();
    for (path, src) in &files {
        for (at, _) in src.match_indices("\nimpl") {
            let header = &src[at + 1..at + src[at..].find('{').unwrap()];
            let Some(name) = TRAITS
                .iter()
                .find(|t| header.contains(&format!(" {t} for ")))
            else {
                continue;
            };
            let wrapper = ident(header.split(" for ").nth(1).unwrap().trim());
            let want = trait_methods(&api, name);
            let body = block(src, at);
            let have = if (*name == "RepoApi" && body.contains("forward_repo_api!("))
                || (*name == "WorkspaceApi" && body.contains("forward_workspace_api!("))
            {
                trait_methods(&api, name)
            } else {
                methods(body)
            };
            for m in want.difference(&have) {
                missing.push(format!("{path}: {name} for {wrapper} does not forward {m}"));
            }
            seen.insert(format!("{name} for {wrapper}"));
        }
    }
    // The parser found the wrappers that carry the mirror's traffic.
    for expected in [
        "ObjectApi for Server",
        "RepoApi for Server",
        "WorkspaceApi for Server",
        "ObjectApi for Api",
        "RepoApi for Api",
        "AdminApi for Api",
        "ObjectApi for Waiting",
        "RepoApi for Waiting",
        "AdminApi for Waiting",
    ] {
        assert!(seen.contains(expected), "{expected} not found in {seen:?}");
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}
