//! The one-time bootstrap admin token (`bootstrap_admin_token_file`).
//!
//! When the key is set and the file does not exist at start-up, the server
//! mints one admin token for each repository in `bootstrap_admin_repos` (the
//! scope `serve --dev` gives its admin token, with the `admin token mint`
//! TTL) and writes it to the file, mode 0600, creating the parent directory
//! 0700. An existing file is left alone. The token is written to the file
//! only: never to a log, an error or `Debug` output.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use graft_auth::SigningKey;
use graft_proto::PrincipalKind;

use crate::mint::{MintRequest, mint, now_s};

/// The subject of the bootstrap token.
const SUBJECT: &str = "bootstrap-admin";

/// Why the bootstrap token could not be written. Names the path, never the
/// token.
#[derive(Debug)]
pub(crate) struct BootstrapError {
    path: PathBuf,
    reason: String,
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bootstrap admin token {}: {}",
            self.path.display(),
            self.reason
        )
    }
}

impl std::error::Error for BootstrapError {}

/// Writes the bootstrap token to `path` unless the file exists. Returns
/// whether it wrote one.
pub(crate) fn ensure(
    path: &Path,
    repos: &[String],
    key: &SigningKey,
) -> Result<bool, BootstrapError> {
    let fail = |reason: String| BootstrapError {
        path: path.to_path_buf(),
        reason,
    };
    match fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(fail(e.to_string())),
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir).map_err(|e| fail(format!("create {}: {e}", dir.display())))?;
    }
    let token = mint(
        key,
        MintRequest {
            subject: SUBJECT.into(),
            kind: PrincipalKind::Human,
            scopes: repos.iter().map(|r| format!("repo:{r}:admin")).collect(),
            ttl_s: crate::admin::DEFAULT_TTL_S,
            session: None,
            work_item: None,
        },
        now_s(),
    )
    .map_err(|e| fail(format!("mint: {e}")))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        // Created by another process since the check: theirs stands.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(fail(e.to_string())),
    };
    let written = file
        .write_all(token.expose().as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        // A partial file would block every later attempt; remove it.
        drop(file);
        let _ = fs::remove_file(path);
        return Err(fail(e.to_string()));
    }
    Ok(true)
}

/// Creates `dir` (and its parents) when absent; a directory this call
/// creates is mode 0700. An existing one is left as it is.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_auth::NoDenylist;

    #[test]
    fn writes_once_and_never_overwrites() {
        let temp = graft_testkit::crash::TempDir::new("server-bootstrap-unit");
        let path = temp.path().join("secrets/admin.token");
        let key = SigningKey::from_seed(&[3; 32]);
        let repos = vec!["graft".to_owned()];
        assert!(ensure(&path, &repos, &key).expect("first"));
        let first = fs::read(&path).expect("read");
        let text = String::from_utf8(first.clone()).expect("utf8");
        let token = graft_auth::Token::from_encoded(text.trim().to_owned());
        let claims = graft_auth::verify(&key.verifying_key(), &token, now_s(), &NoDenylist)
            .expect("valid token");
        assert_eq!(claims.principal.scopes, ["repo:graft:admin"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().expect("dir")), 0o700);
        }
        assert!(!ensure(&path, &repos, &key).expect("second"));
        assert_eq!(fs::read(&path).expect("read again"), first);
    }

    #[cfg(unix)]
    #[test]
    fn a_write_failure_names_the_path_not_the_token() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = graft_testkit::crash::TempDir::new("server-bootstrap-ro");
        let dir = temp.path().join("ro");
        fs::create_dir(&dir).expect("mkdir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod");
        let path = dir.join("admin.token");
        let key = SigningKey::from_seed(&[3; 32]);
        let result = ensure(&path, &["graft".to_owned()], &key);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
        if rustix::process::geteuid().is_root() {
            // Root writes through mode bits; the refusal is not testable.
            return;
        }
        let err = result.expect_err("read-only dir");
        let text = format!("{err} {err:?}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(!path.exists());
    }
}
