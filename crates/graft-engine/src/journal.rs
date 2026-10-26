//! Journal entries (`docs/design.md` §4.8) and `Watch`.
//!
//! An entry's body is a JSON object, stored as the UTF-8 bytes watchers
//! receive, so `Watch` never re-encodes. Ids use their text form
//! (`c<hex>`). The bodies are a durable format:
//!
//! - `ChangesetCreated`: `{"id":"c…","state":"draft","workspace":"w"}` for a
//!   draft created in a workspace, `{"id":"c…","state":"draft","holder":"land:…"}`
//!   for a land candidate held by its land-attempt lease,
//!   `{"id":"c…","state":"abandoned"}` for one created without (nothing
//!   holds it)
//! - `RefMoved`: `{"name":"main","old":"c…"|null,"new":"c…","version":2,"actor":"…"}`
//! - `WorkspaceOpened`: `{"name":"w","owner":"…","line":"main","base":"c…"}`
//! - `WorkspaceRenewed`: `{"name":"w","lease_until_ms":123}`
//! - `WorkspaceHeadMoved`: `{"name":"w","old":"c…"|null,"new":"c…"|null}`
//! - `WorkspaceClosed`: `{"name":"w","reason":"closed"|"lease-expired","abandoned":2}`
//! - `WorkspaceSynced`: `{"name":"w","old_base":"c…","new_base":"c…","old_head":"c…"|null,"new_head":"c…"|null}`
//! - `LandAttempt`: `{"draft":"c…","line":"main","outcome":"landed","landed":"c…"}`
//!   in the land's CAS transaction, or `{"draft":"c…","line":"main","outcome":"conflict","onto":"c…"}`
//! - `LineMerged`: `{"src":"develop","dst":"main","src_changeset":"c…","src_pos":4,"dst_pos":9}`
//!   in a merge landing's CAS transaction, after its `RefMoved` (D29)
//! - `Attested`: `{"changeset":"c…","check":"ci/test","passed":true,"url":"…","actor":"…"}`
//!   for a check that passed or failed with no display name (v0.1, frozen);
//!   otherwise extended with `"kind":"check"|"review"|"approval","status":"pass"|"fail"|"pending"|"error","by":"…"|null`,
//!   and `passed` only for a check that passed or failed
//! - `RevertRequested`: `{"line":"main","changeset":"c…","check":"ci/test","url":"…"}`,
//!   with the failing `Attested` entry of a post-land check (decision D6)
//! - `LandReverted`: `{"line":"main","land":"c…","revert":"c…","check":"ci/test"|null}`
//!   in the revert's land transaction, on a line with post-land checks
//!   (`check` is the required check that failed, if one did)

use std::fmt::Write as _;

use graft_model::ChangesetId;
use graft_proto::types::{JournalEntry, JournalPage, Watch};
use graft_proto::{ApiError, CallContext};
use graft_store::{MetaStore, ObjectStore, Write};

use crate::access::{Need, require};
use crate::{Engine, errors};

/// Kind of the entry written with each new changeset.
pub const CHANGESET_CREATED: &str = "ChangesetCreated";

/// Kind of the entry written with each ref move.
pub const REF_MOVED: &str = "RefMoved";

/// Kind of the entry written when a workspace opens.
pub const WORKSPACE_OPENED: &str = "WorkspaceOpened";

/// Kind of the entry written when a workspace lease is renewed.
pub const WORKSPACE_RENEWED: &str = "WorkspaceRenewed";

/// Kind of the entry written when `SetHead` (or a commit that adopts an
/// abandoned changeset) moves a workspace head.
pub const WORKSPACE_HEAD_MOVED: &str = "WorkspaceHeadMoved";

/// Kind of the entry written when a workspace closes or its lease lapses.
pub const WORKSPACE_CLOSED: &str = "WorkspaceClosed";

/// Kind of the entry written when `Sync` moves a workspace's base (and
/// rebases its stack).
pub const WORKSPACE_SYNCED: &str = "WorkspaceSynced";

/// Kind of the entry written for a land's outcome.
pub const LAND_ATTEMPT: &str = "LandAttempt";

