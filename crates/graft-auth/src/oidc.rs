//! CI identity exchange (`docs/design.md` §12.1, work item G3.8): a GitHub
//! Actions OIDC token, or a Woodpecker job token, for a short graft token.
//!
//! The CI system's token is a JWT. It is checked against the issuer's JWKS
//! (key set) and the deployment's trust rules, in this order: size and
//! shape, algorithm, key, signature, then the claims (issuer, audience,
//! lifetime, repository, ref). Only then is a graft token minted, scoped by
//! the [`Binding`] for that repository and no longer than the CI token's own
//! remaining life.
//!
//! The JWKS is supplied by the caller, already fetched (the server refreshes
//! it on a timer and on an unknown `kid`). That keeps this module free of
//! network code, and means a test can use a recorded key set.
//!
//! Only `RS256` and `EdDSA` are accepted. `none`, the HMAC family (which
//! would let anyone holding the public key forge a token) and everything
//! else are refused before any key is looked at. A token with no `kid` is
//! refused: the key must be named, never guessed.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graft_proto::{ApiError, Principal, PrincipalKind};
use rsa::signature::Verifier as _;
use rsa::{BigUint, RsaPublicKey};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;

use crate::{AuthError, Claims, Scope, SigningKey, Token};

/// Longest CI token `verify_jwt` will look at. Real ones are about 2 KiB;
/// this bounds the work an unauthenticated caller can cause.
pub const MAX_JWT_LEN: usize = 16 * 1024;
/// Largest JWKS document accepted.
pub const MAX_JWKS_LEN: usize = 256 * 1024;
/// Most keys a JWKS may carry.
const MAX_JWKS_KEYS: usize = 64;
/// Smallest RSA modulus accepted, in bits.
const MIN_RSA_BITS: usize = 2048;
/// Default clock-skew allowance for `nbf` and `iat`, in seconds.
pub const DEFAULT_LEEWAY_S: i64 = 60;
/// Default longest `exp - iat` accepted, in seconds.
pub const DEFAULT_MAX_LIFETIME_S: i64 = 3600;
/// Default (and highest) TTL of an exchanged graft token, in seconds.
pub const DEFAULT_TOKEN_TTL_S: u64 = 900;
/// Longest subject an exchanged token carries.
const MAX_SUBJECT_BYTES: usize = 256;
/// Longest changeset id text a pin accepts.
const MAX_PIN_BYTES: usize = 64;

/// Why an exchange was refused. Never carries a token or a claim value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OidcError {
    /// Not a well-formed JWT, or a claim has the wrong shape.
    Malformed,
    /// The `alg` is not one graft accepts (`none`, `HS*`, `ES*`, ...).
    UnsupportedAlg,
    /// No usable key in the JWKS for the token's `kid` and `alg`.
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
    /// The `iss` is not a trusted issuer, or is not this issuer's.
    WrongIssuer,
    /// The `aud` does not include this deployment's audience.
    WrongAudience,
    /// Expired, or its lifetime is longer than policy allows.
    Expired,
    /// Not valid yet (`nbf` or `iat` in the future).
    NotYetValid,
    /// A good token for a repository this deployment has no binding for,
    /// or whose repository id differs from the bound one.
    RepoNotBound,
    /// A good token for a bound repository, from a ref the binding does not
    /// allow.
    RefNotAllowed,
    /// A good token for a bound repository, from a workflow the binding does
    /// not allow.
    WorkflowNotAllowed,
    /// The binding grants `write`, which can attest, but the call did not
    /// pin the token to a changeset. Use [`Exchange::exchange_pinned`].
    PinRequired,
    /// The CI job did not run on the commit that belongs to the changeset
    /// it asked to attest: the token has no commit claim, or the caller's
    /// check of commit against changeset failed.
    CommitMismatch,
}

impl fmt::Display for OidcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed CI token",
            Self::UnsupportedAlg => "unsupported CI token algorithm",
            Self::UnknownKey => "no matching key for CI token",
            Self::BadSignature => "bad CI token signature",
            Self::WrongIssuer => "CI token issuer not trusted",
            Self::WrongAudience => "CI token audience mismatch",
            Self::Expired => "CI token expired",
            Self::NotYetValid => "CI token not yet valid",
            Self::RepoNotBound => "CI token repository is not bound to a graft repository",
            Self::RefNotAllowed => "CI token ref is not allowed for this repository",
            Self::WorkflowNotAllowed => "CI token workflow is not allowed for this repository",
            Self::PinRequired => "this binding needs the token pinned to a changeset",
            Self::CommitMismatch => "CI job commit does not match the changeset",
        })
    }
}

