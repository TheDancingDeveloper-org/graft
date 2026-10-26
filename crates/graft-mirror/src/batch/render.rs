//! PR bodies, commit messages and per-land commit objects (D26, D28).

use std::collections::HashSet;

use graft_git::GitSha;
use graft_git::export::{Committer, commit_object};
use graft_git::map::ImportedCommit;
use graft_model::ChangesetId;

use crate::text::clean_text;

use super::types::{
    BATCH_TRAILER, BRANCH_PREFIX, CHANGESET_TRAILER, Dropped, FIELD_CHARS, LandLine, MAX_CHECKS,
    MAX_DROPS_LISTED, MAX_LANDS, MAX_PR_BODY, git_time_at, short,
};

/// What a batch's PR body and commit message say before the land list.
pub(crate) struct BatchHeader {
    pub(crate) number: u64,
    pub(crate) line: String,
    pub(crate) branch: String,
    pub(crate) base: ChangesetId,
    pub(crate) base_sha: GitSha,
    pub(crate) start: ChangesetId,
    pub(crate) tip: ChangesetId,
    /// The land count stopped at [`MAX_LANDS`].
    pub(crate) capped: bool,
    /// The attestation read stopped at its cap.
    pub(crate) truncated: bool,
    /// Lands dropped from the batch (each reverted in it), oldest first.
    pub(crate) drops: Vec<Dropped>,
    /// Newer lands left for the next batch by the cut.
    pub(crate) held: usize,
    /// Why a configured cap was not applied.
    pub(crate) uncut: Option<String>,
    /// The cut's land cap.
    pub(crate) max_lands: usize,
    /// Why the batch is one squashed commit instead of one per land.
    pub(crate) fallback: Option<String>,
    /// Lands added after the PR opened.
    pub(crate) appended: HashSet<ChangesetId>,
}

/// How many of `lands` (newest first) a batch capped at `max` takes, oldest
/// first: `max`, or more so no dropped land is split from its revert.
pub(crate) fn keep_with_reverts(lands: &[LandLine], max: usize) -> usize {
    let mut keep = max.min(lands.len());
    loop {
        let first_kept = lands.len() - keep;
        let kept: HashSet<ChangesetId> = lands[first_kept..].iter().map(|l| l.id).collect();
        // The oldest revert, among the held lands, of a land in the batch.
        let Some(i) = lands[..first_kept]
            .iter()
            .rposition(|l| l.reverts.is_some_and(|r| kept.contains(&r)))
        else {
            return keep;
        };
        keep = lands.len() - i;
    }
}

/// A land's latest check results, bounded: `none`, or `<check> passed`
/// and `<check> failed`, comma-separated.
fn checks_text(land: &LandLine) -> String {
    let mut checks: Vec<String> = land
        .checks
        .iter()
        .take(MAX_CHECKS)
        .map(|(c, ok)| {
            let c = clean_text(c, FIELD_CHARS, false);
            format!("{c} {}", if *ok { "passed" } else { "failed" })
        })
        .collect();
    if land.checks.len() > MAX_CHECKS {
        checks.push(format!("+{} more", land.checks.len() - MAX_CHECKS));
    }
    if checks.is_empty() {
        "none".to_owned()
    } else {
        checks.join(", ")
    }
}

/// One table row for a land. Every cell is bounded, so a row is too.
pub(crate) fn land_row(land: &LandLine, appended: bool) -> String {
    let cell = |s: &str| clean_text(s, FIELD_CHARS, false).replace('|', "\\|");
    let checks = checks_text(land);
    let appended = if appended { " (appended)" } else { "" };
    format!(
        "| `{}{appended}` | {} | {} | {} | {} | {} |\n",
        short(land.id),
        cell(&land.author),
        cell(land.session.as_deref().unwrap_or("-")),
        cell(land.work_item.as_deref().unwrap_or("-")),
        checks.replace('|', "\\|"),
        cell(&land.subject),
    )
}

