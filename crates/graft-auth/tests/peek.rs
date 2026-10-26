//! `Claims::peek_unverified`: reads claims for display without verifying.

use graft_auth::{AuthError, Claims, NoDenylist, SigningKey, Token, mint, verify};
use graft_proto::{Principal, PrincipalKind};

const NOW: i64 = 1_800_000_000;

fn key() -> SigningKey {
    SigningKey::from_seed(&[0x42; 32])
}

fn claims() -> Claims {
    Claims {
        principal: Principal {
            subject: "ses_peek".into(),
            kind: PrincipalKind::Agent,
            session: Some("ses_peek".into()),
            work_item: Some("WI-1".into()),
            scopes: vec!["repo:graft:read".into()],
        },
        exp: NOW + 3600,
        jti: "tok_peek".into(),
    }
}

#[test]
fn peek_matches_verify() {
    let k = key();
    let t = mint(&k, &claims()).unwrap();
    let verified = verify(&k.verifying_key(), &t, NOW, &NoDenylist).unwrap();
    let peeked = Claims::peek_unverified(&t).unwrap();
    assert_eq!(peeked, verified);
    assert_eq!(peeked, claims());
}

#[test]
fn peek_ignores_signature_but_verify_refuses() {
    let k = key();
    let t = mint(&k, &claims()).unwrap();
    // Flip the last signature character.
    let mut s = t.expose().to_owned();
    let last = s.pop().unwrap();
    s.push(if last == 'A' { 'B' } else { 'A' });
    let broken = Token::from_encoded(s);
    assert_eq!(Claims::peek_unverified(&broken).unwrap(), claims());
    assert_eq!(
        verify(&k.verifying_key(), &broken, NOW, &NoDenylist),
        Err(AuthError::BadSignature)
    );
    // A token from another key also peeks, and is refused.
    let other = mint(&SigningKey::from_seed(&[7; 32]), &claims()).unwrap();
    assert_eq!(Claims::peek_unverified(&other).unwrap(), claims());
    assert!(verify(&k.verifying_key(), &other, NOW, &NoDenylist).is_err());
}

#[test]
fn peek_does_not_check_expiry() {
    let k = key();
    let t = mint(&k, &claims()).unwrap();
    assert_eq!(
        verify(&k.verifying_key(), &t, NOW + 7200, &NoDenylist),
        Err(AuthError::Expired)
    );
    assert_eq!(Claims::peek_unverified(&t).unwrap().exp, NOW + 3600);
}

#[test]
fn garbage_is_malformed_and_not_echoed() {
    let inputs = [
        "SECRETgarbageSECRET",
        "v4.public.SECRETnotbase64!!SECRET",
        "v4.public.U0VDUkVU",
        "v4.public.U0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVUU0VDUkVU",
        "v4.public.abc.SECRETfooter",
    ];
    for input in inputs {
        let err = Claims::peek_unverified(&Token::from_encoded(input.into())).unwrap_err();
        assert_eq!(err, AuthError::Malformed);
        for text in [err.to_string(), format!("{err:?}")] {
            assert!(!text.contains("SECRET"), "{text}");
            assert!(!text.contains(input), "{text}");
        }
    }
}

#[test]
fn peeked_debug_has_no_token() {
    let t = mint(&key(), &claims()).unwrap();
    let peeked = Claims::peek_unverified(&t).unwrap();
    let debug = format!("{peeked:?}");
    let body = t.expose().strip_prefix("v4.public.").unwrap();
    assert!(!debug.contains(t.expose()));
    assert!(!debug.contains(body));
    // The signature is the trailing 86 base64url characters.
    assert!(!debug.contains(&body[body.len() - 86..]));
}
