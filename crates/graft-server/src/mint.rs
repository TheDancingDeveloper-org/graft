//! Minting tokens (`docs/design.md` §12.1): the `MintToken` RPC and
//! `admin token mint`.
//!
//! Over the API a caller can mint only a token no broader than its own: it
//! must hold every scope it asks for, and the TTL is capped. On the server
//! host the operator mints any scopes (`admin token mint`), which is how the
//! first token exists.

use graft_auth::{Claims, Scope, SigningKey, Token};
use graft_model::{Clock as _, SystemClock};
use graft_proto::types::MintToken;
use graft_proto::{ApiError, Principal, PrincipalKind};

/// What to mint.
#[derive(Clone, Debug)]
pub(crate) struct MintRequest {
    pub(crate) subject: String,
    pub(crate) kind: PrincipalKind,
    pub(crate) scopes: Vec<String>,
    pub(crate) ttl_s: u64,
    pub(crate) session: Option<String>,
    pub(crate) work_item: Option<String>,
}

/// Seconds since the epoch, for token expiry.
pub(crate) fn now_s() -> i64 {
    SystemClock.now().ms.div_euclid(1000)
}

/// Mints `req` at `now_s`. Fails with `InvalidChange` on an empty subject,
/// a scope that does not parse, or a zero TTL.
pub(crate) fn mint(key: &SigningKey, req: MintRequest, now_s: i64) -> Result<Token, ApiError> {
    if req.ttl_s == 0 {
        return Err(invalid("ttl"));
    }
    for s in &req.scopes {
        Scope::parse(s).map_err(|_| invalid("scope"))?;
    }
    let jti = graft_auth::new_jti().map_err(|_| ApiError::Unavailable {
        retry_after_ms: 1_000,
    })?;
    let exp = now_s.saturating_add(i64::try_from(req.ttl_s).unwrap_or(i64::MAX));
    let claims = Claims {
        principal: Principal {
            subject: req.subject,
            kind: req.kind,
            session: req.session,
            work_item: req.work_item,
            scopes: req.scopes,
        },
        exp,
        jti,
    };
    graft_auth::mint(key, &claims).map_err(|_| invalid("subject"))
}

/// The `MintToken` RPC: `caller` must hold every requested scope, and the
/// TTL is at most `max_ttl_s` (0 asks for the maximum). A token for a Vogt
/// session is an agent token; otherwise it has the caller's kind.
pub(crate) fn mint_for(
    key: &SigningKey,
    caller: &Principal,
    req: MintToken,
    max_ttl_s: u64,
    now_s: i64,
) -> Result<Token, ApiError> {
    for text in &req.scopes {
        let scope = Scope::parse(text).map_err(|_| invalid("scope"))?;
        let path = match &scope {
            Scope::Write { prefix, .. } => prefix.as_ref(),
            _ => None,
        };
        graft_auth::authorize(caller, &scope, path)?;
    }
    if req.ttl_s > max_ttl_s {
        return Err(ApiError::LimitExceeded {
            limit: "token-ttl".into(),
            max: max_ttl_s,
        });
    }
    let kind = if req.session.is_some() {
        PrincipalKind::Agent
    } else {
        caller.kind
    };
    mint(
        key,
        MintRequest {
            subject: req.subject,
            kind,
            scopes: req.scopes,
            ttl_s: if req.ttl_s == 0 { max_ttl_s } else { req.ttl_s },
            session: req.session,
            work_item: req.work_item,
        },
        now_s,
    )
}

fn invalid(rule: &str) -> ApiError {
    ApiError::InvalidChange {
        path: String::new(),
        rule: rule.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_auth::NoDenylist;

    fn caller(scopes: &[&str]) -> Principal {
        Principal {
            subject: "vogt".into(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn ask(scopes: &[&str], ttl_s: u64) -> MintToken {
        MintToken {
            subject: "ses_1".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            ttl_s,
            session: Some("ses_1".into()),
            work_item: Some("WI-1".into()),
        }
    }

    #[test]
    fn mints_no_broader_than_the_caller() {
        let key = SigningKey::from_seed(&[7; 32]);
        let vogt = caller(&["repo:r:admin"]);
        let token = mint_for(&key, &vogt, ask(&["repo:r:write:src"], 60), 3600, 1_000)
            .expect("narrower scopes mint");
        let claims =
            graft_auth::verify(&key.verifying_key(), &token, 1_000, &NoDenylist).expect("valid");
        assert_eq!(claims.exp, 1_060);
        assert_eq!(claims.principal.kind, PrincipalKind::Agent);
        assert_eq!(claims.principal.scopes, ["repo:r:write:src"]);

        let writer = caller(&["repo:r:write:src"]);
        assert!(matches!(
            mint_for(&key, &writer, ask(&["repo:r:write"], 60), 3600, 0),
            Err(ApiError::PermissionDenied { .. })
        ));
        assert!(matches!(
            mint_for(&key, &vogt, ask(&["repo:other:read"], 60), 3600, 0),
            Err(ApiError::PermissionDenied { .. })
        ));
        assert!(matches!(
            mint_for(&key, &vogt, ask(&["repo:r:read"], 7200), 3600, 0),
            Err(ApiError::LimitExceeded { .. })
        ));
        assert!(matches!(
            mint_for(&key, &vogt, ask(&["nonsense"], 60), 3600, 0),
            Err(ApiError::InvalidChange { .. })
        ));
    }
}
