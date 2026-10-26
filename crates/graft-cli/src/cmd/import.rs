//! `graft import <path-or-url> --repo NAME [--line main]`.
//!
//! The endpoint and token come from the user config (`--server`, or the
//! default). The token is written to a private file and passed to the
//! mirror as `--token-file`. graft-cli names no git type: the source, the
//! repository and the line are strings.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use crate::cli::{Global, ImportArgs};
use crate::config::{self, UserConfig};
use crate::conn::resolve_settings_no_wc;
use crate::error::Failure;

/// Imports `args.source` into `args.repo` on the configured server.
pub(crate) fn run(g: &Global, args: &ImportArgs) -> Result<(), Failure> {
    let user = config::load()?;
    let (endpoint, token) = endpoint_and_token(g, &user)?;
    let token_file = TokenFile::create(&token)?;
    let mut argv = vec![
        "import".to_owned(),
        args.source.display().to_string(),
        "--repo".to_owned(),
        args.repo.clone(),
        "--server".to_owned(),
        endpoint,
        "--token-file".to_owned(),
        token_file.path.display().to_string(),
    ];
    // The mirror default is `refs/heads/main` → `main`. Passing that same
    // pair replaces the default; any other line name is explicit.
    if args.line != "main" {
        argv.push("--line".to_owned());
        argv.push(format!("refs/heads/{}={}", args.line, args.line));
    }
    if let Some(db) = &args.db {
        argv.push("--db".to_owned());
        argv.push(db.display().to_string());
    }
    let code = graft_mirror::run(argv);
    drop(token_file);
    let status = exit_status(code);
    if status == 0 {
        Ok(())
    } else if status == 64 {
        Err(Failure::usage("import usage"))
    } else {
        Err(Failure::new(
            crate::Exit::Failure,
            "import",
            format!("mirror import failed (exit {status})"),
        ))
    }
}

/// Endpoint and token from `--server` or the default server. A missing
/// config names `graft login`.
fn endpoint_and_token(g: &Global, user: &UserConfig) -> Result<(String, String), Failure> {
    if user.servers.is_empty() && g.server.is_none() && g.endpoint.is_none() {
        return Err(Failure::usage("no server configured; run `graft login`"));
    }
    let settings = resolve_settings_no_wc(g, user).map_err(|err| {
        if err.message.contains("not configured") || err.message.contains("no endpoint") {
            Failure::usage("no server configured; run `graft login`")
        } else {
            err
        }
    })?;
    let token = settings
        .token
        .ok_or_else(|| Failure::usage("no server configured; run `graft login`"))?;
    Ok((settings.endpoint.to_string(), token))
}

/// A token file in an unpredictable private directory. Drop removes both,
/// including when import fails or panics. The path, not the token, is what
/// the mirror sees on its argument list.
struct TokenFile {
    // Kept so Drop removes the private directory.
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl TokenFile {
    /// Creates the file with create_new and mode 0600 inside a 0700 temp dir.
    fn create(token: &str) -> Result<Self, Failure> {
        let dir = tempfile::Builder::new()
            .prefix("graft_import_")
            .tempdir()
            .map_err(|e| Failure::new(crate::Exit::Failure, "import", e.to_string()))?;
        let path = dir.path().join("token");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| Failure::new(crate::Exit::Failure, "import", e.to_string()))?;
        file.write_all(token.as_bytes())
            .map_err(|e| Failure::new(crate::Exit::Failure, "import", e.to_string()))?;
        if token.as_bytes().last() != Some(&b'\n') {
            file.write_all(b"\n")
                .map_err(|e| Failure::new(crate::Exit::Failure, "import", e.to_string()))?;
        }
        Ok(Self { _dir: dir, path })
    }
}

fn exit_status(code: std::process::ExitCode) -> u8 {
    // ExitCode has no accessor. SUCCESS is 0; usage is the mirror's 64.
    if code == std::process::ExitCode::SUCCESS {
        0
    } else if code == std::process::ExitCode::from(64) {
        64
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::TokenFile;

    #[test]
    fn token_file_is_removed_on_drop() {
        let path = {
            let file = TokenFile::create("sekret").expect("wrote");
            assert!(file.path.is_file());
            file.path.clone()
        };
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }
}
