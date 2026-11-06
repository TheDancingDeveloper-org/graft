//! The mirror and batch tools (D41 §3.2, §3.5) over a launcher's
//! `MirrorAdmin`: the server's typed refusal passed back to an agent, a cut
//! retried with the same `client_request_id`, bounded pages, and no
//! credential in any result.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use graft_mcp::{McpServer, MirrorAdmin, MirrorFuture};
use graft_proto::{ApiError, CallContext, pb};
use rmcp::ServiceExt;
use serde_json::json;
use support::{Fake, call, principal};

const UP: &str = "https://github.com/o/r.git";

/// A mirror service in memory with the server's grant rules for the calls
/// these tests make. `unavailable` makes the next N cuts fail first.
#[derive(Default)]
struct Service {
    rows: Mutex<BTreeMap<(String, String), pb::Mirror>>,
    cuts: Mutex<Vec<pb::MirrorRequest>>,
    attempts: Mutex<Vec<String>>,
    unavailable: Mutex<u32>,
}

fn holds(cx: &CallContext, scope: &str) -> bool {
    cx.principal.scopes.iter().any(|s| s == scope)
}

fn denied(scope: &str) -> ApiError {
    ApiError::PermissionDenied {
        scope: scope.to_owned(),
        path: None,
    }
}

impl MirrorAdmin for Service {
    fn set_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::SetMirrorRequest,
    ) -> MirrorFuture<'a, pb::Mirror> {
        Box::pin(async move {
            let key = (req.repo.clone(), req.upstream.clone());
            let mut rows = self.rows.lock().unwrap();
            let old = rows.get(&key).cloned();
            match &old {
                None if !holds(cx, "mirror:bind") => return Err(denied("mirror:bind")),
                Some(_) if !holds(cx, "repo:r:admin") && !holds(cx, "mirror:bind") => {
                    return Err(denied("repo:r:admin"));
                }
                _ => {}
            }
            if req.expected_version != old.as_ref().map(|o| o.version) {
                return Err(ApiError::Rejected {
                    policy: "mirror-version".into(),
                    detail: "stale".into(),
                });
            }
            let row = pb::Mirror {
                repo: req.repo,
                upstream: req.upstream,
                lines: req.lines,
                interval_s: req.interval_s.max(15),
                enabled: req.enabled,
                credential_set: old.as_ref().is_some_and(|o| o.credential_set),
                kek_id: old.as_ref().and_then(|o| o.kek_id.clone()),
                export: req.export,
                version: old.map_or(1, |o| o.version + 1),
                status: None,
                created_by: cx.principal.subject.clone(),
                updated_by: cx.principal.subject.clone(),
                updated_at_ms: 1,
            };
            rows.insert(key, row.clone());
            Ok(row)
        })
    }
    fn get_mirror<'a>(
        &'a self,
        _cx: &'a CallContext,
        req: pb::MirrorKey,
    ) -> MirrorFuture<'a, pb::Mirror> {
        Box::pin(async move {
            self.rows
                .lock()
                .unwrap()
                .get(&(req.repo, req.upstream))
                .cloned()
                .ok_or(ApiError::NotFound {
                    what: "mirror".into(),
                })
        })
    }
    fn list_mirrors<'a>(
        &'a self,
        _cx: &'a CallContext,
        req: pb::ListMirrorsRequest,
    ) -> MirrorFuture<'a, pb::MirrorPage> {
        Box::pin(async move {
            let rows = self.rows.lock().unwrap();
            let skip: usize = req.page_token.parse().unwrap_or(0);
            let all: Vec<_> = rows.values().cloned().collect();
            let page: Vec<_> = all
                .iter()
                .skip(skip)
                .take(req.limit as usize)
                .cloned()
                .collect();
            let next = skip + page.len();
            Ok(pb::MirrorPage {
                mirrors: page,
                next_page_token: if next < all.len() {
                    next.to_string()
                } else {
                    String::new()
                },
            })
        })
    }
    fn remove_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        _req: pb::RemoveMirrorRequest,
    ) -> MirrorFuture<'a, pb::RemoveMirrorResult> {
        Box::pin(async move {
            if !holds(cx, "mirror:bind") {
                return Err(denied("mirror:bind"));
            }
            Ok(pb::RemoveMirrorResult::default())
        })
    }
    fn purge_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        _req: pb::PurgeMirrorRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest> {
        Box::pin(async move {
            if !holds(cx, "mirror:bind") {
                return Err(denied("mirror:bind"));
            }
            Ok(pb::MirrorRequest::default())
        })
    }
    fn set_mirror_credential<'a>(
        &'a self,
        _cx: &'a CallContext,
        _req: pb::SetMirrorCredentialRequest,
    ) -> MirrorFuture<'a, pb::Mirror> {
        Box::pin(async { Err(denied("mirror:bind")) })
    }
    fn cut_batch<'a>(
        &'a self,
        _cx: &'a CallContext,
        req: pb::CutBatchRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest> {
        Box::pin(async move {
            self.attempts
                .lock()
                .unwrap()
                .push(req.client_request_id.clone());
            {
                let mut left = self.unavailable.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return Err(ApiError::Unavailable { retry_after_ms: 1 });
                }
            }
            let mut cuts = self.cuts.lock().unwrap();
            if let Some(seen) = cuts
                .iter()
                .find(|c| c.client_request_id == req.client_request_id)
            {
                return Ok(seen.clone());
            }
            let r = pb::MirrorRequest {
                repo: req.repo,
                id: cuts.len() as u64 + 1,
                line: req.line,
                kind: pb::MirrorRequestKind::CutBatch as i32,
                client_request_id: req.client_request_id,
                state: pb::MirrorRequestState::Pending as i32,
                batch_state: pb::BatchState::Requested as i32,
                ..Default::default()
            };
            cuts.push(r.clone());
            Ok(r)
        })
    }
    fn get_batch<'a>(
        &'a self,
        _cx: &'a CallContext,
        _req: pb::BatchKey,
    ) -> MirrorFuture<'a, pb::BatchView> {
        Box::pin(async move {
            Ok(pb::BatchView {
                batch: self.cuts.lock().unwrap().last().cloned(),
            })
        })
    }
    fn list_batches<'a>(
        &'a self,
        _cx: &'a CallContext,
        _req: pb::ListBatchesRequest,
    ) -> MirrorFuture<'a, pb::BatchPage> {
        Box::pin(async move {
            Ok(pb::BatchPage {
                batches: self.cuts.lock().unwrap().clone(),
                next_page_token: String::new(),
            })
        })
    }
    fn cancel_request<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::CancelRequestRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest> {
        Box::pin(async move {
            if !holds(cx, "mirror:bind") {
                return Err(denied("mirror:bind"));
            }
            let mut cuts = self.cuts.lock().unwrap();
            let r = cuts
                .iter_mut()
                .find(|r| r.id == req.id)
                .ok_or(ApiError::NotFound {
                    what: format!("mirror request {}", req.id),
                })?;
            r.state = pb::MirrorRequestState::Failed as i32;
            r.batch_state = pb::BatchState::Settled as i32;
            Ok(r.clone())
        })
    }
}

