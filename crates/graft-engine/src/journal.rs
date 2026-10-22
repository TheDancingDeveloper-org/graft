//! Journal entries (`docs/design.md` §4.8) and `Watch`.
//!
//! An entry's body is a JSON object, stored as the UTF-8 bytes watchers
//! receive, so `Watch` never re-encodes. Ids use their text form
//! (`c<hex>`). The bodies are a durable format:
//!
//! - `ChangesetCreated`: `{"id":"c…","state":"draft"}`
//! - `RefMoved`: `{"name":"main","old":"c…"|null,"new":"c…","version":2,"actor":"…"}`

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

/// The `ChangesetCreated` entry for a new draft.
pub(crate) fn changeset_created(id: ChangesetId) -> Write {
    entry(
        CHANGESET_CREATED,
        format!("{{\"id\":\"{id}\",\"state\":\"draft\"}}"),
    )
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
        let Write::Journal { body, .. } = changeset_created(id) else {
            panic!("a journal write");
        };
        assert_eq!(
            String::from_utf8(body).expect("utf8"),
            format!("{{\"id\":\"{id}\",\"state\":\"draft\"}}")
        );
    }
}
