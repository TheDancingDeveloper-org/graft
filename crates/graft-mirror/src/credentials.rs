//! How the mirror authenticates to GitHub (decision D8, operator's call).
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
    /// Wraps `value`, which must not be empty, nor hold a line break or a
    /// NUL: either would let it add lines to git's credential protocol.
    pub fn new(value: String) -> Option<SecretToken> {
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
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

/// One upstream credential handed to git through a credential helper that
/// reads a named pipe (D41 §3.4). The secret is never in a URL, argv, the
/// environment or a file: the pipe lives in a private (0700) temporary
/// directory, and a writer task answers each `get` the helper makes with
/// the credential from memory. Dropping the pipe stops the writer and
/// removes the directory, so the credential lives for one poll.
pub struct CredentialPipe {
    dir: tempfile::TempDir,
    writer: tokio::task::JoinHandle<()>,
}

impl fmt::Debug for CredentialPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialPipe(<redacted>)")
    }
}

impl CredentialPipe {
    /// The user name git sends with a token (GitHub's convention).
    pub const USERNAME: &'static str = "x-access-token";

    /// Opens a pipe serving `token` to git's credential helper.
    pub async fn open(token: SecretToken) -> Result<CredentialPipe, MirrorError> {
        let dir = tempfile::Builder::new()
            .prefix("graft-mirror-cred-")
            .tempdir()
            .map_err(|e| MirrorError::Config(format!("credential pipe: {e}")))?;
        let path = dir.path().join("pipe");
        let made = tokio::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| MirrorError::Config(format!("credential pipe: cannot run mkfifo: {e}")))?;
        if !made.success() {
            return Err(MirrorError::Config(
                "credential pipe: mkfifo failed".to_owned(),
            ));
        }
        let writer = tokio::spawn(serve_pipe(path, token));
        Ok(CredentialPipe { dir, writer })
    }

    /// The `credential.helper` value for the upstream at `host` (git's
    /// `host=`, with a port if the URL names one): answers `get` from the
    /// pipe only when git asks over https or http for exactly `host`, and
    /// ignores `store`, `erase` and any other host, so a redirect or a
    /// rewritten URL never receives the credential. `None` if `host` holds
    /// anything but the characters of a host name, an address or a port.
    pub fn helper(&self, host: &str) -> Option<String> {
        let safe = !host.is_empty()
            && host.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
            });
        if !safe {
            return None;
        }
        Some(format!(
            "!f() {{ test \"$1\" = get || return 0; h=; p=; \
             while IFS= read -r l && test -n \"$l\"; do case \"$l\" in \
             host=*) h=\"${{l#host=}}\";; protocol=*) p=\"${{l#protocol=}}\";; esac; done; \
             case \"$p\" in https|http) ;; *) return 0;; esac; \
             test \"$h\" = '{host}' && cat '{}'; return 0; }}; f",
            self.dir.path().join("pipe").display()
        ))
    }
}

impl Drop for CredentialPipe {
    fn drop(&mut self) {
        self.writer.abort();
    }
}

