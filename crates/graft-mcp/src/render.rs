//! Bounded results: the byte cap, paging that shrinks to fit, text clipping,
//! and the mapping from typed API errors to tool results (`docs/design.md`
//! §7.10, §10.1).

use std::future::Future;

use graft_proto::ApiError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::workcopy::WorkingCopyError;

/// Every tool result, serialized as a whole, is at most this many bytes.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;

/// Patch text kept per file, in bytes. Requested from the API and clipped
/// again here, so a misbehaving API cannot break the cap.
pub const PATCH_MAX_BYTES: usize = 4 * 1024;

/// Subjects, names, and metadata keys and values are clipped to this.
pub(crate) const FIELD_MAX_BYTES: usize = 256;

/// Changeset messages are clipped to this.
pub(crate) const MESSAGE_MAX_BYTES: usize = 8 * 1024;

/// Clips `s` to at most `max` bytes, cutting only at a character boundary.
/// Returns the kept prefix and whether anything was cut.
pub(crate) fn clip(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// Why a tool produced no result.
#[derive(Debug)]
pub(crate) enum ToolError {
    /// The API returned a typed error.
    Api(ApiError),
    /// An argument did not parse or was out of range.
    Invalid(String),
    /// The tool needs a working copy, which only `graft mcp` over stdio has.
    NeedsWorkingCopy {
        /// The tool name.
        tool: &'static str,
    },
    /// Not even a single item fits under [`MAX_RESULT_BYTES`].
    TooLarge,
    /// The working copy could not do it.
    Local {
        /// Its kind.
        kind: &'static str,
        /// What happened.
        message: String,
    },
}

impl From<WorkingCopyError> for ToolError {
    fn from(e: WorkingCopyError) -> Self {
        match e {
            WorkingCopyError::Api(e) => ToolError::Api(e),
            WorkingCopyError::Invalid(message) => ToolError::Invalid(message),
            WorkingCopyError::Local { kind, message } => ToolError::Local { kind, message },
        }
    }
}

impl ToolError {
    /// The error as a tool result with `is_error` set. Carries kinds and
    /// ids only, never file contents or tokens.
    pub(crate) fn into_result(self) -> CallToolResult {
        let value = match self {
            ToolError::Api(e) => api_error_value(&e),
            ToolError::Invalid(message) => json!({"kind": "invalid_argument", "message": message}),
            ToolError::NeedsWorkingCopy { tool } => json!({
                "kind": "needs_working_copy",
                "tool": tool,
                "message": format!(
                    "{tool} needs a working copy; it is served by `graft mcp` over stdio inside one"
                ),
            }),
            ToolError::TooLarge => {
                json!({"kind": "result_too_large", "max_bytes": MAX_RESULT_BYTES})
            }
            ToolError::Local { kind, message } => json!({
                "kind": kind,
                "message": clip(&message, MESSAGE_MAX_BYTES).0,
            }),
        };
        CallToolResult::structured_error(value)
    }
}

/// Whether a whole tool result is within [`MAX_RESULT_BYTES`].
pub(crate) fn fits(result: &CallToolResult) -> bool {
    serde_json::to_vec(result).is_ok_and(|bytes| bytes.len() <= MAX_RESULT_BYTES)
}

/// Wraps a result that is not paged. It must fit on its own, or the tool
/// reports [`ToolError::TooLarge`].
pub(crate) fn bounded(value: Value) -> Result<CallToolResult, ToolError> {
    let result = CallToolResult::structured(value);
    if fits(&result) {
        Ok(result)
    } else {
        Err(ToolError::TooLarge)
    }
}

/// Fetches one page of `size` items and renders it, halving `size` until the
/// rendered result fits the cap.
///
/// Shrinking keeps paging lossless: the page is re-requested from the same
/// resume point, so the cursor the API returns with the accepted page is
/// the one that continues it. `render` gets the size it was given, so a
/// tool can compute a continuation from the window it asked for.
pub(crate) async fn fit<T, F, Fut, R>(
    size: u32,
    mut fetch: F,
    mut render: R,
) -> Result<CallToolResult, ToolError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
    R: FnMut(T, u32) -> Value,
{
    let mut size = size.max(1);
    loop {
        let data = fetch(size).await.map_err(ToolError::Api)?;
        let result = CallToolResult::structured(render(data, size));
        if fits(&result) {
            return Ok(result);
        }
        if size == 1 {
            return Err(ToolError::TooLarge);
        }
        size /= 2;
    }
}

/// The JSON form of a typed API error: its kind, its ids, and whether a
/// client may retry the same request unchanged.
pub(crate) fn api_error_value(e: &ApiError) -> Value {
    let retryable = e.is_retryable();
    let mut value = match e {
        ApiError::NotFound { what } => json!({"kind": "not_found", "what": what}),
        ApiError::Collected { id } => json!({"kind": "collected", "id": id.to_string()}),
        ApiError::InvalidPath { path, reason } => {
            json!({"kind": "invalid_path", "path": path, "reason": reason})
        }
        ApiError::InvalidChange { path, rule } => {
            json!({"kind": "invalid_change", "path": path, "rule": rule})
        }
        ApiError::UnknownParent { id } => json!({"kind": "unknown_parent", "id": id.to_string()}),
        ApiError::MissingObjects { ids } => json!({
            "kind": "missing_objects",
            "ids": ids.iter().map(|id| id.to_hex()).collect::<Vec<_>>(),
        }),
        ApiError::RefConflict { name, actual } => json!({
            "kind": "ref_conflict",
            "name": name,
            "actual": actual.map(|id| id.to_string()),
        }),
        ApiError::Conflict { paths, onto } => {
            let hex = |id: &Option<graft_model::ObjectId>| id.map(|id| id.to_hex());
            json!({
                "kind": "conflict",
                "onto": onto.to_string(),
                "paths": paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
                "sides": paths
                    .iter()
                    .map(|p| json!({
                        "path": p.path,
                        "base": hex(&p.base),
                        "ours": hex(&p.ours),
                        "theirs": hex(&p.theirs),
                    }))
                    .collect::<Vec<_>>(),
            })
        }
        ApiError::MergeConflict(m) => {
            let mut value = api_error_value(&ApiError::Conflict {
                paths: m.paths.clone(),
                onto: m.onto,
            });
            value["base"] = json!(m.base.to_string());
            value["source"] = json!(m.source.to_string());
            value
        }
        ApiError::Contended { line, tip } => {
            json!({"kind": "contended", "line": line, "tip": tip.to_string()})
        }
        ApiError::StaleBase(detail) => json!({
            "kind": "stale_base",
            "current_tip": detail.current_tip.to_string(),
            "paths": detail.paths,
            "truncated": detail.truncated,
            "guidance": "run graft_sync, then graft_land",
        }),
        ApiError::Rejected { policy, detail } => {
            json!({"kind": "rejected", "policy": policy, "detail": detail})
        }
        ApiError::PermissionDenied { scope, path } => {
            json!({"kind": "permission_denied", "scope": scope, "path": path})
        }
        ApiError::LeaseExpired { workspace } => {
            json!({"kind": "lease_expired", "workspace": workspace})
        }
        ApiError::LimitExceeded { limit, max } => {
            json!({"kind": "limit_exceeded", "limit": limit, "max": max})
        }
        ApiError::CorruptObject { id } => json!({"kind": "corrupt_object", "id": id.to_hex()}),
        ApiError::Obliterated { id } => json!({"kind": "obliterated", "id": id.to_string()}),
        ApiError::Unavailable { retry_after_ms } => {
            json!({"kind": "unavailable", "retry_after_ms": retry_after_ms})
        }
        ApiError::Unauthenticated => json!({"kind": "unauthenticated"}),
        ApiError::IncompatibleProtocol { server } => {
            json!({"kind": "incompatible_protocol", "server": server})
        }
        _ => json!({"kind": "unknown"}),
    };
    value["retryable"] = json!(retryable);
    value
}

/// A parsed `graft://<repo>/<changeset>[/<path>]` resource URI.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResourceUri<'a> {
    /// Repository name.
    pub(crate) repo: &'a str,
    /// Changeset id or ref name.
    pub(crate) changeset: &'a str,
    /// Path in the snapshot, when the URI names one.
    pub(crate) path: Option<&'a str>,
}

