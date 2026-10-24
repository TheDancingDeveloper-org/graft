//! The adversarial harness, part 2: graft's line merge against
//! `git merge-file` (`docs/notes/adversarial-harness.md` §4, work item
//! WI-2346).
//!
//! [`case`] draws one seeded three-way input, shaped to hit a chosen
//! [`Shape`]: adjacent and overlapping hunks, delete against edit, moved
//! code, identical changes on both sides, CRLF and mixed line endings,
//! missing final newlines, very long lines, empty files, bytes that are not
//! UTF-8, NUL bytes, and sizes at the 1 MiB text limit. [`classify`] compares
//! graft's `merge3` with git's answer for the same input and names every
//! disagreement, either as an accepted difference (with its reason, see
//! [`Verdict::reason`]) or as a bug.
//!
//! This module never runs git: the integration test `tests/merge_diff.rs`
//! does, as a test oracle. graft-harness is a test tool, not a product
//! crate, so rule 6 (git stays in `graft-git` and `graft-mirror`) is not
//! touched; nothing here is a land check (D22).

use graft_model::merge::{MAX_TEXT_BYTES, Merge3, merge3, render_conflict_markers};

use crate::rng::Rng;

/// What a generated case is built to exercise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Shape {
    /// Independent random edits of both sides.
    Random,
    /// Both sides edit neighbouring lines, with no line between them.
    Adjacent,
    /// Both sides edit overlapping ranges, differently.
    Overlapping,
    /// One side deletes a range the other edits.
    DeleteVsEdit,
    /// One side moves a block, the other edits inside it.
    Moved,
    /// Both sides make the same change.
    Identical,
    /// Random edits, then CRLF or mixed line endings.
    LineEndings,
    /// Random edits, then the final newline dropped on some sides.
    NoFinalNewline,
    /// Lines of many kilobytes.
    LongLines,
    /// One or more inputs empty.
    Empty,
    /// A byte that is not valid UTF-8 in some input.
    NotUtf8,
    /// A NUL byte in some input.
    Nul,
    /// An input at or just under the 1 MiB text limit.
    AtLimit,
}

impl Shape {
    /// Every shape, in a fixed order.
    pub const ALL: [Shape; 13] = [
        Shape::Random,
        Shape::Adjacent,
        Shape::Overlapping,
        Shape::DeleteVsEdit,
        Shape::Moved,
        Shape::Identical,
        Shape::LineEndings,
        Shape::NoFinalNewline,
        Shape::LongLines,
        Shape::Empty,
        Shape::NotUtf8,
        Shape::Nul,
        Shape::AtLimit,
    ];
}

/// One three-way input.
#[derive(Clone, Debug)]
pub struct Case {
    /// What the case is built to exercise.
    pub shape: Shape,
    /// The common ancestor.
    pub base: Vec<u8>,
    /// One side.
    pub ours: Vec<u8>,
    /// The other side.
    pub theirs: Vec<u8>,
}

/// git's answer for a case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Git {
    /// Exit 0: the merged bytes.
    Clean(Vec<u8>),
    /// Exit 1..=127: the output with conflict markers, and the number of
    /// conflicts git reported.
    Conflicts(Vec<u8>, usize),
    /// git refused (exit 255): it found a binary input.
    Refused,
}

/// How graft and git compare on one case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Verdict {
    /// Same answer, byte for byte.
    Agree,
    /// Both refuse to line-merge: graft `NotText`, git binary.
    BothBinary,
    /// Accepted: graft treats a non-UTF-8 input as not text (D16); git
    /// merges it.
    AcceptedNotUtf8,
    /// Accepted: graft treats an input of 1 MiB or more as not text (D16).
    AcceptedTooLarge,
    /// Accepted: graft treats a NUL anywhere as binary; git looks only at
    /// the first 8000 bytes.
    AcceptedNulPastGitWindow,
    /// Accepted: both conflict in the same regions; git writes CRLF marker
    /// lines next to CRLF content, graft always writes `\n`.
    AcceptedCrlfMarkers,
    /// Accepted: graft's diff ran over its work budget (`TooComplex`).
    AcceptedTooComplex,
    /// Accepted: both conflict, with the same number of regions, but the
    /// regions are placed differently (git releases differ here too).
    AcceptedConflictPlacement,
    /// Bug: graft merged cleanly where git conflicted.
    GraftCleanGitConflict,
    /// Bug: graft conflicted where git merged cleanly.
    GraftConflictGitClean,
    /// Bug: both clean, with different bytes.
    CleanDiffers,
    /// Bug: both conflict, with a different number of regions.
    ConflictCountDiffers,
    /// Bug: graft refused text that git merged, for no listed reason.
    GraftRefusedText,
    /// Bug: graft line-merged an input git found binary.
    GraftMergedBinary,
}