/// Kind of the entry written with each attestation.
pub const ATTESTED: &str = "Attested";

/// Kind of the entry that asks for a landed changeset to be reverted.
pub const REVERT_REQUESTED: &str = "RevertRequested";

/// Kind of the entry written when a land reverts a checked landing.
pub const LAND_REVERTED: &str = "LandReverted";

/// A JSON string literal.
pub(crate) fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn entry(kind: &str, body: String) -> Write {
    Write::Journal {
        kind: kind.to_owned(),
        body: body.into_bytes(),
    }
}

fn opt_id(out: &mut String, id: Option<ChangesetId>) {
    match id {
        Some(id) => {
            let _ = write!(out, "\"{id}\"");
        }
        None => out.push_str("null"),
    }
}

/// The `LineMerged` entry of a merge landing (D29): the landing at
/// `dst_pos` on `dst` merged `src_changeset`, at `src_pos` on `src`.
pub(crate) fn line_merged(
    src: &str,
    dst: &str,
    src_changeset: ChangesetId,
    src_pos: u64,
    dst_pos: u64,
) -> Write {
    let mut body = String::from("{\"src\":");
    json_str(&mut body, src);
    body.push_str(",\"dst\":");
    json_str(&mut body, dst);
    let _ = write!(
        body,
        ",\"src_changeset\":\"{src_changeset}\",\"src_pos\":{src_pos},\"dst_pos\":{dst_pos}}}"
    );
    entry(graft_proto::types::LINE_MERGED, body)
}

/// The `ChangesetCreated` entry for a new changeset: a draft held by
/// `workspace`, or abandoned when nothing holds it.
pub(crate) fn changeset_created(id: ChangesetId, workspace: Option<&str>) -> Write {
    let mut body = format!("{{\"id\":\"{id}\",\"state\":");
    match workspace {
        Some(name) => {
            body.push_str("\"draft\",\"workspace\":");
            json_str(&mut body, name);
        }
        None => body.push_str("\"abandoned\""),
    }
    body.push('}');
    entry(CHANGESET_CREATED, body)
}

/// The `ChangesetCreated` entry for a land candidate, a draft held by its
/// land-attempt lease `holder`.
pub(crate) fn candidate_created(id: ChangesetId, holder: &str) -> Write {
    let mut body = format!("{{\"id\":\"{id}\",\"state\":\"draft\",\"holder\":");
    json_str(&mut body, holder);
    body.push('}');
    entry(CHANGESET_CREATED, body)
}

/// What a land attempt came to, as `LandAttempt` records it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LandOutcome {
    /// The line moved to this changeset.
    Landed(ChangesetId),
    /// The stack conflicts with this tip.
    Conflict(ChangesetId),
}

/// The `LandAttempt` entry.
pub(crate) fn land_attempt(draft: ChangesetId, line: &str, outcome: LandOutcome) -> Write {
    let mut body = format!("{{\"draft\":\"{draft}\",\"line\":");
    json_str(&mut body, line);
    match outcome {
        LandOutcome::Landed(id) => {
            let _ = write!(body, ",\"outcome\":\"landed\",\"landed\":\"{id}\"}}");
        }
        LandOutcome::Conflict(id) => {
            let _ = write!(body, ",\"outcome\":\"conflict\",\"onto\":\"{id}\"}}");
        }
    }
    entry(LAND_ATTEMPT, body)
}