/// Parses a `graft://` resource URI. The path, when present, keeps its
/// interior slashes.
pub(crate) fn parse_resource_uri(uri: &str) -> Result<ResourceUri<'_>, String> {
    let rest = uri
        .strip_prefix("graft://")
        .ok_or_else(|| format!("`{uri}` is not a graft:// URI"))?;
    let mut parts = rest.splitn(3, '/');
    let repo = parts.next().filter(|s| !s.is_empty());
    let changeset = parts.next().filter(|s| !s.is_empty());
    let (Some(repo), Some(changeset)) = (repo, changeset) else {
        return Err(format!("`{uri}` needs graft://<repo>/<changeset>[/<path>]"));
    };
    let path = parts.next().filter(|s| !s.is_empty());
    Ok(ResourceUri {
        repo,
        changeset,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::ChangesetId;

    fn id(byte: u8) -> ChangesetId {
        ChangesetId(graft_model::ObjectId([byte; 32]))
    }

    #[test]
    fn clip_cuts_only_at_char_boundaries() {
        assert_eq!(clip("abc", 3), ("abc", false));
        assert_eq!(clip("abcd", 3), ("abc", true));
        // "é" is two bytes; cutting at byte 1 would split it.
        assert_eq!(clip("é", 1), ("", true));
        assert_eq!(clip("aé", 2), ("a", true));
    }

    #[test]
    fn resource_uris_split_into_repo_changeset_and_path() {
        assert_eq!(
            parse_resource_uri("graft://repo/c1/src/a/b.rs"),
            Ok(ResourceUri {
                repo: "repo",
                changeset: "c1",
                path: Some("src/a/b.rs"),
            })
        );
        assert_eq!(
            parse_resource_uri("graft://repo/main"),
            Ok(ResourceUri {
                repo: "repo",
                changeset: "main",
                path: None,
            })
        );
        assert!(parse_resource_uri("https://repo/main").is_err());
        assert!(parse_resource_uri("graft://repo").is_err());
        assert!(parse_resource_uri("graft:///main").is_err());
    }

    #[test]
    fn api_errors_carry_kind_ids_and_retry_hint_only() {
        let v = api_error_value(&ApiError::Contended {
            line: "main".into(),
            tip: id(2),
        });
        assert_eq!(v["kind"], "contended");
        assert_eq!(v["line"], "main");
        assert_eq!(v["retryable"], true);

        let v = api_error_value(&ApiError::NotFound {
            what: "ref nope".into(),
        });
        assert_eq!(v["kind"], "not_found");
        assert_eq!(v["retryable"], false);

        let v = api_error_value(&ApiError::StaleBase(Box::new(
            graft_proto::error::StaleBase {
                current_tip: id(4),
                paths: vec!["src/lib.rs".into()],
                truncated: false,
            },
        )));
        assert_eq!(v["kind"], "stale_base");
        assert_eq!(v["current_tip"], id(4).to_string());
        assert_eq!(v["paths"], json!(["src/lib.rs"]));
        assert_eq!(v["truncated"], false);
        assert_eq!(v["guidance"], "run graft_sync, then graft_land");
        assert_eq!(v["retryable"], false);
        assert!(!v.to_string().contains("contents"));
    }

    #[test]
    fn conflict_errors_render_the_side_ids_additively() {
        let v = api_error_value(&ApiError::Conflict {
            paths: vec![graft_proto::error::ConflictPath {
                path: "f.txt".into(),
                base: Some(graft_model::ObjectId([1; 32])),
                ours: None,
                theirs: Some(graft_model::ObjectId([3; 32])),
            }],
            onto: id(9),
        });
        assert_eq!(v["kind"], "conflict");
        assert_eq!(v["paths"], json!(["f.txt"]));
        let side = &v["sides"][0];
        assert_eq!(side["path"], "f.txt");
        assert_eq!(side["base"], "01".repeat(32));
        assert_eq!(side["ours"], Value::Null);
        assert_eq!(side["theirs"], "03".repeat(32));
    }

    #[test]
    fn error_results_are_flagged_as_errors() {
        let result = ToolError::Invalid("bad".into()).into_result();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["kind"],
            "invalid_argument"
        );
    }

    #[tokio::test]
    async fn fit_halves_pages_until_the_result_fits() {
        // Each item is 10 KiB, and a result carries its JSON twice (text and
        // structured content), so 20 KiB per item: four overflow the 64 KiB
        // cap and two fit.
        let item = "x".repeat(10 * 1024);
        let result = fit(
            8,
            |size| {
                let items: Vec<String> = (0..size).map(|_| item.clone()).collect();
                std::future::ready(Ok::<_, ApiError>(items))
            },
            |items, size| {
                assert_eq!(items.len() as u32, size);
                json!({"items": items, "size": size})
            },
        )
        .await
        .expect("a page of two fits");
        assert!(fits(&result));
        assert_eq!(result.structured_content.unwrap()["size"], 2);
    }

    #[tokio::test]
    async fn fit_reports_too_large_when_one_item_cannot_fit() {
        let result = fit(
            4,
            |_| std::future::ready(Ok::<_, ApiError>(())),
            |(), _| json!({"blob": "x".repeat(MAX_RESULT_BYTES)}),
        )
        .await;
        assert!(matches!(result, Err(ToolError::TooLarge)));
    }
}
