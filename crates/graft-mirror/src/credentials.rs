//! How the PR bridge authenticates to GitHub (decision D8, operator's call).
//!
//! The choice is made in config and has no default: either a static token
//! read from a file, or a GitHub App (its id, an installation id and a
//! private-key file). A missing or doubled choice is a config error at start
//! (see [`crate::config::GithubFile`]). Tokens never come from argv, and they
//! never appear in a log line, an error or a `Debug` string.
//!
//! A GitHub App mints installation tokens from a short-lived JWT signed with
//! its key. Each installation token lives for an hour; the provider reuses
//! one until [`REFRESH_BEFORE`] is left, then mints a fresh one.

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::MirrorError;
use crate::github::ForgeError;

/// The lifetime GitHub gives an installation token.
pub const INSTALLATION_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// Mint a new installation token once this little life is left on the cached
/// one.
pub const REFRESH_BEFORE: Duration = Duration::from_secs(10 * 60);

/// The JWT lives for the most GitHub allows (ten minutes), with a minute of
/// allowance for clock skew.
const JWT_LIFETIME_SECS: i64 = 9 * 60;
const JWT_SKEW_SECS: i64 = 60;

/// A credential. Its `Debug` is redacted, and it is only ever sent as a
/// header or passed to git through the environment.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    /// Wraps `value`, which must not be empty.
    pub fn new(value: String) -> Option<SecretToken> {
        if value.is_empty() {
            None
        } else {
            Some(SecretToken(value))
        }
    }

    /// The token text, for the one place that puts it on the wire.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(<redacted>)")
    }
}

/// Where the bridge's GitHub credential comes from.
pub trait CredentialProvider: Send + Sync {
    /// A token that is valid now, minted or read as the provider needs.
    fn token(&self) -> impl std::future::Future<Output = Result<SecretToken, ForgeError>> + Send;
}

/// A token read from a file, once, at start.
pub struct StaticToken(SecretToken);

impl StaticToken {
    /// Reads and trims the token file at `path`.
    pub fn load(path: &std::path::Path) -> Result<StaticToken, MirrorError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| MirrorError::Config(format!("reading the GitHub token file: {e}")))?;
        SecretToken::new(text.trim().to_owned())
            .map(StaticToken)
            .ok_or_else(|| MirrorError::Config("the GitHub token file is empty".to_owned()))
    }

    /// A provider for a token already in hand (tests and wiring).
    pub fn new(token: SecretToken) -> StaticToken {
        StaticToken(token)
    }
}

impl CredentialProvider for StaticToken {
    async fn token(&self) -> Result<SecretToken, ForgeError> {
        Ok(self.0.clone())
    }
}

/// The installation token the provider holds, and when it was minted.
struct Cached {
    token: SecretToken,
    minted_ms: i64,
}

/// A GitHub App's installation token, minted on demand and cached.
pub struct GithubApp {
    api: String,
    app_id: u64,
    installation_id: u64,
    key: EncodingKey,
    client: reqwest::Client,
    cache: Mutex<Option<Cached>>,
    clock: fn() -> i64,
}

impl fmt::Debug for GithubApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubApp")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .field("key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct Claims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Deserialize)]
struct Minted {
    token: String,
}

impl GithubApp {
    /// A provider for `app_id`'s installation `installation_id`, signing with
    /// the PEM private key at `key_file`. Reads the key now, so a bad key is
    /// a config error at start.
    pub fn load(
        api: &str,
        app_id: u64,
        installation_id: u64,
        key_file: &std::path::Path,
        clock: fn() -> i64,
    ) -> Result<GithubApp, MirrorError> {
        let pem = std::fs::read(key_file)
            .map_err(|e| MirrorError::Config(format!("reading the GitHub App key: {e}")))?;
        let key = EncodingKey::from_rsa_pem(&pem)
            .map_err(|_| MirrorError::Config("the GitHub App key is not an RSA PEM key".into()))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("graft-mirror")
            .build()
            .map_err(|_| MirrorError::Config("cannot build the HTTP client".to_owned()))?;
        Ok(GithubApp {
            api: api.trim_end_matches('/').to_owned(),
            app_id,
            installation_id,
            key,
            client,
            cache: Mutex::new(None),
            clock,
        })
    }

    /// The signed JWT that asks GitHub for an installation token.
    fn app_jwt(&self, now_secs: i64) -> Result<String, ForgeError> {
        let claims = Claims {
            iat: now_secs - JWT_SKEW_SECS,
            exp: now_secs + JWT_LIFETIME_SECS,
            iss: self.app_id.to_string(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|_| ForgeError::Refused("cannot sign the GitHub App JWT".to_owned()))
    }

    /// The cached token if it still has more than [`REFRESH_BEFORE`] left.
    fn fresh(&self, now_ms: i64) -> Option<SecretToken> {
        let cache = self.cache.lock().expect("credential cache lock");
        let cached = cache.as_ref()?;
        let age = now_ms - cached.minted_ms;
        let left = INSTALLATION_TOKEN_LIFETIME.as_millis() as i64 - age;
        (left > REFRESH_BEFORE.as_millis() as i64).then(|| cached.token.clone())
    }

    async fn mint(&self) -> Result<SecretToken, ForgeError> {
        let now = (self.clock)();
        let jwt = self.app_jwt(now / 1000)?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            self.api, self.installation_id
        );
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {jwt}"))
            .map_err(|_| ForgeError::Refused("the JWT is not a header value".to_owned()))?;
        auth.set_sensitive(true);
        let resp = self
            .client
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, auth)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|_| {
                ForgeError::Transient("no answer minting the installation token".into())
            })?;
        let status = resp.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(ForgeError::Transient(format!(
                "status {} minting the installation token",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(ForgeError::Refused(format!(
                "status {} minting the installation token",
                status.as_u16()
            )));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|_| ForgeError::Refused("malformed token answer".to_owned()))?;
        let minted: Minted = serde_json::from_value(body)
            .map_err(|_| ForgeError::Refused("the token answer has no token".to_owned()))?;
        let token = SecretToken::new(minted.token)
            .ok_or_else(|| ForgeError::Refused("GitHub minted an empty token".to_owned()))?;
        *self.cache.lock().expect("credential cache lock") = Some(Cached {
            token: token.clone(),
            minted_ms: (self.clock)(),
        });
        Ok(token)
    }
}

impl CredentialProvider for GithubApp {
    async fn token(&self) -> Result<SecretToken, ForgeError> {
        if let Some(token) = self.fresh((self.clock)()) {
            return Ok(token);
        }
        self.mint().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints() {
        let t = SecretToken::new("ghs_very_secret".to_owned()).unwrap();
        assert!(!format!("{t:?}").contains("ghs_very_secret"));
        assert!(SecretToken::new(String::new()).is_none());
    }

    #[test]
    fn a_static_token_file_is_read_trimmed_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "  tok-1\n").unwrap();
        let p = StaticToken::load(&path).unwrap();
        let rt = tokio_block(p.token()).unwrap();
        assert_eq!(rt.expose(), "tok-1");
        std::fs::write(&path, "\n").unwrap();
        assert!(
            StaticToken::load(&path).is_err(),
            "an empty file is refused"
        );
        assert!(StaticToken::load(&dir.path().join("missing")).is_err());
    }

    fn tokio_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }
}
