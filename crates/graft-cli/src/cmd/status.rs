//! `graft status`: the working copy's changes against its base. Stat-based
//! and local, so it never contacts the server.

use graft_client::workcopy::FileStatus;
use serde_json::{Value, json};

use crate::cli::Global;
use crate::conn::require_working_copy;
use crate::error::Failure;
use crate::render::{print_json, short};

/// The status word for a change in JSON and in text.
pub(crate) fn word(change: &FileStatus) -> &'static str {
    match change {
        FileStatus::Added(_) => "added",
        FileStatus::Modified(_) => "modified",
        FileStatus::Deleted(_) => "deleted",
    }
}

/// One change as JSON.
pub(crate) fn change_json(change: &FileStatus) -> Value {
    json!({"path": change.path().to_string(), "status": word(change)})
}

/// Prints the line, base, head and the changed paths.
pub(crate) fn run(g: &Global) -> Result<(), Failure> {
    let wc = require_working_copy()?;
    let changes = wc.status()?;
    let state = wc.state();
    if g.json {
        return print_json(&json!({
            "line": state.line,
            "base": state.base.to_string(),
            "head": state.head.map(|h| h.to_string()),
            "clean": changes.is_empty(),
            "changes": changes.iter().map(change_json).collect::<Vec<_>>(),
        }));
    }
    outln!("line {}", state.line);
    outln!("base {}", short(state.base));
    match state.head {
        Some(head) => outln!("head {}", short(head)),
        None => outln!("head (none)"),
    }
    if changes.is_empty() {
        outln!("clean");
    }
    for change in &changes {
        outln!("{:<9} {}", word(change), change.path());
    }
    Ok(())
}
