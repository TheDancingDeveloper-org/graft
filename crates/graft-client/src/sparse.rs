//! The slice of a snapshot a working copy holds (`docs/design.md` §9.1):
//! any number of path prefixes, plus optionally the files at the root.

use graft_model::RepoPath;

/// The sparse file line that includes the files at the repository root.
pub const ROOT_FILES_LINE: &str = "/";

/// Sparse prefixes (`.graft/sparse`). No prefixes means the whole snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sparse {
    /// Included prefixes, sorted, none beneath another.
    pub prefixes: Vec<RepoPath>,
    /// Include files at the repository root (only meaningful with prefixes).
    pub root_files: bool,
}

impl Sparse {
    /// The whole snapshot.
    pub fn full() -> Self {
        Self::default()
    }

    /// A slice of `prefixes`, normalized: sorted, deduplicated, and with any
    /// prefix beneath another dropped.
    pub fn new(prefixes: impl IntoIterator<Item = RepoPath>, root_files: bool) -> Self {
        let mut all: Vec<RepoPath> = prefixes.into_iter().collect();
        all.sort();
        all.dedup();
        let mut kept: Vec<RepoPath> = Vec::with_capacity(all.len());
        for p in all {
            if !kept.iter().any(|k| p.starts_with(k)) {
                kept.push(p);
            }
        }
        Self {
            prefixes: kept,
            root_files,
        }
    }

    /// Whether this is the whole snapshot.
    pub fn is_full(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// Whether a file at `path` is in the slice.
    pub fn includes(&self, path: &RepoPath) -> bool {
        self.is_full()
            || self.prefixes.iter().any(|p| path.starts_with(p))
            || (self.root_files && path.parent().is_none())
    }

    /// Whether the directory `dir` must be read: it is in the slice, or a
    /// prefix lies beneath it.
    pub fn descends(&self, dir: &RepoPath) -> bool {
        self.is_full()
            || self
                .prefixes
                .iter()
                .any(|p| dir.starts_with(p) || p.starts_with(dir))
    }

    /// The `paths` to restrict a `Diff` to, or `None` when the diff must be
    /// unrestricted (the whole snapshot, or root files, which no prefix
    /// names) and filtered with [`Sparse::includes`].
    pub fn diff_paths(&self) -> Option<Vec<RepoPath>> {
        if self.is_full() || self.root_files {
            None
        } else {
            Some(self.prefixes.clone())
        }
    }

    /// Parses the sparse file: one prefix per line, [`ROOT_FILES_LINE`] for
    /// root files; blank lines and `#` comments are ignored. Paths are
    /// escaped as in [`escape`].
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut prefixes = Vec::new();
        let mut root_files = false;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            if line == ROOT_FILES_LINE {
                root_files = true;
                continue;
            }
            let path = unescape(line).ok_or("bad escape in sparse file")?;
            prefixes.push(RepoPath::new(&path).map_err(|e| e.reason().to_owned())?);
        }
        Ok(Self::new(prefixes, root_files))
    }

    /// The sparse file's text.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if self.root_files {
            out.push_str(ROOT_FILES_LINE);
            out.push('\n');
        }
        for p in &self.prefixes {
            out.push_str(&escape(p.as_str()));
            out.push('\n');
        }
        out
    }
}

/// Escapes a path for a line-oriented file: `%`, newline and carriage
/// return become `%25`, `%0A` and `%0D`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            c => out.push(c),
        }
    }
    out
}

/// Reverses [`escape`]; `None` for an escape it never writes.
pub fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let code = rest.get(i + 1..i + 3)?;
        out.push(match code {
            "25" => '%',
            "0A" => '\n',
            "0D" => '\r',
            _ => return None,
        });
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn normalizes_prefixes() {
        let s = Sparse::new([p("b"), p("a/x"), p("a"), p("b")], false);
        assert_eq!(s.prefixes, vec![p("a"), p("b")]);
    }

    #[test]
    fn includes_and_descends() {
        let s = Sparse::new([p("a/b"), p("c")], false);
        assert!(s.includes(&p("a/b/f")));
        assert!(s.includes(&p("c")));
        assert!(!s.includes(&p("a/f")));
        assert!(!s.includes(&p("ab/f")));
        assert!(!s.includes(&p("top")));
        assert!(s.descends(&p("a")));
        assert!(s.descends(&p("a/b/deep")));
        assert!(!s.descends(&p("a/c")));
        assert!(!s.descends(&p("d")));

        let with_root = Sparse::new([p("a/b")], true);
        assert!(with_root.includes(&p("top")));
        assert!(!with_root.includes(&p("a/f")));
        assert_eq!(with_root.diff_paths(), None);
        assert_eq!(s.diff_paths(), Some(vec![p("a/b"), p("c")]));

        let full = Sparse::full();
        assert!(full.includes(&p("x/y")) && full.descends(&p("x")));
        assert_eq!(full.diff_paths(), None);
    }

    #[test]
    fn file_round_trips() {
        let s = Sparse::new([p("a/b"), p("odd\nname%")], true);
        assert_eq!(Sparse::parse(&s.to_text()).unwrap(), s);
        let parsed = Sparse::parse("# comment\n\nsrc\n/\n").unwrap();
        assert_eq!(parsed, Sparse::new([p("src")], true));
        assert!(Sparse::parse("a/../b\n").is_err());
        assert!(Sparse::parse("bad%zz\n").is_err());
    }
}
