//! Tokens: PASETO `v4.public` (Ed25519) over a JSON claim set
//! (`docs/design.md` §12.1). See the crate README for the format choice.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _};
use graft_proto::{Principal, PrincipalKind};
use serde::{Deserialize, Serialize};

use crate::{AuthError, Scope};

/// The PASETO header for version 4, public purpose.
const HEADER: &str = "v4.public.";
/// Ed25519 signature length.
const SIG_LEN: usize = 64;
/// Longest encoded token `verify` will look at. Real tokens are well under
/// 1 KiB; this bounds the work an unauthenticated caller can cause.
pub const MAX_TOKEN_LEN: usize = 8 * 1024;

/// Token claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    /// The principal it establishes. `principal.subject` is the `sub` claim.
    pub principal: Principal,
    /// Expiry, seconds since the epoch. The token is invalid from this
    /// second on.
    pub exp: i64,
    /// Unique id, for revocation.
    pub jti: String,
}

impl Claims {
    /// Reads a token's claims WITHOUT checking its signature, expiry or
    /// revocation. For display only (expiry, scopes, subject), e.g. by
    /// `graft doctor`; never for an authorization decision. Use [`verify`]
    /// for that.
    ///
    /// Fails with `Malformed` on a token `verify` would reject for its shape
    /// or claims. The error never carries any part of the token.
    pub fn peek_unverified(token: &Token) -> Result<Self, AuthError> {
        let (m, _sig) = split(token.expose())?;
        let payload: Payload = serde_json::from_slice(&m).map_err(|_| AuthError::Malformed)?;
        check_payload(&payload)?;
        Ok(payload.into())
    }
}

/// The signed JSON payload. Field order is the encoding order, and is frozen
/// by the golden tokens.
#[derive(Serialize, Deserialize)]
struct Payload {
    sub: String,
    kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    work_item: Option<String>,
    scopes: Vec<String>,
    exp: i64,
    jti: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Human,
    Agent,
    Ci,
    Service,
}

impl From<PrincipalKind> for Kind {
    fn from(k: PrincipalKind) -> Self {
        match k {
            PrincipalKind::Human => Self::Human,
            PrincipalKind::Agent => Self::Agent,
            PrincipalKind::Ci => Self::Ci,
            PrincipalKind::Service => Self::Service,
        }
    }
}

impl From<Kind> for PrincipalKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Human => Self::Human,
            Kind::Agent => Self::Agent,
            Kind::Ci => Self::Ci,
            Kind::Service => Self::Service,
        }
    }
}

/// An encoded token. Opaque; `Debug` redacts it and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Wraps an encoded token (read from a file or the environment).
    pub fn from_encoded(s: String) -> Self {
        Self(s)
    }

    /// The encoded form, for sending.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// The server's signing key. `Debug` redacts it.
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    /// A fresh key from the operating system's random source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed)?;
        Ok(Self::from_seed(&seed))
    }

    /// The key for a 32-byte Ed25519 seed (as stored in a key file).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(seed))
    }

    /// The 32-byte seed, for writing to a key file. Handle as a secret.
    pub fn expose_seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The public half.
    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey(self.0.verifying_key())
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

/// The public half, held by verifiers (server, node).
#[derive(Clone, PartialEq, Eq)]
pub struct VerifyingKey(ed25519_dalek::VerifyingKey);

impl VerifyingKey {
    /// The key for 32 public key bytes. Fails on bytes that are not a valid
    /// Ed25519 point.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, AuthError> {
        ed25519_dalek::VerifyingKey::from_bytes(bytes)
            .map(Self)
            .map_err(|_| AuthError::Malformed)
    }

    /// The 32 public key bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
}

impl fmt::Debug for VerifyingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Public, but kept out of logs like the rest of the key material.
        f.write_str("VerifyingKey(<redacted>)")
    }
}

/// Revoked token ids. The server backs this with the MetaStore and checks it
/// on writes; verifiers that only read may pass [`NoDenylist`].
pub trait Denylist: Send + Sync {
    /// Whether `jti` is revoked.
    fn is_revoked(&self, jti: &str) -> bool;
}

/// A denylist that revokes nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDenylist;

impl Denylist for NoDenylist {
    fn is_revoked(&self, _jti: &str) -> bool {
        false
    }
}

impl Denylist for std::collections::HashSet<String> {
    fn is_revoked(&self, jti: &str) -> bool {
        self.contains(jti)
    }
}

/// A fresh random token id: 16 bytes, base64url.
pub fn new_jti() -> Result<String, getrandom::Error> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b)?;
    Ok(URL_SAFE_NO_PAD.encode(b))
}

/// Mints a token. Fails with `Malformed` on claims `verify` would reject: an
/// empty subject or `jti`, or a scope that does not parse.
pub fn mint(key: &SigningKey, claims: &Claims) -> Result<Token, AuthError> {
    let payload = Payload {
        sub: claims.principal.subject.clone(),
        kind: claims.principal.kind.into(),
        session: claims.principal.session.clone(),
        work_item: claims.principal.work_item.clone(),
        scopes: claims.principal.scopes.clone(),
        exp: claims.exp,
        jti: claims.jti.clone(),
    };
    check_payload(&payload)?;
    let m = serde_json::to_vec(&payload).map_err(|_| AuthError::Malformed)?;
    Ok(Token(sign(&key.0, &m, b"", b"")))
}

