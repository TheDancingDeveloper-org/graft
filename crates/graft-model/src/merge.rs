//! Three-way text merge for line-level rebase (`docs/design.md` §7.8, work
//! item G3.4).
//!
//! The merge follows `git merge-file --diff3 --diff-algorithm=histogram`:
//! each side is diffed against the base with git's histogram algorithm, the
//! hunks are slid the way xdiff's `xdl_change_compact` slides them, and the two
//! hunk lists are combined at xdiff's `EAGER` level (identical changes on both
//! sides are taken once; overlapping or adjacent changes conflict). The
//! differential test in `tests/merge_corpus.rs` pins the agreement and lists
//! the known differences.
//!
//! Cost follows the input (design rule 2). The inputs are capped at
//! [`MAX_TEXT_BYTES`] each by text detection, every structure is linear in
//! their size (the Myers fallback runs in linear space), and each side's diff
//! against the base runs under a deterministic work budget of [`WORK_FLOOR`]
//! plus [`WORK_PER_LINE`] units per line. A merge whose diff runs over it is
//! [`Merge3::TooComplex`], so no input can hold a server for long.

use std::collections::HashMap;

/// The largest input, in bytes, that is merged line by line (decision D16).
/// An input of this size or larger is [`Merge3::NotText`].
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// Lines repeated more often than this in a region make the histogram diff
/// fall back to a plain Myers diff, as git's `xhistogram` does.
const MAX_CHAIN: usize = 64;

/// Work units each side's diff against the base may use, per line of the two
/// files. A unit is one line comparison, one occurrence lookup, one Myers
/// diagonal or one slide step, roughly a few nanoseconds; the largest inputs
/// (two files of just under 1 MiB of empty lines) are allowed about 2.7 * 10^8
/// units.
pub const WORK_PER_LINE: u64 = 128;

/// Work units every diff may use whatever its size, so that small inputs,
/// where quadratic behaviour is cheap, always get git's exact result.
pub const WORK_FLOOR: u64 = 1 << 20;

/// The share of the budget, per line, that git's exact histogram diff may use
/// before the diff starts again anchored on unique lines (see `hunks`).
const EXACT_PER_LINE: u64 = 16;

/// The outcome of a three-way merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge3 {
    /// Merged cleanly.
    Clean(Vec<u8>),
    /// Conflicting regions, as line ranges in `ours`.
    Conflicts(Vec<ConflictRegion>),
    /// One of the inputs is not text (binary, too large, not UTF-8).
    NotText,
    /// Diffing ours or theirs against the base needed more than its work
    /// budget ([`WORK_FLOOR`] plus [`WORK_PER_LINE`] units per line). No line
    /// merge is attempted: callers treat this as a conflict on the whole file,
    /// as they would two divergent binary files.
    /// [`render_conflict_markers`] renders it as one conflict.
    TooComplex,
}

/// A conflicting region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRegion {
    /// First line (0-based) in `ours`.
    pub start: usize,
    /// One past the last line in `ours`.
    pub end: usize,
}

/// Merges `ours` and `theirs` against `base`.
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merge3 {
    if !is_text(base) || !is_text(ours) || !is_text(theirs) {
        return Merge3::NotText;
    }
    let input = Input::new(base, ours, theirs);
    let Ok(merges) = input.merges() else {
        return Merge3::TooComplex;
    };
    let conflicts: Vec<ConflictRegion> = merges
        .iter()
        .filter(|m| m.mode == Mode::Conflict)
        .map(|m| ConflictRegion {
            start: m.ours.start,
            end: m.ours.end,
        })
        .collect();
    if conflicts.is_empty() {
        Merge3::Clean(input.render(&merges))
    } else {
        Merge3::Conflicts(conflicts)
    }
}

/// Renders conflict markers for a client to write into the working copy.
///
/// The output is git's diff3 style with the labels `ours`, `base` and
/// `theirs`. A clean merge renders as the merged bytes. When an input is not
/// text there is nothing to merge line by line, and `ours` is returned. A
/// [`Merge3::TooComplex`] merge renders as one conflict spanning the whole of
/// each file.
pub fn render_conflict_markers(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    if !is_text(base) || !is_text(ours) || !is_text(theirs) {
        return ours.to_vec();
    }
    let input = Input::new(base, ours, theirs);
    match input.merges() {
        Ok(merges) => input.render(&merges),
        Err(TooComplex) => input.render(&[MergeHunk {
            mode: Mode::Conflict,
            base: 0..input.base.ids.len(),
            ours: 0..input.ours.ids.len(),
            theirs: 0..input.theirs.ids.len(),
        }]),
    }
}

