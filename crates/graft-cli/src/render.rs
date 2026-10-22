//! The JSON shapes of every command, built in one place so that `graft api`
//! and the commands agree, and the schemas in `crates/graft-cli/schemas/`
//! describe each one. Object ids in entries are lowercase hex (the kind is
//! in the entry); changeset ids use their text form (`c` plus hex).

use std::io::{self, Write};

use base64::Engine;
use graft_model::{Author, AuthorKind, Change, ChangesetId, EntryKind, Timestamp};
use graft_proto::types::{
    BlameResult, ChangesetSummary, ChangesetView, DiffResult, DraftState, Entries, EntryView,
    FileContent, FileDiff, History, JournalPage, RefKind, RefView, WorkspaceView,
};
use serde_json::{Value, json};

use crate::error::Failure;

/// Writes `value` as pretty JSON and a newline to standard output.
pub(crate) fn print_json(value: &Value) -> Result<(), Failure> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value).map_err(io::Error::from)?;
    writeln!(out)?;
    Ok(())
}

/// A changeset id shortened for people: the kind letter and twelve hex digits.
pub(crate) fn short(id: ChangesetId) -> String {
    id.to_string()[..13].to_owned()
}

/// The name of an entry kind, as it appears in JSON.
pub(crate) fn kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Exec => "exec",
        EntryKind::Symlink => "symlink",
        EntryKind::Tree => "tree",
        EntryKind::Submodule => "submodule",
    }
}

/// The name of a changeset's lifecycle state.
pub(crate) fn state_name(state: DraftState) -> &'static str {
    match state {
        DraftState::Draft => "draft",
        DraftState::Landed => "landed",
        DraftState::Superseded => "superseded",
        DraftState::Abandoned => "abandoned",
    }
}

/// The name of a ref's kind.
pub(crate) fn ref_kind_name(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Line => "line",
        RefKind::Tag => "tag",
        RefKind::Tool => "tool",
    }
}

/// An author.
pub(crate) fn author(author: &Author) -> Value {
    let kind = match author.kind {
        AuthorKind::Human => "human",
        AuthorKind::Agent => "agent",
        AuthorKind::Bot => "bot",
        AuthorKind::Ci => "ci",
    };
    json!({"name": author.name, "email": author.email, "kind": kind})
}

/// A client-supplied time.
pub(crate) fn timestamp(t: Timestamp) -> Value {
    json!({"ms": t.ms, "offset_min": t.offset_min})
}

/// A history line.
pub(crate) fn summary(s: &ChangesetSummary) -> Value {
    json!({
        "id": s.id.to_string(),
        "author": author(&s.author),
        "authored_at": timestamp(s.authored_at),
        "subject": s.subject,
        "line_pos": s.line_pos,
    })
}

/// A page of history.
pub(crate) fn history(h: &History) -> Value {
    json!({
        "changesets": h.changesets.iter().map(summary).collect::<Vec<_>>(),
        "next": h.next,
    })
}

/// A change against the first parent. A put names its kind and object id.
pub(crate) fn change(c: &Change) -> Value {
    match c {
        Change::Put { path, kind, id } => json!({
            "path": path.to_string(),
            "op": "put",
            "kind": kind_name(*kind),
            "id": id.to_hex(),
        }),
        Change::Delete { path } => json!({"path": path.to_string(), "op": "delete"}),
    }
}

/// A changed file, with its unified patch when one was asked for.
pub(crate) fn file_diff(f: &FileDiff) -> Value {
    let mut value = change(&f.change);
    value["patch"] = json!(f.patch);
    value["truncated"] = json!(f.truncated);
    value
}

/// A diff between two changesets, one page.
pub(crate) fn diff(from: ChangesetId, to: ChangesetId, d: &DiffResult) -> Value {
    json!({
        "from": from.to_string(),
        "to": to.to_string(),
        "files": d.files.iter().map(file_diff).collect::<Vec<_>>(),
        "next": d.next,
    })
}