/// Writes the credential to each reader that opens the pipe, until aborted.
async fn serve_pipe(path: std::path::PathBuf, token: SecretToken) {
    use tokio::io::AsyncWriteExt;
    let body = format!(
        "username={}\npassword={}\n",
        CredentialPipe::USERNAME,
        token.expose()
    );
    loop {
        // Opening the write end fails (ENXIO) until a reader has the pipe
        // open; poll for one. Each helper run is one reader.
        match tokio::net::unix::pipe::OpenOptions::new().open_sender(&path) {
            Ok(mut tx) => {
                let _ = tx.write_all(body.as_bytes()).await;
                drop(tx);
                // Let the reader see end of file before the next open.
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

/// Where the mirror's GitHub credential comes from.
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

    /// The token, for wiring that needs it outside a provider.
    pub fn into_secret(self) -> SecretToken {
        self.0
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

impl<T: CredentialProvider> CredentialProvider for &T {
    async fn token(&self) -> Result<SecretToken, ForgeError> {
        (**self).token().await
    }
}

/// No credential: for an upstream reached by path or SSH, where git
/// authenticates on its own. Asking it for a token is refused.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCredentials;

impl CredentialProvider for NoCredentials {
    async fn token(&self) -> Result<SecretToken, ForgeError> {
        Err(ForgeError::Refused(
            "no GitHub credential is configured ([github] in the config)".to_owned(),
        ))
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

    #[tokio::test]
    async fn the_pipe_hands_git_the_credential_without_env_or_argv_holding_it() {
        let secret = "ghp_pipeSECRET123";
        let pipe = CredentialPipe::open(SecretToken::new(secret.to_owned()).unwrap())
            .await
            .expect("pipe");
        let helper = pipe.helper("github.com").expect("a plain host");
        assert!(!helper.contains(secret));
        assert!(!format!("{pipe:?}").contains(secret));
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["credential", "fill"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", "")
            .env("GIT_CONFIG_KEY_1", "credential.helper")
            .env("GIT_CONFIG_VALUE_1", &helper)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        // Twice: each git command asks again, and gets it again.
        for _ in 0..2 {
            let mut child = cmd.spawn().expect("git runs");
            {
                use tokio::io::AsyncWriteExt;
                let mut stdin = child.stdin.take().unwrap();
                stdin
                    .write_all(b"protocol=https\nhost=github.com\n\n")
                    .await
                    .unwrap();
            }
            let out = child.wait_with_output().await.unwrap();
            assert!(out.status.success());
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(text.contains(&format!("password={secret}")), "{text}");
            assert!(text.contains("username=x-access-token"));
        }
        let dir = pipe.dir.path().to_owned();
        drop(pipe);
        assert!(!dir.exists(), "the pipe's directory is removed with it");
    }

    /// Runs `git credential fill` with `helper` for `request`; returns the
    /// password it got, if any.
    async fn fill(helper: &str, request: &str) -> Option<String> {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("git")
            .args(["credential", "fill"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", "")
            .env("GIT_CONFIG_KEY_1", "credential.helper")
            .env("GIT_CONFIG_VALUE_1", helper)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("git runs");
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(request.as_bytes()).await.unwrap();
        drop(stdin);
        let out = child.wait_with_output().await.unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("password=").map(str::to_owned))
    }

    #[tokio::test]
    async fn the_helper_answers_only_for_the_upstream_host_over_http() {
        let secret = "ghp_hostBOUND123";
        let pipe = CredentialPipe::open(SecretToken::new(secret.to_owned()).unwrap())
            .await
            .expect("pipe");
        let helper = pipe.helper("github.com").unwrap();
        // A redirect target, a look-alike host, another port or another
        // protocol gets nothing.
        for request in [
            "protocol=https\nhost=169.254.169.254\n\n",
            "protocol=https\nhost=github.com.evil.example\n\n",
            "protocol=https\nhost=evil.example\n\n",
            "protocol=https\nhost=github.com:8443\n\n",
            "protocol=ftp\nhost=github.com\n\n",
        ] {
            assert_eq!(fill(&helper, request).await, None, "{request:?}");
        }
        assert_eq!(
            fill(&helper, "protocol=https\nhost=github.com\n\n")
                .await
                .as_deref(),
            Some(secret)
        );
    }

    #[tokio::test]
    async fn a_host_with_shell_characters_gets_no_helper() {
        let pipe = CredentialPipe::open(SecretToken::new("t".to_owned()).unwrap())
            .await
            .expect("pipe");
        for host in ["", "a'b", "a b", "a;b", "a$(x)", "a\nb"] {
            assert!(pipe.helper(host).is_none(), "{host:?}");
        }
        assert!(pipe.helper("[2606:4700::1]:443").is_some());
    }

    #[test]
    fn a_token_with_a_line_break_or_nul_is_refused() {
        for bad in ["a\nb", "a\rb", "a\0b", "tok\nhost=evil"] {
            assert!(SecretToken::new(bad.to_owned()).is_none(), "{bad:?}");
        }
        assert!(SecretToken::new("ghp_ok".to_owned()).is_some());
    }
}
