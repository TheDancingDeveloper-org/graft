//! User configuration: named servers and token-file references.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Failure;

pub(crate) const CONFIG_ENV: &str = "GRAFT_CONFIG";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct UserConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_server: Option<String>,
    #[serde(default)]
    pub(crate) servers: BTreeMap<String, ServerConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ServerConfig {
    pub(crate) endpoint: String,
    pub(crate) token_file: PathBuf,
}

pub(crate) fn config_path() -> Result<PathBuf, Failure> {
    if let Some(path) = env::var_os(CONFIG_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| {
            Failure::usage(
                "cannot find the user config directory: set GRAFT_CONFIG, XDG_CONFIG_HOME, or HOME",
            )
        })?;
    Ok(base.join("graft").join("config.toml"))
}

pub(crate) fn load() -> Result<UserConfig, Failure> {
    let path = config_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|error| {
            Failure::new(
                crate::Exit::Failure,
                "config",
                format!("cannot parse {}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UserConfig::default()),
        Err(error) => Err(io_failure("read", &path, error)),
    }
}

pub(crate) fn save(config: &UserConfig) -> Result<(), Failure> {
    let path = config_path()?;
    let text = toml::to_string_pretty(config).map_err(|error| {
        Failure::new(
            crate::Exit::Failure,
            "config",
            format!("cannot encode config: {error}"),
        )
    })?;
    write_private(&path, text.as_bytes())
}

pub(crate) fn token_path(name: &str) -> Result<PathBuf, Failure> {
    validate_name(name)?;
    let path = config_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| Failure::usage("GRAFT_CONFIG must name a file in a directory"))?;
    Ok(dir.join("tokens").join(name))
}

pub(crate) fn store_token(path: &Path, token: &str) -> Result<(), Failure> {
    let token = token.trim();
    if token.is_empty() {
        return Err(Failure::usage("the token is empty"));
    }
    write_private(path, format!("{token}\n").as_bytes())
}

pub(crate) fn remove_token(path: &Path) -> Result<(), Failure> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_failure("remove", path, error)),
    }
}

pub(crate) fn validate_name(name: &str) -> Result<(), Failure> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(Failure::usage(
            "server names may contain only letters, digits, '.', '-' and '_'",
        ))
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    let parent = path
        .parent()
        .ok_or_else(|| Failure::usage("config path has no parent directory"))?;
    fs::create_dir_all(parent).map_err(|error| io_failure("create", parent, error))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|error| io_failure("write", &tmp, error))?;
        file.write_all(bytes)
            .map_err(|error| io_failure("write", &tmp, error))?;
        file.sync_all()
            .map_err(|error| io_failure("sync", &tmp, error))?;
        fs::rename(&tmp, path).map_err(|error| io_failure("replace", path, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|error| io_failure("secure", path, error))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn io_failure(action: &str, path: &Path, error: std::io::Error) -> Failure {
    Failure::new(
        crate::Exit::Failure,
        "config",
        format!("cannot {action} {}: {}", path.display(), error.kind()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_names_that_can_escape_the_token_directory() {
        for name in ["", ".", "..", "a/b", "a b"] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
        for name in ["dev", "team.dev", "server-1", "server_1"] {
            assert!(validate_name(name).is_ok(), "{name:?}");
        }
    }
}
