//! Principals, tokens, scopes and path rules (`docs/design.md` §12, work
//! item G1.9). Token values never appear in `Debug` or `Display` output.

use std::fmt;

use graft_model::RepoPath;
use graft_proto::Principal;

/// A scope, in the text form `repo:<name>:<perm>[:<arg>]` or `objects:read`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Read a repository.
    Read {
        /// Repository.
        repo: String,
    },
    /// Write paths, optionally under a prefix.
    Write {
        /// Repository.
        repo: String,
        /// Prefix restriction.
        prefix: Option<RepoPath>,
    },
    /// Land on a line.
    Land {
        /// Repository.
        repo: String,
        /// Line.
        line: String,
    },
    /// Administer a repository.
    Admin {
        /// Repository.
        repo: String,
    },
    /// Read objects for any repository the caller can read (node daemon).
    ObjectsRead,
}

impl Scope {
    /// Parses the text form.
    pub fn parse(_s: &str) -> Result<Self, AuthError> {
        todo!("G1.9")
    }
}

/// Token claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    /// Subject.
    pub sub: String,
    /// The principal it establishes.
    pub principal: Principal,
    /// Expiry, seconds since the epoch.
    pub exp: i64,
    /// Unique id, for revocation.
    pub jti: String,
}

/// An encoded token. Opaque; `Debug` redacts it.
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
pub struct SigningKey {
    _private: (),
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

/// The public half, held by verifiers (server, node).
#[derive(Clone, Debug)]
pub struct VerifyingKey {
    _private: (),
}

/// Revoked token ids.
pub trait Denylist: Send + Sync {
    /// Whether `jti` is revoked.
    fn is_revoked(&self, jti: &str) -> bool;
}

/// An authentication or authorization failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// Malformed token or scope.
    Malformed,
    /// Bad signature.
    BadSignature,
    /// Expired.
    Expired,
    /// Revoked.
    Revoked,
    /// Lacks the named scope.
    Denied(String),
}

/// Mints a token.
pub fn mint(_key: &SigningKey, _claims: &Claims) -> Token {
    todo!("G1.9")
}

/// Verifies a token and returns its claims.
pub fn verify(
    _key: &VerifyingKey,
    _token: &Token,
    _now_s: i64,
    _deny: &dyn Denylist,
) -> Result<Claims, AuthError> {
    todo!("G1.9")
}

/// Checks that `principal` holds `needed`, applying path prefixes.
pub fn authorize(
    _principal: &Principal,
    _needed: &Scope,
    _path: Option<&RepoPath>,
) -> Result<(), AuthError> {
    todo!("G1.9")
}
