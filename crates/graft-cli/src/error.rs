//! Failures and their exit codes (`docs/design.md` §8.2).
//!
//! A [`Failure`] carries a stable `kind` (the JSON error's `kind`), a message
//! for people, and the exit code. Messages name ids, paths and rules; they
//! never carry file contents or tokens.

use std::fmt;
use std::io;

use graft_client::Error as ClientError;
use graft_client::workcopy::Conflicts;
use graft_model::ChangesetId;
use graft_proto::ApiError;
use graft_proto::error::ConflictPath;
use serde_json::{Map, Value, json};

use crate::Exit;
use crate::render;

/// A command that did not succeed.
#[derive(Debug)]
pub(crate) struct Failure {
    /// The exit code.
    pub(crate) exit: Exit,
    /// A stable name for the failure, in the JSON error object.
    pub(crate) kind: &'static str,
    /// What went wrong, for people.
    pub(crate) message: String,
    /// Whether anything was already printed (so nothing more is).
    silent: bool,
    /// More fields of the JSON error object (`onto`, `conflicts`).
    detail: Map<String, Value>,
    /// More lines for people, printed after the message.
    notes: Vec<String>,
}

impl Failure {
    /// A failure with an exit code, a kind and a message.
    pub(crate) fn new(exit: Exit, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit,
            kind,
            message: message.into(),
            silent: false,
            detail: Map::new(),
            notes: Vec::new(),
        }
    }

    /// Adds a field to the JSON error object.
    pub(crate) fn with_detail(mut self, key: &str, value: Value) -> Self {
        self.detail.insert(key.to_owned(), value);
        self
    }

    /// Adds a line for people, printed after the message.
    pub(crate) fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// The failure's kind.
    pub(crate) fn kind(&self) -> &'static str {
        self.kind
    }

    /// A usage error (exit 2).
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::new(Exit::Usage, "usage", message)
    }

    /// An exit whose output was already printed.
    pub(crate) fn silent(exit: Exit) -> Self {
        Self {
            exit,
            kind: "silent",
            message: String::new(),
            silent: true,
            detail: Map::new(),
            notes: Vec::new(),
        }
    }

    /// Prints the failure: a JSON error object on standard output with
    /// `--json`, else one line on standard error. Returns the exit code.
    pub(crate) fn report(self, json: bool) -> Exit {
        if self.silent {
            return self.exit;
        }
        if json {
            let mut error = Map::new();
            error.insert("kind".to_owned(), self.kind.into());
            error.insert("message".to_owned(), self.message.into());
            error.extend(self.detail);
            let value = json!({ "error": error });
            // A failure to print the error is reported by the exit code alone.
            let _ = render::print_json(&value);
        } else {
            eprintln!("graft: {}", self.message);
            for note in &self.notes {
                eprintln!("  {note}");
            }
        }
        self.exit
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// The exit code and kind for a typed API error.
pub(crate) fn api_exit(e: &ApiError) -> (Exit, &'static str) {
    match e {
        ApiError::Conflict { .. } | ApiError::MergeConflict { .. } => (Exit::Conflict, "conflict"),
        ApiError::RefConflict { .. } => (Exit::Conflict, "ref_conflict"),
        ApiError::Contended { .. } => (Exit::Contended, "contended"),
        ApiError::Rejected { .. } => (Exit::Rejected, "rejected"),
        ApiError::PermissionDenied { .. } => (Exit::Denied, "permission_denied"),
        ApiError::Unauthenticated => (Exit::Denied, "unauthenticated"),
        ApiError::Unavailable { .. } => (Exit::Unavailable, "unavailable"),
        ApiError::InvalidPath { .. } => (Exit::Usage, "invalid_path"),
        ApiError::InvalidChange { .. } => (Exit::Usage, "invalid_change"),
        ApiError::NotFound { .. } => (Exit::Failure, "not_found"),
        ApiError::Collected { .. } => (Exit::Failure, "collected"),
        ApiError::UnknownParent { .. } => (Exit::Failure, "unknown_parent"),
        ApiError::MissingObjects { .. } => (Exit::Failure, "missing_objects"),
        ApiError::LeaseExpired { .. } => (Exit::Failure, "lease_expired"),
        ApiError::LimitExceeded { .. } => (Exit::Failure, "limit_exceeded"),
        ApiError::CorruptObject { .. } => (Exit::Failure, "corrupt_object"),
        ApiError::Obliterated { .. } => (Exit::Failure, "obliterated"),
        ApiError::IncompatibleProtocol { .. } => (Exit::Failure, "incompatible_protocol"),
        _ => (Exit::Failure, "api_error"),
    }
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        let (exit, kind) = api_exit(&e);
        let failure = Failure::new(exit, kind, e.to_string());
        let conflicted = |failure: Failure, paths: &[ConflictPath], onto: ChangesetId| {
            let mut failure = failure
                .with_detail("onto", onto.to_string().into())
                .with_detail(
                    "conflicts",
                    paths
                        .iter()
                        .map(|p| json!({"path": p.path, "mode": null}))
                        .collect(),
                );
            for p in paths {
                failure = failure.with_note(format!("conflict {}", p.path));
            }
            failure
        };
        match e {
            ApiError::Conflict { paths, onto } => conflicted(failure, &paths, onto),
            ApiError::MergeConflict {
                paths,
                onto,
                base,
                source,
            } => conflicted(failure, &paths, onto)
                .with_detail("base", base.to_string().into())
                .with_detail("source", source.to_string().into()),
            _ => failure,
        }
    }
}

