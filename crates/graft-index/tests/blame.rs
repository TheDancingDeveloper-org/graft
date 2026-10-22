//! `Blame`: line origins, the cache, and what it reads.

mod common;

use common::{Fixture, block_on, changeset, delete, path, put, put_blob};
use graft_index::BlameRun;
use graft_index::kinds::{Blame, blame_key};
use graft_model::{Change, Changeset, ChangesetId};
use graft_store::StoreError;

/// A first-parent chain built one changeset at a time; `land` puts the
/// whole chain on `main`.
struct Chain {
    fx: Fixture,
    css: Vec<Changeset>,
}

impl Chain {
    fn new() -> Self {
        Chain {
            fx: Fixture::new(),
            css: Vec::new(),
        }
    }

    /// Adds a changeset writing `files` (`None` deletes) plus `extra`.
    fn commit(&mut self, files: &[(&str, Option<&str>)], extra: Vec<Change>) -> ChangesetId {
        let mut changes = extra;
        for (p, content) in files {
            changes.push(match content {
                Some(c) => put_blob(p, self.fx.blob(c.as_bytes())),
                None => delete(p),
            });
        }
        let parents = self.css.last().map(Changeset::id).into_iter().collect();
        let cs = changeset(parents, self.css.len() as u64, changes);
        self.fx.create(&cs);
        self.css.push(cs);
        self.css.last().unwrap().id()
    }

    fn land(&self) {
        self.fx.land("main", &self.css);
        self.fx.set_line("main", self.css.last().unwrap().id());
    }

    fn blame(&self, at: ChangesetId, p: &str) -> Vec<ChangesetId> {
        let runs = block_on(self.fx.index.blame(self.fx.repo, at, &path(p))).unwrap();
        per_line(&runs)
    }
}

fn per_line(runs: &[BlameRun]) -> Vec<ChangesetId> {
    let mut start = 0;
    let mut out = Vec::new();
    for r in runs {
        assert_eq!(r.start, start, "runs are contiguous");
        assert!(r.end > r.start);
        out.extend(std::iter::repeat_n(r.origin, (r.end - r.start) as usize));
        start = r.end;
    }
    out
}

#[test]
fn origins_follow_edits_along_first_parents() {
    let mut c = Chain::new();
    let v1 = c.commit(&[("f.txt", Some("a\nb\nc\n"))], vec![]);
    let _other = c.commit(&[("g.txt", Some("x\n"))], vec![]);
    let v2 = c.commit(&[("f.txt", Some("a\nB\nc\nd\n"))], vec![]);
    let v3 = c.commit(&[("f.txt", Some("z\na\nB\nd\n"))], vec![]);
    let after = c.commit(&[("g.txt", Some("y\n"))], vec![]);
    c.land();
    assert_eq!(c.blame(v1, "f.txt"), vec![v1, v1, v1]);
    assert_eq!(c.blame(v2, "f.txt"), vec![v1, v2, v1, v2]);
    assert_eq!(c.blame(v3, "f.txt"), vec![v3, v1, v2, v2]);
    // A changeset that did not touch the file sees its last version.
    assert_eq!(c.blame(after, "f.txt"), vec![v3, v1, v2, v2]);
}

#[test]
fn the_cache_serves_repeats_and_later_versions() {
    let mut c = Chain::new();
    let mut versions = Vec::new();
    let mut content = String::new();
    for i in 0..40 {
        content.push_str(&format!("line {i}\n"));
        versions.push(c.commit(&[("f.txt", Some(content.as_str()))], vec![]));
        // Unrelated history between versions.
        for j in 0..5 {
            c.commit(&[("g.txt", Some(format!("{i} {j}\n").as_str()))], vec![]);
        }
    }
    c.land();
    let fx = &c.fx;

    let at = versions[20];
    let want: Vec<_> = versions[..=20].to_vec();
    assert_eq!(c.blame(at, "f.txt"), want);
    let cached = block_on(
        fx.index
            .get::<Blame>(fx.repo, &blame_key(at, &path("f.txt"))),
    )
    .unwrap();
    assert!(cached.is_some());

    // A repeat reads no changeset.
    fx.reads.reset();
    assert_eq!(c.blame(at, "f.txt"), want);
    assert_eq!(fx.reads.changeset(), 0);

    // Five versions later: five versions read (and the cached one), not
    // the history between them.
    fx.reads.reset();
    assert_eq!(c.blame(versions[25], "f.txt"), versions[..=25].to_vec());
    assert!(
        fx.reads.changeset() <= 6,
        "{} changesets",
        fx.reads.changeset()
    );
    // One first page of 16 rows covers them.
    assert!(fx.reads.scan_rows() <= 16, "{} rows", fx.reads.scan_rows());
}

#[test]
fn a_recreated_file_starts_fresh() {
    let mut c = Chain::new();
    let _v1 = c.commit(&[("f.txt", Some("a\nb\n"))], vec![]);
    c.commit(&[("f.txt", None)], vec![]);
    let v3 = c.commit(&[("f.txt", Some("a\nb\nc\n"))], vec![]);
    c.land();
    assert_eq!(c.blame(v3, "f.txt"), vec![v3, v3, v3]);
}

#[test]
fn a_directory_turned_file_starts_fresh() {
    let mut c = Chain::new();
    c.commit(&[("d", Some("a\nb\n"))], vec![]);
    // `d` becomes a directory, then a file again.
    c.commit(&[("d/x", Some("q\n"))], vec![delete("d")]);
    let v3 = c.commit(&[("d", Some("a\nb\nc\n"))], vec![delete("d/x")]);
    let v4 = c.commit(&[("d", Some("a\nb\nc\nd\n"))], vec![]);
    c.land();
    assert_eq!(c.blame(v4, "d"), vec![v3, v3, v3, v4]);
}

#[test]
fn drafts_blame_through_the_stack() {
    let mut c = Chain::new();
    let v1 = c.commit(&[("f.txt", Some("a\nb\n"))], vec![]);
    c.land();
    // Not landed: a draft stack on the line.
    let d1 = c.commit(&[("f.txt", Some("a\nb\nc\n"))], vec![]);
    let d2 = c.commit(&[("g.txt", Some("x\n"))], vec![put("h")]);
    assert_eq!(c.blame(d2, "f.txt"), vec![v1, v1, d1]);
}

#[test]
fn no_file_is_invalid() {
    let mut c = Chain::new();
    c.commit(&[("f.txt", Some("a\n"))], vec![]);
    let gone = c.commit(&[("f.txt", None)], vec![]);
    c.land();
    for (at, p) in [(gone, "f.txt"), (gone, "never.txt")] {
        let err = block_on(c.fx.index.blame(c.fx.repo, at, &path(p))).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }
}

#[test]
fn empty_files_have_no_runs() {
    let mut c = Chain::new();
    let v = c.commit(&[("e", Some(""))], vec![]);
    c.land();
    assert!(c.blame(v, "e").is_empty());
}