/// Text detection (decision D16): valid UTF-8, no NUL byte, under 1 MiB.
pub fn is_text(bytes: &[u8]) -> bool {
    bytes.len() < MAX_TEXT_BYTES && !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Splits into lines, each keeping its `\n`. A final line without one is kept.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|&b| b == b'\n').collect()
}

/// One side's lines, as interned ids for comparison and slices for output.
struct File<'a> {
    lines: Vec<&'a [u8]>,
    ids: Vec<u32>,
}

struct Input<'a> {
    base: File<'a>,
    ours: File<'a>,
    theirs: File<'a>,
    /// The number of distinct lines across the three files.
    ids: usize,
}

impl<'a> Input<'a> {
    fn new(base: &'a [u8], ours: &'a [u8], theirs: &'a [u8]) -> Self {
        let mut table: HashMap<&'a [u8], u32> = HashMap::new();
        let mut file = |bytes: &'a [u8]| {
            let lines = split_lines(bytes);
            let ids = lines
                .iter()
                .map(|line| {
                    let next = table.len() as u32;
                    *table.entry(line).or_insert(next)
                })
                .collect();
            File { lines, ids }
        };
        let base = file(base);
        let ours = file(ours);
        let theirs = file(theirs);
        let ids = table.len();
        Self {
            base,
            ours,
            theirs,
            ids,
        }
    }

    /// Each side gets its own budget, so the outcome does not depend on
    /// which side is ours.
    fn merges(&self) -> Result<Vec<MergeHunk>, TooComplex> {
        let h1 = hunks(&self.base.ids, &self.ours.ids, self.ids)?;
        let h2 = hunks(&self.base.ids, &self.theirs.ids, self.ids)?;
        Ok(combine(
            &h1,
            &h2,
            &self.ours.ids,
            &self.theirs.ids,
            self.base.ids.len(),
        ))
    }

    /// Writes the merged file, with diff3 conflict markers for conflicts.
    fn render(&self, merges: &[MergeHunk]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0;
        for m in merges {
            copy(&mut out, &self.ours.lines[at..m.ours.start], false);
            match m.mode {
                Mode::Ours => copy(&mut out, &self.ours.lines[m.ours.clone()], false),
                Mode::Theirs => copy(&mut out, &self.theirs.lines[m.theirs.clone()], false),
                Mode::Conflict => {
                    out.extend_from_slice(b"<<<<<<< ours\n");
                    copy(&mut out, &self.ours.lines[m.ours.clone()], true);
                    out.extend_from_slice(b"||||||| base\n");
                    copy(&mut out, &self.base.lines[m.base.clone()], true);
                    out.extend_from_slice(b"=======\n");
                    copy(&mut out, &self.theirs.lines[m.theirs.clone()], true);
                    out.extend_from_slice(b">>>>>>> theirs\n");
                }
            }
            at = m.ours.end;
        }
        copy(&mut out, &self.ours.lines[at..], false);
        out
    }
}

/// Appends `lines`; inside a conflict a missing final newline is supplied so
/// the next marker starts on its own line.
fn copy(out: &mut Vec<u8>, lines: &[&[u8]], add_newline: bool) {
    for line in lines {
        out.extend_from_slice(line);
    }
    if add_newline && lines.last().is_some_and(|l| !l.ends_with(b"\n")) {
        out.push(b'\n');
    }
}

/// A diff hunk: `base[b..b + bn]` became `side[s..s + sn]`.
#[derive(Clone, Copy, Debug)]
struct Hunk {
    b: usize,
    bn: usize,
    s: usize,
    sn: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Conflict,
    Ours,
    Theirs,
}

#[derive(Clone, Debug)]
struct MergeHunk {
    mode: Mode,
    base: std::ops::Range<usize>,
    ours: std::ops::Range<usize>,
    theirs: std::ops::Range<usize>,
}

