//! Golden tokens, and the verify failure modes: expiry, wrong key, revoked
//! `jti`, tampering.

use std::collections::HashSet;

use graft_auth::{
    AuthError, Claims, NoDenylist, SigningKey, Token, VerifyingKey, mint, new_jti, verify,
};
use graft_proto::{Principal, PrincipalKind};

const NOW: i64 = 1_800_000_000;

fn key() -> SigningKey {
    SigningKey::from_seed(&[0x42; 32])
}

fn agent_claims() -> Claims {
    Claims {
        principal: Principal {
            subject: "ses_01M5HBKAXKE0S85N0PB3DDYX3V".into(),
            kind: PrincipalKind::Agent,
            session: Some("ses_01M5HBKAXKE0S85N0PB3DDYX3V".into()),
            work_item: Some("WI-2185".into()),
            scopes: vec!["repo:graft:read".into(), "repo:graft:land:main".into()],
        },
        exp: NOW + 3600,
        jti: "tok_0001".into(),
    }
}

fn service_claims() -> Claims {
    Claims {
        principal: Principal {
            subject: "node-b".into(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec!["objects:read".into()],
        },
        exp: NOW + 86_400,
        jti: "tok_0002".into(),
    }
}

/// Frozen encodings. A change here is a change to a durable format and needs
/// a migration and an operator decision (AGENTS.md rule 1).
const GOLDEN_PUBLIC_KEY: &str = "2152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db12";
const GOLDEN_AGENT: &str = "v4.public.eyJzdWIiOiJzZXNfMDFNNUhCS0FYS0UwUzg1TjBQQjNERFlYM1YiLCJraW5kIjoiYWdlbnQiLCJzZXNzaW9uIjoic2VzXzAxTTVIQktBWEtFMFM4NU4wUEIzRERZWDNWIiwid29ya19pdGVtIjoiV0ktMjE4NSIsInNjb3BlcyI6WyJyZXBvOmdyYWZ0OnJlYWQiLCJyZXBvOmdyYWZ0OmxhbmQ6bWFpbiJdLCJleHAiOjE4MDAwMDM2MDAsImp0aSI6InRva18wMDAxIn3JOrYoercTlsW8stBlkG7EgWuCTIKn71w5go_QPqcR8XP7UgWte95yDaN7S2rNUMPZVF40mrITztkUuokvuvMA";
const GOLDEN_SERVICE: &str = "v4.public.eyJzdWIiOiJub2RlLWIiLCJraW5kIjoic2VydmljZSIsInNjb3BlcyI6WyJvYmplY3RzOnJlYWQiXSwiZXhwIjoxODAwMDg2NDAwLCJqdGkiOiJ0b2tfMDAwMiJ9ygda27ZC4p6XjX8fvAkSyCV2oUnduRpFpweTvBEgmph0E_SiIV_6EifhfmMQZwR7Wu_WakcEPy2en2XoUGudDg";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn golden_tokens() {
    let key = key();
    assert_eq!(hex(&key.verifying_key().to_bytes()), GOLDEN_PUBLIC_KEY);
    let agent = mint(&key, &agent_claims()).unwrap();
    assert_eq!(agent.expose(), GOLDEN_AGENT);
    let service = mint(&key, &service_claims()).unwrap();
    assert_eq!(service.expose(), GOLDEN_SERVICE);

    // The goldens verify, and decode to the claims they were minted from.
    let vk = key.verifying_key();
    let got = verify(
        &vk,
        &Token::from_encoded(GOLDEN_AGENT.into()),
        NOW,
        &NoDenylist,
    )
    .unwrap();
    assert_eq!(got, agent_claims());
    let got = verify(
        &vk,
        &Token::from_encoded(GOLDEN_SERVICE.into()),
        NOW,
        &NoDenylist,
    )
    .unwrap();
    assert_eq!(got, service_claims());
}

#[test]
fn round_trip() {
    let key = SigningKey::generate().unwrap();
    let mut claims = agent_claims();
    claims.jti = new_jti().unwrap();
    let token = mint(&key, &claims).unwrap();
    assert!(token.expose().starts_with("v4.public."));
    let got = verify(&key.verifying_key(), &token, NOW, &NoDenylist).unwrap();
    assert_eq!(got, claims);
}

#[test]
fn key_bytes_round_trip() {
    let key = key();
    let again = SigningKey::from_seed(&key.expose_seed());
    assert_eq!(again.verifying_key(), key.verifying_key());
    let vk = VerifyingKey::from_bytes(&key.verifying_key().to_bytes()).unwrap();
    assert_eq!(vk, key.verifying_key());
}

#[test]
fn expiry() {
    let key = key();
    let claims = agent_claims();
    let token = mint(&key, &claims).unwrap();
    let vk = key.verifying_key();
    assert!(verify(&vk, &token, claims.exp - 1, &NoDenylist).is_ok());
    assert_eq!(
        verify(&vk, &token, claims.exp, &NoDenylist),
        Err(AuthError::Expired)
    );
    assert_eq!(
        verify(&vk, &token, claims.exp + 1, &NoDenylist),
        Err(AuthError::Expired)
    );
}

#[test]
fn wrong_key() {
    let token = mint(&key(), &agent_claims()).unwrap();
    let other = SigningKey::from_seed(&[0x43; 32]).verifying_key();
    assert_eq!(
        verify(&other, &token, NOW, &NoDenylist),
        Err(AuthError::BadSignature)
    );
}

#[test]
fn revoked_jti() {
    let key = key();
    let token = mint(&key, &agent_claims()).unwrap();
    let deny: HashSet<String> = ["tok_0001".to_owned()].into();
    assert_eq!(
        verify(&key.verifying_key(), &token, NOW, &deny),
        Err(AuthError::Revoked)
    );
    let other: HashSet<String> = ["tok_9999".to_owned()].into();
    assert!(verify(&key.verifying_key(), &token, NOW, &other).is_ok());
}

#[test]
fn tampered_and_malformed() {
    let key = key();
    let vk = key.verifying_key();
    let good = mint(&key, &agent_claims()).unwrap();

    // Flip a character inside the signed payload.
    let mut s = good.expose().to_owned();
    let i = "v4.public.".len() + 5;
    let c = if &s[i..=i] == "A" { "B" } else { "A" };
    s.replace_range(i..=i, c);
    assert_eq!(
        verify(&vk, &Token::from_encoded(s), NOW, &NoDenylist),
        Err(AuthError::BadSignature)
    );

    for bad in [
        "",
        "v4.public.",
        "v3.public.AAAA",
        "v4.local.AAAA",
        "v4.public.!!!!",
        "v4.public.AAAA",
    ] {
        assert_eq!(
            verify(&vk, &Token::from_encoded(bad.into()), NOW, &NoDenylist),
            Err(AuthError::Malformed),
            "{bad:?}"
        );
    }

    // A footer is never issued, so a token with one is refused.
    let footed = format!("{}.e30", good.expose());
    assert_eq!(
        verify(&vk, &Token::from_encoded(footed), NOW, &NoDenylist),
        Err(AuthError::Malformed)
    );

    // Oversized input is refused before decoding.
    let huge = format!("v4.public.{}", "A".repeat(graft_auth::MAX_TOKEN_LEN));
    assert_eq!(
        verify(&vk, &Token::from_encoded(huge), NOW, &NoDenylist),
        Err(AuthError::Malformed)
    );
}

#[test]
fn mint_refuses_claims_verify_would_reject() {
    let key = key();
    let mut c = agent_claims();
    c.principal.scopes.push("repo:graft:delete".into());
    assert_eq!(mint(&key, &c), Err(AuthError::Malformed));
    let mut c = agent_claims();
    c.jti.clear();
    assert_eq!(mint(&key, &c), Err(AuthError::Malformed));
    let mut c = agent_claims();
    c.principal.subject.clear();
    assert_eq!(mint(&key, &c), Err(AuthError::Malformed));
}
