use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use oce_client::config::{ClientSettings, InitialSync, McpConfiguration};
use oce_client::mcp::McpServer;
use serde_json::json;

#[test]
fn rust_mcp_initializes_lists_and_calls_retrieval() {
    let root = tempfile::tempdir().expect("workspace");
    let (api_url, server_thread) = fake_oce_server();
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

fn fake_oce_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake OCE");
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        for (expected_path, response) in [
            ("/checkpoint-blobs", json!({"new_checkpoint_id": "chain:1"})),
            (
                "/agents/codebase-retrieval",
                json!({
                    "formatted_retrieval": "retrieved context",
                    "codebase_retrieval_elapsed_ms": 3
                }),
            ),
        ] {
            let (mut stream, _) = listener.accept().expect("accept OCE request");
            let path = read_path_and_body(&mut stream);
            assert_eq!(path, expected_path);
            let body = serde_json::to_vec(&response).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (format!("http://{address}"), thread)
}

fn read_path_and_body(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut lines = headers.split("\r\n");
    let path = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let content_length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let count = stream.read(&mut buffer).unwrap();
        bytes.extend_from_slice(&buffer[..count]);
    }
    path
}
