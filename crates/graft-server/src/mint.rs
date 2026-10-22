//! Minting tokens (`docs/design.md` §12.1): the `MintToken` RPC and
//! `admin token mint`.
//!
//! Over the API a caller can mint only a token no broader than its own: it
//! must hold every scope it asks for, it cannot outlive its own token (the
//! TTL is capped by the server's maximum and by the caller's remaining
//! life), and only a service principal mints for another identity. On the
//! server host the operator mints any scopes (`admin token
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

/// Most scopes one minted token may carry.
const MAX_SCOPES: usize = 32;
/// Longest subject, session or work item a minted token may carry.
const MAX_FIELD_BYTES: usize = 128;

/// The `MintToken` RPC. The new token is a subset of `caller` on every axis:
///
/// - **Scopes**: `caller` must hold every requested scope (as written; there
///   are no wildcards, and names and prefixes compare exactly).
/// - **Life**: the TTL is at most the server's `max_ttl_s` and the caller's
///   remaining life (`caller_exp_s`), so the new token never outlives the
///   one that minted it. 0 asks for that maximum.
/// - **Identity**: only a service principal (Vogt) delegates: it names the
///   subject, session and work item of the token it mints, which is an
///   agent token. Any other caller mints for itself: an empty subject means
///   its own, a different one is refused, and the session and work item are
///   its own (unset, or equal to them). So nobody takes over another
///   principal's workspaces or forges the audit fields on its spans.
pub(crate) fn mint_for(
    key: &SigningKey,
    caller: &Principal,
    caller_exp_s: i64,
    req: MintToken,
    max_ttl_s: u64,
    now_s: i64,
) -> Result<Token, ApiError> {
    if req.scopes.is_empty() {
        return Err(invalid("scope"));
    }
    if req.scopes.len() > MAX_SCOPES {
        return Err(ApiError::LimitExceeded {
            limit: "token-scopes".into(),
            max: MAX_SCOPES as u64,
        });
    }
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
    let (subject, kind, session, work_item) =
        identity(caller, req.subject, req.session, req.work_item)?;
    mint(
        key,
        MintRequest {
            subject,
            kind,
            scopes: req.scopes,
            ttl_s: if req.ttl_s == 0 { limit } else { req.ttl_s },
            session,
            work_item,
        },
        now_s,
    )
}

type Identity = (String, PrincipalKind, Option<String>, Option<String>);

/// Who a token minted by `caller` is: see [`mint_for`].
fn identity(
    caller: &Principal,
    subject: String,
    session: Option<String>,
    work_item: Option<String>,
) -> Result<Identity, ApiError> {
    for (rule, field) in [
        ("subject", Some(&subject)),
        ("session", session.as_ref()),
        ("work-item", work_item.as_ref()),
    ] {
        if field.is_some_and(|f| {
            f.len() > MAX_FIELD_BYTES || f.chars().any(|c| c.is_whitespace() || c.is_control())
        }) {
            return Err(invalid(rule));
        }
    }
    if caller.kind == PrincipalKind::Service {
        let subject = if subject.is_empty() {
            caller.subject.clone()
        } else {
            subject
        };
        let kind = if subject != caller.subject || session.is_some() {
            PrincipalKind::Agent
        } else {
            caller.kind
        };
        return Ok((subject, kind, session, work_item));
    }
    let own = |asked: Option<String>, held: &Option<String>| match asked {
        Some(a) if Some(&a) != held.as_ref() => Err(()),
        _ => Ok(held.clone()),
    };
    let denied = || ApiError::PermissionDenied {
        scope: "delegate".into(),
        path: None,
    };
    if !subject.is_empty() && subject != caller.subject {
        return Err(denied());
    }
    let session = own(session, &caller.session).map_err(|()| denied())?;
    let work_item = own(work_item, &caller.work_item).map_err(|()| denied())?;
    Ok((caller.subject.clone(), caller.kind, session, work_item))
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
        let token = mint_for(
            &key,
            &vogt,
            1_000 + 3600,
            ask(&["repo:r:write:src"], 60),
            3600,
            1_000,
        )
        .expect("narrower scopes mint");
        let claims =
            graft_auth::verify(&key.verifying_key(), &token, 1_000, &NoDenylist).expect("valid");
        assert_eq!(claims.exp, 1_060);
        assert_eq!(claims.principal.kind, PrincipalKind::Agent);
        assert_eq!(claims.principal.scopes, ["repo:r:write:src"]);

        let writer = caller(&["repo:r:write:src"]);
        assert!(matches!(
            mint_for(
                &key,
                &writer,
                4_600,
                ask(&["repo:r:write"], 60),
                3600,
                1_000
            ),
            Err(ApiError::PermissionDenied { .. })
        ));
        assert!(matches!(
            mint_for(
                &key,
                &vogt,
                4_600,
                ask(&["repo:other:read"], 60),
                3600,
                1_000
            ),
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

    #[test]
    fn only_a_service_mints_for_another_identity() {
        let key = SigningKey::from_seed(&[7; 32]);
        let verify = |t: &Token| {
            graft_auth::verify(&key.verifying_key(), t, 1_000, &NoDenylist)
                .expect("valid")
                .principal
        };
        let bob = Principal {
            subject: "bob".into(),
            kind: PrincipalKind::Human,
            session: Some("ses_b".into()),
            work_item: None,
            scopes: vec!["repo:r:write".into()],
        };
        let as_bob = |subject: &str, session: Option<&str>, work_item: Option<&str>| MintToken {
            subject: subject.into(),
            scopes: vec!["repo:r:read".into()],
            ttl_s: 60,
            session: session.map(Into::into),
            work_item: work_item.map(Into::into),
        };
        for req in [
            as_bob("alice", None, None),
            as_bob("", Some("ses_x"), None),
            as_bob("bob", None, Some("WI-1")),
        ] {
            assert!(
                matches!(
                    mint_for(&key, &bob, 4_600, req.clone(), 3600, 1_000),
                    Err(ApiError::PermissionDenied { .. })
                ),
                "{req:?}"
            );
        }
        // Its own identity, named or not, with its own session.
        for req in [as_bob("", None, None), as_bob("bob", Some("ses_b"), None)] {
            let p = verify(&mint_for(&key, &bob, 4_600, req, 3600, 1_000).expect("mints"));
            assert_eq!(
                (
                    p.subject.as_str(),
                    p.kind,
                    p.session.as_deref(),
                    p.work_item
                ),
                ("bob", PrincipalKind::Human, Some("ses_b"), None)
            );
        }
        // A service names the identity; the token is an agent's.
        let vogt = caller(&["repo:r:write"]);
        let p = verify(
            &mint_for(&key, &vogt, 4_600, as_bob("ses_9", None, None), 3600, 1_000)
                .expect("delegates"),
        );
        assert_eq!(
            (p.subject.as_str(), p.kind),
            ("ses_9", PrincipalKind::Agent)
        );
        // Fields are bounded and printable.
        for req in [
            as_bob(&"x".repeat(MAX_FIELD_BYTES + 1), None, None),
            as_bob("ses 9", None, None),
            as_bob("ses_9", Some("a\nb"), None),
        ] {
            assert!(matches!(
                mint_for(&key, &vogt, 4_600, req, 3600, 1_000),
                Err(ApiError::InvalidChange { .. })
            ));
        }
    }
}
