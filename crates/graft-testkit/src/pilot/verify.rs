//! The fidelity check: every imported changeset, cloned with `graft clone
//! --at`, against `git checkout` of its commit — paths, kinds, exec bits,
//! symlink targets and bytes.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use graft_model::{ChangesetId, ObjectId, ObjectKind};

use super::PilotError;
use super::proc::{git, run};

/// One imported commit: its git sha (40 hex) and its changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapped {
    /// The git commit.
    pub sha: String,
    /// The graft changeset.
    pub changeset: ChangesetId,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Every commit the mirror DB at `db` records.
pub fn commit_map(db: &Path) -> Result<Vec<Mapped>, PilotError> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| PilotError::Step(format!("open mirror DB: {e}")))?;
    let mut stmt = conn
        .prepare("SELECT changeset, sha FROM commits")
        .map_err(|e| PilotError::Step(format!("mirror DB: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| PilotError::Step(format!("mirror DB: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let (cs, sha) = row.map_err(|e| PilotError::Step(format!("mirror DB: {e}")))?;
        let id: [u8; 32] = cs
            .try_into()
            .map_err(|_| PilotError::Step("mirror DB: a changeset id is not 32 bytes".into()))?;
        out.push(Mapped {
            sha: hex(&sha),
            changeset: ChangesetId(ObjectId(id)),
        });
    }
    Ok(out)
}

/// What is at one path of a checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A regular file: exec bit and content hash.
    File {
        /// Any exec bit set.
        exec: bool,
        /// Size.
        len: u64,
        /// Content hash (never the content).
        hash: ObjectId,
    },
    /// A symlink and its target.
    Link(Vec<u8>),
}

/// Every file and symlink under `root`, by relative path, skipping `skip`
/// at the top level (`.graft`). Directories are implied by their contents,
/// as in git.
pub fn walk(root: &Path, skip: &[&str]) -> Result<BTreeMap<String, Item>, PilotError> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for e in fs::read_dir(&dir).map_err(|e| PilotError::io(&dir, e))? {
            let e = e.map_err(|e| PilotError::io(&dir, e))?;
            let name = String::from_utf8_lossy(e.file_name().as_bytes()).into_owned();
            if prefix.is_empty() && skip.contains(&name.as_str()) {
                continue;
            }
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let path = e.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| PilotError::io(&path, e))?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                let target = fs::read_link(&path).map_err(|e| PilotError::io(&path, e))?;
                out.insert(rel, Item::Link(target.as_os_str().as_bytes().to_vec()));
            } else if ft.is_dir() {
                stack.push((path, rel));
            } else {
                let bytes = fs::read(&path).map_err(|e| PilotError::io(&path, e))?;
                out.insert(
                    rel,
                    Item::File {
                        exec: meta.permissions().mode() & 0o111 != 0,
                        len: bytes.len() as u64,
                        hash: ObjectId::hash(ObjectKind::Blob, &bytes),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// The differences between git's checkout and graft's, one line each, by
/// path and kind of difference (never contents).
pub fn compare(git: &BTreeMap<String, Item>, graft: &BTreeMap<String, Item>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, want) in git {
        match graft.get(path) {
            None => out.push(format!("{path}: missing from graft")),
            Some(got) if got == want => {}
            Some(got) => {
                let what = match (want, got) {
                    (Item::File { exec: a, .. }, Item::File { exec: b, .. }) if a != b => {
                        "exec bit differs"
                    }
                    (Item::File { .. }, Item::File { .. }) => "bytes differ",
                    (Item::Link(_), Item::Link(_)) => "symlink target differs",
                    _ => "kind differs (file vs symlink)",
                };
                out.push(format!("{path}: {what}"));
            }
        }
    }
    for path in graft.keys().filter(|p| !git.contains_key(*p)) {
        out.push(format!("{path}: only in graft"));
    }
    out
}

/// Checks out `sha` of the bare repository `bare` into `dest` (created),
/// with a private index file.
pub fn git_checkout(bare: &Path, sha: &str, dest: &Path) -> Result<(), PilotError> {
    fs::create_dir_all(dest).map_err(|e| PilotError::io(dest, e))?;
    let index = dest.with_extension("index");
    let with = |args: &[&str]| {
        let mut c = git(None);
        c.arg("--git-dir").arg(bare).arg("--work-tree").arg(dest);
        c.env("GIT_INDEX_FILE", &index).args(args);
        c
    };
    run(&mut with(&["read-tree", sha]))?;
    run(&mut with(&["checkout-index", "--all", "--force"]))?;
    let _ = fs::remove_file(&index);
    Ok(())
}

/// The fidelity result.
#[derive(Clone, Debug, Default)]
pub struct Fidelity {
    /// Commits compared.
    pub checked: usize,
    /// Commits that matched exactly.
    pub matched: usize,
    /// Files and symlinks compared, summed over commits.
    pub entries: usize,
    /// `<sha>: <path>: <difference>`, at most 50 lines.
    pub problems: Vec<String>,
    /// Commits with a difference.
    pub failed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crash::TempDir;

    fn file(dir: &Path, rel: &str, bytes: &[u8], mode: u32) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, bytes).expect("write");
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    #[test]
    fn compare_names_each_kind_of_difference() {
        let a = TempDir::new("verify-a");
        let b = TempDir::new("verify-b");
        for d in [a.path(), b.path()] {
            file(d, "same.txt", b"x", 0o644);
            std::os::unix::fs::symlink("same.txt", d.join("link")).expect("symlink");
        }
        file(a.path(), "run.sh", b"#!", 0o755);
        file(b.path(), "run.sh", b"#!", 0o644);
        file(a.path(), "dir/bytes", b"one", 0o644);
        file(b.path(), "dir/bytes", b"two", 0o644);
        file(a.path(), "missing", b"", 0o644);
        file(b.path(), "extra", b"", 0o644);
        std::os::unix::fs::symlink("elsewhere", a.path().join("target")).expect("symlink");
        std::os::unix::fs::symlink("same.txt", b.path().join("target")).expect("symlink");
        file(a.path(), "kind", b"f", 0o644);
        std::os::unix::fs::symlink("same.txt", b.path().join("kind")).expect("symlink");
        file(b.path(), ".graft/state", b"skipped", 0o644);

        let want = walk(a.path(), &[]).expect("walk");
        let got = walk(b.path(), &[".graft"]).expect("walk");
        let mut problems = compare(&want, &got);
        problems.sort();
        assert_eq!(
            problems,
            [
                "dir/bytes: bytes differ",
                "extra: only in graft",
                "kind: kind differs (file vs symlink)",
                "missing: missing from graft",
                "run.sh: exec bit differs",
                "target: symlink target differs",
            ]
        );
        assert!(compare(&want, &want).is_empty());
    }
}
