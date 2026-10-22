//! The server's token signing key (`docs/design.md` §12.1).
//!
//! The key file holds the raw 32-byte Ed25519 seed, mode 0600. It is created
//! on first use, so a fresh server and `admin token mint` agree on one key
//! without a separate setup step.

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
}