impl std::error::Error for OidcError {}

impl From<OidcError> for ApiError {
    fn from(e: OidcError) -> Self {
        match e {
            OidcError::RepoNotBound
            | OidcError::RefNotAllowed
            | OidcError::WorkflowNotAllowed
            | OidcError::PinRequired
            | OidcError::CommitMismatch => ApiError::PermissionDenied {
                scope: "ci:exchange".into(),
                path: None,
            },
            _ => ApiError::Unauthenticated,
        }
    }
}

impl From<OidcError> for AuthError {
    fn from(e: OidcError) -> Self {
        match e {
            OidcError::RepoNotBound
            | OidcError::RefNotAllowed
            | OidcError::WorkflowNotAllowed
            | OidcError::PinRequired
            | OidcError::CommitMismatch => AuthError::Denied {
                scope: "ci:exchange".into(),
                path: None,
            },
            OidcError::Expired => AuthError::Expired,
            OidcError::BadSignature => AuthError::BadSignature,
            _ => AuthError::Malformed,
        }
    }
}

/// Which CI system issued a token. It decides which claims name the
/// repository and the ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    /// GitHub Actions: `repository`, `repository_id`, `ref`.
    GitHubActions,
    /// A Woodpecker job token: `repo` (`owner/name`), `repo_id`, `ref`.
    Woodpecker,
}

impl Provider {
    fn repo_claim(self) -> &'static str {
        match self {
            Self::GitHubActions => "repository",
            Self::Woodpecker => "repo",
        }
    }

    fn repo_id_claim(self) -> &'static str {
        match self {
            Self::GitHubActions => "repository_id",
            Self::Woodpecker => "repo_id",
        }
    }

    /// GitHub treats owner and repository names case-insensitively.
    /// The claim naming the workflow file that is running, if the CI has one.
    fn workflow_claim(self) -> Option<&'static str> {
        match self {
            Self::GitHubActions => Some("job_workflow_ref"),
            Self::Woodpecker => None,
        }
    }

    /// The claim naming the commit the job runs on. For Woodpecker the
    /// name is an assumption (see `ci/README.md`).
    fn sha_claim(self) -> &'static str {
        match self {
            Self::GitHubActions => "sha",
            Self::Woodpecker => "commit",
        }
    }

    fn names_fold_case(self) -> bool {
        matches!(self, Self::GitHubActions)
    }

    fn subject_prefix(self) -> &'static str {
        match self {
            Self::GitHubActions => "ci:github:",
            Self::Woodpecker => "ci:woodpecker:",
        }
    }
}

/// A key set (RFC 7517). Keys of a type graft does not verify are skipped.
#[derive(Clone, Debug, Default)]
pub struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Clone)]
struct Jwk {
    kid: String,
    /// The `alg` the key restricts itself to, if it says.
    alg: Option<String>,
    material: KeyMaterial,
}

impl fmt::Debug for Jwk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Public key material, but kept out of logs like the rest.
        f.debug_struct("Jwk").field("kid", &self.kid).finish()
    }
}

#[derive(Clone)]
enum KeyMaterial {
    Rsa(RsaPublicKey),
    Ed25519(ed25519_dalek::VerifyingKey),
}

#[derive(Deserialize)]
struct RawJwks {
    keys: Vec<RawJwk>,
}

#[derive(Deserialize)]
struct RawJwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default, rename = "use")]
    use_: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
}

impl Jwks {
    /// Parses a JWKS document. Fails with `Malformed` on a document that is
    /// not a key set. Within a key set, a key that is not usable (an unknown
    /// `kty`, an encryption key, an RSA key under 2048 bits, no `kid`) is
    /// skipped rather than failing the set, so one odd key at the issuer does
    /// not lock every CI job out.
    pub fn from_json(bytes: &[u8]) -> Result<Self, OidcError> {
        if bytes.len() > MAX_JWKS_LEN {
            return Err(OidcError::Malformed);
        }
        let raw: RawJwks = serde_json::from_slice(bytes).map_err(|_| OidcError::Malformed)?;
        if raw.keys.len() > MAX_JWKS_KEYS {
            return Err(OidcError::Malformed);
        }
        let keys = raw.keys.iter().filter_map(Jwk::from_raw).collect();
        Ok(Self { keys })
    }

