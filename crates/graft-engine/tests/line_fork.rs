//! WI-2316: a new line forks from the line holding its nearest landed
//! ancestor and indexes only its own lands. Log, path log, ancestry and
//! blame read through the fork, and through a fork of a fork; on both
//! stores. Creating a line on a deep history writes O(new lands) rows.

mod common;

use std::sync::Arc;

use common::*;
use graft_engine::EngineConfig;
use graft_model::{ChangesetId, RepoPath};
use graft_proto::RepoApi;
use graft_proto::types::*;
use graft_store::MetaStore;
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use rusqlite::Connection;

on_both_stores!(history_reads_through_a_fork_of_a_fork);

fn create<M: MetaStore>(r: &Repo<'_, M>, name: &str, from: ChangesetId) {
    block_on(r.e().create_line(
        &r.cx,
        CreateLine {
            repo: r.name.clone(),
            name: name.to_owned(),
            from,
        },
    ))
    .expect("create line");
}

fn land<M: MetaStore>(r: &Repo<'_, M>, line: &str, head: ChangesetId) {
    let landed = block_on(r.e().land(
        &r.cx,
        Land {
            repo: r.name.clone(),
            head,
            line: line.to_owned(),
            squash: false,
            merge: MergeMode::File,
            deadline_ms: None,
        },
    ));
    assert!(matches!(landed, Ok(Landed::Landed { .. })), "{landed:?}");
}

fn log<M: MetaStore>(r: &Repo<'_, M>, from: ChangesetId, paths: &[&str]) -> Vec<ChangesetId> {
    block_on(r.e().log(
        &r.cx,
        Log {
            repo: r.name.clone(),
            from,
            until: None,
            paths: paths.iter().map(|p| RepoPath::new(p).unwrap()).collect(),
            page: Page::default(),
        },
    ))
    .expect("log")
    .changesets
    .into_iter()
    .map(|s| s.id)
    .collect()
}

/// One log page of `limit` from `from` (or the resume `token`), and the
/// next token.
fn log_page<M: MetaStore>(
    r: &Repo<'_, M>,
    from: ChangesetId,
    limit: u32,
    token: Option<String>,
) -> (Vec<ChangesetId>, Option<String>) {
    let page = block_on(r.e().log(
        &r.cx,
        Log {
            repo: r.name.clone(),
            from,
            until: None,
            paths: vec![],
            page: Page { limit, token },
        },
    ))
    .expect("log");
    (
        page.changesets.into_iter().map(|s| s.id).collect(),
        page.next,
    )
}

fn is_ancestor<M: MetaStore>(r: &Repo<'_, M>, a: ChangesetId, b: ChangesetId) -> bool {
    block_on(r.e().is_ancestor(
        &r.cx,
        Pair {
            repo: r.name.clone(),
            a,
            b,
        },
    ))
    .expect("is_ancestor")
}

fn blame<M: MetaStore>(r: &Repo<'_, M>, at: ChangesetId) -> Vec<(u32, u32, ChangesetId)> {
    block_on(r.e().blame(
        &r.cx,
        Blame {
            repo: r.name.clone(),
            at,
            path: RepoPath::new("f").unwrap(),
            lines: None,
        },
    ))
    .expect("blame")
    .ranges
    .into_iter()
    .map(|b| (b.start, b.end, b.origin.id))
    .collect()
}

/// Commits a child of `parent` that appends `line` to `f` (and touches `g`
/// when `g`), returning its id and the new contents.
fn append<M: MetaStore>(
    r: &Repo<'_, M>,
    parent: Option<ChangesetId>,
    text: &str,
    line: &str,
    g: bool,
) -> (ChangesetId, String) {
    let text = format!("{text}{line}\n");
    let mut files: Vec<(&str, &[u8])> = vec![("f", text.as_bytes())];
    if g {
        files.push(("g", line.as_bytes()));
    }
    let id = r.commit(parent.into_iter().collect(), &files, &[]).id;
    (id, text)
}

