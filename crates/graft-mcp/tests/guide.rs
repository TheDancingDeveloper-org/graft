//! Documentation coverage for the public MCP tool registry.

use std::fs;

#[test]
fn mcp_guide_lists_every_registered_tool() {
    let guide = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide/mcp.md"),
    )
    .expect("read docs/guide/mcp.md");

    let missing: Vec<&str> = graft_mcp::TOOLS
        .iter()
        .map(|tool| tool.name)
        .filter(|name| !guide.contains(&format!("### `{name}`")))
        .collect();

    assert!(
        missing.is_empty(),
        "docs/guide/mcp.md is missing registered MCP tools: {}",
        missing.join(", ")
    );
}