/// The `Attested` entry.
///
/// A `check` that passed or failed, with no display name, writes exactly
/// the v0.1 body (`changeset`, `check`, `passed`, `url`, `actor`): that
/// encoding is frozen. Any other attestation writes the extended body,
/// which adds `kind`, `status` and `by`. The extended body carries
/// `passed` only for a `check` that passed or failed. A pending or
/// errored check, a review and an approval leave it out, so a v0.1 reader
/// (which requires `passed`) skips the entry instead of taking it for a
/// failed check.
pub(crate) fn attested(a: &AttestedEntry<'_>) -> Write {
    let verdict = a.kind == "check" && matches!(a.status, "pass" | "fail");
    let mut body = format!("{{\"changeset\":\"{}\",\"check\":", a.changeset);
    json_str(&mut body, a.check);
    if verdict {
        let _ = write!(body, ",\"passed\":{}", a.status == "pass");
    }
    body.push_str(",\"url\":");
    json_str(&mut body, a.url);
    body.push_str(",\"actor\":");
    json_str(&mut body, a.actor);
    if !verdict || a.by.is_some() {
        let _ = write!(
            body,
            ",\"kind\":\"{}\",\"status\":\"{}\",\"by\":",
            a.kind, a.status
        );
        match a.by {
            Some(by) => json_str(&mut body, by),
            None => body.push_str("null"),
        }
    }
    body.push('}');
    entry(ATTESTED, body)
}

/// The fields of an `Attested` entry; `kind` and `status` are their
/// lower-case names.
pub(crate) struct AttestedEntry<'a> {
    pub(crate) changeset: ChangesetId,
    pub(crate) kind: &'a str,
    pub(crate) check: &'a str,
    pub(crate) status: &'a str,
    pub(crate) url: &'a str,
    pub(crate) actor: &'a str,
    pub(crate) by: Option<&'a str>,
}

/// The `LandReverted` entry.
pub(crate) fn land_reverted(
    line: &str,
    land: ChangesetId,
    revert: ChangesetId,
    check: Option<&str>,
) -> Write {
    let mut body = String::from("{\"line\":");
    json_str(&mut body, line);
    let _ = write!(
        body,
        ",\"land\":\"{land}\",\"revert\":\"{revert}\",\"check\":"
    );
    match check {
        Some(check) => json_str(&mut body, check),
        None => body.push_str("null"),
    }
    body.push('}');
    entry(LAND_REVERTED, body)
}

/// The `RevertRequested` entry.
pub(crate) fn revert_requested(
    line: &str,
    changeset: ChangesetId,
    check: &str,
    url: &str,
) -> Write {
    let mut body = String::from("{\"line\":");
    json_str(&mut body, line);
    let _ = write!(body, ",\"changeset\":\"{changeset}\",\"check\":");
    json_str(&mut body, check);
    body.push_str(",\"url\":");
    json_str(&mut body, url);
    body.push('}');
    entry(REVERT_REQUESTED, body)
}

/// The `WorkspaceOpened` entry.
pub(crate) fn workspace_opened(name: &str, owner: &str, line: &str, base: ChangesetId) -> Write {
    let mut body = String::from("{\"name\":");
    json_str(&mut body, name);
    body.push_str(",\"owner\":");
    json_str(&mut body, owner);
    body.push_str(",\"line\":");
    json_str(&mut body, line);
    let _ = write!(body, ",\"base\":\"{base}\"}}");
    entry(WORKSPACE_OPENED, body)
}

/// The `WorkspaceRenewed` entry.
pub(crate) fn workspace_renewed(name: &str, lease_until_ms: i64) -> Write {
    let mut body = String::from("{\"name\":");
    json_str(&mut body, name);
    let _ = write!(body, ",\"lease_until_ms\":{lease_until_ms}}}");
    entry(WORKSPACE_RENEWED, body)
}

/// The `WorkspaceHeadMoved` entry.
pub(crate) fn workspace_head_moved(
    name: &str,
    old: Option<ChangesetId>,
    new: Option<ChangesetId>,
) -> Write {
    let mut body = String::from("{\"name\":");
    json_str(&mut body, name);
    body.push_str(",\"old\":");
    opt_id(&mut body, old);
    body.push_str(",\"new\":");
    opt_id(&mut body, new);
    body.push('}');
    entry(WORKSPACE_HEAD_MOVED, body)
}

/// The `WorkspaceClosed` entry; `reason` is `closed` or `lease-expired`.
pub(crate) fn workspace_closed(name: &str, reason: &str, abandoned: usize) -> Write {
    let mut body = String::from("{\"name\":");
    json_str(&mut body, name);
    body.push_str(",\"reason\":");
    json_str(&mut body, reason);
    let _ = write!(body, ",\"abandoned\":{abandoned}}}");
    entry(WORKSPACE_CLOSED, body)
}

