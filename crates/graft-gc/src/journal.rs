//! The journal entries the sweep writes (`docs/design.md` §4.8). Bodies are
//! JSON objects in the engine's style, and a durable format:
//!
//! - `WorkspaceClosed`: `{"name":"w","reason":"lease-expired","abandoned":2}`,
//!   exactly as the engine writes it when it ends a lapsed workspace
//! - `Collected`: `{"ids":["c…"],"objects":3,"moved":1}`: the changeset
//!   collected, the objects it doomed, and the rows it moved to its successor
//! - `RowsExpired`: `{"holder":"upload:…","rows":4,"objects":2}`: the expired
//!   upload or claim rows of one holder removed, and the objects doomed
//! - `ObjectDeleted`: `{"id":"<hex>","bytes":120}`: a doomed object's frame
//!   deleted, and its size (0 when a crashed pass had already deleted it)
//! - `AdminAction`: `{"what":"gc-deletion-on","actor":"…"}` (or
//!   `gc-deletion-off`): the operator's deletion switch

use std::fmt::Write as _;

use graft_model::{ChangesetId, ObjectId};
use graft_store::Write;

/// Kind of the entry written when a lapsed workspace is closed.
pub const WORKSPACE_CLOSED: &str = "WorkspaceClosed";

/// Kind of the entry written with each collection.
pub const COLLECTED: &str = "Collected";

/// Kind of the entry written when expired upload or claim rows go.
pub const ROWS_EXPIRED: &str = "RowsExpired";

/// Kind of the entry written when a frame is deleted.
pub const OBJECT_DELETED: &str = "ObjectDeleted";

/// Kind of the entry written when the operator turns deletion on or off.
pub const ADMIN_ACTION: &str = "AdminAction";

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

/// The `WorkspaceClosed` entry for a lapsed lease.
pub(crate) fn workspace_closed(name: &str, abandoned: usize) -> Write {
    let mut body = String::from("{\"name\":");
    json_str(&mut body, name);
    let _ = write!(
        body,
        ",\"reason\":\"lease-expired\",\"abandoned\":{abandoned}}}"
    );
    entry(WORKSPACE_CLOSED, body)
}

/// The `Collected` entry.
pub(crate) fn collected(id: ChangesetId, objects: usize, moved: usize) -> Write {
    entry(
        COLLECTED,
        format!("{{\"ids\":[\"{id}\"],\"objects\":{objects},\"moved\":{moved}}}"),
    )
}

/// The `RowsExpired` entry.
pub(crate) fn rows_expired(holder: &str, rows: usize, objects: usize) -> Write {
    let mut body = String::from("{\"holder\":");
    json_str(&mut body, holder);
    let _ = write!(body, ",\"rows\":{rows},\"objects\":{objects}}}");
    entry(ROWS_EXPIRED, body)
}

/// The `ObjectDeleted` entry.
pub(crate) fn object_deleted(id: ObjectId, bytes: u64) -> Write {
    entry(
        OBJECT_DELETED,
        format!("{{\"id\":\"{}\",\"bytes\":{bytes}}}", id.to_hex()),
    )
}

/// The `AdminAction` entry for the deletion switch.
pub(crate) fn deletion_switched(on: bool, actor: &str) -> Write {
    let what = if on {
        "gc-deletion-on"
    } else {
        "gc-deletion-off"
    };
    let mut body = format!("{{\"what\":\"{what}\",\"actor\":");
    json_str(&mut body, actor);
    body.push('}');
    entry(ADMIN_ACTION, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(w: Write) -> (String, String) {
        match w {
            Write::Journal { kind, body } => (kind, String::from_utf8(body).expect("utf-8")),
            other => panic!("not a journal entry: {other:?}"),
        }
    }

    #[test]
    fn bodies_are_the_documented_json() {
        let id = ChangesetId(ObjectId([1; 32]));
        assert_eq!(
            body(workspace_closed("w\"1", 2)),
            (
                "WorkspaceClosed".to_owned(),
                r#"{"name":"w\"1","reason":"lease-expired","abandoned":2}"#.to_owned()
            )
        );
        assert_eq!(
            body(collected(id, 3, 1)).1,
            format!(r#"{{"ids":["{id}"],"objects":3,"moved":1}}"#)
        );
        assert_eq!(
            body(rows_expired("upload:5:t", 4, 2)).1,
            r#"{"holder":"upload:5:t","rows":4,"objects":2}"#
        );
        assert_eq!(
            body(object_deleted(ObjectId([0xab; 32]), 120)).1,
            format!(r#"{{"id":"{}","bytes":120}}"#, "ab".repeat(32))
        );
        assert_eq!(
            body(deletion_switched(true, "ops")),
            (
                "AdminAction".to_owned(),
                r#"{"what":"gc-deletion-on","actor":"ops"}"#.to_owned()
            )
        );
    }
}