/// A land's per-land commit message (D26): its own message (or a stand-in
/// when it has none), then the `Graft-*` trailers.
pub(crate) fn land_message(number: u64, land: &LandLine) -> String {
    let text = land.message.trim();
    let mut m = if text.is_empty() {
        format!("graft land {}", short(land.id))
    } else {
        text.to_owned()
    };
    m.push_str(&format!("\n\n{CHANGESET_TRAILER} {}\n", land.id));
    if let Some(s) = &land.trailer_session {
        m.push_str(&format!("Graft-Session: {s}\n"));
    }
    if let Some(w) = &land.trailer_work_item {
        m.push_str(&format!("Graft-Work-Item: {w}\n"));
    }
    if let Some(model) = &land.trailer_model {
        m.push_str(&format!("Graft-Model: {model}\n"));
    }
    for co in &land.trailer_coauthors {
        m.push_str(&format!("Co-authored-by: {co}\n"));
    }
    if let Some(claimed) = &land.trailer_claimed_author {
        m.push_str(claimed);
        m.push('\n');
    }
    if let Some(f) = &land.trailer_merged_from {
        m.push_str(&format!("Graft-Merged-From: {f}\n"));
    }
    // No `Graft-Checks` trailer: phase 1 consumes no checks (D22). Check
    // results in commits are phase-2 surface (D25), which starts only with
    // the graft-primary authority switch.
    m.push_str(&format!("{BATCH_TRAILER} {number}\n"));
    m
}

/// A land's per-land commit object on `parent`: the land's author and
/// author time, `committer` at the same time, and [`land_message`]. Built
/// from recorded fields only, so the same land on the same parent gives the
/// same sha.
pub(crate) fn land_commit(
    number: u64,
    land: &LandLine,
    tree: GitSha,
    parent: GitSha,
    committer: &Committer,
) -> Vec<u8> {
    let when = git_time_at(land.authored);
    let headers = format!(
        "author {} <{}> {when}\ncommitter {} <{}> {when}\n\n",
        land.ident.0, land.ident.1, committer.name, committer.email
    );
    let record = ImportedCommit {
        sha: GitSha([0; 20]),
        headers: headers.into_bytes(),
        message: None,
        raw_trees: Vec::new(),
    };
    commit_object(tree, &[parent], &record, &land_message(number, land))
}

/// A raw commit's parents, and the land its message names when its last
/// paragraph (the trailers) has exactly one `Graft-Changeset` line. A
/// squash-merge of several lands names more and maps to none; a line in
/// the message body is not a trailer.
pub(crate) fn parse_commit(raw: &[u8]) -> (Vec<GitSha>, Option<ChangesetId>) {
    let text = String::from_utf8_lossy(raw);
    let (head, message) = text.split_once("\n\n").unwrap_or((text.as_ref(), ""));
    let message = message.trim_end().rsplit("\n\n").next().unwrap_or("");
    let parents = head
        .lines()
        .filter_map(|l| l.strip_prefix("parent "))
        .filter_map(GitSha::from_hex)
        .collect();
    let mut named = message
        .lines()
        .filter_map(|l| l.strip_prefix(CHANGESET_TRAILER))
        .map(str::trim);
    let land = match (named.next(), named.next()) {
        (Some(id), None) => ChangesetId::parse(id).ok(),
        _ => None,
    };
    (parents, land)
}

