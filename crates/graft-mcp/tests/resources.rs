//! The `graft://` resources, read over the MCP protocol: the templates a
//! client lists, and what `resources/read` returns for each URI shape.

mod support;

use rmcp::model::{ReadResourceRequestParams, ResourceContents};
use serde_json::Value;
use support::{Fake, cid, connect};

/// The JSON text of the first content of a resource read.
async fn read_json(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    uri: &str,
) -> Result<Value, String> {
    let result = client
        .read_resource(ReadResourceRequestParams::new(uri))
        .await
        .map_err(|e| e.to_string())?;
    let ResourceContents::TextResourceContents {
        text,
        mime_type,
        uri: got,
        ..
    } = &result.contents[0]
    else {
        panic!("resources are text");
    };
    assert_eq!(got, uri, "the content names its URI");
    assert_eq!(mime_type.as_deref(), Some("application/json"));
    Ok(serde_json::from_str(text).expect("the content is JSON"))
}

#[tokio::test]
async fn the_templates_are_listed() {
    let client = connect(Fake::with_files()).await;
    let templates = client
        .list_all_resource_templates()
        .await
        .expect("resources/templates/list");
    let uris: Vec<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();
    assert_eq!(
        uris,
        [
            "graft://{repo}/{changeset}",
            "graft://{repo}/{changeset}/{path}"
        ]
    );
    for template in &templates {
        assert_eq!(template.mime_type.as_deref(), Some("application/json"));
        assert!(
            template
                .description
                .as_deref()
                .is_some_and(|d| !d.is_empty())
        );
    }
}

#[tokio::test]
async fn a_changeset_resource_is_its_metadata_and_first_patch_page() {
    let client = connect(Fake::with_files()).await;
    let page = read_json(&client, "graft://r/main").await.unwrap();
    assert_eq!(page["changeset"]["id"], cid(3).to_string());
    assert_eq!(page["changeset"]["state"], "landed");
    let files = page["files"].as_array().unwrap();
    assert_eq!(files.len(), 5, "the changes of c3 are the first page");
    assert!(page["next"].is_null());
}

#[tokio::test]
async fn a_file_resource_is_the_first_chunk_of_the_file() {
    let client = connect(Fake::with_files()).await;
    let file = read_json(&client, "graft://r/main/README.md")
        .await
        .unwrap();
    assert_eq!(file["path"], "README.md");
    assert_eq!(file["encoding"], "utf8");
    assert_eq!(file["offset"], 0);
    assert!(file["content"].as_str().unwrap().starts_with("# fixture\n"));
}

#[tokio::test]
async fn a_file_resource_keeps_interior_slashes_in_the_path() {
    let client = connect(Fake::with_files()).await;
    let file = read_json(&client, "graft://r/main/src/util/mod.rs")
        .await
        .unwrap();
    assert_eq!(file["path"], "src/util/mod.rs");
    assert!(file["content"].as_str().unwrap().contains("pub fn id"));
}

#[tokio::test]
async fn a_resource_names_a_changeset_id_too() {
    let client = connect(Fake::with_files()).await;
    let page = read_json(&client, &format!("graft://r/{}", cid(2)))
        .await
        .unwrap();
    assert_eq!(page["changeset"]["id"], cid(2).to_string());
}

#[tokio::test]
async fn an_unknown_path_is_an_error_with_the_kind() {
    let client = connect(Fake::with_files()).await;
    let error = read_json(&client, "graft://r/main/nope.txt")
        .await
        .unwrap_err();
    assert!(error.contains("not_found"), "{error}");
}

#[tokio::test]
async fn an_unknown_ref_is_an_error() {
    let client = connect(Fake::with_files()).await;
    let error = read_json(&client, "graft://r/nope").await.unwrap_err();
    assert!(error.contains("not_found"), "{error}");
}

#[tokio::test]
async fn a_uri_that_is_not_graft_is_an_error() {
    let client = connect(Fake::with_files()).await;
    assert!(read_json(&client, "https://r/main").await.is_err());
    assert!(read_json(&client, "graft://r").await.is_err());
}