/// The conflicts a sync wrote, as the JSON error object's `conflicts`.
pub(crate) fn conflicts_json(c: &Conflicts) -> Value {
    c.files
        .iter()
        .map(|f| json!({"path": f.path.to_string(), "mode": f.mode.name()}))
        .collect()
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Api(api) => api.into(),
            ClientError::Endpoint(what) => Failure::new(Exit::Usage, "endpoint", what),
            ClientError::Dirty(paths) => Failure::new(
                Exit::Conflict,
                "dirty",
                format!(
                    "{} paths have local changes; commit or revert them first",
                    paths.len()
                ),
            ),
            ClientError::State(what) => Failure::new(Exit::Failure, "state", what),
            ClientError::NothingToCommit => {
                Failure::new(Exit::Failure, "nothing_to_commit", "nothing to commit")
            }
            ClientError::NoDraft => Failure::new(
                Exit::Failure,
                "no_draft",
                "the workspace has no draft: commit first",
            ),
            ClientError::Unstable(path) => Failure::new(
                Exit::Failure,
                "unstable",
                format!("{path} kept changing while it was read; try again"),
            ),
            e @ ClientError::Diverged { .. } => {
                Failure::new(Exit::Conflict, "diverged", e.to_string())
            }
            e @ ClientError::StillQueued { .. } => {
                Failure::new(Exit::Contended, "queued", e.to_string())
            }
            e @ ClientError::LandedDirty { .. } => {
                Failure::new(Exit::Conflict, "landed_dirty", e.to_string())
            }
            ClientError::Conflicted(c) => {
                let message = ClientError::Conflicted(c.clone()).to_string();
                let mut failure = Failure::new(Exit::Conflict, "conflicted", message)
                    .with_detail("onto", c.onto.to_string().into())
                    .with_detail("conflicts", conflicts_json(&c));
                for f in &c.files {
                    failure = failure.with_note(format!("{:<8} {}", f.mode.name(), f.path));
                }
                failure.with_note(
                    "edit the files (and delete any .graft-conflict sidecar), then `graft commit` and `graft land`",
                )
            }
            ClientError::Unresolved(paths) => {
                let message = format!(
                    "{}: remove the markers and any .graft-conflict sidecar",
                    ClientError::Unresolved(paths.clone())
                );
                let mut failure = Failure::new(Exit::Conflict, "unresolved", message).with_detail(
                    "conflicts",
                    paths
                        .iter()
                        .map(|p| json!({"path": p.to_string(), "mode": null}))
                        .collect(),
                );
                for p in &paths {
                    failure = failure.with_note(format!("unresolved {p}"));
                }
                failure
            }
            ClientError::Unfinished => Failure::new(
                Exit::Failure,
                "unfinished",
                "an earlier update did not finish: run `graft sync`",
            ),
            ClientError::Io(e) => e.into(),
            ClientError::Store(e) => {
                Failure::new(Exit::Failure, "store", format!("object cache: {e:?}"))
            }
            other => Failure::new(Exit::Failure, "client", other.to_string()),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        // A reader that closed early (`graft log | head`) is not a failure.
        if e.kind() == io::ErrorKind::BrokenPipe {
            return Failure::silent(Exit::Ok);
        }
        Failure::new(Exit::Failure, "io", e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use graft_model::ChangesetId;

    use super::*;

    #[test]
    fn api_errors_map_to_design_exit_codes() {
        let id = ChangesetId::hash(b"x");
        let cases: Vec<(ApiError, Exit)> = vec![
            (
                ApiError::Conflict {
                    paths: vec![],
                    onto: id,
                },
                Exit::Conflict,
            ),
            (
                ApiError::Contended {
                    line: "main".into(),
                    tip: id,
                },
                Exit::Contended,
            ),
            (
                ApiError::Rejected {
                    policy: "p".into(),
                    detail: "d".into(),
                },
                Exit::Rejected,
            ),
            (
                ApiError::PermissionDenied {
                    scope: "read".into(),
                    path: None,
                },
                Exit::Denied,
            ),
            (ApiError::Unauthenticated, Exit::Denied),
            (
                ApiError::Unavailable { retry_after_ms: 0 },
                Exit::Unavailable,
            ),
            (
                ApiError::InvalidPath {
                    path: "a//b".into(),
                    reason: "empty".into(),
                },
                Exit::Usage,
            ),
        ];
        for (err, want) in cases {
            assert_eq!(api_exit(&err).0, want, "{err:?}");
        }
    }

    #[test]
    fn client_errors_map_to_design_exit_codes() {
        let dirty: Failure = ClientError::Dirty(vec![]).into();
        assert_eq!(dirty.exit, Exit::Conflict);
        let usage: Failure = ClientError::Endpoint("nope".into()).into();
        assert_eq!(usage.exit, Exit::Usage);
    }
}