/// Combines the two hunk lists as xdiff's `xdl_do_merge` does at the `EAGER`
/// level.
fn combine(
    h1: &[Hunk],
    h2: &[Hunk],
    ours: &[u32],
    theirs: &[u32],
    base_len: usize,
) -> Vec<MergeHunk> {
    let mut merges: Vec<MergeHunk> = Vec::new();
    let (mut a, mut b) = (0, 0);
    while a < h1.len() && b < h2.len() {
        let (x, y) = (h1[a], h2[b]);
        if x.b + x.bn < y.b {
            let t = span(y.s as isize - y.b as isize + x.b as isize, x.bn);
            append(&mut merges, Mode::Ours, x.b..x.b + x.bn, x.s..x.s + x.sn, t);
            a += 1;
            continue;
        }
        if y.b + y.bn < x.b {
            let o = span(x.s as isize - x.b as isize + y.b as isize, y.bn);
            append(
                &mut merges,
                Mode::Theirs,
                y.b..y.b + y.bn,
                o,
                y.s..y.s + y.sn,
            );
            b += 1;
            continue;
        }
        let same = x.b == y.b
            && x.bn == y.bn
            && x.sn == y.sn
            && ours[x.s..x.s + x.sn] == theirs[y.s..y.s + y.sn];
        let (x_end, y_end) = (x.b + x.bn, y.b + y.bn);
        if !same {
            let b0 = x.b.min(y.b);
            let b1 = x_end.max(y_end);
            let ours = span(
                x.s as isize - (x.b - b0) as isize,
                x.sn + (x.b - b0) + (b1 - x_end),
            );
            let theirs = span(
                y.s as isize - (y.b - b0) as isize,
                y.sn + (y.b - b0) + (b1 - y_end),
            );
            append(&mut merges, Mode::Conflict, b0..b1, ours, theirs);
        }
        if x_end >= y_end {
            b += 1;
        }
        if y_end >= x_end {
            a += 1;
        }
    }
    for x in &h1[a..] {
        let t = span(
            x.b as isize + theirs.len() as isize - base_len as isize,
            x.bn,
        );
        append(&mut merges, Mode::Ours, x.b..x.b + x.bn, x.s..x.s + x.sn, t);
    }
    for y in &h2[b..] {
        let o = span(y.b as isize + ours.len() as isize - base_len as isize, y.bn);
        append(
            &mut merges,
            Mode::Theirs,
            y.b..y.b + y.bn,
            o,
            y.s..y.s + y.sn,
        );
    }
    merges
}

/// The range of `len` lines from `start`, as xdiff computes it in signed
/// arithmetic. A position mapped across a hunk the loop has already passed can
/// fall before line 0; such a hunk always joins the conflict before it, which
/// keeps only its end, so clamping the start is exact.
fn span(start: isize, len: usize) -> std::ops::Range<usize> {
    let end = start + len as isize;
    start.max(0) as usize..end.max(0) as usize
}

/// Adds a merge hunk, joining it to the previous one when they overlap or
/// touch in `ours` or `theirs` (xdiff's `xdl_append_merge`).
fn append(
    merges: &mut Vec<MergeHunk>,
    mode: Mode,
    base: std::ops::Range<usize>,
    ours: std::ops::Range<usize>,
    theirs: std::ops::Range<usize>,
) {
    if let Some(m) = merges.last_mut()
        && (ours.start <= m.ours.end || theirs.start <= m.theirs.end)
    {
        if m.mode != mode {
            m.mode = Mode::Conflict;
        }
        // As in xdiff, the joined hunk ends where the new one does: a
        // conflict's extrapolated end is corrected by the hunk that follows.
        m.base.end = base.end;
        m.ours.end = ours.end;
        m.theirs.end = theirs.end;
        return;
    }
    merges.push(MergeHunk {
        mode,
        base,
        ours,
        theirs,
    });
}

/// The work budget of a diff ran out.
#[derive(Debug)]
struct TooComplex;

/// Work units left for one diff. A unit is one line comparison, one
/// occurrence lookup, one Myers diagonal or one slide step.
struct Budget {
    left: u64,
}

impl Budget {
    fn new(limit: u64) -> Self {
        Self { left: limit }
    }

    fn spend(&mut self, units: usize) -> Result<(), TooComplex> {
        match self.left.checked_sub(units as u64) {
            Some(left) => {
                self.left = left;
                Ok(())
            }
            None => {
                self.left = 0;
                Err(TooComplex)
            }
        }
    }
}