    /// How many usable keys the set holds.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set holds no usable key.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn find(&self, kid: &str) -> Option<&Jwk> {
        self.keys.iter().find(|k| k.kid == kid)
    }
}

impl Jwk {
    fn from_raw(raw: &RawJwk) -> Option<Self> {
        if raw.use_.as_deref().is_some_and(|u| u != "sig") {
            return None;
        }
        let kid = raw.kid.clone().filter(|k| !k.is_empty())?;
        let b64 = |s: &Option<String>| URL_SAFE_NO_PAD.decode(s.as_deref()?).ok();
        let material = match raw.kty.as_str() {
            "RSA" => {
                let n = BigUint::from_bytes_be(&b64(&raw.n)?);
                let e = BigUint::from_bytes_be(&b64(&raw.e)?);
                if n.bits() < MIN_RSA_BITS {
                    return None;
                }
                KeyMaterial::Rsa(RsaPublicKey::new(n, e).ok()?)
            }
            "OKP" if raw.crv.as_deref() == Some("Ed25519") => {
                let x: [u8; 32] = b64(&raw.x)?.try_into().ok()?;
                KeyMaterial::Ed25519(ed25519_dalek::VerifyingKey::from_bytes(&x).ok()?)
            }
            _ => return None,
        };
        Some(Self {
            kid,
            alg: raw.alg.clone(),
            material,
        })
    }
}

/// What a deployment trusts from one CI issuer.
#[derive(Clone, Debug)]
pub struct Issuer {
    /// Which CI system it is.
    pub provider: Provider,
    /// The expected `iss`, compared exactly (for GitHub Actions,
    /// `https://token.actions.githubusercontent.com`).
    pub issuer: String,
    /// The audience jobs must request (GitHub's `audience` input). A value
    /// specific to this graft deployment, so a token minted for another
    /// service is refused.
    pub audience: String,
    /// The issuer's current keys.
    pub jwks: Jwks,
    /// Clock-skew allowance for `nbf` and `iat`, seconds. Never applied to
    /// `exp`.
    pub leeway_s: i64,
    /// Longest `exp - iat` accepted, seconds.
    pub max_lifetime_s: i64,
}

impl Issuer {
    /// An issuer with the default leeway and lifetime limit.
    pub fn new(provider: Provider, issuer: String, audience: String, jwks: Jwks) -> Self {
        Self {
            provider,
            issuer,
            audience,
            jwks,
            leeway_s: DEFAULT_LEEWAY_S,
            max_lifetime_s: DEFAULT_MAX_LIFETIME_S,
        }
    }
}

/// The facts about a CI job a verified token establishes.
#[derive(Clone, PartialEq, Eq)]
pub struct CiIdentity {
    /// The CI system.
    pub provider: Provider,
    /// The CI repository as the CI names it (`owner/name`).
    pub repo: String,
    /// The CI's numeric repository id, when the token carries one.
    pub repo_id: Option<String>,
    /// The git ref the job runs for (`refs/heads/graft/...`), when present.
    pub git_ref: Option<String>,
    /// The workflow that is running (GitHub's `job_workflow_ref`:
    /// `owner/repo/.github/workflows/file.yml@ref`; for a reusable workflow,
    /// the called one), when the CI says.
    pub workflow: Option<String>,
    /// The commit the job runs on (GitHub's `sha`), when the token has one.
    pub sha: Option<String>,
    /// The token's `sub`.
    pub subject: String,
    /// Expiry of the CI token, seconds since the epoch.
    pub exp: i64,
}

impl fmt::Debug for CiIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CiIdentity")
            .field("provider", &self.provider)
            .field("repo", &self.repo)
            .field("git_ref", &self.git_ref)
            .finish_non_exhaustive()
    }
}