async fn serve(
    scopes: &[&str],
    admin: Option<Arc<dyn MirrorAdmin>>,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let mut principal = principal();
    principal.scopes = scopes.iter().map(|s| s.to_string()).collect();
    let mut server = McpServer::new(Fake::standard(), principal);
    if let Some(admin) = admin {
        server = server.with_mirror_admin(admin);
    }
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(server_io);
    tokio::spawn(async move {
        let _ = server.serve_io(read, write).await;
    });
    let (read, write) = tokio::io::split(client_io);
    ().serve((read, write)).await.expect("handshake")
}

fn text(result: &rmcp::model::CallToolResult) -> String {
    serde_json::to_string(result).unwrap()
}

#[tokio::test]
async fn an_agent_adding_removing_or_purging_gets_a_typed_permission_error() {
    let service = Arc::new(Service::default());
    let client = serve(&["repo:r:admin"], Some(service.clone())).await;
    let key = json!({"repo": "r", "upstream": UP});
    for (tool, args) in [
        (
            "graft_mirror_add",
            json!({"repo": "r", "upstream": UP, "lines": [{"branch": "main", "line": "main"}]}),
        ),
        ("graft_mirror_remove", key.clone()),
        ("graft_mirror_purge", key.clone()),
    ] {
        let result = call(&client, tool, args).await;
        assert_eq!(result.is_error, Some(true), "{tool}");
        let value = result.structured_content.as_ref().expect("structured");
        assert_eq!(value["kind"], "permission_denied", "{tool}: {value}");
    }
    assert!(service.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_repo_admin_sets_what_an_operator_added() {
    let service = Arc::new(Service::default());
    let operator = serve(&["mirror:bind"], Some(service.clone())).await;
    let added = call(
        &operator,
        "graft_mirror_add",
        json!({"repo": "r", "upstream": UP, "lines": [{"branch": "main", "line": "main"}]}),
    )
    .await;
    assert_ne!(added.is_error, Some(true), "{added:?}");

    let agent = serve(&["repo:r:admin"], Some(service.clone())).await;
    let set = call(
        &agent,
        "graft_mirror_set",
        json!({"repo": "r", "upstream": UP, "enabled": false, "interval_s": 60}),
    )
    .await;
    assert_ne!(set.is_error, Some(true), "{set:?}");
    let row = &set.structured_content.as_ref().unwrap()["mirror"];
    assert_eq!(row["enabled"], false);
    assert_eq!(row["interval_s"], 60);
    assert_eq!(row["version"], 2);
    assert_eq!(row["lines"][0]["direction"], "follow");
}

#[tokio::test]
async fn a_cut_retried_after_unavailable_reuses_its_request_id() {
    let service = Arc::new(Service::default());
    *service.unavailable.lock().unwrap() = 1;
    let client = serve(&["repo:r:admin"], Some(service.clone())).await;
    let first = call(
        &client,
        "graft_batch_cut",
        json!({"repo": "r", "line": "main"}),
    )
    .await;
    assert_ne!(first.is_error, Some(true), "{first:?}");
    let attempts = service.attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 2, "one failed attempt, one retry");
    assert_eq!(attempts[0], attempts[1], "the retry sends the same key");
    let request = &first.structured_content.as_ref().unwrap()["request"];
    assert_eq!(request["client_request_id"], attempts[0].as_str());
    assert_eq!(request["kind"], "cut_batch");
    assert_eq!(service.cuts.lock().unwrap().len(), 1);

    // The caller's own retry, with the key it was given, gets the same request.
    let again = call(
        &client,
        "graft_batch_cut",
        json!({"repo": "r", "line": "main", "client_request_id": attempts[0]}),
    )
    .await;
    assert_eq!(
        again.structured_content.as_ref().unwrap()["request"]["id"],
        request["id"]
    );
    assert_eq!(service.cuts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn only_an_operator_cancels_a_stuck_batch() {
    let service = Arc::new(Service::default());
    let agent = serve(&["repo:r:admin"], Some(service.clone())).await;
    let cut = call(
        &agent,
        "graft_batch_cut",
        json!({"repo": "r", "line": "main"}),
    )
    .await;
    let id = cut.structured_content.as_ref().unwrap()["request"]["id"].clone();
    let refused = call(&agent, "graft_batch_cancel", json!({"repo": "r", "id": id})).await;
    assert_eq!(refused.is_error, Some(true));
    let value = refused.structured_content.as_ref().expect("structured");
    assert_eq!(value["kind"], "permission_denied", "{value}");

    let operator = serve(&["mirror:bind"], Some(service.clone())).await;
    let done = call(
        &operator,
        "graft_batch_cancel",
        json!({"repo": "r", "id": id}),
    )
    .await;
    assert_ne!(done.is_error, Some(true), "{done:?}");
    let request = &done.structured_content.as_ref().unwrap()["request"];
    assert_eq!(request["state"], "failed");
    assert_eq!(request["batch_state"], "settled");
}

#[tokio::test]
async fn a_cut_that_stays_unavailable_names_the_key_to_retry_with() {
    let service = Arc::new(Service::default());
    *service.unavailable.lock().unwrap() = 10;
    let client = serve(&["repo:r:admin"], Some(service.clone())).await;
    let result = call(
        &client,
        "graft_batch_cut",
        json!({"repo": "r", "line": "main", "client_request_id": "k-1"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("k-1"), "{}", text(&result));
    let attempts = service.attempts.lock().unwrap().clone();
    assert!(attempts.iter().all(|a| a == "k-1"), "{attempts:?}");
}

#[tokio::test]
async fn status_and_list_say_a_credential_is_set_but_never_carry_it() {
    let service = Arc::new(Service::default());
    service.rows.lock().unwrap().insert(
        ("r".into(), UP.into()),
        pb::Mirror {
            repo: "r".into(),
            upstream: UP.into(),
            credential_set: true,
            kek_id: Some("k1".into()),
            version: 3,
            ..Default::default()
        },
    );
    let client = serve(&["repo:r:read"], Some(service)).await;
    let status = call(
        &client,
        "graft_mirror_status",
        json!({"repo": "r", "upstream": UP}),
    )
    .await;
    let row = &status.structured_content.as_ref().unwrap()["mirror"];
    assert_eq!(row["credential_set"], true);
    assert_eq!(row["kek_id"], "k1");
    let list = call(&client, "graft_mirror_list", json!({"repo": "r"})).await;
    for result in [&status, &list] {
        let body = text(result);
        assert!(!body.contains("\"credential\""), "{body}");
    }
}

#[tokio::test]
async fn mirror_list_pages_with_a_token() {
    let service = Arc::new(Service::default());
    for n in 0..3 {
        let up = format!("https://github.com/o/r{n}.git");
        service.rows.lock().unwrap().insert(
            ("r".into(), up.clone()),
            pb::Mirror {
                repo: "r".into(),
                upstream: up,
                ..Default::default()
            },
        );
    }
    let client = serve(&["repo:r:read"], Some(service)).await;
    let first = call(
        &client,
        "graft_mirror_list",
        json!({"repo": "r", "limit": 2}),
    )
    .await;
    let page = first.structured_content.unwrap();
    assert_eq!(page["mirrors"].as_array().unwrap().len(), 2);
    let token = page["next_page_token"].as_str().expect("more").to_owned();
    let second = call(
        &client,
        "graft_mirror_list",
        json!({"repo": "r", "limit": 2, "page_token": token}),
    )
    .await;
    let page = second.structured_content.unwrap();
    assert_eq!(page["mirrors"].as_array().unwrap().len(), 1);
    assert!(page["next_page_token"].is_null());

    let over = call(
        &client,
        "graft_mirror_list",
        json!({"repo": "*", "limit": 101}),
    )
    .await;
    assert_eq!(over.is_error, Some(true));
}

#[tokio::test]
async fn the_tools_are_unavailable_without_a_mirror_admin() {
    let client = serve(&["mirror:bind"], None).await;
    let result = call(
        &client,
        "graft_mirror_status",
        json!({"repo": "r", "upstream": UP}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("not available"));
}
