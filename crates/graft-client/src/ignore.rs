//! Ignore files for the working-copy walk (WI-2411): `.gitignore` and
//! `.graftignore`, at the root and in any directory below it. The format is
//! a plain pattern list, parsed here; nothing else of git is involved.
//!
//! - Blank lines and lines starting `#` are skipped; trailing spaces are
//!   dropped unless escaped with `\`.
//! - `!pattern` re-includes what an earlier rule ignored. A file under an
//!   ignored directory can't be re-included: the walk never descends there.
//! - A trailing `/` matches directories only.
//! - A pattern with no other `/` matches a name at any depth below the
//!   ignore file's directory; one with a `/` (a leading one included) is
//!   anchored to that directory.
//! - `*` and `?` match within one name, `[...]` is a character class (`!`
//!   or `^` negates it), and `**` matches any number of directories.
//!
//! Rules from a deeper directory win over shallower ones, and within one
//! directory later rules win (`.graftignore` is read after `.gitignore`).
//! Ignore rules apply only to untracked files: the walk that finds new
//! files consults them, and tracked files are checked from the index.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;

use graft_model::RepoPath;

/// The ignore files read in each directory, in order.
pub const IGNORE_FILES: [&str; 2] = [".gitignore", ".graftignore"];

/// One pattern line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    glob: Vec<char>,
    negate: bool,
    dir_only: bool,
    anchored: bool,
}

impl Rule {
    fn parse(line: &str) -> Option<Rule> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut s = trim_trailing_spaces(line);
        let negate = s.starts_with('!');
        if negate {
            s = &s[1..];
        }
        let dir_only = s.ends_with('/') && !s.ends_with("\\/");
        if dir_only {
            s = &s[..s.len() - 1];
        }
        let anchored = s.contains('/');
        let s = s.strip_prefix('/').unwrap_or(s);
        if s.is_empty() {
            return None;
        }
        Some(Rule {
            glob: s.chars().collect(),
            negate,
            dir_only,
            anchored,
        })
    }

    /// Whether the rule matches `rel`, the path relative to the ignore
    /// file's directory.
    fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let text: Vec<char> = if self.anchored {
            rel.chars().collect()
        } else {
            rel.rsplit('/').next().unwrap_or(rel).chars().collect()
        };
        glob(&self.glob, &text)
    }
}