/// Verifies `jwt` against `issuer` at `now_s` (seconds since the epoch) and
/// returns the identity it establishes. Does not look at bindings: use
/// [`Exchange`] for the whole exchange.
pub fn verify_jwt(issuer: &Issuer, jwt: &str, now_s: i64) -> Result<CiIdentity, OidcError> {
    let parts = Parts::split(jwt)?;
    let header: Header = parts.json(parts.header)?;
    let alg = match header.alg.as_str() {
        "RS256" => Alg::Rs256,
        "EdDSA" => Alg::EdDsa,
        _ => return Err(OidcError::UnsupportedAlg),
    };
    let key = issuer
        .jwks
        .find(header.kid.as_deref().ok_or(OidcError::UnknownKey)?)
        .ok_or(OidcError::UnknownKey)?;
    if key.alg.as_deref().is_some_and(|a| a != header.alg) {
        return Err(OidcError::UnknownKey);
    }
    let sig = URL_SAFE_NO_PAD
        .decode(parts.sig)
        .map_err(|_| OidcError::Malformed)?;
    verify_signature(key, alg, parts.signing_input, &sig)?;

    // The signature is good: from here the claims are the issuer's own.
    let claims: Value = parts.json(parts.payload)?;
    check_claims(issuer, &claims, now_s)
}

#[derive(Clone, Copy)]
enum Alg {
    Rs256,
    EdDsa,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

struct Parts<'a> {
    signing_input: &'a [u8],
    header: &'a str,
    payload: &'a str,
    sig: &'a str,
}

impl<'a> Parts<'a> {
    fn split(jwt: &'a str) -> Result<Self, OidcError> {
        if jwt.len() > MAX_JWT_LEN {
            return Err(OidcError::Malformed);
        }
        let mut it = jwt.split('.');
        let (Some(header), Some(payload), Some(sig), None) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(OidcError::Malformed);
        };
        let signing_input = &jwt.as_bytes()[..header.len() + 1 + payload.len()];
        Ok(Self {
            signing_input,
            header,
            payload,
            sig,
        })
    }

    fn json<T: serde::de::DeserializeOwned>(&self, part: &str) -> Result<T, OidcError> {
        let raw = URL_SAFE_NO_PAD
            .decode(part)
            .map_err(|_| OidcError::Malformed)?;
        serde_json::from_slice(&raw).map_err(|_| OidcError::Malformed)
    }
}

fn verify_signature(key: &Jwk, alg: Alg, msg: &[u8], sig: &[u8]) -> Result<(), OidcError> {
    match (alg, &key.material) {
        (Alg::Rs256, KeyMaterial::Rsa(pk)) => {
            let vk = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(pk.clone());
            let sig =
                rsa::pkcs1v15::Signature::try_from(sig).map_err(|_| OidcError::BadSignature)?;
            vk.verify(msg, &sig).map_err(|_| OidcError::BadSignature)
        }
        (Alg::EdDsa, KeyMaterial::Ed25519(vk)) => {
            let sig =
                ed25519_dalek::Signature::from_slice(sig).map_err(|_| OidcError::BadSignature)?;
            vk.verify_strict(msg, &sig)
                .map_err(|_| OidcError::BadSignature)
        }
        // The header names an algorithm the key is not for.
        _ => Err(OidcError::UnknownKey),
    }
}

fn check_claims(issuer: &Issuer, claims: &Value, now_s: i64) -> Result<CiIdentity, OidcError> {
    let text = |name: &str| claims.get(name).and_then(Value::as_str);
    let int = |name: &str| {
        claims
            .get(name)
            .map(|v| v.as_i64().ok_or(OidcError::Malformed))
    };

    if text("iss") != Some(issuer.issuer.as_str()) {
        return Err(OidcError::WrongIssuer);
    }
    let aud_ok = match claims.get("aud") {
        Some(Value::String(a)) => *a == issuer.audience,
        Some(Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(&issuer.audience)),
        _ => false,
    };
    if !aud_ok {
        return Err(OidcError::WrongAudience);
    }

    let exp = int("exp").ok_or(OidcError::Malformed)??;
    if now_s >= exp {
        return Err(OidcError::Expired);
    }
    let iat = int("iat").transpose()?;
    let nbf = int("nbf").transpose()?;
    for t in [iat, nbf].into_iter().flatten() {
        if t > now_s.saturating_add(issuer.leeway_s) {
            return Err(OidcError::NotYetValid);
        }
    }
    // A token with no `iat` cannot have its lifetime checked, and one with a
    // lifetime beyond policy is not "short-lived" in the sense we rely on.
    let iat = iat.ok_or(OidcError::Malformed)?;
    if exp.saturating_sub(iat) > issuer.max_lifetime_s {
        return Err(OidcError::Expired);
    }

    let repo = text(issuer.provider.repo_claim())
        .filter(|r| !r.is_empty())
        .ok_or(OidcError::Malformed)?;
    let repo_id = match claims.get(issuer.provider.repo_id_claim()) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => return Err(OidcError::Malformed),
    };
    let subject = text("sub")
        .filter(|s| !s.is_empty())
        .ok_or(OidcError::Malformed)?;
    Ok(CiIdentity {
        provider: issuer.provider,
        repo: repo.to_owned(),
        repo_id,
        git_ref: text("ref").map(str::to_owned),
        sha: text(issuer.provider.sha_claim()).map(str::to_owned),
        workflow: issuer
            .provider
            .workflow_claim()
            .and_then(text)
            .map(str::to_owned),
        subject: subject.to_owned(),
        exp,
    })
}

