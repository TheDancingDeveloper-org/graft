//! Zero-configuration development server setup.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use graft_auth::{Claims, NoDenylist, Token};
use graft_proto::{Principal, PrincipalKind};

use crate::config::ServerConfig;

/// Inputs for a local development server.
#[derive(Clone, Debug)]
pub struct DevOptions {
    /// Server data directory (objects and SQLite metadata).
    pub data_dir: PathBuf,
    /// Private state directory containing the signing key.
    pub state_dir: PathBuf,
    /// Main listener. Port zero asks the operating system for a free port.
    pub listen: SocketAddr,
    /// Repository to create and administer, when requested.
    pub repo: Option<String>,
    /// Permit a non-loopback listener. The caller must opt in explicitly.
    pub insecure_bind: bool,
    /// Previously stored token to reuse, if any.
    pub token: Option<String>,
}

impl DevOptions {
    /// Local defaults with an operating-system-selected port.
    pub fn new(data_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            state_dir: state_dir.into(),
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            repo: None,
            insecure_bind: false,
            token: None,
        }
    }
}

/// Prepared development server state. Its debug form never includes a token.
pub struct DevServer {
    config: ServerConfig,
    token: Option<Token>,
}

impl fmt::Debug for DevServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevServer")
            .field("listen", &self.config.listen)
            .field("objects_dir", &self.config.objects_dir)
            .field("meta_db", &self.config.meta_db)
            .field("key_file", &self.config.key_file)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl DevServer {
    /// Builds local server configuration, creates its key and optional
    /// repository, and mints or validates the repository admin token.
    pub fn prepare(options: DevOptions) -> Result<Self, String> {
        if !options.listen.ip().is_loopback() && !options.insecure_bind {
            return Err(format!(
                "refusing non-loopback development listener {}; pass --insecure-bind to allow it",
                options.listen
            ));
        }
        std::fs::create_dir_all(&options.data_dir)
            .map_err(|error| format!("create {}: {error}", options.data_dir.display()))?;
        create_private_dir(&options.state_dir)?;

        let mut config = ServerConfig {
            listen: options.listen.to_string(),
            objects_dir: options.data_dir.join("objects"),
            meta_db: options.data_dir.join("meta.db"),
            key_file: Some(options.state_dir.join("signing.key")),
            json_logs: false,
            min_free_fraction: 0.0,
            ..ServerConfig::default()
        };
        config.check().map_err(|error| error.to_string())?;
        let signing = crate::keys::load_or_create(&config.key_path())
            .map_err(|error| format!("signing key: {error}"))?;

        let token = match options.repo.as_deref() {
            Some(repo) => {
                crate::admin::repo_create_quiet(&config, repo)?;
                Some(match options.token {
                    Some(encoded) => validate_token(&signing, encoded, repo)?,
                    None => mint_token(&signing, repo)?,
                })
            }
            None if options.token.is_some() => {
                return Err("a stored development token needs --repo".into());
            }
            None => None,
        };

        config.key_file = Some(config.key_path());
        Ok(Self { config, token })
    }

    /// Encoded admin token for the configured repository, if one was named.
    /// Treat the returned value as a secret.
    pub fn token(&self) -> Option<&str> {
        self.token.as_ref().map(Token::expose)
    }

    /// Listener requested by the caller. Port zero is resolved when serving.
    pub fn listen(&self) -> &str {
        &self.config.listen
    }

    /// Runs until shutdown and reports the bound address once ready.
    pub fn serve(
        self,
        ready: impl FnOnce(SocketAddr) -> Result<(), String> + Send + 'static,
    ) -> Result<(), String> {
        crate::serve::serve_ready(self.config, ready)
    }
}

fn create_private_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("secure {}: {error}", path.display()))?;
    }
    Ok(())
}

fn validate_token(
    signing: &graft_auth::SigningKey,
    encoded: String,
    repo: &str,
) -> Result<Token, String> {
    let token = Token::from_encoded(encoded);
    let claims = graft_auth::verify(
        &signing.verifying_key(),
        &token,
        crate::mint::now_s(),
        &NoDenylist,
    )
    .map_err(|error| format!("stored development token: {error}"))?;
    let scope = format!("repo:{repo}:admin");
    if claims.principal.scopes.iter().any(|held| held == &scope) {
        Ok(token)
    } else {
        Err(format!("stored development token lacks {scope}"))
    }
}

fn mint_token(signing: &graft_auth::SigningKey, repo: &str) -> Result<Token, String> {
    let claims = Claims {
        principal: Principal {
            subject: "dev-admin".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{repo}:admin")],
        },
        exp: i64::MAX,
        jti: graft_auth::new_jti().map_err(|error| format!("token id: {error}"))?,
    };
    graft_auth::mint(signing, &claims).map_err(|error| format!("mint token: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn paths(name: &str) -> (graft_testkit::crash::TempDir, PathBuf, PathBuf) {
        let temp = graft_testkit::crash::TempDir::new(name);
        let data = temp.path().join("data");
        let state = temp.path().join("state");
        (temp, data, state)
    }

    #[test]
    fn prepares_private_reusable_credentials() {
        let (_temp, data, state) = paths("server-dev-setup");
        let mut options = DevOptions::new(&data, &state);
        options.repo = Some("demo".into());
        let first = DevServer::prepare(options.clone()).expect("first setup");
        let encoded = first.token().expect("admin token").to_owned();

        options.token = Some(encoded.clone());
        let second = DevServer::prepare(options).expect("second setup");
        assert_eq!(second.token(), Some(encoded.as_str()));
        assert!(!format!("{second:?}").contains(&encoded));
        assert!(data.join("meta.db").is_file());
        assert!(state.join("signing.key").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(state.join("signing.key"))
                .expect("stat key")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let state_mode = fs::metadata(&state)
                .expect("stat state")
                .permissions()
                .mode();
            assert_eq!(state_mode & 0o777, 0o700);
        }
    }

    #[test]
    fn refuses_non_loopback_without_explicit_opt_in() {
        let (_temp, data, state) = paths("server-dev-bind");
        let mut options = DevOptions::new(data, state);
        options.listen = "0.0.0.0:0".parse().expect("address");
        assert!(DevServer::prepare(options.clone()).is_err());
        options.insecure_bind = true;
        assert!(DevServer::prepare(options).is_ok());
    }

    #[test]
    fn rejects_a_stored_token_for_another_repository() {
        let (_temp, data, state) = paths("server-dev-token-scope");
        let mut first = DevOptions::new(&data, &state);
        first.repo = Some("one".into());
        let token = DevServer::prepare(first)
            .expect("first setup")
            .token()
            .expect("token")
            .to_owned();
        let mut second = DevOptions::new(data, state);
        second.repo = Some("two".into());
        second.token = Some(token);
        assert!(DevServer::prepare(second).is_err());
    }
}
