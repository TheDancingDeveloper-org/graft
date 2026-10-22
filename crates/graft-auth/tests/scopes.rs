//! Scope text forms and `authorize`.
//!
//! Tests that build a `RepoPath` are `#[ignore]`d until G1.1 lands
//! `RepoPath::new`; run them with `cargo test -p graft-auth -- --ignored`.

use graft_auth::{AuthError, Scope, authorize};
use graft_model::RepoPath;
use graft_proto::{Principal, PrincipalKind};

/// The only place these tests construct a path.
fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("valid test path")
}

fn principal(scopes: &[&str]) -> Principal {
    Principal {
        subject: "ses_test".into(),
        kind: PrincipalKind::Agent,
        session: Some("ses_test".into()),
        work_item: None,
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn read(repo: &str) -> Scope {
    Scope::Read { repo: repo.into() }
}

fn write() -> Scope {
    Scope::Write {
        repo: "graft".into(),
        prefix: None,
    }
}

fn land(line: &str) -> Scope {
    Scope::Land {
        repo: "graft".into(),
        line: line.into(),
    }
}

#[test]
fn text_forms_round_trip() {
    for s in [
        "repo:graft:read",
        "repo:graft:write",
        "repo:graft:land:main",
        "repo:graft:land:release/1.0",
        "repo:graft:admin",
        "objects:read",
    ] {
        let scope = Scope::parse(s).unwrap();
        assert_eq!(scope.to_string(), s);
        assert_eq!(s.parse::<Scope>().unwrap(), scope);
    }
    assert_eq!(Scope::parse("repo:graft:read").unwrap(), read("graft"));
    assert_eq!(Scope::parse("repo:graft:land:main").unwrap(), land("main"));
}

#[test]
#[ignore = "needs RepoPath::new (G1.1)"]
fn write_prefix_round_trips() {
    let s = "repo:graft:write:crates/graft-auth";
    let scope = Scope::parse(s).unwrap();
    assert_eq!(
        scope,
        Scope::Write {
            repo: "graft".into(),
            prefix: Some(path("crates/graft-auth")),
        }
    );
    assert_eq!(scope.to_string(), s);
    assert_eq!(
        Scope::parse("repo:graft:write:/abs"),
        Err(AuthError::Malformed)
    );
    assert_eq!(
        Scope::parse("repo:graft:write:a/../b"),
        Err(AuthError::Malformed)
    );
}

#[test]
fn malformed_scopes() {
    for s in [
        "",
        "repo",
        "repo:",
        "repo::read",
        "repo:graft",
        "repo:graft:",
        "repo:graft:read:extra",
        "repo:graft:admin:extra",
        "repo:graft:land",
        "repo:graft:land:",
        "repo:graft:delete",
        "repo:gr aft:read",
        "objects:write",
        "objects:read:x",
        "REPO:graft:read",
    ] {
        assert_eq!(Scope::parse(s), Err(AuthError::Malformed), "{s:?}");
    }
}

#[test]
fn missing_scope_is_denied() {
    let p = principal(&["repo:graft:read"]);
    assert_eq!(authorize(&p, &read("graft"), None), Ok(()));
    assert_eq!(
        authorize(&p, &read("other"), None),
        Err(AuthError::Denied {
            scope: "repo:other:read".into(),
            path: None,
        })
    );
    assert!(authorize(&p, &write(), None).is_err());
    assert!(authorize(&p, &land("main"), None).is_err());
    assert!(authorize(&p, &Scope::ObjectsRead, None).is_err());
    assert!(authorize(&principal(&[]), &read("graft"), None).is_err());
}

#[test]
fn implied_scopes() {
    // write and land imply read on their repository.
    let p = principal(&["repo:graft:write", "repo:graft:land:main"]);
    assert_eq!(authorize(&p, &read("graft"), None), Ok(()));
    assert_eq!(authorize(&p, &write(), None), Ok(()));
    assert!(authorize(&p, &read("other"), None).is_err());

    // land is per line.
    assert_eq!(authorize(&p, &land("main"), None), Ok(()));
    assert!(authorize(&p, &land("release"), None).is_err());

    // admin grants everything on its repository, and nothing elsewhere.
    let admin = principal(&["repo:graft:admin"]);
    assert_eq!(authorize(&admin, &land("anything"), None), Ok(()));
    assert_eq!(authorize(&admin, &write(), None), Ok(()));
    assert!(authorize(&admin, &read("other"), None).is_err());
    assert!(authorize(&admin, &Scope::ObjectsRead, None).is_err());

    // objects:read is granted only by itself.
    let node = principal(&["objects:read"]);
    assert_eq!(authorize(&node, &Scope::ObjectsRead, None), Ok(()));
    assert!(authorize(&node, &read("graft"), None).is_err());
}

#[test]
#[ignore = "needs RepoPath::new (G1.1)"]
fn write_path_prefix() {
    let p = principal(&["repo:graft:write:crates/graft-auth"]);
    for ok in ["crates/graft-auth", "crates/graft-auth/src/lib.rs"] {
        assert_eq!(authorize(&p, &write(), Some(&path(ok))), Ok(()), "{ok}");
    }
    for denied in ["crates/graft-auth-extra/x", "crates", "README.md"] {
        assert_eq!(
            authorize(&p, &write(), Some(&path(denied))),
            Err(AuthError::Denied {
                scope: "repo:graft:write".into(),
                path: Some(denied.into()),
            }),
            "{denied}"
        );
    }
    // A prefixed write never grants a write that names no path.
    assert!(authorize(&p, &write(), None).is_err());
    // An unrestricted write grants any path.
    let any = principal(&["repo:graft:write"]);
    assert_eq!(authorize(&any, &write(), Some(&path("README.md"))), Ok(()));
}
