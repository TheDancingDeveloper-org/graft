//! The server's token signing key (`docs/design.md` §12.1).
//!
//! The key file holds the raw 32-byte Ed25519 seed, mode 0600. It is created
//! on first use, so a fresh server and `admin token mint` agree on one key
//! without a separate setup step.
//!
//! An existing key file is refused unless it is a regular file owned by the
//! user running the server with no group or other permission bits: a
//! restored or copied key left at 0644 would let any local user mint tokens.

use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::Path;

use graft_auth::SigningKey;

/// Seed length.
const SEED_LEN: usize = 32;

/// Loads the key at `path`, creating it (and its directory) if absent.
pub(crate) fn load_or_create(path: &Path) -> io::Result<SigningKey> {
    match load(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => create(path),
        other => other,
    }
}

fn load(path: &Path) -> io::Result<SigningKey> {
    let mut file = fs::File::open(path)?;
    check_private(path, &file.metadata()?)?;
    let mut seed = [0u8; SEED_LEN];
    file.read_exact(&mut seed)
        .map_err(|_| invalid(path, "must hold exactly 32 bytes"))?;
    if file.read(&mut [0u8; 1])? != 0 {
        return Err(invalid(path, "must hold exactly 32 bytes"));
    }
    Ok(SigningKey::from_seed(&seed))
}

fn create(path: &Path) -> io::Result<SigningKey> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let key = SigningKey::generate().map_err(|e| io::Error::other(e.to_string()))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&key.expose_seed())?;
            file.sync_all()?;
            Ok(key)
        }
        // Another process created it first; use theirs.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load(path),
        Err(e) => Err(e),
    }
}

/// Refuses a key file that is not a private regular file of this user.
#[cfg(unix)]
fn check_private(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    if !meta.is_file() {
        return Err(insecure(path, "is not a regular file"));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(insecure(
            path,
            &format!("has mode {mode:03o}; run `chmod 600 {}`", path.display()),
        ));
    }
    let me = rustix::process::geteuid().as_raw();
    if meta.uid() != me {
        return Err(insecure(
            path,
            &format!(
                "is owned by uid {}, not uid {me} running graft-server; run `chown {me} {}`",
                meta.uid(),
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    if meta.is_file() {
        Ok(())
    } else {
        Err(insecure(path, "is not a regular file"))
    }
}

fn insecure(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("refusing signing key file {}: it {why}", path.display()),
    )
}

fn invalid(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("key file {}: {why}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_once_then_reloaded() {
        let dir = graft_testkit::crash::TempDir::new("server-key");
        let path = dir.path().join("sub/signing.key");
        let a = load_or_create(&path).expect("create");
        let b = load_or_create(&path).expect("load");
        assert_eq!(a.verifying_key(), b.verifying_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::write(&path, b"short").expect("write");
        assert!(load_or_create(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_readable_keys_are_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = graft_testkit::crash::TempDir::new("server-key-mode");
        let path = dir.path().join("signing.key");
        load_or_create(&path).expect("create");
        for mode in [0o644, 0o640, 0o604, 0o660, 0o620, 0o602] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
            let e = load_or_create(&path).expect_err("refused");
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            let msg = e.to_string();
            if mode & 0o077 != 0 {
                assert!(msg.contains("chmod 600"), "{msg}");
                assert!(msg.contains(&path.display().to_string()), "{msg}");
            }
        }
        for mode in [0o600, 0o400, 0o700] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
            load_or_create(&path).expect("private key loads");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_is_not_a_key() {
        let dir = graft_testkit::crash::TempDir::new("server-key-dir");
        let e = load(dir.path()).expect_err("refused");
        assert!(e.to_string().contains("regular file"), "{e}");
    }
}
