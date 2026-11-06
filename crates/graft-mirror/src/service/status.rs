//! A task's status (D41 §3.3): typed failure codes, never git's own words,
//! and at most one `SetMirrorStatus` per poll, skipped when nothing changed
//! but for a once-a-minute liveness write.

use std::time::Duration;

use graft_proto::ApiError;
use graft_proto::pb::{self, MirrorErrorCode};

use crate::MirrorError;
use crate::github::ForgeError;

/// The longest `error_message` written.
pub const MESSAGE_MAX: usize = 200;

/// How often an unchanged status is still written, to show the task lives.
pub const LIVENESS: Duration = Duration::from_secs(60);

/// Why a poll failed, as status reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollFailure {
    /// The typed code.
    pub code: MirrorErrorCode,
    /// A message written by the service from the code and safe context.
    pub message: String,
    /// When the upstream asked to be left alone this long.
    pub retry_after: Option<Duration>,
    /// The graft server, not the upstream, failed: back off on the server
    /// schedule, not the repository's.
    pub server: bool,
}

impl PollFailure {
    /// A failure with `code` and a message naming it.
    pub fn new(code: MirrorErrorCode, message: impl Into<String>) -> PollFailure {
        let mut message = message.into();
        if message.len() > MESSAGE_MAX {
            let mut end = MESSAGE_MAX;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        PollFailure {
            code,
            message,
            retry_after: None,
            server: false,
        }
    }

    /// The graft server failed.
    pub fn server(e: &ApiError) -> PollFailure {
        match e {
            ApiError::LeaseLost { .. } => lease_lost(),
            ApiError::Unavailable { retry_after_ms } => PollFailure {
                retry_after: (*retry_after_ms > 0).then(|| Duration::from_millis(*retry_after_ms)),
                server: true,
                ..PollFailure::new(MirrorErrorCode::Internal, "the graft server is unavailable")
            },
            e => PollFailure {
                server: true,
                ..PollFailure::new(
                    MirrorErrorCode::Internal,
                    format!("the graft server refused: {}", api_error_name(e)),
                )
            },
        }
    }
}

/// The lease is held under another epoch.
pub fn lease_lost() -> PollFailure {
    PollFailure::new(
        MirrorErrorCode::LeaseLost,
        "another mirror process holds this repository's lease",
    )
}

/// A stable name for an API error's kind, carrying none of its fields.
fn api_error_name(e: &ApiError) -> &'static str {
    match e {
        ApiError::NotFound { .. } => "not found",
        ApiError::PermissionDenied { .. } => "permission denied",
        ApiError::Unauthenticated => "unauthenticated",
        ApiError::RefConflict { .. } => "ref conflict",
        ApiError::Rejected { .. } => "rejected",
        ApiError::LimitExceeded { .. } => "limit exceeded",
        ApiError::LeaseLost { .. } => "lease lost",
        ApiError::Unavailable { .. } => "unavailable",
        _ => "error",
    }
}

/// The failure class of a git network error's text. Only the class leaves
/// here: the text itself can echo a URL or a credential.
fn git_class(text: &str) -> (MirrorErrorCode, &'static str) {
    let t = text.to_ascii_lowercase();
    if t.contains("timed out") || t.contains("timeout") {
        (
            MirrorErrorCode::Timeout,
            "the upstream did not answer in time",
        )
    } else if t.contains("429") || t.contains("rate limit") || t.contains("too many requests") {
        (
            MirrorErrorCode::RateLimited,
            "the upstream rate-limited the mirror",
        )
    } else if t.contains("authentication")
        || t.contains("could not read username")
        || t.contains("could not read password")
        || t.contains("401")
        || t.contains("403")
        || t.contains("permission denied")
    {
        (
            MirrorErrorCode::AuthFailed,
            "the upstream refused the mirror's credential",
        )
    } else if t.contains("not found")
        || t.contains("does not appear to be a git repository")
        || t.contains("404")
    {
        (
            MirrorErrorCode::NotFound,
            "the upstream repository was not found",
        )
    } else {
        (
            MirrorErrorCode::UpstreamUnreachable,
            "the upstream could not be reached",
        )
    }
}

