//! Encrypted mirror credentials (D41, `docs/notes/mirror-service.md` §3.4).
//!
//! The server holds a ring of credential key-encryption keys (KEKs),
//! unrelated to the signing key: one active KEK that seals, and retired
//! ones that only open. Each row gets a fresh 256-bit data key, wrapped by
//! the active KEK; the data key seals the credential. Both are
//! XChaCha20-Poly1305, and both bind the row through associated data:
//!
//! ```text
//! aad(label) = label ‖ lp(repo) ‖ lp(upstream) ‖ lp(kek_id)
//!   lp(x)    = u32-be byte length of x ‖ x
//!   label    = "graft.mirror.cred.v1" (credential) or
//!              "graft.mirror.dek.v1"  (data-key wrap)
//! ```
//!
//! The length prefixes keep two different `(repo, upstream)` pairs from
//! ever producing the same associated data. A ciphertext moved to another
//! row, or relabelled with another key id, fails to open.
//!
//! The envelope (a durable encoding, frozen by the golden vector below):
//!
//! ```text
//! 0x01 ‖ wrap_nonce[24] ‖ wrapped_data_key[48] ‖ nonce[24] ‖ ciphertext[n+16]
//! ```
//!
//! The key id is stored beside the envelope (`repo_mirrors.kek_id`), not in
//! it. Re-wrapping under a new KEK keeps the data key, wraps it afresh and
//! re-seals the credential under fresh nonces, since the key id is part of
//! the associated data.

use std::fmt;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// The envelope's format byte.
const VERSION: u8 = 1;
const NONCE: usize = 24;
const KEY: usize = 32;
const TAG: usize = 16;
const WRAPPED: usize = KEY + TAG;
const HEADER: usize = 1 + NONCE + WRAPPED + NONCE;

const CRED_LABEL: &[u8] = b"graft.mirror.cred.v1";
const DEK_LABEL: &[u8] = b"graft.mirror.dek.v1";

/// The longest credential accepted, in bytes.
pub(crate) const MAX_CREDENTIAL: usize = 8 * 1024;

/// The longest key id accepted.
const MAX_KEK_ID: usize = 64;

/// A plaintext credential. `Debug` and `Display` print `<redacted>`, and
/// the bytes are zeroed on drop. Never log it or write it to disk.
pub(crate) struct Secret(Vec<u8>);

impl Secret {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The plaintext, for the one response that carries it.
    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

fn wipe(bytes: &mut [u8]) {
    bytes.fill(0);
    // Keeps the zeroing from being optimised away as a dead store.
    std::hint::black_box(bytes);
}

/// One key-encryption key. `Debug` shows only its id.
#[derive(Clone)]
pub(crate) struct Kek {
    id: String,
    key: [u8; KEY],
}

impl Kek {
    pub(crate) fn new(id: impl Into<String>, key: [u8; KEY]) -> Result<Self, CredentialError> {
        let id = id.into();
        if id.is_empty()
            || id.len() > MAX_KEK_ID
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(CredentialError::BadKeyId);
        }
        Ok(Self { id, key })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Debug for Kek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kek")
            .field("id", &self.id)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl Drop for Kek {
    fn drop(&mut self) {
        wipe(&mut self.key);
    }
}

/// Why a credential did not seal or open. Never carries key or plaintext
/// bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialError {
    /// No active KEK is configured, so nothing can be sealed.
    NoActiveKey,
    /// The row names a key id the ring does not hold.
    UnknownKey,
    /// A key id outside `[A-Za-z0-9._-]{1,64}`.
    BadKeyId,
    /// Two keys in the ring share an id.
    DuplicateKeyId,
    /// The envelope is malformed, or fails authentication for this row.
    DoesNotOpen,
    /// The system random source failed.
    Random,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoActiveKey => "no active credential_kek is configured",
            Self::UnknownKey => "the credential's key id is not in the key ring",
            Self::BadKeyId => "a key id is 1 to 64 of A-Z a-z 0-9 . _ -",
            Self::DuplicateKeyId => "two credential keys share an id",
            Self::DoesNotOpen => "the credential does not open for this row",
            Self::Random => "the system random source failed",
        })
    }
}