/// The `WorkspaceSynced` entry.
pub(crate) fn workspace_synced(
    name: &str,
    old_base: ChangesetId,
    new_base: ChangesetId,
    old_head: Option<ChangesetId>,
    new_head: Option<ChangesetId>,
) -> Write {
    let mut body = String::from("{\"name\":");
    json_str(&mut body, name);
    let _ = write!(
        body,
        ",\"old_base\":\"{old_base}\",\"new_base\":\"{new_base}\",\"old_head\":"
    );
    opt_id(&mut body, old_head);
    body.push_str(",\"new_head\":");
    opt_id(&mut body, new_head);
    body.push('}');
    entry(WORKSPACE_SYNCED, body)
}

/// The `RefMoved` entry for a ref move.
pub(crate) fn ref_moved(
    name: &str,
    old: Option<ChangesetId>,
    new: ChangesetId,
    version: u64,
    actor: &str,
) -> Write {
    let mut body = String::from("{\"name\":");
    json_str(&mut body, name);
    match old {
        Some(old) => {
            let _ = write!(body, ",\"old\":\"{old}\"");
        }
        None => body.push_str(",\"old\":null"),
    }
    let _ = write!(body, ",\"new\":\"{new}\",\"version\":{version},\"actor\":");
    json_str(&mut body, actor);
    body.push('}');
    entry(REF_MOVED, body)
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// One page of the journal from `from_lsn`. `wait_ms` is not honoured
    /// yet: the `MetaStore` port has no tail notification, so an empty page
    /// returns at once and the caller polls.
    pub(crate) async fn do_watch(
        &self,
        cx: &CallContext,
        req: Watch,
    ) -> Result<JournalPage, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        let from = req.from_lsn.max(1);
        let limit = self.page_size(req.limit);
        let rows = self
            .meta
            .journal(repo, from, limit)
            .await
            .map_err(errors::store)?;
        let next_lsn = rows.last().map_or(from, |(lsn, _, _)| lsn + 1);
        let entries = rows
            .into_iter()
            .filter(|(_, kind, _)| req.kinds.is_empty() || req.kinds.contains(kind))
            .map(|(lsn, kind, body)| JournalEntry {
                lsn,
                kind,
                body: String::from_utf8_lossy(&body).into_owned(),
            })
            .collect();
        Ok(JournalPage { entries, next_lsn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::ObjectId;

    #[test]
    fn bodies_are_json_with_escaped_strings() {
        let id = ChangesetId(ObjectId([0xab; 32]));
        let Write::Journal { kind, body } = ref_moved("ma\"in", None, id, 1, "a\\b\n") else {
            panic!("a journal write");
        };
        assert_eq!(kind, REF_MOVED);
        let body = String::from_utf8(body).expect("utf8");
        assert_eq!(
            body,
            format!(
                "{{\"name\":\"ma\\\"in\",\"old\":null,\"new\":\"{id}\",\"version\":1,\"actor\":\"a\\\\b\\n\"}}"
            )
        );
        let text = |w: Write| match w {
            Write::Journal { body, .. } => String::from_utf8(body).expect("utf8"),
            _ => panic!("a journal write"),
        };
        assert_eq!(
            text(changeset_created(id, Some("w\"1"))),
            format!("{{\"id\":\"{id}\",\"state\":\"draft\",\"workspace\":\"w\\\"1\"}}")
        );
        assert_eq!(
            text(changeset_created(id, None)),
            format!("{{\"id\":\"{id}\",\"state\":\"abandoned\"}}")
        );
        assert_eq!(
            text(workspace_opened("w", "o", "main", id)),
            format!("{{\"name\":\"w\",\"owner\":\"o\",\"line\":\"main\",\"base\":\"{id}\"}}")
        );
        assert_eq!(
            text(workspace_renewed("w", 7)),
            "{\"name\":\"w\",\"lease_until_ms\":7}"
        );
        assert_eq!(
            text(workspace_head_moved("w", None, Some(id))),
            format!("{{\"name\":\"w\",\"old\":null,\"new\":\"{id}\"}}")
        );
        assert_eq!(
            text(workspace_closed("w", "lease-expired", 2)),
            "{\"name\":\"w\",\"reason\":\"lease-expired\",\"abandoned\":2}"
        );
        assert_eq!(
            text(workspace_synced("w", id, id, Some(id), None)),
            format!(
                "{{\"name\":\"w\",\"old_base\":\"{id}\",\"new_base\":\"{id}\",\"old_head\":\"{id}\",\"new_head\":null}}"
            )
        );
        assert_eq!(
            text(candidate_created(id, "land:9:ab")),
            format!("{{\"id\":\"{id}\",\"state\":\"draft\",\"holder\":\"land:9:ab\"}}")
        );
        assert_eq!(
            text(land_attempt(id, "main", LandOutcome::Landed(id))),
            format!(
                "{{\"draft\":\"{id}\",\"line\":\"main\",\"outcome\":\"landed\",\"landed\":\"{id}\"}}"
            )
        );
        assert_eq!(
            text(land_attempt(id, "main", LandOutcome::Conflict(id))),
            format!(
                "{{\"draft\":\"{id}\",\"line\":\"main\",\"outcome\":\"conflict\",\"onto\":\"{id}\"}}"
            )
        );
        assert_eq!(
            text(attested(&AttestedEntry {
                changeset: id,
                kind: "check",
                check: "ci/test",
                status: "fail",
                url: "https://ci/1",
                actor: "ci-bot",
                by: None,
            })),
            format!(
                "{{\"changeset\":\"{id}\",\"check\":\"ci/test\",\"passed\":false,\"url\":\"https://ci/1\",\"actor\":\"ci-bot\"}}"
            )
        );
        // The extended body (v0.2): a check with a display name keeps
        // `passed`; a pending check, a review and an approval leave it out.
        assert_eq!(
            text(attested(&AttestedEntry {
                changeset: id,
                kind: "check",
                check: "ci/test",
                status: "pass",
                url: "u",
                actor: "a",
                by: Some("Ann"),
            })),
            format!(
                "{{\"changeset\":\"{id}\",\"check\":\"ci/test\",\"passed\":true,\"url\":\"u\",\"actor\":\"a\",\"kind\":\"check\",\"status\":\"pass\",\"by\":\"Ann\"}}"
            )
        );
        assert_eq!(
            text(attested(&AttestedEntry {
                changeset: id,
                kind: "check",
                check: "ci/test",
                status: "pending",
                url: "u",
                actor: "a",
                by: None,
            })),
            format!(
                "{{\"changeset\":\"{id}\",\"check\":\"ci/test\",\"url\":\"u\",\"actor\":\"a\",\"kind\":\"check\",\"status\":\"pending\",\"by\":null}}"
            )
        );
        assert_eq!(
            text(attested(&AttestedEntry {
                changeset: id,
                kind: "review",
                check: "review/x",
                status: "fail",
                url: "u",
                actor: "a",
                by: Some("Ann"),
            })),
            format!(
                "{{\"changeset\":\"{id}\",\"check\":\"review/x\",\"url\":\"u\",\"actor\":\"a\",\"kind\":\"review\",\"status\":\"fail\",\"by\":\"Ann\"}}"
            )
        );
        assert_eq!(
            text(land_reverted("main", id, id, Some("ci/test"))),
            format!(
                "{{\"line\":\"main\",\"land\":\"{id}\",\"revert\":\"{id}\",\"check\":\"ci/test\"}}"
            )
        );
        assert_eq!(
            text(land_reverted("main", id, id, None)),
            format!("{{\"line\":\"main\",\"land\":\"{id}\",\"revert\":\"{id}\",\"check\":null}}")
        );
        assert_eq!(
            text(revert_requested("main", id, "ci/test", "u")),
            format!(
                "{{\"line\":\"main\",\"changeset\":\"{id}\",\"check\":\"ci/test\",\"url\":\"u\"}}"
            )
        );
    }
}
