//! Document URIs: `file:` URIs for the working copy, and `graft:` URIs for
//! a file at any changeset.
//!
//! A `graft:` URI is `graft://<repo>/<rev>/<path>`. `<rev>` is a changeset
//! id or a ref name; each part is percent-encoded, so a ref name with a `/`
//! is written with `%2F`. `<path>` is the repository path.

use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use graft_model::RepoPath;

/// The URI scheme of virtual documents.
pub const SCHEME: &str = "graft";

/// A file at a revision, named by a `graft:` URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraftUri {
    /// The repository.
    pub repo: String,
    /// A changeset id or a ref name.
    pub rev: String,
    /// The file.
    pub path: RepoPath,
}

impl GraftUri {
    /// Parses `graft://<repo>/<rev>/<path>`.
    pub fn parse(uri: &str) -> Result<Self, String> {
        let rest = uri
            .strip_prefix("graft://")
            .ok_or_else(|| "not a graft:// URI".to_owned())?;
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let mut parts = rest.splitn(3, '/');
        let (Some(repo), Some(rev), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            return Err("expected graft://<repo>/<rev>/<path>".to_owned());
        };
        let decode = |s: &str, what: &str| {
            decode(s)
                .and_then(|b| String::from_utf8(b).ok())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("the {what} is empty or badly encoded"))
        };
        let repo = decode(repo, "repository")?;
        let rev = decode(rev, "revision")?;
        let path = decode(path, "path")?;
        let path = RepoPath::new(&path).map_err(|e| format!("invalid path: {}", e.reason()))?;
        Ok(Self { repo, rev, path })
    }
}

impl fmt::Display for GraftUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "graft://{}/{}/{}",
            encode(self.repo.as_bytes(), false),
            encode(self.rev.as_bytes(), false),
            encode(self.path.as_str().as_bytes(), true)
        )
    }
}

/// The local path of a `file:` URI, if it is one.
pub fn file_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    Some(PathBuf::from(OsStr::from_bytes(&decode(rest)?)))
}

/// The `file:` URI of an absolute path.
pub fn file_uri(path: &Path) -> String {
    format!("file://{}", encode(path.as_os_str().as_bytes(), true))
}

fn encode(bytes: &[u8], keep_slash: bool) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graft_uris_round_trip() {
        let uri = GraftUri {
            repo: "r".into(),
            rev: "release/1.0".into(),
            path: RepoPath::new("src/a b#.rs").unwrap(),
        };
        let text = uri.to_string();
        assert_eq!(text, "graft://r/release%2F1.0/src/a%20b%23.rs");
        assert_eq!(GraftUri::parse(&text).unwrap(), uri);
    }

    #[test]
    fn graft_uris_refuse_bad_shapes() {
        for bad in [
            "file:///x",
            "graft://r/main",
            "graft://r//a",
            "graft://r/main/../a",
            "graft://r/main/a%zz",
            "graft:///main/a",
        ] {
            assert!(GraftUri::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn file_uris_round_trip() {
        let path = Path::new("/home/me/my repo/ü.rs");
        let uri = file_uri(path);
        assert_eq!(uri, "file:///home/me/my%20repo/%C3%BC.rs");
        assert_eq!(file_path(&uri).unwrap(), path);
        assert_eq!(
            file_path("file://localhost/a/b").unwrap(),
            Path::new("/a/b")
        );
        assert_eq!(file_path("graft://r/main/a"), None);
        assert_eq!(file_path("file://host/a"), None);
    }
}