impl Verdict {
    /// Whether the disagreement is a bug (not agreement or a listed
    /// difference).
    pub fn is_bug(self) -> bool {
        self >= Verdict::GraftCleanGitConflict
    }

    /// Why an accepted difference is accepted.
    pub fn reason(self) -> &'static str {
        match self {
            Verdict::Agree => "same answer",
            Verdict::BothBinary => "both refuse binary input",
            Verdict::AcceptedNotUtf8 => "D16: text must be valid UTF-8",
            Verdict::AcceptedTooLarge => "D16: text is under 1 MiB",
            Verdict::AcceptedNulPastGitWindow => {
                "D16: any NUL means binary; git checks only the first 8000 bytes"
            }
            Verdict::AcceptedCrlfMarkers => "graft writes conflict markers with \\n only",
            Verdict::AcceptedTooComplex => "work budget, design rule 2",
            Verdict::AcceptedConflictPlacement => {
                "diff3 regions may be placed differently; both valid (see merge_corpus.rs)"
            }
            _ => "bug",
        }
    }
}

/// Compares graft's merge of `case` with git's answer.
pub fn classify(case: &Case, git: &Git) -> Verdict {
    let (b, o, t) = (&case.base[..], &case.ours[..], &case.theirs[..]);
    let all = [b, o, t];
    match (merge3(b, o, t), git) {
        (Merge3::NotText, Git::Refused) => Verdict::BothBinary,
        (Merge3::NotText, _) => {
            if all.iter().any(|x| x.len() >= MAX_TEXT_BYTES) {
                Verdict::AcceptedTooLarge
            } else if all.iter().any(|x| x.contains(&0)) {
                Verdict::AcceptedNulPastGitWindow
            } else if all.iter().any(|x| std::str::from_utf8(x).is_err()) {
                Verdict::AcceptedNotUtf8
            } else {
                Verdict::GraftRefusedText
            }
        }
        (_, Git::Refused) => Verdict::GraftMergedBinary,
        (Merge3::TooComplex, _) => Verdict::AcceptedTooComplex,
        (Merge3::Clean(ours), Git::Clean(theirs)) => {
            if &ours == theirs {
                Verdict::Agree
            } else {
                Verdict::CleanDiffers
            }
        }
        (Merge3::Clean(_), Git::Conflicts(..)) => Verdict::GraftCleanGitConflict,
        (Merge3::Conflicts(_), Git::Clean(_)) => Verdict::GraftConflictGitClean,
        (Merge3::Conflicts(regions), Git::Conflicts(out, n)) => {
            let rendered = render_conflict_markers(b, o, t);
            if &rendered == out {
                Verdict::Agree
            } else if regions.len() != *n {
                Verdict::ConflictCountDiffers
            } else if strip_marker_cr(out) == rendered {
                Verdict::AcceptedCrlfMarkers
            } else {
                Verdict::AcceptedConflictPlacement
            }
        }
    }
}

/// git's output with the `\r` taken off conflict marker lines only.
fn strip_marker_cr(out: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(out.len());
    for line in out.split_inclusive(|&c| c == b'\n') {
        let marker = [&b"<<<<<<< "[..], b"||||||| ", b"=======", b">>>>>>> "]
            .iter()
            .any(|m| line.starts_with(m));
        if marker && line.ends_with(b"\r\n") {
            v.extend_from_slice(&line[..line.len() - 2]);
            v.push(b'\n');
        } else {
            v.extend_from_slice(line);
        }
    }
    v
}

/// Generates case `n` of the run with `seed`. The shape cycles through
/// [`Shape::ALL`], so every shape is drawn equally.
pub fn case(seed: u64, n: u64) -> Case {
    let mut rng = Rng::new(crate::rng::mix(seed ^ n.wrapping_mul(0x9E37_79B9)));
    let shape = Shape::ALL[(n % Shape::ALL.len() as u64) as usize];
    let mut g = Gen {
        rng: &mut rng,
        fresh: 0,
    };
    let (b, o, t) = g.shaped(shape);
    let (base, ours, theirs) = g.encode(shape, b, o, t);
    Case {
        shape,
        base,
        ours,
        theirs,
    }
}