/// Drops trailing spaces that are not escaped with a backslash.
fn trim_trailing_spaces(line: &str) -> &str {
    let mut end = line.len();
    while line[..end].ends_with(' ') {
        let before = &line[..end - 1];
        let slashes = before.len() - before.trim_end_matches('\\').len();
        if slashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

/// Glob match of a whole `text` against `pat`.
fn glob(pat: &[char], text: &[char]) -> bool {
    let Some((&c, rest)) = pat.split_first() else {
        return text.is_empty();
    };
    match c {
        '*' if rest.first() == Some(&'*') => {
            let after = &rest[1..];
            match after.split_first() {
                // Trailing `**`: everything below.
                None => true,
                // `**/`: zero or more whole directories.
                Some(('/', tail)) => {
                    if glob(tail, text) {
                        return true;
                    }
                    text.iter()
                        .enumerate()
                        .any(|(i, &ch)| ch == '/' && glob(tail, &text[i + 1..]))
                }
                // `**` elsewhere acts like `*`.
                Some(_) => star(after, text),
            }
        }
        '*' => star(rest, text),
        '?' => matches!(text.split_first(), Some((&t, tail)) if t != '/' && glob(rest, tail)),
        '[' => match (class(rest), text.split_first()) {
            (Some((ok, tail_pat)), Some((&t, tail))) => t != '/' && ok(t) && glob(tail_pat, tail),
            // An unclosed `[` is a literal.
            (None, Some((&'[', tail))) => glob(rest, tail),
            _ => false,
        },
        '\\' if !rest.is_empty() => {
            matches!(text.split_first(), Some((&t, tail)) if t == rest[0] && glob(&rest[1..], tail))
        }
        _ => matches!(text.split_first(), Some((&t, tail)) if t == c && glob(rest, tail)),
    }
}

/// `*`: any run of characters within one name.
fn star(rest: &[char], text: &[char]) -> bool {
    for i in 0..=text.len() {
        if glob(rest, &text[i..]) {
            return true;
        }
        if text.get(i) == Some(&'/') {
            break;
        }
    }
    false
}

/// Parses a character class after its `[`: the predicate and the pattern
/// after the closing `]`, or `None` when it is never closed.
fn class(pat: &[char]) -> Option<(impl Fn(char) -> bool + '_, &[char])> {
    let (negate, body) = match pat.first() {
        Some('!' | '^') => (true, &pat[1..]),
        _ => (false, pat),
    };
    // A `]` first in the class is literal.
    let start = usize::from(body.first() == Some(&']'));
    let close = start + body[start..].iter().position(|&c| c == ']')?;
    let set = &body[..close];
    let ok = move |t: char| {
        let mut i = 0;
        let mut hit = false;
        while i < set.len() {
            if i + 2 < set.len() && set[i + 1] == '-' {
                hit |= set[i] <= t && t <= set[i + 2];
                i += 3;
            } else {
                hit |= set[i] == t;
                i += 1;
            }
        }
        hit != negate
    };
    Some((ok, &body[close + 1..]))
}

/// The rules of one directory's ignore files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    /// The directory, `None` for the root.
    base: Option<RepoPath>,
    rules: Vec<Rule>,
}

impl Rules {
    /// Parses ignore-file text for the directory `base`.
    pub fn parse(base: Option<&RepoPath>, text: &str) -> Rules {
        Rules {
            base: base.cloned(),
            rules: text.lines().filter_map(Rule::parse).collect(),
        }
    }

    /// Reads the ignore files in `dir` (repository path `base`). Missing
    /// files are no rules.
    pub fn load(dir: &Path, base: Option<&RepoPath>) -> io::Result<Rules> {
        let mut rules = Rules {
            base: base.cloned(),
            rules: Vec::new(),
        };
        for name in IGNORE_FILES {
            match fs::read(dir.join(name)) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    rules.rules.extend(text.lines().filter_map(Rule::parse));
                }
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::IsADirectory) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(rules)
    }

    /// The last rule here that matches, as "ignored" (`Some(true)`) or
    /// "re-included" (`Some(false)`); `None` when none matches.
    fn decide(&self, path: &RepoPath, is_dir: bool) -> Option<bool> {
        let rel = match &self.base {
            None => path.as_str(),
            Some(b) => path.as_str().strip_prefix(b.as_str())?.strip_prefix('/')?,
        };
        self.rules
            .iter()
            .rev()
            .find(|r| r.matches(rel, is_dir))
            .map(|r| !r.negate)
    }
}

/// The ignore rules in force at one point of a walk: one [`Rules`] per
/// directory from the root down.
#[derive(Clone, Debug, Default)]
pub struct Ignore {
    stack: Vec<Rules>,
}

impl Ignore {
    /// Enters a directory: its rules now apply, ahead of its parents'.
    pub fn push(&mut self, rules: Rules) {
        self.stack.push(rules);
    }

    /// Leaves the directory last entered.
    pub fn pop(&mut self) {
        self.stack.pop();
    }