/// The server's KEK ring: at most one active key, and retired keys that
/// only open.
#[derive(Debug, Default)]
pub(crate) struct KeyRing {
    active: Option<Kek>,
    retired: Vec<Kek>,
}

/// A sealed credential: the envelope and the id of the KEK that wrapped it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sealed {
    pub(crate) envelope: Vec<u8>,
    pub(crate) kek_id: String,
}

impl KeyRing {
    pub(crate) fn new(active: Option<Kek>, retired: Vec<Kek>) -> Result<Self, CredentialError> {
        let mut ids: Vec<&str> = active.iter().chain(&retired).map(Kek::id).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(CredentialError::DuplicateKeyId);
        }
        Ok(Self { active, retired })
    }

    /// The active key's id, if one is configured.
    pub(crate) fn active_id(&self) -> Option<&str> {
        self.active.as_ref().map(Kek::id)
    }

    fn find(&self, id: &str) -> Option<&Kek> {
        self.active.iter().chain(&self.retired).find(|k| k.id == id)
    }

    /// Seals `plaintext` for the row `(repo, upstream)` under the active key
    /// with a fresh data key.
    pub(crate) fn seal(
        &self,
        repo: &str,
        upstream: &str,
        plaintext: &[u8],
    ) -> Result<Sealed, CredentialError> {
        let kek = self.active.as_ref().ok_or(CredentialError::NoActiveKey)?;
        let mut dek = random::<KEY>()?;
        let sealed = seal_with(kek, &dek, &random()?, &random()?, repo, upstream, plaintext);
        wipe(&mut dek);
        Ok(sealed)
    }

    /// Opens the row's credential under the key it names, active or retired.
    pub(crate) fn open(
        &self,
        repo: &str,
        upstream: &str,
        kek_id: &str,
        envelope: &[u8],
    ) -> Result<Secret, CredentialError> {
        let kek = self.find(kek_id).ok_or(CredentialError::UnknownKey)?;
        let (mut dek, plain) = open_with(kek, repo, upstream, envelope)?;
        wipe(&mut dek);
        Ok(plain)
    }

    /// Re-wraps the row's data key under the active key. `None` when it is
    /// already sealed under the active key.
    pub(crate) fn rewrap(
        &self,
        repo: &str,
        upstream: &str,
        kek_id: &str,
        envelope: &[u8],
    ) -> Result<Option<Sealed>, CredentialError> {
        let active = self.active.as_ref().ok_or(CredentialError::NoActiveKey)?;
        if active.id == kek_id {
            return Ok(None);
        }
        let old = self.find(kek_id).ok_or(CredentialError::UnknownKey)?;
        let (mut dek, plain) = open_with(old, repo, upstream, envelope)?;
        let sealed = seal_with(
            active,
            &dek,
            &random()?,
            &random()?,
            repo,
            upstream,
            plain.expose(),
        );
        wipe(&mut dek);
        Ok(Some(sealed))
    }
}

fn random<const N: usize>() -> Result<[u8; N], CredentialError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| CredentialError::Random)?;
    Ok(b)
}

fn aad(label: &[u8], repo: &str, upstream: &str, kek_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(label.len() + 12 + repo.len() + upstream.len() + kek_id.len());
    out.extend_from_slice(label);
    for part in [repo, upstream, kek_id] {
        let len = u32::try_from(part.len()).expect("bounded at the boundary");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

/// Seals with the data key and nonces given. Deterministic, for the golden
/// vector; [`KeyRing::seal`] draws them at random.
fn seal_with(
    kek: &Kek,
    dek: &[u8; KEY],
    wrap_nonce: &[u8; NONCE],
    nonce: &[u8; NONCE],
    repo: &str,
    upstream: &str,
    plaintext: &[u8],
) -> Sealed {
    let wrapped = XChaCha20Poly1305::new((&kek.key).into())
        .encrypt(
            XNonce::from_slice(wrap_nonce),
            Payload {
                msg: dek,
                aad: &aad(DEK_LABEL, repo, upstream, &kek.id),
            },
        )
        .expect("a 32-byte message always seals");
    let ciphertext = XChaCha20Poly1305::new(dek.into())
        .encrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad: &aad(CRED_LABEL, repo, upstream, &kek.id),
            },
        )
        .expect("a bounded message always seals");
    let mut envelope = Vec::with_capacity(HEADER + ciphertext.len());
    envelope.push(VERSION);
    envelope.extend_from_slice(wrap_nonce);
    envelope.extend_from_slice(&wrapped);
    envelope.extend_from_slice(nonce);
    envelope.extend_from_slice(&ciphertext);
    Sealed {
        envelope,
        kek_id: kek.id.clone(),
    }
}

