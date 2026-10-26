//! The server over the stdio framing, and the tool schemas a client sees.

mod support;

use std::collections::BTreeSet;

use graft_mcp::{TOOLS, ToolBound};
use serde_json::json;
use support::{Fake, call, cid, connect};

#[tokio::test]
async fn the_handshake_and_tool_list_match_the_design() {
    let client = connect(Fake::standard()).await;
    let tools = client.list_all_tools().await.expect("tools/list");

    let served: BTreeSet<String> = tools.iter().map(|t| t.name.to_string()).collect();
    // Every design tool, `graft_revert` included.
    let designed: BTreeSet<String> = TOOLS.iter().map(|t| t.name.to_owned()).collect();
    assert_eq!(served, designed, "the read and write tools are served");

    for tool in &tools {
        let annotations = tool.annotations.as_ref().expect("annotations");
        let spec = TOOLS.iter().find(|t| t.name == tool.name).unwrap();
        assert!(matches!(
            spec.bound,
            ToolBound::Paginated | ToolBound::Capped
        ));
        assert_eq!(
            annotations.read_only_hint,
            Some(spec.read_only),
            "{} read-only hint",
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
async fn land_defaults_to_a_line_merge_in_its_schema() {
    let client = connect(Fake::standard()).await;
    let tools = client.list_all_tools().await.expect("tools/list");
    let land = tools.iter().find(|t| t.name == "graft_land").unwrap();
    let schema = serde_json::to_value(&*land.input_schema).unwrap();
    let merge = &schema["properties"]["merge"];
    assert_eq!(merge["default"], "line", "{merge}");
    let described = merge["description"].as_str().unwrap();
    assert!(described.starts_with("`line` (default)"), "{described}");
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

#[tokio::test]
async fn no_tool_description_names_the_tracker() {
    let client = connect(Fake::standard()).await;
    let tools = client.list_all_tools().await.expect("tools/list");
    assert!(tools.len() > 1, "the server lists its tools");
    for tool in &tools {
        let description = tool.description.as_deref().unwrap_or("");
        assert!(
            !description.to_ascii_lowercase().contains("vogt"),
            "{} description names the tracker: {description}",
            tool.name
        );
        let schema = serde_json::to_value(&*tool.input_schema).unwrap();
        let properties = schema["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (name, prop) in properties {
            let text = prop["description"].as_str().unwrap_or("");
            assert!(
                !text.to_ascii_lowercase().contains("vogt"),
                "{} parameter {name} names the tracker: {text}",
                tool.name
            );
        }
    }
}
