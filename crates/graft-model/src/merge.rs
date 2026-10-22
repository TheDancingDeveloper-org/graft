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
//! Cost is linear in the size of the three inputs plus the diff work, and the
//! inputs are capped at [`MAX_TEXT_BYTES`] each by text detection.

use std::collections::HashMap;

/// The largest input, in bytes, that is merged line by line (decision D16).
/// An input of this size or larger is [`Merge3::NotText`].
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// Lines repeated more often than this in a region make the histogram diff
/// fall back to a plain Myers diff, as git's `xhistogram` does.
const MAX_CHAIN: usize = 64;

/// The outcome of a three-way merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge3 {
    /// Merged cleanly.
    Clean(Vec<u8>),
    /// Conflicting regions, as line ranges in `ours`.
    Conflicts(Vec<ConflictRegion>),
    /// One of the inputs is not text (binary, too large, not UTF-8).
    NotText,
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
    let merges = input.merges();
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
/// text there is nothing to merge line by line, and `ours` is returned.
pub fn render_conflict_markers(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    if !is_text(base) || !is_text(ours) || !is_text(theirs) {
        return ours.to_vec();
    }
    let input = Input::new(base, ours, theirs);
    input.render(&input.merges())
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
        Self { base, ours, theirs }
    }

    fn merges(&self) -> Vec<MergeHunk> {
        let h1 = hunks(&self.base.ids, &self.ours.ids);
        let h2 = hunks(&self.base.ids, &self.theirs.ids);
        combine(
            &h1,
            &h2,
            &self.ours.ids,
            &self.theirs.ids,
            self.base.ids.len(),
        )
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

/// Diffs `a` against `b` and returns the hunks, in order.
fn hunks(a: &[u32], b: &[u32]) -> Vec<Hunk> {
    let mut ca = vec![false; a.len()];
    let mut cb = vec![false; b.len()];
    histogram(a, b, &mut ca, &mut cb);
    compact(a, &mut ca, &cb);
    compact(b, &mut cb, &ca);
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
    out
}

/// A region still to diff: `a[a0..a1]` against `b[b0..b1]`.
#[derive(Clone, Copy)]
struct Region {
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
}

/// git's histogram diff (`xhistogram.c`), marking changed lines in `ca` and
/// `cb`. Regions are kept on an explicit stack so deep inputs cannot overflow
/// the call stack; the order regions are processed in does not change the
/// marks.
fn histogram(a: &[u32], b: &[u32], ca: &mut [bool], cb: &mut [bool]) {
    let mut stack = vec![Region {
        a0: 0,
        a1: a.len(),
        b0: 0,
        b1: b.len(),
    }];
    while let Some(r) = stack.pop() {
        if r.a0 == r.a1 {
            cb[r.b0..r.b1].fill(true);
            continue;
        }
        if r.b0 == r.b1 {
            ca[r.a0..r.a1].fill(true);
            continue;
        }
        match find_lcs(a, b, r) {
            Lcs::Fallback => myers(a, b, r, ca, cb),
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
}

enum Lcs {
    Found(Region),
    None,
    Fallback,
}

/// Finds the longest common run of lines with the lowest occurrence count, as
/// `find_lcs`/`try_lcs` in `xhistogram.c` (half-open ranges here).
fn find_lcs(a: &[u32], b: &[u32], r: Region) -> Lcs {
    // Occurrences of each line in `a[r]`: first position and count, and a
    // chain from each position to the next occurrence of the same line.
    let mut records: HashMap<u32, (usize, usize)> = HashMap::new();
    let mut next = vec![usize::MAX; r.a1 - r.a0];
    for i in (r.a0..r.a1).rev() {
        let rec = records.entry(a[i]).or_insert((usize::MAX, 0));
        next[i - r.a0] = rec.0;
        rec.0 = i;
        rec.1 += 1;
    }
    let count = |i: usize| records[&a[i]].1;

    let mut best: Option<Region> = None;
    let mut best_cnt = MAX_CHAIN + 1;
    let mut has_common = false;
    let mut bp = r.b0;
    while bp < r.b1 {
        let mut b_next = bp + 1;
        if let Some(&(first, cnt)) = records.get(&b[bp]) {
            has_common = true;
            if cnt <= best_cnt {
                let mut as_ = first;
                loop {
                    let np = next[as_ - r.a0];
                    let (mut s1, mut s2, mut e1, mut e2) = (as_, bp, as_ + 1, bp + 1);
                    let mut rc = cnt;
                    while r.a0 < s1 && r.b0 < s2 && a[s1 - 1] == b[s2 - 1] {
                        s1 -= 1;
                        s2 -= 1;
                        if rc > 1 {
                            rc = rc.min(count(s1));
                        }
                    }
                    while e1 < r.a1 && e2 < r.b1 && a[e1] == b[e2] {
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
                    let mut np = np;
                    while np != usize::MAX && np < e1 {
                        np = next[np - r.a0];
                    }
                    if np == usize::MAX {
                        break;
                    }
                    as_ = np;
                }
            }
        }
        bp = b_next;
    }
    if has_common && best_cnt > MAX_CHAIN {
        return Lcs::Fallback;
    }
    match best {
        Some(l) => Lcs::Found(l),
        None => Lcs::None,
    }
}

/// A plain Myers diff of one region, for regions whose lines all repeat too
/// often for the histogram diff.
fn myers(a: &[u32], b: &[u32], r: Region, ca: &mut [bool], cb: &mut [bool]) {
    let (n, m) = (r.a1 - r.a0, r.b1 - r.b0);
    let max = n + m;
    let off = max as isize + 1;
    let mut v = vec![0usize; 2 * max + 3];
    let mut trace: Vec<Vec<usize>> = Vec::new();
    'outer: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let ki = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[ki - 1] < v[ki + 1]) {
                v[ki + 1]
            } else {
                v[ki - 1] + 1
            };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[r.a0 + x] == b[r.b0 + y] {
                x += 1;
                y += 1;
            }
            v[ki] = x;
            if x >= n && y >= m {
                break 'outer;
            }
            k += 2;
        }
    }
    // Walk back through the saved frontiers, marking the edits.
    let (mut x, mut y) = (n as isize, m as isize);
    for d in (1..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let ki = (k + off) as usize;
        let prev_k = if k == -d || (k != d && v[ki - 1] < v[ki + 1]) {
            k + 1
        } else {
            k - 1
        };
        let px = v[(prev_k + off) as usize] as isize;
        let py = px - prev_k;
        while x > px && y > py {
            x -= 1;
            y -= 1;
        }
        if x == px {
            // Came down: an insertion of b[py].
            cb[r.b0 + py as usize] = true;
        } else {
            ca[r.a0 + px as usize] = true;
        }
        x = px;
        y = py;
    }
}

/// Slides each group of changed lines in `file` as xdiff's
/// `xdl_change_compact` does without the indent heuristic: as far down as it
/// goes, then back up to line up with a change in the other file if one was
/// passed on the way.
fn compact(file: &[u32], chg: &mut [bool], other: &[bool]) {
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
                }
                let earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = Some(g.end);
                }
                while g.slide_down(file, chg) {
                    go.next(other);
                    if go.end > go.start {
                        end_matching_other = Some(g.end);
                    }
                }
                if size == g.end - g.start {
                    if g.end != earliest_end && end_matching_other.is_some() {
                        while go.end == go.start {
                            g.slide_up(file, chg);
                            go.previous(other);
                        }
                    }
                    break;
                }
            }
        }
        if g.end == n {
            break;
        }
        g.next(chg);
        go.next(other);
    }
}