/// `main` = m0..m5; `b` forks from it at m3 and lands b1, b2; `c` forks
/// from `b` at b1 (a position `b` indexes itself) and lands c1.
fn history_reads_through_a_fork_of_a_fork<M: MetaStore>(f: &Fixture<M>) {
    let r = Repo::new(f, "r");
    let mut m = Vec::new();
    let mut texts = vec![String::new()];
    for i in 0..6 {
        let (id, t) = append(&r, m.last().copied(), &texts[i], &format!("m{i}"), i == 1);
        m.push(id);
        texts.push(t);
    }
    r.update_ref("main", None, m[5], RefKind::Line)
        .expect("main");

    create(&r, "b", m[3]);
    let text = &texts[4];
    let (b1, b1_text) = append(&r, Some(m[3]), text, "b1", true);
    land(&r, "b", b1);
    let (b2, _) = append(&r, Some(b1), &b1_text, "b2", false);
    land(&r, "b", b2);

    create(&r, "c", b1);
    let (c1, _) = append(&r, Some(b1), &b1_text, "c1", false);
    land(&r, "c", c1);

    // Log through both forks, with and without a path.
    assert_eq!(log(&r, c1, &[]), vec![c1, b1, m[3], m[2], m[1], m[0]]);
    assert_eq!(log(&r, c1, &["g"]), vec![b1, m[1]]);
    assert_eq!(log(&r, b2, &[]), vec![b2, b1, m[3], m[2], m[1], m[0]]);
    assert_eq!(
        log(&r, m[5], &["g"]),
        vec![m[1]],
        "a parent never sees a child"
    );

    // Ancestry across the forks.
    assert!(is_ancestor(&r, m[0], c1));
    assert!(is_ancestor(&r, m[3], c1));
    assert!(is_ancestor(&r, b1, c1));
    assert!(!is_ancestor(&r, m[4], c1), "above the fork on main");
    assert!(!is_ancestor(&r, b2, c1), "above the fork on b");
    assert!(!is_ancestor(&r, c1, b2));

    // Blame: each line's origin, across both forks.
    assert_eq!(
        blame(&r, c1),
        vec![
            (0, 1, m[0]),
            (1, 2, m[1]),
            (2, 3, m[2]),
            (3, 4, m[3]),
            (4, 5, b1),
            (5, 6, c1),
        ]
    );

    // A page that straddles both fork hops, then resumes below them.
    let (first, next) = log_page(&r, c1, 3, None);
    assert_eq!(first, vec![c1, b1, m[3]]);
    let (second, _) = log_page(&r, c1, 3, next);
    assert_eq!(second, vec![m[2], m[1], m[0]]);

    // Parents that land after the fork change none of the child's answers.
    let answers = |r: &Repo<'_, M>| {
        (
            log(r, c1, &[]),
            log(r, c1, &["g"]),
            blame(r, c1),
            [m[4], m[5], b1, b2].map(|a| is_ancestor(r, a, c1)),
        )
    };
    let before = answers(&r);
    let (m6, _) = append(&r, Some(m[5]), &texts[6], "m6", true);
    land(&r, "main", m6);
    let (b3, _) = append(&r, Some(b2), &b1_text, "b3", true);
    land(&r, "b", b3);
    assert_eq!(answers(&r), before);
    assert!(!is_ancestor(&r, m6, c1));
    assert!(!is_ancestor(&r, b3, c1));
}

fn rows(conn: &Connection, table: &str, line: &str) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE line = ?1"),
        [line],
        |r| r.get(0),
    )
    .expect("count")
}

/// Creating a line on a deep landed history writes one fork row and no
/// position or path rows; at a draft stack it indexes only the stack.
#[test]
fn creating_a_line_writes_only_its_own_rows() {
    const DEPTH: usize = 300;
    let dir = TempDir::new("fork-cost");
    let db = dir.path().join("meta.sqlite");
    let meta = SqliteMetaStore::open(SqliteConfig::new(db.clone())).expect("open");
    let f = build(Arc::new(meta), EngineConfig::default(), Some(dir));
    let r = Repo::new(&f, "r");
    let mut tip = None;
    for i in 0..DEPTH {
        let body = format!("{i}");
        tip = Some(
            r.commit(tip.into_iter().collect(), &[("f", body.as_bytes())], &[])
                .id,
        );
    }
    let tip = tip.expect("tip");
    r.update_ref("main", None, tip, RefKind::Line)
        .expect("main");

    create(&r, "at-tip", tip);
    let d1 = r.commit(vec![tip], &[("f", b"d1")], &[]).id;
    let d2 = r.commit(vec![d1], &[("f", b"d2")], &[]).id;
    create(&r, "at-stack", d2);

    // The tip is held directly only by `main`; `at-tip` holds it through
    // its fork, so another line at the tip forks from `main`, not from
    // `at-tip`: hop chains don't grow with repeated creates.
    create(&r, "again", tip);

    let conn = Connection::open(&db).expect("inspect");
    let parent = |line: &str| -> String {
        conn.query_row(
            "SELECT parent FROM line_forks WHERE line = ?1",
            [line],
            |r| r.get(0),
        )
        .expect("fork")
    };
    assert_eq!(parent("at-tip"), "main");
    assert_eq!(parent("again"), "main");
    assert_eq!(parent("at-stack"), "main");
    assert_eq!(rows(&conn, "line_pos", "main"), DEPTH as i64);
    assert_eq!(rows(&conn, "line_pos", "at-tip"), 0);
    assert_eq!(rows(&conn, "line_paths", "at-tip"), 0);
    assert_eq!(rows(&conn, "line_forks", "at-tip"), 1);
    assert_eq!(rows(&conn, "line_pos", "at-stack"), 2);
    assert_eq!(rows(&conn, "line_forks", "at-stack"), 1);

    // Reads still see the whole history through the fork.
    assert_eq!(log(&r, d2, &[]).len(), (DEPTH + 2).min(100));
    assert!(is_ancestor(&r, d1, d2));
    let deep = *log(&r, tip, &[]).last().expect("page");
    assert!(is_ancestor(&r, deep, d2));
}
