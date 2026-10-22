//! Properties and examples of `merge3` (work item G3.4).

use graft_model::merge::{
    ConflictRegion, MAX_TEXT_BYTES, Merge3, is_text, merge3, render_conflict_markers,
};
use proptest::prelude::*;

/// Text from a small line alphabet, so lines repeat, with or without a final
/// newline.
fn text() -> impl Strategy<Value = Vec<u8>> {
    (
        proptest::collection::vec(
            prop::sample::select(vec!["a", "b", "c", "{", "}", ""]),
            0..16,
        ),
        any::<bool>(),
    )
        .prop_map(|(lines, nl)| {
            let mut s: String = lines.iter().map(|l| format!("{l}\n")).collect();
            if !nl {
                s.pop();
            }
            s.into_bytes()
        })
}

proptest! {
    #[test]
    fn unchanged_ours_takes_theirs(x in text(), y in text()) {
        prop_assert_eq!(merge3(&x, &x, &y), Merge3::Clean(y));
    }

    #[test]
    fn unchanged_theirs_takes_ours(b in text(), o in text()) {
        prop_assert_eq!(merge3(&b, &o, &b), Merge3::Clean(o));
    }

    #[test]
    fn same_change_on_both_sides_is_clean(b in text(), o in text()) {
        prop_assert_eq!(merge3(&b, &o, &o), Merge3::Clean(o));
    }

    #[test]
    fn swapping_sides_mirrors_the_result(b in text(), o in text(), t in text()) {
        match (merge3(&b, &o, &t), merge3(&b, &t, &o)) {
            (Merge3::Clean(x), Merge3::Clean(y)) => prop_assert_eq!(x, y),
            (Merge3::Conflicts(x), Merge3::Conflicts(y)) => {
                prop_assert_eq!(x.len(), y.len());
                // The markers mirror too: swap the ours and theirs sections.
                let mirrored = mirror(&render_conflict_markers(&b, &t, &o));
                prop_assert_eq!(render_conflict_markers(&b, &o, &t), mirrored);
            }
            (x, y) => prop_assert!(false, "{:?} vs {:?}", x, y),
        }
    }
}

/// Swaps the ours and theirs sections of every conflict.
fn mirror(rendered: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut ours, mut base, mut theirs) = (Vec::new(), Vec::new(), Vec::new());
    let mut section = 0;
    for line in rendered.split_inclusive(|&c| c == b'\n') {
        match (section, line) {
            (0, b"<<<<<<< ours\n") => section = 1,
            (1, b"||||||| base\n") => section = 2,
            (2, b"=======\n") => section = 3,
            (3, b">>>>>>> theirs\n") => {
                out.extend_from_slice(b"<<<<<<< ours\n");
                out.append(&mut theirs);
                out.extend_from_slice(b"||||||| base\n");
                out.append(&mut base);
                out.extend_from_slice(b"=======\n");
                out.append(&mut ours);
                out.extend_from_slice(b">>>>>>> theirs\n");
                section = 0;
            }
            (0, l) => out.extend_from_slice(l),
            (1, l) => ours.extend_from_slice(l),
            (2, l) => base.extend_from_slice(l),
            (_, l) => theirs.extend_from_slice(l),
        }
    }
    out
}

#[test]
fn separate_edits_merge() {
    let base = b"one\ntwo\nthree\nfour\nfive\n";
    let ours = b"ONE\ntwo\nthree\nfour\nfive\n";
    let theirs = b"one\ntwo\nthree\nfour\nFIVE\n";
    assert_eq!(
        merge3(base, ours, theirs),
        Merge3::Clean(b"ONE\ntwo\nthree\nfour\nFIVE\n".to_vec())
    );
}

#[test]
fn overlapping_edits_conflict_with_markers() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nB1\nc\n";
    let theirs = b"a\nB2\nc";
    assert_eq!(
        merge3(base, ours, theirs),
        Merge3::Conflicts(vec![ConflictRegion { start: 1, end: 3 }])
    );
    assert_eq!(
        String::from_utf8(render_conflict_markers(base, ours, theirs)).unwrap(),
        "a\n<<<<<<< ours\nB1\nc\n||||||| base\nb\nc\n=======\nB2\nc\n>>>>>>> theirs\n"
    );
}

#[test]
fn adjacent_edits_conflict() {
    // As in git, changes to neighbouring lines are a conflict.
    let base = b"a\nb\n";
    assert!(matches!(
        merge3(base, b"A\nb\n", b"a\nB\n"),
        Merge3::Conflicts(_)
    ));
}

#[test]
fn text_detection() {
    assert!(is_text(b""));
    assert!(is_text("caf\u{e9}\n".as_bytes()));
    assert!(!is_text(b"a\0b"));
    assert!(!is_text(&[0xff, 0xfe]));
    assert!(is_text(&vec![b'a'; MAX_TEXT_BYTES - 1]));
    assert!(!is_text(&vec![b'a'; MAX_TEXT_BYTES]));
    assert_eq!(merge3(b"a", b"\0", b"a"), Merge3::NotText);
    assert_eq!(render_conflict_markers(b"a", b"x\0", b"b"), b"x\0".to_vec());
}

#[test]
fn heavily_repeated_lines_use_the_fallback() {
    // Every common line occurs more than 64 times: the Myers fallback runs.
    let base = "x\n".repeat(100);
    let ours = format!("y\n{base}");
    let theirs = format!("{base}z\n");
    assert_eq!(
        merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
        Merge3::Clean(format!("y\n{base}z\n").into_bytes())
    );
}
