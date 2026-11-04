//! Who may revoke a token (`docs/design.md` §12.1): the `RevokeToken` RPC.
//!
//! Revocation uses the same denylist as `admin token revoke`, checked on
//! every write. Over the API a caller may revoke a token only if it is
//!
//! - the **service principal that minted it** through `MintToken`, recorded
//!   when the token was minted (by subject, so a service that rotated its own
//!   token still revokes what it minted before), or
//! - an **admin of the token**: it holds `repo:<r>:admin` on every repository
//!   the token names, and the token names at least one. There is no
//!   server-wide admin scope, so a token that names no repository (for
//!   example `objects:read`) has no admin and only its minter or the host
//!   operator revokes it.
//!
//! Everyone else is refused. Tokens minted by `admin token mint` have no
//! minter, so only an admin revokes them over the API.

use graft_auth::{Scope, authorize};
use graft_proto::{ApiError, Principal, PrincipalKind};

/// Why a caller may revoke a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Basis {
    /// The caller is a service principal that minted the token.
    Minter,
    /// The caller administers every repository the token names.
    Admin,
}

impl Basis {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Minter => "minter",
            Self::Admin => "admin",
        }
    }
}

/// Whether `caller` may revoke a token whose scopes are `target_scopes`,
/// given whether `caller` is recorded as its minter.
pub(crate) fn may_revoke(
    caller: &Principal,
    target_scopes: &[String],
    caller_minted_it: bool,
) -> Result<Basis, ApiError> {
    if caller.kind == PrincipalKind::Service && caller_minted_it {
        return Ok(Basis::Minter);
    }
    if is_admin_of(caller, target_scopes) {
        return Ok(Basis::Admin);
    }
    Err(ApiError::PermissionDenied {
        scope: "revoke".into(),
        path: None,
    })
}

fn is_admin_of(caller: &Principal, target_scopes: &[String]) -> bool {
    let mut repos: Vec<String> = target_scopes
        .iter()
        .filter_map(|s| Scope::parse(s).ok())
        .filter_map(|s| s.repo().map(str::to_owned))
        .collect();
    repos.sort_unstable();
    repos.dedup();
    !repos.is_empty()
        && repos
            .into_iter()
            .all(|repo| authorize(caller, &Scope::Admin { repo }, None).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(subject: &str, kind: PrincipalKind, scopes: &[&str]) -> Principal {
        Principal {
            subject: subject.into(),
            kind,
            session: None,
            work_item: None,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn scopes(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_service_revokes_what_it_minted() {
        let vogt = principal("vogt", PrincipalKind::Service, &["repo:r:write"]);
        let target = scopes(&["repo:r:write:src"]);
        assert_eq!(may_revoke(&vogt, &target, true), Ok(Basis::Minter));
        assert!(may_revoke(&vogt, &target, false).is_err());
    }

    #[test]
    fn only_a_service_counts_as_a_minter() {
        // A non-service principal that shares the minter's subject is not
        // the minter.
        let imposter = principal("vogt", PrincipalKind::Agent, &["repo:r:write"]);
        assert!(may_revoke(&imposter, &scopes(&["repo:r:read"]), true).is_err());
    }

    #[test]
    fn an_admin_revokes_a_token_on_repositories_it_administers() {
        let admin = principal(
            "op",
            PrincipalKind::Human,
            &["repo:r:admin", "repo:s:admin"],
        );
        for target in [
            &["repo:r:write"][..],
            &["repo:r:read", "repo:s:land:main"][..],
        ] {
            assert_eq!(may_revoke(&admin, &scopes(target), false), Ok(Basis::Admin));
        }
        // Not on a repository it does not administer, even if it also
        // names one it does.
        for target in [
            &["repo:other:read"][..],
            &["repo:r:read", "repo:other:read"][..],
        ] {
            assert!(may_revoke(&admin, &scopes(target), false).is_err());
        }
        // A token that names no repository has no admin.
        assert!(may_revoke(&admin, &scopes(&["objects:read"]), false).is_err());
        assert!(may_revoke(&admin, &[], false).is_err());
    }

    #[test]
    fn everyone_else_is_refused() {
        let writer = principal(
            "ses_1",
            PrincipalKind::Agent,
            &["repo:r:write", "repo:r:land:main"],
        );
        let reader = principal("ci", PrincipalKind::Ci, &["repo:r:read"]);
        for p in [&writer, &reader] {
            let e = may_revoke(p, &scopes(&["repo:r:write"]), false).expect_err("refused");
            assert!(matches!(e, ApiError::PermissionDenied { .. }));
        }
    }
}