/// A maximal run `[start, end)` of changed lines (possibly empty).
struct Group {
    start: usize,
    end: usize,
}

impl Group {
    fn first(chg: &[bool]) -> Self {
        let mut end = 0;
        while end < chg.len() && chg[end] {
            end += 1;
        }
        Self { start: 0, end }
    }

    fn next(&mut self, chg: &[bool]) {
        self.start = self.end + 1;
        self.end = self.start;
        while self.end < chg.len() && chg[self.end] {
            self.end += 1;
        }
    }

    fn previous(&mut self, chg: &[bool]) {
        self.end = self.start - 1;
        self.start = self.end;
        while self.start > 0 && chg[self.start - 1] {
            self.start -= 1;
        }
    }

    fn slide_down(&mut self, file: &[u32], chg: &mut [bool]) -> bool {
        if self.end < file.len() && file[self.start] == file[self.end] {
            chg[self.start] = false;
            chg[self.end] = true;
            self.start += 1;
            self.end += 1;
            while self.end < chg.len() && chg[self.end] {
                self.end += 1;
            }
            true
        } else {
            false
        }
    }

    fn slide_up(&mut self, file: &[u32], chg: &mut [bool]) -> bool {
        if self.start > 0 && file[self.start - 1] == file[self.end - 1] {
            self.start -= 1;
            self.end -= 1;
            chg[self.start] = true;
            chg[self.end] = false;
            while self.start > 0 && chg[self.start - 1] {
                self.start -= 1;
            }
            true
        } else {
            false
        }
    }
}