/// Which CI repository maps to which graft repository, and what its jobs may
/// do.
#[derive(Clone, Debug)]
pub struct Binding {
    provider: Provider,
    ci_repo: String,
    ci_repo_id: Option<String>,
    graft_repo: String,
    refs: Vec<String>,
    workflows: Vec<String>,
    scopes: Vec<String>,
}

impl Binding {
    /// Binds `ci_repo` (`owner/name`) to `graft_repo`, granting `scopes`.
    ///
    /// Fails with `Malformed` when a scope does not parse, names another
    /// repository, or is `admin` or `land` (CI attests and reads; it never
    /// lands or administers), or when no scope is given.
    pub fn new(
        provider: Provider,
        ci_repo: &str,
        graft_repo: &str,
        scopes: &[&str],
    ) -> Result<Self, AuthError> {
        if ci_repo.is_empty() || scopes.is_empty() {
            return Err(AuthError::Malformed);
        }
        for text in scopes {
            let scope = Scope::parse(text)?;
            let allowed = matches!(scope, Scope::Read { .. } | Scope::Write { .. });
            if !allowed || scope.repo() != Some(graft_repo) {
                return Err(AuthError::Malformed);
            }
        }
        Ok(Self {
            provider,
            ci_repo: ci_repo.to_owned(),
            ci_repo_id: None,
            graft_repo: graft_repo.to_owned(),
            refs: Vec::new(),
            workflows: Vec::new(),
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    /// Also requires the token's numeric repository id to equal `id`, so a
    /// deleted repository's name, taken over by someone else, does not
    /// inherit the binding.
    #[must_use]
    pub fn with_repo_id(mut self, id: &str) -> Self {
        self.ci_repo_id = Some(id.to_owned());
        self
    }

    /// Restricts jobs to the given refs. An entry ending in `/`
    /// (`refs/heads/graft/`) matches every ref under it; any other entry is
    /// an exact ref (`refs/heads/main`), so `refs/heads/main-evil` does not
    /// pass. With none set, any ref is accepted.
    #[must_use]
    pub fn with_refs(mut self, refs: &[&str]) -> Self {
        self.refs = refs.iter().map(|p| (*p).to_owned()).collect();
        self
    }

    /// Restricts jobs to the given workflows (GitHub's `job_workflow_ref`),
    /// matched like [`Binding::with_refs`]. Pin the shared reusable workflow
    /// here: otherwise anyone who can push a branch can add a workflow that
    /// asks for a token and posts attestations of its own choosing. A job
    /// whose token names no workflow is refused when this is set.
    #[must_use]
    pub fn with_workflows(mut self, workflows: &[&str]) -> Self {
        self.workflows = workflows.iter().map(|p| (*p).to_owned()).collect();
        self
    }

    /// The graft repository the binding grants access to.
    pub fn graft_repo(&self) -> &str {
        &self.graft_repo
    }

    /// Whether jobs under this binding can attest or otherwise write.
    fn grants_write(&self) -> bool {
        self.scopes
            .iter()
            .any(|s| matches!(Scope::parse(s), Ok(Scope::Write { .. })))
    }

    fn matches_repo(&self, id: &CiIdentity) -> bool {
        let same = |a: &str, b: &str| {
            if self.provider.names_fold_case() {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        id.provider == self.provider
            && same(&id.repo, &self.ci_repo)
            && self
                .ci_repo_id
                .as_deref()
                .is_none_or(|want| id.repo_id.as_deref() == Some(want))
    }

    fn allows_ref(&self, id: &CiIdentity) -> bool {
        allowed(&self.refs, id.git_ref.as_deref())
    }

    fn allows_workflow(&self, id: &CiIdentity) -> bool {
        allowed(&self.workflows, id.workflow.as_deref())
    }
}

/// Whether `value` passes `rules`: no rules allow anything; otherwise the
/// value must equal a rule, or sit under a rule that ends in `/`.
fn allowed(rules: &[String], value: Option<&str>) -> bool {
    rules.is_empty()
        || value.is_some_and(|v| {
            rules
                .iter()
                .any(|r| v == r || (r.ends_with('/') && v.starts_with(r.as_str())))
        })
}

/// The whole exchange: trusted issuers and repository bindings.
#[derive(Clone, Debug)]
pub struct Exchange {
    issuers: Vec<Issuer>,
    bindings: Vec<Binding>,
    max_ttl_s: u64,
}

impl Default for Exchange {
    fn default() -> Self {
        Self::new()
    }
}

impl Exchange {
    /// An exchange that trusts nothing yet, minting tokens of at most
    /// [`DEFAULT_TOKEN_TTL_S`].
    pub fn new() -> Self {
        Self {
            issuers: Vec::new(),
            bindings: Vec::new(),
            max_ttl_s: DEFAULT_TOKEN_TTL_S,
        }
    }

    /// Trusts `issuer`.
    #[must_use]
    pub fn trust(mut self, issuer: Issuer) -> Self {
        self.issuers.push(issuer);
        self
    }

    /// Adds a repository binding.
    #[must_use]
    pub fn bind(mut self, binding: Binding) -> Self {
        self.bindings.push(binding);
        self
    }

    /// Sets the longest TTL of an exchanged token, seconds. Never above
    /// [`DEFAULT_TOKEN_TTL_S`].
    #[must_use]
    pub fn with_max_ttl_s(mut self, ttl_s: u64) -> Self {
        self.max_ttl_s = ttl_s.min(DEFAULT_TOKEN_TTL_S);
        self
    }

    /// Replaces the keys of the issuer `issuer` (after a JWKS refresh).
    /// Returns whether such an issuer exists.
    pub fn set_jwks(&mut self, issuer: &str, jwks: Jwks) -> bool {
        let mut found = false;
        for i in self.issuers.iter_mut().filter(|i| i.issuer == issuer) {
            i.jwks = jwks.clone();
            found = true;
        }
        found
    }

    /// Verifies `jwt` and, if its repository is bound, mints a graft token
    /// (kind `ci`) at `now_s` for a binding that grants no `write`.
    ///
    /// A binding with `write` can attest, and such a token must be pinned
    /// to one changeset: this fails with `PinRequired` for it. Use
    /// [`Exchange::exchange_pinned`].
    ///
    /// The token carries exactly the binding's scopes. Its life is the
    /// smaller of the configured maximum and what the CI token has left, so
    /// a graft token never outlives the proof it came from. Its subject is
    /// `ci:<provider>:<repo>`, so reads are attributed to the repository
    /// that made them.
    pub fn exchange(&self, key: &SigningKey, jwt: &str, now_s: i64) -> Result<Token, OidcError> {
        let id = self.verify(jwt, now_s)?;
        let binding = self.select(&id)?;
        if binding.grants_write() {
            return Err(OidcError::PinRequired);
        }
        let subject = format!("{}{}", id.provider.subject_prefix(), id.repo);
        self.mint(key, &id, binding, subject, now_s)
    }

    /// Like [`Exchange::exchange`], for a binding that grants `write`, and
    /// the only way to get such a token. The job asks to attest `changeset`,
    /// and `commit_is(sha)` must say that the commit the job ran on (the
    /// token's `sha` claim) is the one the mirror exported for that
    /// changeset. It is called once, after the token and binding checks, and
    /// only with a verified `sha`; a token without one is `CommitMismatch`.
    ///
    /// Without this, anyone who can push a branch could run a workflow on a
    /// commit that carries another draft's trailer and attest that draft.
    ///
    /// A write binding must also name its refs, and for GitHub the pinned
    /// reusable workflow (`job_workflow_ref`): a Woodpecker pipeline is the
    /// branch's own file and cannot be pinned, so Woodpecker write bindings
    /// are refused with `WorkflowNotAllowed`.
    ///
    /// The subject is `ci:<provider>:<repo>#<changeset>`; see
    /// [`pinned_changeset`]. The holder of the token may attest that
    /// changeset only, if the server enforces the pin where it checks
    /// `Attest`.
    pub fn exchange_pinned(
        &self,
        key: &SigningKey,
        jwt: &str,
        now_s: i64,
        changeset: &str,
        commit_is: impl FnOnce(&str) -> bool,
    ) -> Result<Token, OidcError> {
        if changeset.is_empty()
            || changeset.len() > MAX_PIN_BYTES
            || !changeset.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(OidcError::Malformed);
        }
        let id = self.verify(jwt, now_s)?;
        let binding = self.select(&id)?;
        if binding.grants_write() {
            if binding.refs.is_empty() {
                return Err(OidcError::RefNotAllowed);
            }
            if id.provider.workflow_claim().is_none() || binding.workflows.is_empty() {
                return Err(OidcError::WorkflowNotAllowed);
            }
        }
        match id.sha.as_deref() {
            Some(sha) if !sha.is_empty() && commit_is(sha) => {}
            _ => return Err(OidcError::CommitMismatch),
        }
        let subject = format!("{}{}#{changeset}", id.provider.subject_prefix(), id.repo);
        self.mint(key, &id, binding, subject, now_s)
    }

    /// The binding for `id`: among those for its repository, the first
    /// whose ref and workflow rules pass. When none does, the first one's
    /// refusal, so the error names a rule that exists.
    fn select(&self, id: &CiIdentity) -> Result<&Binding, OidcError> {
        let mut refused = None;
        for b in self.bindings.iter().filter(|b| b.matches_repo(id)) {
            if !b.allows_ref(id) {
                refused.get_or_insert(OidcError::RefNotAllowed);
            } else if !b.allows_workflow(id) {
                refused.get_or_insert(OidcError::WorkflowNotAllowed);
            } else {
                return Ok(b);
            }
        }
        Err(refused.unwrap_or(OidcError::RepoNotBound))
    }

    fn mint(
        &self,
        key: &SigningKey,
        id: &CiIdentity,
        binding: &Binding,
        subject: String,
        now_s: i64,
    ) -> Result<Token, OidcError> {
        let remaining = u64::try_from(id.exp.saturating_sub(now_s)).unwrap_or(0);
        let ttl = self.max_ttl_s.min(remaining);
        if ttl == 0 || subject.len() > MAX_SUBJECT_BYTES {
            return Err(OidcError::Malformed);
        }
        let jti = crate::new_jti().map_err(|_| OidcError::Malformed)?;
        let claims = Claims {
            principal: Principal {
                subject,
                kind: PrincipalKind::Ci,
                session: None,
                work_item: None,
                scopes: binding.scopes.clone(),
            },
            exp: now_s.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX)),
            jti,
        };
        crate::mint(key, &claims).map_err(|_| OidcError::Malformed)
    }

    /// Verifies `jwt` against the issuer its (unverified) `iss` names.
    /// Reading `iss` first only selects whose keys to check with; every
    /// claim, `iss` included, is checked after the signature.
    pub fn verify(&self, jwt: &str, now_s: i64) -> Result<CiIdentity, OidcError> {
        let iss = peek_issuer(jwt)?;
        let mut last = OidcError::WrongIssuer;
        // Two providers may share an issuer URL in a test or a proxy setup;
        // try each until one accepts.
        for issuer in self.issuers.iter().filter(|i| i.issuer == iss) {
            match verify_jwt(issuer, jwt, now_s) {
                Ok(id) => return Ok(id),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

/// The changeset a CI token is pinned to, from its subject
/// (`ci:<provider>:<repo>#<changeset>`, as minted by
/// [`Exchange::exchange_pinned`]). `None` for any other subject.
///
/// A server must refuse an `Attest` by a `ci` principal whose subject is
/// pinned to a different changeset, and one whose subject is not pinned.
pub fn pinned_changeset(subject: &str) -> Option<&str> {
    let rest = subject.strip_prefix("ci:")?;
    let (_, pin) = rest.rsplit_once('#')?;
    (!pin.is_empty()).then_some(pin)
}

fn peek_issuer(jwt: &str) -> Result<String, OidcError> {
    let parts = Parts::split(jwt)?;
    let claims: Value = parts.json(parts.payload)?;
    claims
        .get("iss")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(OidcError::Malformed)
}