/// A ref and its target.
pub(crate) fn ref_view(r: &RefView) -> Value {
    json!({
        "name": r.name,
        "target": r.target.to_string(),
        "version": r.version,
        "kind": ref_kind_name(r.kind),
    })
}

/// A changeset with its server record. The changes are against the first
/// parent, without patches.
pub(crate) fn changeset_view(v: &ChangesetView) -> Value {
    let c = &v.changeset;
    json!({
        "id": c.id().to_string(),
        "parents": c.parents.iter().map(ChangesetId::to_string).collect::<Vec<_>>(),
        "root": c.root.0.to_hex(),
        "author": author(&c.author),
        "authored_at": timestamp(c.authored_at),
        "message": c.message,
        "metadata": c.metadata,
        "changes": c.changes.iter().map(change).collect::<Vec<_>>(),
        "lsn": v.lsn,
        "state": state_name(v.state),
        "superseded_by": v.superseded_by.map(|id| id.to_string()),
    })
}

/// A workspace.
pub(crate) fn workspace_view(w: &WorkspaceView) -> Value {
    json!({
        "name": w.name,
        "owner": w.owner,
        "session": w.session,
        "line": w.line,
        "base": w.base.to_string(),
        "head": w.head.map(|h| h.to_string()),
        "lease_until_ms": w.lease_until_ms,
    })
}

/// One entry of a snapshot.
pub(crate) fn entry(e: &EntryView) -> Value {
    json!({
        "path": e.path.to_string(),
        "kind": kind_name(e.kind),
        "id": e.id.to_hex(),
    })
}

/// A page of directory entries.
pub(crate) fn entries(e: &Entries) -> Value {
    json!({
        "entries": e.entries.iter().map(entry).collect::<Vec<_>>(),
        "next": e.next,
    })
}

/// A file's content at a snapshot. Text is UTF-8; anything else is base64.
pub(crate) fn file_content(at: ChangesetId, path: &str, f: &FileContent) -> Value {
    let (encoding, content) = match std::str::from_utf8(&f.data) {
        Ok(text) => ("utf8", text.to_owned()),
        Err(_) => (
            "base64",
            base64::engine::general_purpose::STANDARD.encode(&f.data),
        ),
    };
    json!({
        "at": at.to_string(),
        "path": path,
        "kind": kind_name(f.kind),
        "id": f.id.0.to_hex(),
        "size": f.data.len(),
        "encoding": encoding,
        "content": content,
    })
}

/// Line origins for a file at a snapshot. Ranges are 0-based, end exclusive.
pub(crate) fn blame(at: ChangesetId, path: &str, b: &BlameResult) -> Value {
    json!({
        "at": at.to_string(),
        "path": path,
        "ranges": b.ranges.iter().map(|r| json!({
            "start": r.start,
            "end": r.end,
            "origin": summary(&r.origin),
        })).collect::<Vec<_>>(),
    })
}

/// A page of journal entries.
pub(crate) fn journal(page: &JournalPage) -> Value {
    json!({
        "entries": page.entries.iter().map(|e| json!({
            "lsn": e.lsn,
            "kind": e.kind,
            "body": e.body,
        })).collect::<Vec<_>>(),
        "next_lsn": page.next_lsn,
    })
}

#[cfg(test)]
mod tests {
    use graft_model::{ChangesetId, RepoPath};

    use super::*;

    #[test]
    fn short_ids_keep_the_kind_letter() {
        let id = ChangesetId::hash(b"graft");
        let text = short(id);
        assert_eq!(text.len(), 13);
        assert!(id.to_string().starts_with(&text));
    }

    #[test]
    fn delete_changes_carry_no_kind() {
        let path = RepoPath::new("a/b.txt").unwrap();
        let value = change(&Change::Delete { path });
        assert_eq!(value, json!({"path": "a/b.txt", "op": "delete"}));
    }
}