/// Verifies a token at `now_s` (seconds since the epoch) and returns its
/// claims. Checks, in order: shape, signature, claims, expiry, revocation.
pub fn verify(
    key: &VerifyingKey,
    token: &Token,
    now_s: i64,
    deny: &dyn Denylist,
) -> Result<Claims, AuthError> {
    let m = open(&key.0, token.expose(), b"")?;
    let payload: Payload = serde_json::from_slice(&m).map_err(|_| AuthError::Malformed)?;
    check_payload(&payload)?;
    if now_s >= payload.exp {
        return Err(AuthError::Expired);
    }
    if deny.is_revoked(&payload.jti) {
        return Err(AuthError::Revoked);
    }
    Ok(payload.into())
}

impl From<Payload> for Claims {
    fn from(p: Payload) -> Self {
        Self {
            principal: Principal {
                subject: p.sub,
                kind: p.kind.into(),
                session: p.session,
                work_item: p.work_item,
                scopes: p.scopes,
            },
            exp: p.exp,
            jti: p.jti,
        }
    }
}

fn check_payload(p: &Payload) -> Result<(), AuthError> {
    if p.sub.is_empty() || p.jti.is_empty() {
        return Err(AuthError::Malformed);
    }
    for s in &p.scopes {
        Scope::parse(s)?;
    }
    Ok(())
}

/// PASETO pre-authentication encoding.
fn pae(pieces: &[&[u8]]) -> Vec<u8> {
    fn le64(n: usize) -> [u8; 8] {
        // PAE clears the top bit; lengths here never reach it.
        ((n as u64) & (u64::MAX >> 1)).to_le_bytes()
    }
    let mut out = Vec::with_capacity(8 + pieces.iter().map(|p| 8 + p.len()).sum::<usize>());
    out.extend_from_slice(&le64(pieces.len()));
    for p in pieces {
        out.extend_from_slice(&le64(p.len()));
        out.extend_from_slice(p);
    }
    out
}

/// `v4.public` sign: message `m`, footer `f`, implicit assertion `i`.
fn sign(key: &ed25519_dalek::SigningKey, m: &[u8], f: &[u8], i: &[u8]) -> String {
    let sig = key.sign(&pae(&[HEADER.as_bytes(), m, f, i]));
    let mut body = Vec::with_capacity(m.len() + SIG_LEN);
    body.extend_from_slice(m);
    body.extend_from_slice(&sig.to_bytes());
    let mut out = format!("{HEADER}{}", URL_SAFE_NO_PAD.encode(body));
    if !f.is_empty() {
        out.push('.');
        out.push_str(&URL_SAFE_NO_PAD.encode(f));
    }
    out
}

/// `v4.public` verify; returns the message. graft never issues a footer, so
/// a token carrying one is malformed.
fn open(key: &ed25519_dalek::VerifyingKey, token: &str, i: &[u8]) -> Result<Vec<u8>, AuthError> {
    let (raw, sig) = split(token)?;
    key.verify_strict(&pae(&[HEADER.as_bytes(), &raw, b"", i]), &sig)
        .map_err(|_| AuthError::BadSignature)?;
    Ok(raw)
}

/// Checks a `v4.public` token's size and shape and splits it into message
/// and signature, without verifying the signature.
fn split(token: &str) -> Result<(Vec<u8>, Signature), AuthError> {
    if token.len() > MAX_TOKEN_LEN {
        return Err(AuthError::Malformed);
    }
    let body = token.strip_prefix(HEADER).ok_or(AuthError::Malformed)?;
    if body.contains('.') {
        return Err(AuthError::Malformed);
    }
    let mut raw = URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| AuthError::Malformed)?;
    if raw.len() < SIG_LEN {
        return Err(AuthError::Malformed);
    }
    let sig = raw.split_off(raw.len() - SIG_LEN);
    let sig = Signature::from_slice(&sig).map_err(|_| AuthError::Malformed)?;
    Ok((raw, sig))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    /// PASETO test vector 4-S-1 (paseto-standard/test-vectors, v4.json):
    /// proves the envelope interoperates with other PASETO implementations.
    #[test]
    fn paseto_vector_4_s_1() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&hex32(
            "b4cbfb43df4ce210727d953e4a713307fa19bb7d9f85041438d9e11b942a3774",
        ));
        assert_eq!(
            sk.verifying_key().to_bytes(),
            hex32("1eb9dbbbbc047c03fd70604e0071f0987e16b28b757225c11f00415d0e20b1a2")
        );
        let m = br#"{"data":"this is a signed message","exp":"2022-01-01T00:00:00+00:00"}"#;
        let expected = "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9bg_XBBzds8lTZShVlwwKSgeKpLT3yukTw6JUz3W4h_ExsQV-P0V54zemZDcAxFaSeef1QlXEFtkqxT1ciiQEDA";
        assert_eq!(sign(&sk, m, b"", b""), expected);
        assert_eq!(open(&sk.verifying_key(), expected, b"").unwrap(), m);
    }

    #[test]
    fn pae_matches_spec_examples() {
        assert_eq!(pae(&[]), b"\x00\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(
            pae(&[b"test"]),
            b"\x01\x00\x00\x00\x00\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00test"
        );
    }
}
