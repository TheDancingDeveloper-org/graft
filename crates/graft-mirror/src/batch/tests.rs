use std::collections::BTreeMap;

use graft_git::GitSha;
use graft_git::export::Committer;
use graft_model::{Author, AuthorKind, ChangesetId, Timestamp};

use crate::export::default_committer;
use crate::text::clean_message;

use super::render::{
    BatchHeader, land_commit, land_message, land_row, parse_commit, render, settled_keep,
};
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
        checked: false,
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

/// The four attribution fields, under one key prefix.
fn attribution_meta(
    prefix: &str,
    session: &str,
    work: &str,
    model: &str,
    co: &str,
) -> graft_model::Metadata {
    [
        ("session", session),
        ("work_item", work),
        ("model", model),
        ("coauthors", co),
    ]
    .into_iter()
    .map(|(field, value)| (format!("{prefix}.{field}"), value.to_owned()))
    .collect()
}

fn read_four(meta: &graft_model::Metadata) -> (String, String, String, Vec<String>) {
    use graft_proto::attribution::Key;
    (
        super::attribution_cell(meta, Key::Session).unwrap(),
        super::attribution_cell(meta, Key::WorkItem).unwrap(),
        super::attribution_trailer(meta, Key::Model).unwrap(),
        super::coauthor_lines(meta),
    )
}

#[test]
fn attribution_new_keys_only() {
    let meta = attribution_meta(
        "agent",
        "ses_new",
        "WI-9",
        "model-a",
        "Ada <ada@example.com>",
    );
    assert_eq!(
        read_four(&meta),
        (
            "ses_new".into(),
            "WI-9".into(),
            "model-a".into(),
            vec!["Ada <ada@example.com>".into()]
        )
    );
}

#[test]
fn attribution_legacy_keys_only() {
    let meta = attribution_meta(
        "vogt",
        "ses_old",
        "WI-1",
        "model-z",
        "Bea <bea@example.com>",
    );
    assert_eq!(
        read_four(&meta),
        (
            "ses_old".into(),
            "WI-1".into(),
            "model-z".into(),
            vec!["Bea <bea@example.com>".into()]
        )
    );
}

#[test]
fn attribution_both_prefers_new() {
    let mut meta = attribution_meta(
        "agent",
        "ses_new",
        "WI-9",
        "model-a",
        "Ada <ada@example.com>",
    );
    meta.extend(attribution_meta(
        "vogt",
        "ses_old",
        "WI-1",
        "model-z",
        "Bea <bea@example.com>",
    ));
    assert_eq!(
        read_four(&meta),
        (
            "ses_new".into(),
            "WI-9".into(),
            "model-a".into(),
            vec!["Ada <ada@example.com>".into()]
        )
    );
}

#[test]
fn pr_body_says_session_and_work_item_not_the_tracker() {
    let (body, message) = render(&header(false), &[land(1, false)]);
    for text in [&body, &message] {
        assert!(text.contains("| Session | Work item |"), "{text}");
        assert!(!text.contains("Vogt") && !text.contains("vogt"), "{text}");
    }
    // Export trailers stay the Graft-* names this crate already wrote.
    let msg = land_message(3, &land(1, false));
    assert!(msg.contains("Graft-Session: ses_x\n"));
    assert!(msg.contains("Graft-Work-Item: WI-1\n"));
    assert!(!msg.contains("Vogt-"));
    let raw = land_commit(
        3,
        &land(1, false),
        GitSha([2; 20]),
        GitSha([3; 20]),
        &default_committer(),
    );
    let again = land_commit(
        3,
        &land(1, false),
        GitSha([2; 20]),
        GitSha([3; 20]),
        &default_committer(),
    );
    assert_eq!(raw, again, "the same land still hashes to the same commit");
    let text = String::from_utf8(raw).unwrap();
    assert!(text.contains("Graft-Session: ses_x\n"));
    assert!(!text.contains("Vogt-"));
}

// ---- The settled cut on a checked line (design §8.7 step 6) ----

use std::collections::HashMap;

use graft_proto::types::{LandCheck, LandCheckState};

/// Lands 1..=n, returned newest first as the batcher holds them, with
/// `reverts[i] = Some(j)` making land `i + 1` a revert of land `j`.
fn line_of(reverts: &[Option<u32>]) -> Vec<LandLine> {
    let mut lands: Vec<LandLine> = (1..=reverts.len() as u32)
        .map(|n| {
            let mut l = land(n, false);
            l.reverts = reverts[n as usize - 1].map(|t| land(t, false).id);
            l
        })
        .collect();
    lands.reverse();
    lands
}

/// Rows for lands 1..: `states[i]` for land `i + 1`, `None` for no row.
fn rows_of(states: &[Option<LandCheckState>]) -> HashMap<ChangesetId, LandCheck> {
    states
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let id = land(i as u32 + 1, false).id;
            s.map(|state| {
                (
                    id,
                    LandCheck {
                        pos: i as u64 + 10,
                        changeset: id,
                        state,
                        revert: None,
                    },
                )
            })
        })
        .collect()
}

use LandCheckState::{Bad, Good, Pending, Reverted};

#[test]
fn the_cut_stops_before_a_pending_land() {
    let lands = line_of(&[None; 4]);
    let rows = rows_of(&[Some(Good), Some(Good), Some(Pending), Some(Good)]);
    assert_eq!(
        settled_keep(&lands, &rows, usize::MAX),
        (2, Some(land(3, false).id))
    );
}

