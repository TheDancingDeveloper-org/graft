//! The repository index: which cache directory holds which repository.
//!
//! The frame store keys objects by a numeric [`RepoId`]; the node learns
//! repositories by name from its callers. This index maps one to the other,
//! so each repository's objects live apart from every other's and an object
//! cached for one repository is never served for another. It is a small text
//! file, rewritten whole (temp file, fsync, rename, directory fsync) when a
//! repository is added, before any object of that repository is written.
//!
//! A name gets an entry only when the node is about to cache an object the
//! upstream served for it, so a caller naming repositories that don't exist
//! can't fill the index. It holds at most `max` entries.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use graft_store::RepoId;

/// The first line of the file.
const HEADER: &str = "graft-node repos v1";

/// The longest repository name the node accepts.
pub(crate) const MAX_NAME: usize = 255;

/// Whether `name` can be a repository name: what a scope may name
/// (non-empty, no `:`, whitespace or control characters), bounded.
pub(crate) fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name
            .chars()
            .any(|c| c == ':' || c.is_whitespace() || c.is_control())
}

/// Why an entry could not be added.
#[derive(Debug)]
pub(crate) enum AddError {
    /// The index holds `max` repositories.
    Full,
    /// Not a repository name.
    BadName,
    /// The file could not be written.
    Io(io::Error),
}

/// The index, loaded.
#[derive(Debug)]
pub(crate) struct Repos {
    path: PathBuf,
    max: u32,
    map: Mutex<HashMap<String, RepoId>>,
}

impl Repos {
    /// Loads the index at `path` (empty if absent). A file that does not
    /// parse is an error: the caller wipes the cache.
    pub(crate) fn load(path: &Path, max: u32) -> Result<Self, String> {
        let map = match fs::read_to_string(path) {
            Ok(text) => parse(&text).ok_or_else(|| format!("{} is malformed", path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        Ok(Self {
            path: path.to_owned(),
            max,
            map: Mutex::new(map),
        })
    }

    /// The id of `name`, if it has one.
    pub(crate) fn get(&self, name: &str) -> Option<RepoId> {
        self.lock().get(name).copied()
    }

    /// The id of `name`, adding it (durably) if it has none. Blocking.
    pub(crate) fn get_or_add(&self, name: &str) -> Result<RepoId, AddError> {
        if !is_name(name) {
            return Err(AddError::BadName);
        }
        let mut map = self.lock();
        if let Some(id) = map.get(name) {
            return Ok(*id);
        }
        if map.len() >= self.max as usize {
            return Err(AddError::Full);
        }
        let next = map.values().map(|r| r.0 + 1).max().unwrap_or(0);
        let id = RepoId(next);
        map.insert(name.to_owned(), id);
        if let Err(e) = write(&self.path, &map) {
            map.remove(name);
            return Err(AddError::Io(e));
        }
        Ok(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RepoId>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn parse(text: &str) -> Option<HashMap<String, RepoId>> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let mut map = HashMap::new();
    let mut ids = std::collections::HashSet::new();
    for line in lines {
        let (id, name) = line.split_once(' ')?;
        let id: u32 = id.parse().ok()?;
        if !is_name(name) || !ids.insert(id) || map.insert(name.to_owned(), RepoId(id)).is_some() {
            return None;
        }
    }
    Some(map)
}

fn write(path: &Path, map: &HashMap<String, RepoId>) -> io::Result<()> {
    let mut rows: Vec<(&String, &RepoId)> = map.iter().collect();
    rows.sort_by_key(|(_, id)| id.0);
    let mut text = format!("{HEADER}\n");
    for (name, id) in rows {
        text.push_str(&format!("{} {name}\n", id.0));
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_persist_and_stay_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos");
        let repos = Repos::load(&path, 2).unwrap();
        let a = repos.get_or_add("alpha").unwrap();
        let b = repos.get_or_add("beta").unwrap();
        assert_ne!(a, b);
        assert_eq!(repos.get_or_add("alpha").unwrap(), a);
        assert!(matches!(repos.get_or_add("gamma"), Err(AddError::Full)));
        assert!(matches!(repos.get_or_add("a:b"), Err(AddError::BadName)));

        let again = Repos::load(&path, 2).unwrap();
        assert_eq!(again.get("alpha"), Some(a));
        assert_eq!(again.get("beta"), Some(b));
        assert_eq!(again.get("gamma"), None);
    }

    #[test]
    fn a_malformed_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos");
        for text in [
            "nonsense\n",
            "graft-node repos v1\n0 a\n0 b\n",
            "graft-node repos v1\n0 a\n1 a\n",
            "graft-node repos v1\nx a\n",
            "graft-node repos v1\n0 a b\n",
        ] {
            fs::write(&path, text).unwrap();
            assert!(Repos::load(&path, 10).is_err(), "{text:?} accepted");
        }
    }
}
