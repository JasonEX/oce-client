use oce_client::config::{ClientSettings, InitialSync, McpConfiguration};
use oce_client::mcp::McpServer;
use serde_json::json;

mod common;

#[test]
fn rust_mcp_initializes_lists_and_calls_retrieval() {
    let root = tempfile::tempdir().expect("workspace");
    let (api_url, server_thread) = common::fake_oce_server(vec![
        (
            "/checkpoint-blobs",
            common::json_response(json!({"new_checkpoint_id": "chain:1"})),
        ),
        (
            "/agents/codebase-retrieval",
            common::json_response(json!({
                "formatted_retrieval": "retrieved context",
                "codebase_retrieval_elapsed_ms": 3
            })),
        ),
    ]);
    let server = McpServer::new(McpConfiguration {
        workspaces: vec![ClientSettings {
            root: root.path().canonicalize().unwrap(),
            api_url,
            api_key: "test-key".to_owned(),
            state_path: None,
            runtime_patterns: Vec::new(),
        }],
        debounce_ms: 10,
        initial_sync: InitialSync::Off,
        ready_timeout_seconds: 1.0,
    })
    .expect("MCP server");

    assert!(
        server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }))
            .is_none()
    );
    let initialized = server
        .handle_message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "1"}
            }
        }))
        .expect("initialize response");
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(initialized["result"]["serverInfo"]["name"], "oce-client");

    let listed = server
        .handle_message(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .expect("tools/list response");
    let tool = &listed["result"]["tools"][0];
    assert_eq!(tool["name"], "codebase-retrieval");
    assert_eq!(
        tool["inputSchema"]["required"],
        json!(["information_request"])
    );

    let called = server
        .handle_message(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "codebase-retrieval",
                "arguments": {"information_request": "find x"}
            }
        }))
        .expect("tools/call response");
    let result = &called["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["status"], "ready");
    assert_eq!(
        result["structuredContent"]["formatted_retrieval"],
        "retrieved context"
    );
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("retrieved context")
    );

    server.stop().expect("stop MCP indexer");
    server_thread.join().expect("fake OCE server");
}
