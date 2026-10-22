//! One test per path rule and per portability rule (design §4.2).

mod common;

use common::{MemSource, block_on, build, path, put};
use graft_model::changeset::Rule;
use graft_model::path::{MAX_COMPONENT_BYTES, MAX_PATH_BYTES};
use graft_model::tree_ops::{ApplyError, ApplyOptions};
use graft_model::{PathError, Portability, RepoPath, apply_changes, apply_changes_with};

#[test]
fn accepts_ordinary_paths() {
    for p in [
        "a",
        "a/b/c.rs",
        "src/main.rs",
        ".github/x.yml",
        "ünïcødé/文件",
        "a b/c d",
    ] {
        assert_eq!(RepoPath::new(p).unwrap().as_str(), p);
    }
}

#[test]
fn rule_root_is_not_a_path() {
    assert_eq!(RepoPath::new(""), Err(PathError::Empty));
}

#[test]
fn rule_no_leading_slash() {
    assert_eq!(RepoPath::new("/a"), Err(PathError::EmptyComponent));
}

#[test]
fn rule_no_trailing_slash() {
    assert_eq!(RepoPath::new("a/"), Err(PathError::EmptyComponent));
}

#[test]
fn rule_no_empty_component() {
    assert_eq!(RepoPath::new("a//b"), Err(PathError::EmptyComponent));
    assert_eq!(RepoPath::new("/"), Err(PathError::EmptyComponent));
}

#[test]
fn rule_no_dot_component() {
    assert_eq!(RepoPath::new("."), Err(PathError::DotComponent));
    assert_eq!(RepoPath::new("a/./b"), Err(PathError::DotComponent));
}

#[test]
fn rule_no_dot_dot_component() {
    assert_eq!(RepoPath::new(".."), Err(PathError::DotComponent));
    assert_eq!(RepoPath::new("a/../b"), Err(PathError::DotComponent));
    // Dots inside a name are fine.
    assert!(RepoPath::new("a/...").is_ok());
    assert!(RepoPath::new("..a").is_ok());
}

#[test]
fn rule_no_nul() {
    assert_eq!(RepoPath::new("a\0b"), Err(PathError::Nul));
}

#[test]
fn rule_component_at_most_255_bytes() {
    let ok = "x".repeat(MAX_COMPONENT_BYTES);
    assert!(RepoPath::new(&ok).is_ok());
    let long = "x".repeat(MAX_COMPONENT_BYTES + 1);
    assert_eq!(
        RepoPath::new(&format!("a/{long}")),
        Err(PathError::ComponentTooLong)
    );
    // Bytes, not characters: 128 two-byte characters is 256 bytes.
    assert_eq!(
        RepoPath::new(&"é".repeat(128)),
        Err(PathError::ComponentTooLong)
    );
}

#[test]
fn rule_path_at_most_4096_bytes() {
    let mut p = format!("{}bb", "a/".repeat(2047));
    assert_eq!(p.len(), MAX_PATH_BYTES);
    assert!(RepoPath::new(&p).is_ok());
    p.push('y');
    assert_eq!(RepoPath::new(&p), Err(PathError::TooLong));
}

#[test]
fn rule_utf8_only() {
    assert_eq!(RepoPath::from_bytes(b"a/\xff"), Err(PathError::NotUtf8));
    assert_eq!(RepoPath::from_bytes(b"a/b").unwrap().as_str(), "a/b");
}

#[test]
fn paths_are_case_sensitive_and_sort_by_bytes() {
    assert_ne!(path("A"), path("a"));
    let mut v = vec![path("a/b"), path("a.txt"), path("B"), path("a")];
    v.sort();
    let s: Vec<_> = v.iter().map(RepoPath::as_str).collect();
    assert_eq!(s, ["B", "a", "a.txt", "a/b"]);
}

#[test]
fn helpers() {
    let p = path("a/b/c.rs");
    assert_eq!(p.name(), "c.rs");
    assert_eq!(p.parent(), Some(path("a/b")));
    assert_eq!(path("a").parent(), None);
    assert!(p.starts_with(&path("a/b")));
    assert!(!path("a/bc").starts_with(&path("a/b")));
    assert_eq!(path("a").join("b"), Ok(path("a/b")));
    assert_eq!(path("a").join(".."), Err(PathError::DotComponent));
    assert_eq!(path("a").join("b/c"), Err(PathError::EmptyComponent));
}

#[test]
fn portability_reserved_device_names() {
    for p in [
        "CON",
        "aux.rs",
        "src/nul",
        "Com1.txt",
        "lpt9",
        "src/prn.tar.gz",
        "CONIN$",
    ] {
        assert_eq!(
            path(p).check_portable(),
            Err(PathError::NotPortable(Portability::ReservedName)),
            "{p}"
        );
    }
    for p in ["console", "auxiliary.rs", "com10", "src/connect"] {
        assert!(path(p).check_portable().is_ok(), "{p}");
    }
}

#[test]
fn portability_reserved_characters() {
    for p in [
        "a:b", "a<b", "a>b", "a\"b", "a\\b", "a|b", "a?b", "a*b", "a\u{1}b",
    ] {
        assert_eq!(
            path(p).check_portable(),
            Err(PathError::NotPortable(Portability::ReservedChar)),
            "{p:?}"
        );
    }
}

#[test]
fn portability_trailing_dot_or_space() {
    for p in ["a.", "dir /x", "x/y "] {
        assert_eq!(
            path(p).check_portable(),
            Err(PathError::NotPortable(Portability::TrailingDotOrSpace)),
            "{p:?}"
        );
    }
}

#[test]
fn portability_case_clash_within_a_directory() {
    let source = MemSource::default();
    let root = build(
        &source,
        &[(
            path("src/Readme.md"),
            (graft_model::EntryKind::File, common::blob("x")),
        )]
        .into(),
    );
    let err = block_on(apply_changes(&source, root, &[put("src/README.md", "y")])).unwrap_err();
    match err {
        ApplyError::Invalid(v) => {
            assert_eq!(v.path, Some(path("src/README.md")));
            assert_eq!(
                v.rule,
                Rule::Path(PathError::NotPortable(Portability::CaseClash))
            );
            assert_eq!(v.rule.as_str(), "portability");
        }
        other => panic!("unexpected {other:?}"),
    }
    // A directory that clashes with a file.
    let err = block_on(apply_changes(&source, root, &[put("SRC/a", "y")])).unwrap_err();
    assert!(matches!(err, ApplyError::Invalid(v) if v.path == Some(path("SRC"))));
    // Same names in different directories are fine.
    assert!(block_on(apply_changes(&source, root, &[put("other/README.md", "y")])).is_ok());
    // The check can be turned off.
    let opts = ApplyOptions { case_clash: false };
    assert!(
        block_on(apply_changes_with(
            &source,
            root,
            &[put("src/README.md", "y")],
            &opts
        ))
        .is_ok()
    );
}

#[test]
fn path_errors_have_stable_reasons() {
    assert_eq!(PathError::Empty.reason(), "empty");
    assert_eq!(
        PathError::NotPortable(Portability::ReservedName).reason(),
        "portability"
    );
    assert_eq!(Portability::CaseClash.as_str(), "case-clash");
}
