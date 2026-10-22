//! Journal entries (`docs/design.md` §4.8) and `Watch`.
//!
//! An entry's body is a JSON object, stored as the UTF-8 bytes watchers
//! receive, so `Watch` never re-encodes. Ids use their text form
//! (`c<hex>`). The bodies are a durable format:
//!
//! - `ChangesetCreated`: `{"id":"c…","state":"draft","workspace":"w"}` for a
//!   draft created in a workspace, `{"id":"c…","state":"abandoned"}` for one
//!   created without (nothing holds it)
//! - `RefMoved`: `{"name":"main","old":"c…"|null,"new":"c…","version":2,"actor":"…"}`
//! - `WorkspaceOpened`: `{"name":"w","owner":"…","line":"main","base":"c…"}`
//! - `WorkspaceRenewed`: `{"name":"w","lease_until_ms":123}`
//! - `WorkspaceHeadMoved`: `{"name":"w","old":"c…"|null,"new":"c…"|null}`
//! - `WorkspaceClosed`: `{"name":"w","reason":"closed"|"lease-expired","abandoned":2}`

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

/// A JSON string literal.
fn json_str(out: &mut String, s: &str) {
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
    }
}