/// What status says about a failed poll.
pub fn classify(e: &MirrorError) -> PollFailure {
    match e {
        MirrorError::Api(e) => PollFailure::server(e),
        MirrorError::Fetch(text) => {
            let (code, msg) = git_class(text);
            PollFailure::new(code, msg)
        }
        MirrorError::Cli(e) => {
            let (code, msg) = git_class(&e.0);
            PollFailure::new(code, msg)
        }
        MirrorError::Forge(ForgeError::Transient(_)) => PollFailure::new(
            MirrorErrorCode::RateLimited,
            "the forge is unavailable or rate-limited the mirror",
        ),
        MirrorError::Forge(ForgeError::Refused(_)) => {
            PollFailure::new(MirrorErrorCode::AuthFailed, "the forge refused the mirror")
        }
        MirrorError::Git(_) | MirrorError::Unmapped { .. } => PollFailure::new(
            MirrorErrorCode::Internal,
            "importing the upstream commits failed",
        ),
        MirrorError::Config(_) | MirrorError::Switch(_) | MirrorError::Append { .. } => {
            PollFailure::new(
                MirrorErrorCode::Internal,
                "the mirror task is misconfigured",
            )
        }
    }
}

/// The part of a status that, when unchanged, makes a write skippable.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    sha: Option<String>,
    changeset: Option<Vec<u8>>,
    code: i32,
    message: Option<String>,
}

fn key(s: &pb::MirrorStatus) -> Key {
    Key {
        sha: s.last_imported_sha.clone(),
        changeset: s.last_changeset.clone(),
        code: s.error_code,
        message: s.error_message.clone(),
    }
}

/// Decides which statuses to write: a change, or the first unchanged one a
/// [`LIVENESS`] after the last write.
#[derive(Debug, Default)]
pub struct StatusWriter {
    last: Option<(Key, i64)>,
}

impl StatusWriter {
    /// Whether to write `status` at `now_ms`. A `true` answer counts as
    /// written: call [`StatusWriter::unwritten`] if the write failed.
    pub fn should_write(&mut self, status: &pb::MirrorStatus, now_ms: i64) -> bool {
        let k = key(status);
        let write = match &self.last {
            None => true,
            Some((last, at)) => *last != k || now_ms - at >= LIVENESS.as_millis() as i64,
        };
        if write {
            self.last = Some((k, now_ms));
        }
        write
    }

    /// The last write failed: the next status is written whatever it says.
    pub fn unwritten(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(sha: &str, code: MirrorErrorCode, at: i64) -> pb::MirrorStatus {
        pb::MirrorStatus {
            last_poll_at_ms: Some(at),
            last_imported_sha: Some(sha.to_owned()),
            error_code: code as i32,
            ..Default::default()
        }
    }

    #[test]
    fn unchanged_status_is_written_once_a_minute() {
        let mut w = StatusWriter::default();
        let mut writes = Vec::new();
        // Polls every 15 s for three minutes, nothing changing.
        for i in 0..12 {
            let at = i * 15_000;
            if w.should_write(&status("a", MirrorErrorCode::Unspecified, at), at) {
                writes.push(at);
            }
        }
        assert_eq!(writes, [0, 60_000, 120_000]);
    }

    #[test]
    fn a_change_is_written_at_once_and_a_failed_write_retried() {
        let mut w = StatusWriter::default();
        assert!(w.should_write(&status("a", MirrorErrorCode::Unspecified, 0), 0));
        assert!(w.should_write(&status("b", MirrorErrorCode::Unspecified, 1), 1));
        assert!(w.should_write(&status("b", MirrorErrorCode::Timeout, 2), 2));
        assert!(!w.should_write(&status("b", MirrorErrorCode::Timeout, 3), 3));
        w.unwritten();
        assert!(w.should_write(&status("b", MirrorErrorCode::Timeout, 4), 4));
    }

    #[test]
    fn git_failures_become_codes_without_their_text() {
        let secret = "https://x-access-token:ghp_SECRET@github.com/o/r: Authentication failed";
        let f = classify(&MirrorError::Fetch(secret.to_owned()));
        assert_eq!(f.code, MirrorErrorCode::AuthFailed);
        assert!(!f.message.contains("ghp_SECRET"));
        assert!(!f.message.contains("github.com"));
        let f = classify(&MirrorError::Fetch(
            "fatal: repository 'https://h/x' not found".to_owned(),
        ));
        assert_eq!(f.code, MirrorErrorCode::NotFound);
        let f = classify(&MirrorError::Api(ApiError::LeaseLost {
            repo: "r".to_owned(),
        }));
        assert_eq!(f.code, MirrorErrorCode::LeaseLost);
        assert!(
            classify(&MirrorError::Api(ApiError::Unavailable {
                retry_after_ms: 0
            }))
            .server
        );
    }

    #[test]
    fn messages_are_capped() {
        let f = PollFailure::new(MirrorErrorCode::Internal, "é".repeat(300));
        assert!(f.message.len() <= MESSAGE_MAX);
    }
}
