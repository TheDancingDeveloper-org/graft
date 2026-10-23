//! Text for editors: short ids, times, author kinds, and the line ranges a
//! unified patch touches.

use graft_model::{AuthorKind, ChangesetId, Timestamp};

/// The short form of a changeset id: the kind letter and 12 hex digits,
/// as the CLI prints it.
pub(crate) fn short(id: ChangesetId) -> String {
    id.to_string()[..13].to_owned()
}

/// The name of an author kind.
pub(crate) fn kind_name(kind: AuthorKind) -> &'static str {
    match kind {
        AuthorKind::Human => "human",
        AuthorKind::Agent => "agent",
        AuthorKind::Bot => "bot",
        AuthorKind::Ci => "ci",
    }
}

/// Parses an author kind name.
pub(crate) fn parse_kind(text: &str) -> Option<AuthorKind> {
    Some(match text {
        "human" => AuthorKind::Human,
        "agent" => AuthorKind::Agent,
        "bot" => AuthorKind::Bot,
        "ci" => AuthorKind::Ci,
        _ => return None,
    })
}

/// `YYYY-MM-DD HH:MM` in the author's offset, with the offset.
pub(crate) fn time(t: Timestamp) -> String {
    let local_ms = t.ms + i64::from(t.offset_min) * 60_000;
    let secs = local_ms.div_euclid(1000);
    let (days, day_secs) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let offset = if t.offset_min == 0 {
        "UTC".to_owned()
    } else {
        let sign = if t.offset_min < 0 { '-' } else { '+' };
        let abs = t.offset_min.unsigned_abs();
        format!("{sign}{:02}{:02}", abs / 60, abs % 60)
    };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} {offset}",
        day_secs / 3600,
        day_secs % 3600 / 60
    )
}

/// The proleptic Gregorian date of a day count from 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The old-side lines each hunk of a unified patch replaces, as 0-based
/// half-open ranges. A pure insertion is the one line it follows (or line
/// 0), so it still has somewhere to show.
pub(crate) fn old_ranges(patch: &str) -> Vec<(u32, u32)> {
    patch
        .lines()
        .filter_map(|line| {
            let old = line.strip_prefix("@@ -")?.split(' ').next()?;
            let (start, count) = match old.split_once(',') {
                Some((s, c)) => (s.parse::<u32>().ok()?, c.parse::<u32>().ok()?),
                None => (old.parse::<u32>().ok()?, 1),
            };
            Some(if count == 0 {
                let at = start.saturating_sub(1);
                (at, at + 1)
            } else {
                let at = start.saturating_sub(1);
                (at, at + count)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_render_in_the_authors_offset() {
        let t = |ms, offset_min| time(Timestamp { ms, offset_min });
        assert_eq!(t(0, 0), "1970-01-01 00:00 UTC");
        assert_eq!(t(1_700_000_000_000, 0), "2023-11-14 22:13 UTC");
        assert_eq!(t(1_700_000_000_000, 600), "2023-11-15 08:13 +1000");
        assert_eq!(t(1_700_000_000_000, -90), "2023-11-14 20:43 -0130");
        assert_eq!(t(951_782_400_000, 0), "2000-02-29 00:00 UTC");
        assert_eq!(t(-1, 0), "1969-12-31 23:59 UTC");
    }

    #[test]
    fn hunks_give_old_side_ranges() {
        let patch = "@@ -3,2 +3,4 @@\n-a\n-b\n+c\n@@ -10 +12 @@\n-x\n+y\n@@ -0,0 +1,3 @@\n+n\n@@ -7,0 +9,1 @@\n+z\n";
        assert_eq!(old_ranges(patch), vec![(2, 4), (9, 10), (0, 1), (6, 7)]);
        assert_eq!(
            old_ranges("not a patch\n@@ -x +y @@"),
            Vec::<(u32, u32)>::new()
        );
    }
}