type Lines = Vec<String>;

struct Gen<'a> {
    rng: &'a mut Rng,
    fresh: usize,
}

impl Gen<'_> {
    fn below(&mut self, n: usize) -> usize {
        self.rng.below(n as u64) as usize
    }

    /// A line without its newline: mostly from a small alphabet, so lines
    /// repeat and alignments are ambiguous.
    fn line(&mut self) -> String {
        if self.below(4) == 0 {
            self.fresh += 1;
            format!("unique {}", self.fresh)
        } else {
            const WORDS: [&str; 8] = ["{", "}", "a", "b", "", "return x;", "    x += 1;", "fn f()"];
            WORDS[self.below(WORDS.len())].to_string()
        }
    }

    fn lines(&mut self, n: usize) -> Lines {
        (0..n).map(|_| self.line()).collect()
    }

    fn random_edit(&mut self, base: &[String]) -> Lines {
        let mut out = base.to_vec();
        for _ in 0..1 + self.below(6) {
            let at = self.below(out.len() + 1);
            match self.below(3) {
                0 => {
                    let n = 1 + self.below(5);
                    let new = self.lines(n);
                    out.splice(at..at, new);
                }
                1 if at < out.len() => {
                    let n = (1 + self.below(3)).min(out.len() - at);
                    out.drain(at..at + n);
                }
                _ if at < out.len() => out[at] = self.line(),
                _ => out.push(self.line()),
            }
        }
        out
    }

    /// Replaces `base[r]` with fresh lines.
    fn replace(&mut self, base: &[String], start: usize, end: usize) -> Lines {
        let mut out = base.to_vec();
        let n = 1 + self.below(3);
        let new = self.lines(n);
        out.splice(start..end, new);
        out
    }

    fn shaped(&mut self, shape: Shape) -> (Lines, Lines, Lines) {
        let len = 6 + self.below(30);
        let base = self.lines(len);
        match shape {
            Shape::Adjacent => {
                let a = self.below(len - 2);
                let n = 1 + self.below(2.min(len - a - 1));
                let o = self.replace(&base, a, a + n);
                let m = 1 + self.below(2.min(len - a - n).max(1));
                let t = self.replace(&base, a + n, (a + n + m).min(len));
                (base, o, t)
            }
            Shape::Overlapping => {
                let a = self.below(len - 3);
                let o = self.replace(&base, a, a + 2);
                let t = self.replace(&base, a + 1, a + 3);
                (base, o, t)
            }
            Shape::DeleteVsEdit => {
                let a = self.below(len - 3);
                let mut o = base.clone();
                o.drain(a..a + 3);
                let t = self.replace(&base, a + 1, a + 2);
                (base, o, t)
            }
            Shape::Moved => {
                // ours cuts a block and pastes it elsewhere; theirs edits a
                // line inside the block.
                let a = self.below(len - 3);
                let n = 2 + self.below(2.min(len - a - 2).max(1));
                let n = n.min(len - a);
                let mut o = base.clone();
                let block: Lines = o.drain(a..a + n).collect();
                let to = self.below(o.len() + 1);
                o.splice(to..to, block);
                let i = a + self.below(n);
                let mut t = base.clone();
                t[i] = self.line();
                (base, o, t)
            }
            Shape::Identical => {
                let o = self.random_edit(&base);
                let t = if self.below(2) == 0 {
                    o.clone()
                } else {
                    // The same change plus one more, elsewhere.
                    let mut t = o.clone();
                    t.push(self.line());
                    t
                };
                (base, o, t)
            }
            Shape::LongLines => {
                let mut base = base;
                for _ in 0..1 + self.below(3) {
                    let i = self.below(base.len());
                    let k = 4_000 + self.below(60_000);
                    base[i] = "x".repeat(k);
                }
                let o = self.random_edit(&base);
                let mut t = self.random_edit(&base);
                if !t.is_empty() {
                    let i = self.below(t.len());
                    t[i].push_str(&"y".repeat(10_000));
                }
                (base, o, t)
            }
            Shape::Empty => {
                let o = self.random_edit(&base);
                let t = self.random_edit(&base);
                match self.below(4) {
                    0 => (Vec::new(), o, t),
                    1 => (base, Vec::new(), t),
                    2 => (base, o, Vec::new()),
                    _ => (Vec::new(), Vec::new(), t),
                }
            }
            Shape::AtLimit => {
                // A base just under or at the limit, made of repeated
                // unique-ish lines so the diff stays cheap.
                let target = MAX_TEXT_BYTES - 64 + self.below(128);
                let mut big = Vec::new();
                let mut size = 0;
                let mut i = 0;
                while size < target {
                    let l = format!("line {i:08} of a large file, padded out to sixty-four b");
                    size += l.len() + 1;
                    big.push(l);
                    i += 1;
                }
                let o = self.replace(&big, 10, 12);
                let n = big.len();
                let t = self.replace(&big, n - 12, n - 10);
                (big, o, t)
            }
            _ => {
                let o = self.random_edit(&base);
                let t = self.random_edit(&base);
                (base, o, t)
            }
        }
    }

    /// Joins lines into bytes, applying the shape's encoding hazards.
    fn encode(
        &mut self,
        shape: Shape,
        b: Lines,
        o: Lines,
        t: Lines,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let eol = match shape {
            Shape::LineEndings => self.below(3) + 1, // 1 CRLF, 2 mixed, 3 per side
            _ => 0,
        };
        let crlf_side = self.below(3);
        let mut out = [b, o, t].map(Some);
        let mut bytes: [Vec<u8>; 3] = Default::default();
        for (side, lines) in out.iter_mut().enumerate() {
            let lines = lines.take().unwrap_or_default();
            let mut v = Vec::new();
            for l in &lines {
                v.extend_from_slice(l.as_bytes());
                let cr = match eol {
                    1 => true,
                    // Mixed endings: a stable choice per base line content,
                    // so unchanged lines keep their ending on every side.
                    2 => {
                        crate::rng::mix(
                            l.bytes()
                                .fold(l.len() as u64, |h, c| h.wrapping_mul(31) ^ u64::from(c)),
                        ) & 1
                            == 0
                    }
                    3 => side == crlf_side,
                    _ => false,
                };
                v.extend_from_slice(if cr { b"\r\n" } else { b"\n" });
            }
            bytes[side] = v;
        }
        match shape {
            Shape::NoFinalNewline => {
                for v in &mut bytes {
                    if self.below(2) == 0 && v.last() == Some(&b'\n') {
                        v.pop();
                    }
                }
            }
            Shape::NotUtf8 | Shape::Nul => {
                let bad: &[u8] = if shape == Shape::Nul {
                    b"\0"
                } else {
                    const BAD: [&[u8]; 4] = [b"\xff", b"\xc3", b"\xed\xa0\x80", b"\xe9t\xe9"];
                    BAD[self.below(BAD.len())]
                };
                // In the base (so all sides share it), or on one side only.
                let sides: &[usize] = match self.below(3) {
                    0 => &[0, 1, 2],
                    1 => &[1],
                    _ => &[2],
                };
                let pos = self.rng.next_u64();
                for &s in sides {
                    let v = &mut bytes[s];
                    let at = if v.is_empty() {
                        0
                    } else {
                        (pos % v.len() as u64) as usize
                    };
                    // Put it at a line start so line structure is kept.
                    let at = v[..at]
                        .iter()
                        .rposition(|&c| c == b'\n')
                        .map_or(0, |p| p + 1);
                    v.splice(at..at, bad.iter().copied());
                }
                // Now and then, a NUL far into a long file, past git's
                // 8000-byte binary window.
                if shape == Shape::Nul && self.below(3) == 0 {
                    for v in &mut bytes {
                        v.retain(|&c| c != 0);
                    }
                    let pad = "z".repeat(90);
                    let mut head = Vec::new();
                    for i in 0..120 {
                        head.extend_from_slice(format!("{i} {pad}\n").as_bytes());
                    }
                    for v in &mut bytes {
                        v.splice(0..0, head.iter().copied());
                    }
                    bytes[1 + self.below(2)].extend_from_slice(b"tail\0\n");
                }
            }
            _ => {}
        }
        let [b, o, t] = bytes;
        (b, o, t)
    }
}
