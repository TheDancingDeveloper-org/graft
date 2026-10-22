//! Failures and their exit codes (`docs/design.md` §8.2).
//!
//! A [`Failure`] carries a stable `kind` (the JSON error's `kind`), a message
//! for people, and the exit code. Messages name ids, paths and rules; they
//! never carry file contents or tokens.

use std::fmt;
use std::io;

use graft_client::Error as ClientError;
use graft_proto::ApiError;
use serde_json::json;

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
}

impl Failure {
    /// A failure with an exit code, a kind and a message.
    pub(crate) fn new(exit: Exit, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit,
            kind,
            message: message.into(),
            silent: false,
        }
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
        }
    }

    /// Prints the failure: a JSON error object on standard output with
    /// `--json`, else one line on standard error. Returns the exit code.
    pub(crate) fn report(self, json: bool) -> Exit {
        if self.silent {
            return self.exit;
        }
        if json {
            let value = json!({"error": {"kind": self.kind, "message": self.message}});
            // A failure to print the error is reported by the exit code alone.
            let _ = render::print_json(&value);
        } else {
            eprintln!("graft: {}", self.message);
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
        ApiError::Conflict { .. } => (Exit::Conflict, "conflict"),
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
        Failure::new(exit, kind, e.to_string())
    }
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