#[test]
fn a_bad_land_is_taken_with_its_revert() {
    let lands = line_of(&[None, None, Some(2), None]);
    let mut rows = rows_of(&[Some(Good), Some(Reverted), Some(Good), Some(Good)]);
    rows.get_mut(&land(2, false).id).unwrap().revert = Some(land(3, false).id);
    assert_eq!(settled_keep(&lands, &rows, usize::MAX), (4, None));
    // `bad` with the revert named only by the revert's metadata.
    let rows = rows_of(&[Some(Good), Some(Bad), Some(Good), Some(Good)]);
    assert_eq!(settled_keep(&lands, &rows, usize::MAX), (4, None));
}

#[test]
fn a_bad_land_with_no_revert_stops_the_cut() {
    let lands = line_of(&[None; 3]);
    let rows = rows_of(&[Some(Good), Some(Bad), Some(Good)]);
    assert_eq!(
        settled_keep(&lands, &rows, usize::MAX),
        (1, Some(land(2, false).id))
    );
    let rows = rows_of(&[Some(Bad), Some(Good), Some(Good)]);
    assert_eq!(
        settled_keep(&lands, &rows, usize::MAX),
        (0, Some(land(1, false).id))
    );
}

#[test]
fn a_pending_revert_holds_the_land_it_reverts() {
    let lands = line_of(&[None, None, Some(2)]);
    let rows = rows_of(&[Some(Good), Some(Bad), Some(Pending)]);
    assert_eq!(
        settled_keep(&lands, &rows, usize::MAX),
        (1, Some(land(3, false).id))
    );
}

#[test]
fn a_land_without_a_row_waits_unless_it_reverts_a_kept_land() {
    let lands = line_of(&[None, None]);
    let rows = rows_of(&[Some(Good), None]);
    assert_eq!(
        settled_keep(&lands, &rows, usize::MAX),
        (1, Some(land(2, false).id))
    );
    // An operator's drop: the revert moved onto the line has no row.
    let lands = line_of(&[None, Some(1), None]);
    let rows = rows_of(&[Some(Pending), None, Some(Good)]);
    assert_eq!(settled_keep(&lands, &rows, usize::MAX), (3, None));
}

#[test]
fn the_land_cap_applies_within_the_settled_prefix_and_never_splits_a_revert() {
    let lands = line_of(&[None; 4]);
    let rows = rows_of(&[Some(Good); 4]);
    assert_eq!(settled_keep(&lands, &rows, 3).0, 3);
    // Land 2 and its revert (land 3) stay together: a cap of 2 stops
    // before the pair, a cap of 3 takes it.
    let lands = line_of(&[None, None, Some(2), None]);
    let rows = rows_of(&[Some(Good), Some(Bad), Some(Good), Some(Good)]);
    assert_eq!(settled_keep(&lands, &rows, 2).0, 1);
    assert_eq!(settled_keep(&lands, &rows, 3).0, 3);
    // A cap below every valid cut takes the smallest: the pair whole.
    let lands = line_of(&[None, Some(1), None]);
    let rows = rows_of(&[Some(Bad), Some(Good), Some(Good)]);
    assert_eq!(settled_keep(&lands, &rows, 1).0, 2);
}

fn message_for(text: &str) -> String {
    let mut l = land(1, false);
    l.message = text.to_owned();
    l.trailer_session = None;
    l.trailer_work_item = None;
    land_message(4, &l)
}

#[test]
fn trailers_join_a_final_trailer_block() {
    let msg = message_for("fix: thing\n\nbody text\n\nCo-authored-by: X <x@example.com>\n");
    let id = land(1, false).id;
    assert_eq!(
        msg,
        format!(
            "fix: thing\n\nbody text\n\nCo-authored-by: X <x@example.com>\n\
             Graft-Changeset: {id}\nGraft-Batch: 4\n"
        )
    );
}

#[test]
fn trailers_start_a_new_block_after_a_plain_body() {
    let id = land(1, false).id;
    assert_eq!(
        message_for("fix: thing\n\nbody text"),
        format!("fix: thing\n\nbody text\n\nGraft-Changeset: {id}\nGraft-Batch: 4\n")
    );
    // A subject alone is never a trailer block, even with a colon.
    assert_eq!(
        message_for("fix: thing"),
        format!("fix: thing\n\nGraft-Changeset: {id}\nGraft-Batch: 4\n")
    );
}

#[test]
fn a_body_line_with_a_colon_is_not_a_trailer() {
    let id = land(1, false).id;
    assert_eq!(
        message_for("subject\n\nThis fixes the parser: it now stops."),
        format!(
            "subject\n\nThis fixes the parser: it now stops.\n\n\
             Graft-Changeset: {id}\nGraft-Batch: 4\n"
        )
    );
    // One prose line spoils the block, as in git.
    assert!(
        message_for("subject\n\nSigned-off-by: A <a@example.com>\nand some prose")
            .contains("prose\n\nGraft-Changeset:")
    );
}

#[test]
fn crlf_and_trailing_whitespace_still_join_the_block() {
    let id = land(1, false).id;
    assert_eq!(
        message_for("subject  \r\n\r\nCo-authored-by: X <x@example.com>  \r\n\r\n  "),
        format!(
            "subject\n\nCo-authored-by: X <x@example.com>\n\
             Graft-Changeset: {id}\nGraft-Batch: 4\n"
        )
    );
}
