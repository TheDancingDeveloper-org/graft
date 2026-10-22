//! Minting tokens (`docs/design.md` §12.1): the `MintToken` RPC and
//! `admin token mint`.
//!
//! Over the API a caller can mint only a token no broader than its own: it
//! must hold every scope it asks for, and it cannot outlive its own token
//! (the TTL is capped by the server's maximum and by the caller's remaining
//! life). On the server host the operator mints any scopes (`admin token
//! mint`), which is how the first token exists.

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
/// TTL is at most the server's `max_ttl_s` and the caller's remaining life
/// (`caller_exp_s`), so the new token never outlives the one that minted it.
/// 0 asks for that maximum. A token for a Vogt session is an agent token;
/// otherwise it has the caller's kind.
pub(crate) fn mint_for(
    key: &SigningKey,
    caller: &Principal,
    caller_exp_s: i64,
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
    let remaining = u64::try_from(caller_exp_s.saturating_sub(now_s)).unwrap_or(0);
    let limit = max_ttl_s.min(remaining);
    if req.ttl_s > limit {
        return Err(ApiError::LimitExceeded {
            limit: "token-ttl".into(),
            max: limit,
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
            ttl_s: if req.ttl_s == 0 { limit } else { req.ttl_s },
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
        let token = mint_for(&key, &vogt, 1_000 + 3600, ask(&["repo:r:write:src"], 60), 3600, 1_000)
            .expect("narrower scopes mint");
        let claims =
            graft_auth::verify(&key.verifying_key(), &token, 1_000, &NoDenylist).expect("valid");
        assert_eq!(claims.exp, 1_060);
        assert_eq!(claims.principal.kind, PrincipalKind::Agent);
        assert_eq!(claims.principal.scopes, ["repo:r:write:src"]);

        let writer = caller(&["repo:r:write:src"]);
        assert!(matches!(
            mint_for(&key, &writer, 4_600, ask(&["repo:r:write"], 60), 3600, 1_000),
            Err(ApiError::PermissionDenied { .. })
        ));
        assert!(matches!(
            mint_for(&key, &vogt, 4_600, ask(&["repo:other:read"], 60), 3600, 1_000),
            Err(ApiError::PermissionDenied { .. })
        ));
        assert!(matches!(
            mint_for(&key, &vogt, 4_600, ask(&["repo:r:read"], 7200), 3600, 1_000),
            Err(ApiError::LimitExceeded { .. })
        ));
        assert!(matches!(
            mint_for(&key, &vogt, 4_600, ask(&["nonsense"], 60), 3600, 1_000),
            Err(ApiError::InvalidChange { .. })
        ));
    }

    #[test]
    fn a_token_never_outlives_its_caller() {
        let key = SigningKey::from_seed(&[7; 32]);
        let vogt = caller(&["repo:r:admin"]);
        // The caller has 30 s left: a longer TTL is refused, and the default
        // (0) is capped at what is left.
        assert!(matches!(
            mint_for(&key, &vogt, 1_030, ask(&["repo:r:read"], 60), 3600, 1_000),
            Err(ApiError::LimitExceeded { max: 30, .. })
        ));
        let token = mint_for(&key, &vogt, 1_030, ask(&["repo:r:read"], 0), 3600, 1_000)
            .expect("capped default");
        let claims =
            graft_auth::verify(&key.verifying_key(), &token, 1_000, &NoDenylist).expect("valid");
        assert_eq!(claims.exp, 1_030);
        // A caller whose token has expired mints nothing.
        assert!(mint_for(&key, &vogt, 1_000, ask(&["repo:r:read"], 0), 3600, 1_000).is_err());
    }
}
