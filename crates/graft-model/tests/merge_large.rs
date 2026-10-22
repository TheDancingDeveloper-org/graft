//! Large inputs to `merge3` stay bounded in time and memory (work item G3.4,
//! design rule 2): each finishes within a stated bound or returns
//! `Merge3::TooComplex`.
//!
//! The two shapes are the review probes: a file of empty lines (every line
//! repeats, so the Myers fallback runs with an edit distance of about the file
//! size), and a file of unique lines with a change on every other line (many
//! scattered hunks, which make git's histogram diff re-scan the file for every
//! hunk). The full-size probes are `#[ignore]`d because they are slow in a
//! debug build; run them with
//! `cargo test -p graft-model --release -- --ignored merge`.
//!
//! Memory is the process's peak resident set (`VmHWM`), reset before each
//! test, so it is only checked on Linux. The tests in this file hold a lock so
//! that they do not run at the same time.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use graft_model::merge::{Merge3, merge3, render_conflict_markers};

static SERIAL: Mutex<()> = Mutex::new(());

/// Resets the peak resident set, where the platform allows it.
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// The peak resident set in bytes, where the platform reports it.
fn peak() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Runs `merge3` and checks it took less than `time` and grew the resident
/// set by less than `memory` bytes.
fn bounded(base: &[u8], ours: &[u8], theirs: &[u8], time: Duration, memory: usize) -> Merge3 {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_peak();
    let before = peak();
    let start = Instant::now();
    let result = merge3(base, ours, theirs);
    let took = start.elapsed();
    let grew = peak().zip(before).map(|(p, b)| p.saturating_sub(b));
    eprintln!(
        "{} + {} + {} bytes: {took:?}, peak resident set grew {grew:?} bytes, {}",
        base.len(),
        ours.len(),
        theirs.len(),
        match &result {
            Merge3::Clean(_) => "clean".to_string(),
            Merge3::Conflicts(c) => format!("{} conflicts", c.len()),
            other => format!("{other:?}"),
        }
    );
    assert!(took < time, "took {took:?}, bound {time:?}");
    if let Some(grew) = grew {
        assert!(grew < memory, "grew {grew} bytes, bound {memory}");
    }
    result
}

/// `lines` lines from `line`, with every `every`th line (from the first)
/// replaced by `edit`.
fn file(
    lines: usize,
    line: impl Fn(usize) -> String,
    every: Option<(usize, &dyn Fn(usize) -> String)>,
) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..lines {
        match every {
            Some((n, edit)) if i % n == 0 => out.push_str(&edit(i)),
            _ => out.push_str(&line(i)),
        }
    }
    out.into_bytes()
}

fn blank(_: usize) -> String {
    "\n".to_string()
}

fn unique(i: usize) -> String {
    format!("line {i}\n")
}

/// Short edits for the empty-line files, so 250 000 lines stay under 1 MiB.
fn o(_: usize) -> String {
    "o\n".to_string()
}

fn t(_: usize) -> String {
    "t\n".to_string()
}

fn ours_edit(i: usize) -> String {
    format!("ours {i}\n")
}

fn theirs_edit(i: usize) -> String {
    format!("theirs {i}\n")
}

const MIB: usize = 1 << 20;

/// The first review probe, at the size where the old Myers fallback could no
/// longer allocate (20 000 lines).
#[test]
fn merge_blank_lines_is_bounded() {
    let base = file(20_000, blank, None);
    let ours = file(20_000, blank, Some((2, &o)));
    let theirs = file(20_000, blank, Some((3, &t)));
    let result = bounded(&base, &ours, &theirs, Duration::from_secs(10), 64 * MIB);
    assert_eq!(result, Merge3::TooComplex);
}

/// The second review probe, at a tenth of its size.
#[test]
fn merge_scattered_hunks_is_bounded() {
    let base = file(6_000, unique, None);
    let ours = file(6_000, unique, Some((2, &ours_edit)));
    let result = bounded(&base, &ours, &base, Duration::from_secs(10), 64 * MIB);
    assert_eq!(result, Merge3::Clean(ours));
}

#[test]
fn too_complex_renders_one_whole_file_conflict() {
    let base = file(20_000, blank, None);
    let ours = file(20_000, blank, Some((2, &o)));
    let theirs = file(20_000, blank, Some((3, &t)));
    let mut expected = b"<<<<<<< ours\n".to_vec();
    expected.extend_from_slice(&ours);
    expected.extend_from_slice(b"||||||| base\n");
    expected.extend_from_slice(&base);
    expected.extend_from_slice(b"=======\n");
    expected.extend_from_slice(&theirs);
    expected.extend_from_slice(b">>>>>>> theirs\n");
    assert!(render_conflict_markers(&base, &ours, &theirs) == expected);
}

/// The first review probe at full size: 250 000 empty lines, ours editing
/// every 2nd line and theirs every 3rd. The old code was killed for lack of
/// memory.
#[test]
#[ignore = "slow in a debug build"]
fn merge_blank_lines_probe() {
    let base = file(250_000, blank, None);
    let ours = file(250_000, blank, Some((2, &o)));
    let theirs = file(250_000, blank, Some((3, &t)));
    let result = bounded(&base, &ours, &theirs, Duration::from_secs(2), 64 * MIB);
    assert_eq!(result, Merge3::TooComplex);
}

/// The second review probe at full size: 60 000 unique lines (about 500 KB),
/// ours changing every 2nd line and theirs the base. The old code took 36 s.
#[test]
#[ignore = "slow in a debug build"]
fn merge_scattered_hunks_probe() {
    let base = file(60_000, unique, None);
    let ours = file(60_000, unique, Some((2, &ours_edit)));
    let result = bounded(&base, &ours, &base, Duration::from_secs(1), 64 * MIB);
    assert_eq!(result, Merge3::Clean(ours));
}

/// The same with theirs changing every 3rd line, next to ours' changes.
#[test]
#[ignore = "slow in a debug build"]
fn merge_scattered_hunks_both_sides_probe() {
    let base = file(60_000, unique, None);
    let ours = file(60_000, unique, Some((2, &ours_edit)));
    let theirs = file(60_000, unique, Some((3, &theirs_edit)));
    let result = bounded(&base, &ours, &theirs, Duration::from_secs(1), 64 * MIB);
    assert!(matches!(result, Merge3::Conflicts(_)));
}

/// The largest inputs text detection allows: just under 1 MiB of empty lines
/// each, so every diff spends its whole budget.
#[test]
#[ignore = "slow in a debug build"]
fn merge_largest_inputs_probe() {
    let lines = (1 << 20) - 1;
    let base = file(lines, blank, None);
    let ours = file(lines, blank, Some((2, &o)));
    let theirs = file(lines, blank, Some((3, &t)));
    let ours = &ours[..ours.len().min(lines)];
    let theirs = &theirs[..theirs.len().min(lines)];
    let result = bounded(&base, ours, theirs, Duration::from_secs(5), 256 * MIB);
    assert_eq!(result, Merge3::TooComplex);
}
