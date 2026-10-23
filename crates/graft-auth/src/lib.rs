//! Principals, tokens, scopes and path rules (`docs/design.md` §12, work
//! item G1.9), and the exchange of CI identity tokens for graft tokens
//! ([`oidc`], G3.8). Token and key values never appear in `Debug` or `Display`
//! output.

use std::fmt;

use graft_proto::ApiError;

pub mod oidc;
mod scope;
mod token;

pub use scope::{Scope, authorize};
pub use token::{
    Claims, Denylist, MAX_TOKEN_LEN, NoDenylist, SigningKey, Token, VerifyingKey, mint, new_jti,
    verify,
};

/// An authentication or authorization failure. Never carries a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// Malformed token, claims or scope.
    Malformed,
    /// Bad signature (including a token signed by another key).
    BadSignature,
    /// Expired.
    Expired,
    /// Revoked.
    Revoked,
    /// Lacks the needed scope.
    Denied {
        /// The scope that was needed, in its text form.
        scope: String,
        /// The path, for path-scoped checks.
        path: Option<String>,
    },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed token or scope"),
            Self::BadSignature => f.write_str("bad token signature"),
            Self::Expired => f.write_str("token expired"),
            Self::Revoked => f.write_str("token revoked"),
            Self::Denied { scope, path: None } => write!(f, "permission denied: needs {scope}"),
            Self::Denied {
                scope,
                path: Some(path),
            } => write!(f, "permission denied: needs {scope} for {path}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Malformed
            | AuthError::BadSignature
            | AuthError::Expired
            | AuthError::Revoked => ApiError::Unauthenticated,
            AuthError::Denied { scope, path } => ApiError::PermissionDenied { scope, path },
        }
    }
}