/// A region still to diff: `a[a0..a1]` against `b[b0..b1]`.
#[derive(Clone, Copy, Debug)]
struct Region {
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
}

/// Where each line occurs in a file: the positions of line `id` are
/// `pos[start[id]..start[id + 1]]`, in order. It is built once per diff, so
/// the histogram diff looks occurrences up in a region by binary search
/// instead of rebuilding a table for every region.
struct Occurrences {
    start: Vec<usize>,
    pos: Vec<usize>,
}

impl Occurrences {
    fn new(file: &[u32], ids: usize) -> Self {
        let mut start = vec![0; ids + 1];
        for &id in file {
            start[id as usize + 1] += 1;
        }
        for i in 0..ids {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut pos = vec![0; file.len()];
        for (i, &id) in file.iter().enumerate() {
            pos[fill[id as usize]] = i;
            fill[id as usize] += 1;
        }
        Self { start, pos }
    }

    fn all(&self, id: u32) -> &[usize] {
        &self.pos[self.start[id as usize]..self.start[id as usize + 1]]
    }

    /// The positions of line `id` in `lo..hi`.
    fn within(&self, id: u32, lo: usize, hi: usize) -> &[usize] {
        let all = self.all(id);
        let from = all.partition_point(|&p| p < lo);
        let to = from + all[from..].partition_point(|&p| p < hi);
        &all[from..to]
    }
}

/// Diffs `a` against `b` and returns the hunks, in order, or [`TooComplex`]
/// when the work runs over [`WORK_FLOOR`] plus [`WORK_PER_LINE`] per line.
///
/// git's histogram diff runs first, with [`EXACT_PER_LINE`] per line of the
/// budget. It re-scans the rest of a region for every common run it finds, so
/// many scattered hunks make it quadratic; when it runs over its share, the
/// diff starts again by anchoring on unique lines ([`anchored`]).
fn hunks(a: &[u32], b: &[u32], ids: usize) -> Result<Vec<Hunk>, TooComplex> {
    let lines = (a.len() + b.len()) as u64;
    let mut budget = Budget::new(WORK_FLOOR + WORK_PER_LINE * lines);
    let occ = Occurrences::new(a, ids);
    budget.spend(a.len() + ids)?;
    let mut ca = vec![false; a.len()];
    let mut cb = vec![false; b.len()];
    let whole = Region {
        a0: 0,
        a1: a.len(),
        b0: 0,
        b1: b.len(),
    };
    let exact = WORK_FLOOR + EXACT_PER_LINE * lines;
    let mut trial = Budget::new(exact);
    let done = histogram(a, b, &occ, whole, &mut ca, &mut cb, &mut trial).is_ok();
    budget.left = budget.left.saturating_sub(exact - trial.left);
    if !done {
        ca.fill(false);
        cb.fill(false);
        anchored(a, b, &occ, ids, &mut ca, &mut cb, &mut budget)?;
    }
    compact(a, &mut ca, &cb, &mut budget)?;
    compact(b, &mut cb, &ca, &mut budget)?;
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if (i < a.len() && ca[i]) || (j < b.len() && cb[j]) {
            let (si, sj) = (i, j);
            while i < a.len() && ca[i] {
                i += 1;
            }
            while j < b.len() && cb[j] {
                j += 1;
            }
            out.push(Hunk {
                b: si,
                bn: i - si,
                s: sj,
                sn: j - sj,
            });
        } else {
            i += 1;
            j += 1;
        }
    }
    Ok(out)
}

/// Diffs `a` against `b` by first matching the lines that occur exactly once
/// in each file (the longest run of them in the same order, as patience diff
/// does), then running the histogram diff in the gaps between those anchors.
/// Each gap is diffed on its own, so the cost follows the gaps' sizes, not the
/// number of hunks times the file size.
fn anchored(
    a: &[u32],
    b: &[u32],
    occ_a: &Occurrences,
    ids: usize,
    ca: &mut [bool],
    cb: &mut [bool],
    budget: &mut Budget,
) -> Result<(), TooComplex> {
    let occ_b = Occurrences::new(b, ids);
    budget.spend(2 * b.len() + ids)?;
    // (position in a, position in b) of each unique line, in b's order.
    let pairs: Vec<(usize, usize)> = b
        .iter()
        .enumerate()
        .filter_map(|(j, &id)| {
            let (x, y) = (occ_a.all(id), occ_b.all(id));
            (x.len() == 1 && y.len() == 1).then(|| (x[0], j))
        })
        .collect();
    let anchors = increasing(&pairs, budget)?;
    let (mut a0, mut b0) = (0, 0);
    for &(a1, b1) in anchors.iter().chain(&[(a.len(), b.len())]) {
        let gap = Region { a0, a1, b0, b1 };
        histogram(a, b, occ_a, gap, ca, cb, budget)?;
        (a0, b0) = (a1 + 1, b1 + 1);
    }
    Ok(())
}