fn open_with(
    kek: &Kek,
    repo: &str,
    upstream: &str,
    envelope: &[u8],
) -> Result<([u8; KEY], Secret), CredentialError> {
    if envelope.len() < HEADER + TAG || envelope[0] != VERSION {
        return Err(CredentialError::DoesNotOpen);
    }
    let (wrap_nonce, rest) = envelope[1..].split_at(NONCE);
    let (wrapped, rest) = rest.split_at(WRAPPED);
    let (nonce, ciphertext) = rest.split_at(NONCE);
    let mut dek_bytes = XChaCha20Poly1305::new((&kek.key).into())
        .decrypt(
            XNonce::from_slice(wrap_nonce),
            Payload {
                msg: wrapped,
                aad: &aad(DEK_LABEL, repo, upstream, &kek.id),
            },
        )
        .map_err(|_| CredentialError::DoesNotOpen)?;
    let mut dek = [0u8; KEY];
    dek.copy_from_slice(&dek_bytes);
    wipe(&mut dek_bytes);
    let plain = XChaCha20Poly1305::new((&dek).into())
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: &aad(CRED_LABEL, repo, upstream, &kek.id),
            },
        )
        .map_err(|_| {
            wipe(&mut dek);
            CredentialError::DoesNotOpen
        })?;
    Ok((dek, Secret::new(plain)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "r";
    const UP: &str = "https://github.com/o/r.git";

    fn kek(id: &str, byte: u8) -> Kek {
        Kek::new(id, [byte; KEY]).expect("key")
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Frozen (rule 1): changing the envelope breaks every stored row.
    #[test]
    fn golden_vector() {
        let k = kek("k1", 0x11);
        let sealed = seal_with(
            &k,
            &[0x22; KEY],
            &[0x33; NONCE],
            &[0x44; NONCE],
            REPO,
            UP,
            b"ghp_example",
        );
        assert_eq!(sealed.kek_id, "k1");
        assert_eq!(hex(&sealed.envelope), GOLDEN);
        let ring = KeyRing::new(Some(k), vec![]).expect("ring");
        let plain = ring.open(REPO, UP, "k1", &sealed.envelope).expect("open");
        assert_eq!(plain.expose(), b"ghp_example");
        assert_eq!(
            hex(&aad(CRED_LABEL, REPO, UP, "k1")),
            "67726166742e6d6972726f722e637265642e7631\
             0000000172\
             0000001a68747470733a2f2f6769746875622e636f6d2f6f2f722e676974\
             000000026b31"
        );
    }

    /// Checked against an independent implementation (Python
    /// `cryptography` ChaCha20-Poly1305 with HChaCha20 for the X nonce).
    const GOLDEN: &str = concat!(
        "01",                                               // version
        "333333333333333333333333333333333333333333333333", // wrap nonce
        "988655ab5a492115a0b60a86eccfdcf4a9655edc6e9156ae",
        "c946b458b235dd63757656183cb92b92f5b01e849c68788e", // wrapped data key
        "444444444444444444444444444444444444444444444444", // nonce
        "d9266a210d828a233d5c4a359b84235d350bc538984c792803a046", // ciphertext and tag
    );

    #[test]
    fn a_ciphertext_moved_to_another_row_fails_to_open() {
        let ring = KeyRing::new(Some(kek("k1", 1)), vec![]).expect("ring");
        let s = ring.seal(REPO, UP, b"pat").expect("seal");
        assert!(ring.open(REPO, UP, "k1", &s.envelope).is_ok());
        for (repo, up) in [
            ("other", UP),
            (REPO, "https://github.com/o/other.git"),
            // A shifted boundary between the fields is another row too.
            ("rh", "ttps://github.com/o/r.git"),
        ] {
            assert_eq!(
                ring.open(repo, up, "k1", &s.envelope).err(),
                Some(CredentialError::DoesNotOpen),
                "{repo} {up}"
            );
        }
        let mut flipped = s.envelope.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert_eq!(
            ring.open(REPO, UP, "k1", &flipped).err(),
            Some(CredentialError::DoesNotOpen)
        );
        assert_eq!(
            ring.open(REPO, UP, "k1", &s.envelope[..HEADER]).err(),
            Some(CredentialError::DoesNotOpen)
        );
    }

    #[test]
    fn a_relabelled_key_id_fails_to_open() {
        // Same key bytes under two ids: the id is bound, not just the key.
        let ring = KeyRing::new(Some(kek("k2", 1)), vec![kek("k1", 1)]).expect("ring");
        let old = KeyRing::new(Some(kek("k1", 1)), vec![]).expect("ring");
        let s = old.seal(REPO, UP, b"pat").expect("seal");
        assert!(ring.open(REPO, UP, "k1", &s.envelope).is_ok());
        assert_eq!(
            ring.open(REPO, UP, "k2", &s.envelope).err(),
            Some(CredentialError::DoesNotOpen)
        );
    }

    #[test]
    fn a_retired_key_opens_but_never_seals() {
        let retired_only = KeyRing::new(None, vec![kek("k1", 1)]).expect("ring");
        assert_eq!(
            retired_only.seal(REPO, UP, b"pat").err(),
            Some(CredentialError::NoActiveKey)
        );
        let before = KeyRing::new(Some(kek("k1", 1)), vec![]).expect("ring");
        let s = before.seal(REPO, UP, b"pat").expect("seal");
        assert_eq!(
            retired_only
                .open(REPO, UP, "k1", &s.envelope)
                .expect("opens")
                .expose(),
            b"pat"
        );
        // With a new active key, sealing uses it, never the retired one.
        let rotated = KeyRing::new(Some(kek("k2", 2)), vec![kek("k1", 1)]).expect("ring");
        assert_eq!(rotated.seal(REPO, UP, b"x").expect("seal").kek_id, "k2");
    }

    #[test]
    fn rewrap_then_retire_the_old_key_still_opens() {
        let v1 = KeyRing::new(Some(kek("k1", 1)), vec![]).expect("ring");
        let s = v1.seal(REPO, UP, b"pat").expect("seal");
        let v2 = KeyRing::new(Some(kek("k2", 2)), vec![kek("k1", 1)]).expect("ring");
        let r = v2
            .rewrap(REPO, UP, &s.kek_id, &s.envelope)
            .expect("rewrap")
            .expect("changed");
        assert_eq!(r.kek_id, "k2");
        assert_eq!(v2.rewrap(REPO, UP, &r.kek_id, &r.envelope), Ok(None));
        // The data key is kept; only its wrap and the nonces change.
        let v3 = KeyRing::new(Some(kek("k2", 2)), vec![]).expect("ring");
        assert_eq!(
            v3.open(REPO, UP, "k2", &r.envelope).expect("open").expose(),
            b"pat"
        );
        assert_eq!(
            v3.open(REPO, UP, "k1", &s.envelope).err(),
            Some(CredentialError::UnknownKey)
        );
    }

    #[test]
    fn rings_refuse_bad_and_duplicate_ids() {
        assert!(Kek::new("", [0; KEY]).is_err());
        assert!(Kek::new("a b", [0; KEY]).is_err());
        assert!(Kek::new("x".repeat(65), [0; KEY]).is_err());
        assert_eq!(
            KeyRing::new(Some(kek("k1", 1)), vec![kek("k1", 2)]).err(),
            Some(CredentialError::DuplicateKeyId)
        );
    }

    #[test]
    fn secrets_and_keys_are_redacted() {
        let s = Secret::new(b"ghp_plaintext".to_vec());
        let k = kek("k1", 0x61);
        let ring = KeyRing::new(Some(k.clone()), vec![]).expect("ring");
        for shown in [
            format!("{s:?}"),
            format!("{s}"),
            format!("{s:#?}"),
            format!("{k:?}"),
            format!("{ring:?}"),
        ] {
            assert!(shown.contains("<redacted>"), "{shown}");
            assert!(!shown.contains("ghp_plaintext"), "{shown}");
            assert!(!shown.contains("97, 97"), "{shown}");
        }
    }
}
