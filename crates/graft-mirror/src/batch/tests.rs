use std::collections::BTreeMap;

use graft_git::GitSha;
use graft_git::export::Committer;
use graft_model::{Author, AuthorKind, ChangesetId, Timestamp};

use crate::export::default_committer;
use crate::text::clean_message;

use super::render::{BatchHeader, land_commit, land_message, land_row, parse_commit, render};
use super::types::{
    AuthorIdentity, BatchState, CHANGESET_TRAILER, LandLine, MAX_LANDS, MAX_PR_BODY, MESSAGE_CHARS,
    claimed_author_trailer, git_time, git_time_at, ident_part, land_ident, short,
};

#[test]
fn states_round_trip() {
    for s in [
        BatchState::Planned,
        BatchState::Pushed,
        BatchState::Open,
        BatchState::Merged,
        BatchState::Rejected,
    ] {
        assert_eq!(BatchState::parse(s.as_str()), Some(s));
    }
    assert!(BatchState::Merged.is_terminal());
    assert!(!BatchState::Open.is_terminal());
    assert_eq!(BatchState::parse("landed"), None);
}

fn header(capped: bool) -> BatchHeader {
    let id = ChangesetId(graft_model::ObjectId([7; 32]));
    BatchHeader {
        number: 9,
        line: "integration".to_owned(),
        branch: "main".to_owned(),
        base: id,
        base_sha: GitSha([1; 20]),
        start: id,
        tip: id,
        capped,
        truncated: false,
        drops: Vec::new(),
        held: 0,
        uncut: None,
        max_lands: 0,
        fallback: Some("test".to_owned()),
        appended: std::collections::HashSet::new(),
    }
}

fn land(n: u32, long: bool) -> LandLine {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    let text = |s: &str| if long { s.repeat(200) } else { s.to_owned() };
    let mut checks = BTreeMap::new();
    for c in 0..if long { 40 } else { 1 } {
        checks.insert(text(&format!("ci/check-{c}|")), c % 2 == 0);
    }
    LandLine {
        id: ChangesetId(graft_model::ObjectId(bytes)),
        author: text("Tess|"),
        session: Some(text("ses_x")),
        work_item: Some(text("WI-1")),
        subject: text("subject "),
        checks,
        authored_ms: n as i64,
        reverts: None,
        parent: None,
        ident: ("Tess".to_owned(), "tess@example.com".to_owned()),
        authored: Timestamp {
            ms: 1_700_000_000_000 + i64::from(n),
            offset_min: 600,
        },
        message: text("subject "),
        trailer_session: Some(text("ses_x")),
        trailer_work_item: Some(text("WI-1")),
        trailer_model: None,
        trailer_coauthors: Vec::new(),
        trailer_claimed_author: None,
        trailer_merged_from: None,
    }
}

#[test]
fn a_small_batch_lists_every_land_in_both() {
    let lands: Vec<LandLine> = (0..3).map(|n| land(n, false)).collect();
    let (body, message) = render(&header(false), &lands);
    assert_eq!(body, message);
    for l in &lands {
        assert!(body.contains(&short(l.id)));
    }
    assert!(body.ends_with(&format!("Graft-Changeset: {}\n", header(false).tip)));
    assert!(body.contains("Graft-Batch: 9\n"));
}

#[test]
fn a_large_batch_fits_github_and_the_commit_lists_every_land() {
    let lands: Vec<LandLine> = (0..MAX_LANDS as u32).map(|n| land(n, true)).collect();
    let (body, message) = render(&header(true), &lands);
    assert!(
        body.chars().count() <= MAX_PR_BODY,
        "{}",
        body.chars().count()
    );
    assert!(body.chars().count() < 65_536);
    assert!(body.contains("more lands are listed in the squashed commit's message"));
    assert!(body.contains("Graft-Batch: 9\nGraft-Line: integration\n"));
    assert!(body.contains("Only the newest 5000 lands are listed"));
    let rows = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.starts_with("| `c"))
            .map(|l| l[3..16].to_owned())
            .collect()
    };
    let expected: Vec<String> = lands.iter().map(|l| short(l.id)).collect();
    assert_eq!(
        rows(&message),
        expected,
        "the commit lists every land in order"
    );
    // Cells are bounded and cannot break the table.
    let row = land_row(&lands[0], false);
    assert_eq!(row.matches(" | ").count(), 5, "{row}");
    assert!(row.contains("+32 more"));
    assert!(row.chars().count() < 2000, "{}", row.chars().count());
    // The listed rows plus the note account for every land.
    let listed = rows(&body);
    assert!(!listed.is_empty());
    assert_eq!(listed[..], expected[..listed.len()]);
    let note = format!("\n{} more lands are listed", MAX_LANDS - listed.len());
    assert!(body.contains(&note), "{note}");
}