/// The PR body and the squashed commit's message body. Both say what the
/// batch is, list its lands (newest first) and end with the trailers. The
/// commit message lists every land. The PR body lists as many as fit in
/// [`MAX_PR_BODY`] and points to the commit message for the rest.
pub(crate) fn render(h: &BatchHeader, lands: &[LandLine]) -> (String, String) {
    let line = clean_text(&h.line, FIELD_CHARS, false);
    let branch = clean_text(&h.branch, FIELD_CHARS, false);
    let mut head = match &h.fallback {
        None => format!(
            "Batch {} of graft line `{line}`, one commit per land, oldest first, by \
             graft-mirror (decision D26). Merge it with rebase-merge to keep the commits \
             (D28). graft never merges or queues this PR.\n\n",
            h.number
        ),
        Some(why) => format!(
            "Batch {} of graft line `{line}`, squashed into one commit by graft-mirror \
             (the D26 fallback: {}). graft never merges or queues this PR.\n\n",
            h.number,
            clean_text(why, 300, false)
        ),
    };
    head.push_str(&format!(
        "- Base: `{branch}` at {} (graft {})\n",
        h.base_sha, h.base
    ));
    head.push_str(&format!(
        "- Lands: {}{} ({}..{})\n",
        lands.len(),
        if h.capped { "+" } else { "" },
        h.start,
        h.tip
    ));
    if h.capped {
        head.push_str(&format!(
            "- Only the newest {MAX_LANDS} lands are listed; the batch has more\n"
        ));
    }
    if h.truncated {
        head.push_str(
            "- Attestations: the journal read stopped at its cap; the list may be incomplete\n",
        );
    }
    if h.held > 0 {
        head.push_str(&format!(
            "- Cut at {} lands: {} newer {} for the next batch\n",
            h.max_lands,
            h.held,
            if h.held == 1 {
                "land waits"
            } else {
                "lands wait"
            }
        ));
    }
    if let Some(why) = &h.uncut {
        head.push_str(&format!("- Not cut: {why}\n"));
    }
    if !h.drops.is_empty() {
        head.push_str(&format!(
            "- Dropped: {} land{}, each reverted on the line (both are in this batch, so \
             they cancel out)\n",
            h.drops.len(),
            if h.drops.len() == 1 { "" } else { "s" }
        ));
        for d in h.drops.iter().take(MAX_DROPS_LISTED) {
            head.push_str(&format!(
                "  - `{}` reverted by `{}`: {}\n",
                short(d.land),
                short(d.revert),
                clean_text(&d.reason, FIELD_CHARS, false).replace('|', "\\|")
            ));
        }
        if h.drops.len() > MAX_DROPS_LISTED {
            head.push_str(&format!(
                "  - and {} more\n",
                h.drops.len() - MAX_DROPS_LISTED
            ));
        }
    }
    head.push_str("\n| Changeset | Author | Session | Work item | Attestations | Subject |\n");
    head.push_str("| --- | --- | --- | --- | --- | --- |\n");
    let trailers = format!(
        "\n{BATCH_TRAILER} {}\nGraft-Line: {line}\nGraft-Changeset: {}\n",
        h.number, h.tip
    );
    let rows: Vec<String> = lands
        .iter()
        .map(|land| land_row(land, h.appended.contains(&land.id)))
        .collect();

    let mut message = head.clone();
    for r in &rows {
        message.push_str(r);
    }
    message.push_str(&trailers);

    // The note for lands the PR body has no room for, at its longest.
    let note = |n: usize| match h.fallback {
        None => format!(
            "\n{n} more lands are on the branch, one commit each \
             (`git log {}..{BRANCH_PREFIX}{}`).\n",
            h.base_sha, h.number
        ),
        Some(_) => format!(
            "\n{n} more lands are listed in the squashed commit's message \
             (`git log -1 {BRANCH_PREFIX}{}`).\n",
            h.number
        ),
    };
    let reserve = note(lands.len()).chars().count() + trailers.chars().count();
    let mut body = head;
    let mut len = body.chars().count();
    let mut listed = 0;
    for r in &rows {
        let n = r.chars().count();
        if len + n + reserve > MAX_PR_BODY {
            break;
        }
        body.push_str(r);
        len += n;
        listed += 1;
    }
    if listed < rows.len() {
        body.push_str(&note(rows.len() - listed));
    }
    body.push_str(&trailers);
    (body, message)
}
