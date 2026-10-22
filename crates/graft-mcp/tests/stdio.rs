//! The server over the stdio framing, and the tool schemas a client sees.

mod support;

use std::collections::BTreeSet;

use graft_mcp::TOOLS;
use serde_json::json;
use support::{Fake, call, cid, connect};

#[tokio::test]
async fn the_handshake_and_tool_list_match_the_design() {
    let client = connect(Fake::standard()).await;
    let tools = client.list_all_tools().await.expect("tools/list");

    let served: BTreeSet<String> = tools.iter().map(|t| t.name.to_string()).collect();
    let read_only: BTreeSet<String> = TOOLS
        .iter()
        .filter(|t| t.read_only)
        .map(|t| t.name.to_owned())
        .collect();
    assert_eq!(served, read_only, "exactly the read tools are served");

    for tool in &tools {
        let annotations = tool.annotations.as_ref().expect("annotations");
        assert_eq!(
            annotations.read_only_hint,
            Some(true),
            "{} is read-only",
            tool.name
        );

        let schema = serde_json::to_value(&*tool.input_schema).unwrap();
        assert_eq!(schema["type"], "object", "{} takes an object", tool.name);
        assert!(
            schema["properties"]["repo"].is_object(),
            "{} names a repo",
            tool.name
        );
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("repo")),
            "{} requires repo",
            tool.name
        );
        assert_eq!(
            schema["additionalProperties"], false,
            "{} rejects unknown arguments",
            tool.name
        );
        assert!(
            tool.description.as_deref().is_some_and(|d| !d.is_empty()),
            "{} has a description",
            tool.name
        );
    }
}

#[tokio::test]
async fn a_call_round_trips_over_the_framing() {
    let client = connect(Fake::standard()).await;
    let result = call(
        &client,
        "graft_log",
        json!({"repo": "r", "from": cid(2).to_string()}),
    )
    .await;
    assert_eq!(result.is_error, Some(false));
    let page = result.structured_content.expect("structured content");
    assert_eq!(page["changesets"][0]["id"], cid(2).to_string());
    assert_eq!(page["changesets"][1]["id"], cid(1).to_string());
    assert!(page["next"].is_null());
}

#[tokio::test]
async fn unknown_arguments_are_refused_by_name() {
    let client = connect(Fake::standard()).await;
    let arguments = json!({"repo": "r", "from": "main", "bogus": true});
    let result = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("graft_log")
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .expect("the call completes");
    assert_eq!(result.is_error, Some(true));
    let text = format!("{:?}", result.content);
    assert!(
        text.contains("bogus"),
        "the error names the unknown field: {text}"
    );
}
