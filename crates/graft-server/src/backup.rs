//! `admin backup`: a consistent metadata snapshot, and optionally a copy of
//! the object tree (`docs/design.md` §6.7).
//!
//! The metadata goes first: objects are a superset of what any snapshot
//! references, so copying them after the snapshot gives a restorable pair.
//! The snapshot is SQLite `VACUUM INTO` over a read-only connection, which
//! is safe while the server runs. Objects are immutable, so the copy is
//! incremental: files already in the backup are skipped. The signing key is
//! a secret and is not copied; keep it in Infisical.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// What a backup wrote.
#[derive(Debug, Default)]
pub(crate) struct Summary {
    pub(crate) meta: PathBuf,
    pub(crate) objects_copied: u64,
    pub(crate) objects_present: u64,
    pub(crate) bytes_copied: u64,
}

/// Snapshots `meta_db` to `<dir>/meta.db`, then (with `objects`) copies
/// the object tree to `<dir>/objects`.
pub(crate) fn backup(
    meta_db: &Path,
    objects: Option<&Path>,
    dir: &Path,
) -> Result<Summary, String> {
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let target = dir.join("meta.db");
    let tmp = dir.join("meta.db.tmp");
    let _ = fs::remove_file(&tmp);
    let conn = Connection::open_with_flags(
        meta_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {}: {e}", meta_db.display()))?;
    let tmp_text = tmp
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", tmp.display()))?;
    conn.execute("VACUUM INTO ?1", [tmp_text])
        .map_err(|e| format!("snapshot {}: {e}", meta_db.display()))?;
    drop(conn);
    fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("sync {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &target).map_err(|e| format!("rename {}: {e}", target.display()))?;
    sync_dir(dir).map_err(|e| format!("sync {}: {e}", dir.display()))?;

    let mut summary = Summary {
        meta: target,
        ..Summary::default()
    };
    if let Some(objects) = objects {
        copy_tree(objects, &dir.join("objects"), &mut summary)
            .map_err(|e| format!("copy objects: {e}"))?;
    }
    Ok(summary)
}

/// Copies regular files under `from` to `to`, skipping the store's lock
/// file, temp files, and files already present with the same size.
fn copy_tree(from: &Path, to: &Path, summary: &mut Summary) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text == ".lock" || text.ends_with(".tmp") {
            continue;
        }
        let kind = entry.file_type()?;
        let dest = to.join(&name);
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest, summary)?;
        } else if kind.is_file() {
            let len = entry.metadata()?.len();
            if fs::metadata(&dest).is_ok_and(|m| m.len() == len) {
                summary.objects_present += 1;
                continue;
            }
            let tmp = to.join(format!("{text}.tmp"));
            fs::copy(entry.path(), &tmp)?;
            fs::File::open(&tmp)?.sync_all()?;
            fs::rename(&tmp, &dest)?;
            summary.objects_copied += 1;
            summary.bytes_copied += len;
        }
    }
    sync_dir(to)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}
