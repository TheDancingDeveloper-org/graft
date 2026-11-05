//! `graft_repo_create` (D41): served through the launcher's `RepoAdmin`,
//! with the API's typed refusal passed back, and unavailable without one.

mod support;

use std::sync::{Arc, Mutex};

use graft_mcp::{McpServer, RepoAdmin, RepoAdminFuture};
use graft_proto::{ApiError, CallContext};
use rmcp::ServiceExt;
use serde_json::json;
use support::{Fake, call, principal};

/// Creates what it is asked for, unless the caller lacks `repos:admin`.
#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl RepoAdmin for Recording {
    fn create_repo<'a>(&'a self, cx: &'a CallContext, name: String) -> RepoAdminFuture<'a> {
        Box::pin(async move {
            if !cx.principal.scopes.iter().any(|s| s == "repos:admin") {
                return Err(ApiError::PermissionDenied {
                    scope: "repos:admin".to_owned(),
                    path: None,
                });
            }
            self.0.lock().unwrap().push(name);
            Ok(())
        })
    }
}

async fn serve(
    scopes: &[&str],
    admin: Option<Arc<dyn RepoAdmin>>,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let mut principal = principal();
    principal.scopes = scopes.iter().map(|s| s.to_string()).collect();
    let mut server = McpServer::new(Fake::standard(), principal);
    if let Some(admin) = admin {
        server = server.with_repo_admin(admin);
    }
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(server_io);
    tokio::spawn(async move {
        let _ = server.serve_io(read, write).await;
    });
    let (read, write) = tokio::io::split(client_io);
    ().serve((read, write)).await.expect("handshake")
}

#[tokio::test]
async fn repo_create_goes_through_the_repo_admin() {
    let admin = Arc::new(Recording::default());
    let client = serve(&["repos:admin"], Some(admin.clone())).await;
    let result = call(&client, "graft_repo_create", json!({"repo": "made"})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(result.structured_content, Some(json!({"repo": "made"})));
    assert_eq!(*admin.0.lock().unwrap(), ["made"]);
}

#[tokio::test]
async fn repo_create_passes_back_a_permission_refusal() {
    let admin = Arc::new(Recording::default());
    let client = serve(&["repo:r:admin"], Some(admin.clone())).await;
    let result = call(&client, "graft_repo_create", json!({"repo": "made"})).await;
    assert_eq!(result.is_error, Some(true));
    let text = serde_json::to_string(&result).unwrap();
    assert!(text.contains("permission_denied"), "{text}");
    assert!(admin.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn repo_create_is_unavailable_without_a_repo_admin() {
    let client = serve(&["repos:admin"], None).await;
    let result = call(&client, "graft_repo_create", json!({"repo": "made"})).await;
    assert_eq!(result.is_error, Some(true));
}