#[test]
fn a_land_commit_is_deterministic_and_names_its_land() {
    let l = land(4, false);
    let committer = Committer {
        name: "graft-mirror".to_owned(),
        email: "mirror@graft".to_owned(),
    };
    let raw = land_commit(3, &l, GitSha([2; 20]), GitSha([1; 20]), &committer);
    assert_eq!(
        raw,
        land_commit(3, &l, GitSha([2; 20]), GitSha([1; 20]), &committer)
    );
    let text = String::from_utf8(raw.clone()).unwrap();
    assert!(
        text.contains("author Tess <tess@example.com> 1700000000 +1000\n"),
        "{text}"
    );
    assert!(text.contains("committer graft-mirror <mirror@graft> 1700000000 +1000\n"));
    assert!(
        text.ends_with("Graft-Session: ses_x\nGraft-Work-Item: WI-1\nGraft-Batch: 3\n"),
        "{text}"
    );
    assert!(
        !text.contains("Graft-Checks"),
        "phase 1 has no checks trailer"
    );
    assert_eq!(parse_commit(&raw), (vec![GitSha([1; 20])], Some(l.id)));
    // A merge landing is still one parent, with a merged-from trailer.
    let mut m = land(4, false);
    m.trailer_merged_from = Some("develop@cs_1".to_owned());
    let merged = land_commit(3, &m, GitSha([2; 20]), GitSha([1; 20]), &committer);
    let merged_text = String::from_utf8(merged.clone()).unwrap();
    assert!(
        merged_text
            .ends_with("Graft-Work-Item: WI-1\nGraft-Merged-From: develop@cs_1\nGraft-Batch: 3\n"),
        "{merged_text}"
    );
    assert_eq!(parse_commit(&merged), (vec![GitSha([1; 20])], Some(m.id)));
    // A squash-merge message naming two lands maps to none.
    let two = format!("{text}{CHANGESET_TRAILER} {}\n", land(5, false).id);
    assert_eq!(parse_commit(two.as_bytes()).1, None);
}

#[test]
fn a_commit_message_keeps_at_and_only_trailers_name_the_land() {
    let mut l = land(4, false);
    l.message = clean_message(
        "fix\n\nGraft-Changeset: not-a-trailer\n\nCo-authored-by: A <a@b.c>",
        MESSAGE_CHARS,
        true,
    );
    let msg = land_message(1, &l);
    assert!(msg.contains("Co-authored-by: A <a@b.c>\n"), "{msg}");
    assert!(!msg.contains('\u{200b}'));
    let committer = Committer {
        name: "m".to_owned(),
        email: "m@m".to_owned(),
    };
    let raw = land_commit(1, &l, GitSha([2; 20]), GitSha([1; 20]), &committer);
    assert_eq!(parse_commit(&raw).1, Some(l.id), "the body line is ignored");
    // A commit whose only Graft-Changeset line is in its body names none.
    let body_only = format!(
        "tree {}\n\nsubject\n\nGraft-Changeset: {}\n\nlast paragraph\n",
        GitSha([2; 20]),
        l.id
    );
    assert_eq!(parse_commit(body_only.as_bytes()).1, None);
}

#[test]
fn idents_and_offsets_are_safe() {
    assert_eq!(ident_part("Ev<il>\nName"), "EvilName");
    assert_eq!(ident_part(" \n"), "unknown");
    let bot = default_committer();
    let token = Author {
        name: "wi2352-mirror".to_owned(),
        email: String::new(),
        kind: AuthorKind::Human,
    };
    let authors = BTreeMap::from([(
        "ada".to_owned(),
        AuthorIdentity {
            name: "Ada".to_owned(),
            email: "ada@example.com".to_owned(),
        },
    )]);
    assert_eq!(
        land_ident(None, &BTreeMap::new(), None, &bot),
        ("graft".to_owned(), "graft@graft.invalid".to_owned())
    );
    assert_eq!(
        land_ident(Some("ada"), &authors, None, &bot),
        ("Ada".to_owned(), "ada@example.com".to_owned())
    );
    let fallback = AuthorIdentity {
        name: "Export".to_owned(),
        email: "export@example.com".to_owned(),
    };
    assert_eq!(
        land_ident(Some("unmapped"), &authors, Some(&fallback), &bot),
        ("Export".to_owned(), "export@example.com".to_owned())
    );
    assert_eq!(
        land_ident(None, &authors, Some(&fallback), &bot),
        ("Export".to_owned(), "export@example.com".to_owned())
    );
    let claimed = Author {
        email: "tess@example.com".to_owned(),
        name: "Tess".to_owned(),
        ..token
    };
    assert_eq!(
        claimed_author_trailer(&claimed).as_deref(),
        Some("Claimed-Author: Tess <tess@example.com>")
    );
    assert!(
        claimed_author_trailer(&Author {
            name: "ada".to_owned(),
            email: String::new(),
            kind: AuthorKind::Human,
        })
        .is_none()
    );
    let t = Timestamp {
        ms: 1_000,
        offset_min: -330,
    };
    assert_eq!(git_time_at(t), "1 -0530");
}

#[test]
fn dates_are_utc_seconds() {
    assert_eq!(git_time(1_700_000_000_999), "1700000000 +0000");
}