/// The longest subsequence of `pairs` (sorted by their second field) whose
/// first fields increase, by patience sorting.
fn increasing(
    pairs: &[(usize, usize)],
    budget: &mut Budget,
) -> Result<Vec<(usize, usize)>, TooComplex> {
    budget.spend(pairs.len() * (1 + pairs.len().max(1).ilog2() as usize))?;
    // tails[l] ends the best increasing run of length l + 1 found so far.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; pairs.len()];
    for (k, &(i, _)) in pairs.iter().enumerate() {
        let len = tails.partition_point(|&t| pairs[t].0 < i);
        if len > 0 {
            prev[k] = tails[len - 1];
        }
        if len == tails.len() {
            tails.push(k);
        } else {
            tails[len] = k;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut k = tails.last().copied().unwrap_or(usize::MAX);
    while k != usize::MAX {
        out.push(pairs[k]);
        k = prev[k];
    }
    out.reverse();
    Ok(out)
}

/// git's histogram diff (`xhistogram.c`) of region `root`, marking changed
/// lines in `ca` and `cb`. Regions are kept on an explicit stack so deep
/// inputs cannot overflow the call stack; the order regions are processed in
/// does not change the marks.
fn histogram(
    a: &[u32],
    b: &[u32],
    occ: &Occurrences,
    root: Region,
    ca: &mut [bool],
    cb: &mut [bool],
    budget: &mut Budget,
) -> Result<(), TooComplex> {
    let mut stack = vec![root];
    while let Some(r) = stack.pop() {
        budget.spend(1)?;
        if r.a0 == r.a1 {
            cb[r.b0..r.b1].fill(true);
            continue;
        }
        if r.b0 == r.b1 {
            ca[r.a0..r.a1].fill(true);
            continue;
        }
        match find_lcs(a, b, occ, r, budget)? {
            Lcs::Fallback => myers(a, b, r, ca, cb, budget)?,
            Lcs::None => {
                ca[r.a0..r.a1].fill(true);
                cb[r.b0..r.b1].fill(true);
            }
            Lcs::Found(l) => {
                stack.push(Region {
                    a0: l.a1,
                    a1: r.a1,
                    b0: l.b1,
                    b1: r.b1,
                });
                stack.push(Region {
                    a0: r.a0,
                    a1: l.a0,
                    b0: r.b0,
                    b1: l.b0,
                });
            }
        }
    }
    Ok(())
}

enum Lcs {
    Found(Region),
    None,
    Fallback,
}

/// Finds the longest common run of lines with the lowest occurrence count, as
/// `find_lcs`/`try_lcs` in `xhistogram.c` (half-open ranges here). The
/// occurrences of a line in `a[r]` come from `occ`, in order, so the chain
/// git walks is a slice here.
fn find_lcs(
    a: &[u32],
    b: &[u32],
    occ: &Occurrences,
    r: Region,
    budget: &mut Budget,
) -> Result<Lcs, TooComplex> {
    let count = |i: usize| occ.within(a[i], r.a0, r.a1).len();
    let mut best: Option<Region> = None;
    let mut best_cnt = MAX_CHAIN + 1;
    let mut has_common = false;
    let mut bp = r.b0;
    while bp < r.b1 {
        budget.spend(1)?;
        let mut b_next = bp + 1;
        let chain = occ.within(b[bp], r.a0, r.a1);
        let cnt = chain.len();
        if cnt > 0 {
            has_common = true;
        }
        if cnt > 0 && cnt <= best_cnt {
            let mut at = 0;
            while at < cnt {
                budget.spend(1)?;
                let as_ = chain[at];
                let (mut s1, mut s2, mut e1, mut e2) = (as_, bp, as_ + 1, bp + 1);
                let mut rc = cnt;
                while r.a0 < s1 && r.b0 < s2 && a[s1 - 1] == b[s2 - 1] {
                    budget.spend(1)?;
                    s1 -= 1;
                    s2 -= 1;
                    if rc > 1 {
                        rc = rc.min(count(s1));
                    }
                }
                while e1 < r.a1 && e2 < r.b1 && a[e1] == b[e2] {
                    budget.spend(1)?;
                    if rc > 1 {
                        rc = rc.min(count(e1));
                    }
                    e1 += 1;
                    e2 += 1;
                }
                if b_next < e2 {
                    b_next = e2;
                }
                let best_len = best.map_or(0, |l| l.a1 - l.a0);
                if best_len < e1 - s1 || rc < best_cnt {
                    best = Some(Region {
                        a0: s1,
                        a1: e1,
                        b0: s2,
                        b1: e2,
                    });
                    best_cnt = rc;
                }
                // The next occurrence past the end of this run.
                at += 1;
                while at < cnt && chain[at] < e1 {
                    at += 1;
                }
            }
        }
        bp = b_next;
    }
    if has_common && best_cnt > MAX_CHAIN {
        return Ok(Lcs::Fallback);
    }
    Ok(match best {
        Some(l) => Lcs::Found(l),
        None => Lcs::None,
    })
}

/// The furthest-reaching x on each diagonal `k` (`x - y`) of a Myers search.
struct Diagonals {
    offset: isize,
    v: Vec<usize>,
}

impl Diagonals {
    fn new(max_d: usize) -> Self {
        Self {
            offset: max_d as isize,
            v: vec![0; 2 * max_d + 1],
        }
    }

    fn get(&self, k: isize) -> usize {
        self.v[(k + self.offset) as usize]
    }

    fn set(&mut self, k: isize, x: usize) {
        self.v[(k + self.offset) as usize] = x;
    }
}

/// A Myers diff of one region in linear space (the middle-snake divide and
/// conquer of Myers 1986, section 4b), for regions whose lines all repeat too
/// often for the histogram diff. Memory is O(n + m); time is O((n + m) D) for
/// D edits, charged to the budget.
fn myers(
    a: &[u32],
    b: &[u32],
    root: Region,
    ca: &mut [bool],
    cb: &mut [bool],
    budget: &mut Budget,
) -> Result<(), TooComplex> {
    let max_d = (root.a1 - root.a0 + root.b1 - root.b0).div_ceil(2) + 1;
    let mut vf = Diagonals::new(max_d);
    let mut vb = Diagonals::new(max_d);
    budget.spend(2 * max_d)?;
    let mut stack = vec![root];
    while let Some(mut r) = stack.pop() {
        let mut steps = 1;
        while r.a0 < r.a1 && r.b0 < r.b1 && a[r.a0] == b[r.b0] {
            r.a0 += 1;
            r.b0 += 1;
            steps += 1;
        }
        while r.a0 < r.a1 && r.b0 < r.b1 && a[r.a1 - 1] == b[r.b1 - 1] {
            r.a1 -= 1;
            r.b1 -= 1;
            steps += 1;
        }
        budget.spend(steps)?;
        if r.a0 == r.a1 {
            cb[r.b0..r.b1].fill(true);
            continue;
        }
        if r.b0 == r.b1 {
            ca[r.a0..r.a1].fill(true);
            continue;
        }
        match middle_snake(a, b, r, &mut vf, &mut vb, budget)? {
            Some((x, y)) => {
                stack.push(Region {
                    a0: x,
                    a1: r.a1,
                    b0: y,
                    b1: r.b1,
                });
                stack.push(Region {
                    a0: r.a0,
                    a1: x,
                    b0: r.b0,
                    b1: y,
                });
            }
            None => {
                ca[r.a0..r.a1].fill(true);
                cb[r.b0..r.b1].fill(true);
            }
        }
    }
    Ok(())
}

/// Finds a point on an optimal edit path through `r`, where it divides, by
/// running the Myers search from both ends until they meet. Both ends of `r`
/// differ (the caller trims common lines first).
fn middle_snake(
    a: &[u32],
    b: &[u32],
    r: Region,
    vf: &mut Diagonals,
    vb: &mut Diagonals,
    budget: &mut Budget,
) -> Result<Option<(usize, usize)>, TooComplex> {
    let (n, m) = (r.a1 - r.a0, r.b1 - r.b0);
    let delta = n as isize - m as isize;
    let odd = delta & 1 == 1;
    vf.set(1, 0);
    vb.set(1, 0);
    let d_max = ((n + m).div_ceil(2) + 1) as isize;
    for d in 0..d_max {
        let mut steps = 2 * (d as usize + 1);
        // Forward, from (0, 0).
        let mut k = d;
        while k >= -d {
            let mut x = if k == -d || (k != d && vf.get(k - 1) < vf.get(k + 1)) {
                vf.get(k + 1)
            } else {
                vf.get(k - 1) + 1
            };
            let (x0, y0) = (x, (x as isize - k) as usize);
            let mut y = y0;
            while x < n && y < m && a[r.a0 + x] == b[r.b0 + y] {
                x += 1;
                y += 1;
                steps += 1;
            }
            vf.set(k, x);
            if odd && (k - delta).abs() < d && x + vb.get(delta - k) >= n {
                budget.spend(steps)?;
                return Ok(Some((r.a0 + x0, r.b0 + y0)));
            }
            k -= 2;
        }
        // Backward, from (n, m), with x and y counted from the end.
        let mut k = d;
        while k >= -d {
            let mut x = if k == -d || (k != d && vb.get(k - 1) < vb.get(k + 1)) {
                vb.get(k + 1)
            } else {
                vb.get(k - 1) + 1
            };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[r.a1 - x - 1] == b[r.b1 - y - 1] {
                x += 1;
                y += 1;
                steps += 1;
            }
            vb.set(k, x);
            if !odd && (k - delta).abs() <= d && x + vf.get(delta - k) >= n {
                budget.spend(steps)?;
                return Ok(Some((r.a1 - x, r.b1 - y)));
            }
            k -= 2;
        }
        budget.spend(steps)?;
    }
    Ok(None)
}

/// Slides each group of changed lines in `file` as xdiff's
/// `xdl_change_compact` does without the indent heuristic: as far down as it
/// goes, then back up to line up with a change in the other file if one was
/// passed on the way.
fn compact(
    file: &[u32],
    chg: &mut [bool],
    other: &[bool],
    budget: &mut Budget,
) -> Result<(), TooComplex> {
    let n = file.len();
    let mut g = Group::first(chg);
    let mut go = Group::first(other);
    loop {
        if g.end != g.start {
            loop {
                let size = g.end - g.start;
                let mut end_matching_other = None;
                while g.slide_up(file, chg) {
                    go.previous(other);
                    charge(budget, &mut g, &mut go)?;
                }
                let earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = Some(g.end);
                }
                while g.slide_down(file, chg) {
                    go.next(other);
                    charge(budget, &mut g, &mut go)?;
                    if go.end > go.start {
                        end_matching_other = Some(g.end);
                    }
                }
                if size == g.end - g.start {
                    if g.end != earliest_end && end_matching_other.is_some() {
                        while go.end == go.start {
                            g.slide_up(file, chg);
                            go.previous(other);
                            charge(budget, &mut g, &mut go)?;
                        }
                    }
                    break;
                }
                charge(budget, &mut g, &mut go)?;
            }
        }
        if g.end == n {
            break;
        }
        g.next(chg);
        go.next(other);
        charge(budget, &mut g, &mut go)?;
    }
    Ok(())
}

/// Charges the lines the two groups have stepped over since the last charge.
fn charge(budget: &mut Budget, g: &mut Group, go: &mut Group) -> Result<(), TooComplex> {
    budget.spend(std::mem::take(&mut g.work) + std::mem::take(&mut go.work))
}

/// A maximal run `[start, end)` of changed lines (possibly empty), and the
/// lines stepped over since its work was last charged.
struct Group {
    start: usize,
    end: usize,
    work: usize,
}

impl Group {
    fn first(chg: &[bool]) -> Self {
        let mut end = 0;
        while end < chg.len() && chg[end] {
            end += 1;
        }
        Self {
            start: 0,
            end,
            work: end + 1,
        }
    }

    fn next(&mut self, chg: &[bool]) {
        self.start = self.end + 1;
        self.end = self.start;
        while self.end < chg.len() && chg[self.end] {
            self.end += 1;
        }
        self.work += 1 + self.end - self.start;
    }

    fn previous(&mut self, chg: &[bool]) {
        self.end = self.start - 1;
        self.start = self.end;
        while self.start > 0 && chg[self.start - 1] {
            self.start -= 1;
        }
        self.work += 1 + self.end - self.start;
    }

    fn slide_down(&mut self, file: &[u32], chg: &mut [bool]) -> bool {
        self.work += 1;
        if self.end < file.len() && file[self.start] == file[self.end] {
            chg[self.start] = false;
            chg[self.end] = true;
            self.start += 1;
            self.end += 1;
            while self.end < chg.len() && chg[self.end] {
                self.end += 1;
                self.work += 1;
            }
            true
        } else {
            false
        }
    }

    fn slide_up(&mut self, file: &[u32], chg: &mut [bool]) -> bool {
        self.work += 1;
        if self.start > 0 && file[self.start - 1] == file[self.end - 1] {
            self.start -= 1;
            self.end -= 1;
            chg[self.start] = true;
            chg[self.end] = false;
            while self.start > 0 && chg[self.start - 1] {
                self.start -= 1;
                self.work += 1;
            }
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// splitmix64, so the cases are stable.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) % n
        }

        fn file(&mut self, alphabet: u64, max_len: u64) -> Vec<u32> {
            let len = self.below(max_len + 1);
            (0..len).map(|_| self.below(alphabet) as u32).collect()
        }
    }

    fn lcs_len(a: &[u32], b: &[u32]) -> usize {
        let mut row = vec![0; b.len() + 1];
        for &x in a {
            let mut diag = 0;
            for (j, &y) in b.iter().enumerate() {
                let up = row[j + 1];
                row[j + 1] = if x == y { diag + 1 } else { up.max(row[j]) };
                diag = up;
            }
        }
        row[b.len()]
    }

    /// The unmarked lines of `a` and `b` must be the same sequence.
    fn assert_alignment(a: &[u32], b: &[u32], ca: &[bool], cb: &[bool]) -> usize {
        let kept_a: Vec<u32> = a
            .iter()
            .zip(ca)
            .filter(|(_, c)| !**c)
            .map(|(x, _)| *x)
            .collect();
        let kept_b: Vec<u32> = b
            .iter()
            .zip(cb)
            .filter(|(_, c)| !**c)
            .map(|(x, _)| *x)
            .collect();
        assert_eq!(kept_a, kept_b, "a {a:?} b {b:?}");
        kept_a.len()
    }

    #[test]
    fn linear_space_myers_is_minimal() {
        let mut rng = Rng(7);
        for _ in 0..3000 {
            let alphabet = 1 + rng.below(4);
            let a = rng.file(alphabet, 14);
            let b = rng.file(alphabet, 14);
            let (mut ca, mut cb) = (vec![false; a.len()], vec![false; b.len()]);
            let whole = Region {
                a0: 0,
                a1: a.len(),
                b0: 0,
                b1: b.len(),
            };
            let mut budget = Budget::new(u64::MAX);
            myers(&a, &b, whole, &mut ca, &mut cb, &mut budget).unwrap();
            assert_eq!(assert_alignment(&a, &b, &ca, &cb), lcs_len(&a, &b));
        }
    }

    #[test]
    fn anchored_diff_is_an_alignment() {
        let mut rng = Rng(11);
        for _ in 0..2000 {
            let alphabet = 1 + rng.below(30);
            let a = rng.file(alphabet, 40);
            let b = rng.file(alphabet, 40);
            let (mut ca, mut cb) = (vec![false; a.len()], vec![false; b.len()]);
            let ids = alphabet as usize;
            let occ = Occurrences::new(&a, ids);
            let mut budget = Budget::new(u64::MAX);
            anchored(&a, &b, &occ, ids, &mut ca, &mut cb, &mut budget).unwrap();
            assert_alignment(&a, &b, &ca, &cb);
        }
    }

    #[test]
    fn the_budget_bounds_a_diff() {
        // Every line repeats: the Myers fallback, with D of about n.
        let a: Vec<u32> = vec![0; 20_000];
        let b: Vec<u32> = (0..20_000).map(|i| u32::from(i % 2 == 0)).collect();
        assert!(hunks(&a, &b, 2).is_err());
        // Small inputs always fit the floor.
        assert!(hunks(&a[..500], &b[..500], 2).is_ok());
    }
}