    /// Whether `path` (a directory when `is_dir`) is ignored. Its parent
    /// directories are assumed not to be: the walk never enters those.
    pub fn is_ignored(&self, path: &RepoPath, is_dir: bool) -> bool {
        self.stack
            .iter()
            .rev()
            .find_map(|r| r.decide(path, is_dir))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn root(text: &str) -> Ignore {
        let mut ig = Ignore::default();
        ig.push(Rules::parse(None, text));
        ig
    }

    #[test]
    fn names_match_at_any_depth_and_anchors_do_not() {
        let ig = root("node_modules/\n/dist\ntarget\n*.log\nbuild/out\n");
        assert!(ig.is_ignored(&p("node_modules"), true));
        assert!(ig.is_ignored(&p("web/node_modules"), true));
        assert!(ig.is_ignored(&p("dist"), true));
        assert!(!ig.is_ignored(&p("web/dist"), true));
        assert!(ig.is_ignored(&p("crates/x/target"), true));
        assert!(ig.is_ignored(&p("a/b/c.log"), false));
        assert!(ig.is_ignored(&p("build/out"), false));
        assert!(!ig.is_ignored(&p("x/build/out"), false));
        assert!(!ig.is_ignored(&p("src/main.rs"), false));
    }

    #[test]
    fn dir_only_rules_skip_files() {
        let ig = root("node_modules/\n");
        assert!(ig.is_ignored(&p("node_modules"), true));
        assert!(!ig.is_ignored(&p("node_modules"), false));
    }

    #[test]
    fn negation_re_includes_and_last_rule_wins() {
        let ig = root("*.log\n!keep.log\n");
        assert!(ig.is_ignored(&p("a.log"), false));
        assert!(!ig.is_ignored(&p("keep.log"), false));
        assert!(!ig.is_ignored(&p("x/keep.log"), false));
        let ig = root("!keep.log\n*.log\n");
        assert!(ig.is_ignored(&p("keep.log"), false));
    }

    #[test]
    fn nested_files_apply_below_their_directory_and_win() {
        let mut ig = root("*.gen\n");
        ig.push(Rules::parse(Some(&p("web")), "!*.gen\n/out/\ncache\n"));
        assert!(!ig.is_ignored(&p("web/a.gen"), false));
        assert!(ig.is_ignored(&p("web/out"), true));
        assert!(ig.is_ignored(&p("web/x/cache"), false));
        // Rules in `web/` don't reach a sibling.
        assert!(ig.is_ignored(&p("api/a.gen"), false));
        assert!(!ig.is_ignored(&p("api/cache"), false));
        ig.pop();
        assert!(ig.is_ignored(&p("web/a.gen"), false));
    }

    #[test]
    fn globs_classes_and_double_stars() {
        let ig = root("**/gen/*.rs\ndocs/**\nfoo?.[ch]\nlib[!x].a\na/**/z\n");
        assert!(ig.is_ignored(&p("gen/a.rs"), false));
        assert!(ig.is_ignored(&p("x/y/gen/a.rs"), false));
        assert!(!ig.is_ignored(&p("gen/a/b.rs"), false));
        assert!(ig.is_ignored(&p("docs/a/b"), false));
        assert!(!ig.is_ignored(&p("docs"), true));
        assert!(ig.is_ignored(&p("foo1.c"), false));
        assert!(ig.is_ignored(&p("src/fooZ.h"), false));
        assert!(!ig.is_ignored(&p("foo12.c"), false));
        assert!(ig.is_ignored(&p("libm.a"), false));
        assert!(!ig.is_ignored(&p("libx.a"), false));
        assert!(ig.is_ignored(&p("a/z"), false));
        assert!(ig.is_ignored(&p("a/b/c/z"), false));
    }

    #[test]
    fn comments_blanks_escapes_and_trailing_spaces() {
        let ig = root("# note\n\n\\#hash\n\\!bang\nsp\\ \ntrail   \r\n");
        assert!(ig.is_ignored(&p("#hash"), false));
        assert!(ig.is_ignored(&p("!bang"), false));
        assert!(ig.is_ignored(&p("sp "), false));
        assert!(ig.is_ignored(&p("trail"), false));
        assert!(!ig.is_ignored(&p("# note"), false));
    }

    #[test]
    fn star_stays_within_a_name() {
        let ig = root("/a*c\n");
        assert!(ig.is_ignored(&p("abc"), false));
        assert!(!ig.is_ignored(&p("ab/c"), false));
    }

    #[test]
    fn graftignore_is_read_after_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "*.tmp\n").unwrap();
        fs::write(dir.path().join(".graftignore"), "!keep.tmp\nvendor/\n").unwrap();
        let mut ig = Ignore::default();
        ig.push(Rules::load(dir.path(), None).unwrap());
        assert!(ig.is_ignored(&p("a.tmp"), false));
        assert!(!ig.is_ignored(&p("keep.tmp"), false));
        assert!(ig.is_ignored(&p("vendor"), true));
    }
}
