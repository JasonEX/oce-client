use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use oce_client::identity::calculate_blob_identity;
use serde_json::{Value, json};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oce-client"))
}

fn clean_command() -> Command {
    let mut command = Command::new(binary());
    for name in [
        "OCE_API_URL",
        "OCE_API_KEY",
        "OCE_WORKSPACE",
        "OCE_WORKSPACES",
        "OCE_STATE_PATH",
        "OCE_STATE_DIR",
        "OCE_IGNORE",
        "OCE_DEBOUNCE_MS",
        "OCE_INITIAL_SYNC",
        "OCE_READY_TIMEOUT",
    ] {
        command.env_remove(name);
    }
    command
}

#[test]
fn rust_cli_sync_and_retrieve_run_the_real_wire_and_state_path() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.py"), "print(1)").unwrap();
    let state_path = root.path().join("rust-state.sqlite3");
    let (api_url, server) = fake_oce_server();

    let sync = clean_command()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--api-url",
            &api_url,
            "--state-path",
            state_path.to_str().unwrap(),
            "sync",
            "--json",
        ])
        .env("OCE_API_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        sync.status.success(),
        "sync stderr: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    let sync: Value = serde_json::from_slice(&sync.stdout).unwrap();
    assert_eq!(sync["checkpoint_id"], "chain:1");
    assert_eq!(sync["uploaded_blob_names"].as_array().unwrap().len(), 1);

    let retrieve = clean_command()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--api-url",
            &api_url,
            "--state-path",
            state_path.to_str().unwrap(),
            "retrieve",
            "where is a?",
            "--json",
        ])
        .env("OCE_API_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        retrieve.status.success(),
        "retrieve stderr: {}",
        String::from_utf8_lossy(&retrieve.stderr)
    );
    let retrieve: Value = serde_json::from_slice(&retrieve.stdout).unwrap();
    assert_eq!(retrieve["formatted_retrieval"], "retrieved context");

    server.join().expect("fake OCE server");
}

#[test]
fn rust_cli_observe_and_status_preserve_explicit_content_state() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let common = [
        "--root",
        "~/workspace",
        "--state-path",
        "~/.oce-client-test/state.sqlite3",
    ];
    let observed = clean_command()
        .args(common)
        .args(["observe", "a.py", "--content", "unsaved", "--json"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(observed.status.success());

    let status = clean_command()
        .args(common)
        .args(["status", "--json"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["files"]["a.py"]["source"], "explicit");
    assert_eq!(status["files"]["a.py"]["status"], "present");
}

#[test]
fn rust_binary_runs_mcp_stdio_and_supports_legacy_argv0() {
    let root = tempfile::tempdir().unwrap();
    let mut process = clean_command();
    let mut process = process
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "mcp",
            "--initial-sync",
            "off",
        ])
        .env("OCE_API_KEY", "test-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    for message in [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}
            }
        }),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "MCP stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "oce-client");
    assert_eq!(
        responses[1]["result"]["tools"][0]["name"],
        "codebase-retrieval"
    );

    let alias_directory = tempfile::tempdir().unwrap();
    let alias = alias_directory.path().join(if cfg!(windows) {
        "oce-client-mcp.exe"
    } else {
        "oce-client-mcp"
    });
    fs::copy(binary(), &alias).unwrap();
    let help = Command::new(alias).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("--workspace")
    );
}

#[test]
fn rust_skill_is_embedded_in_the_single_binary() {
    let root = tempfile::tempdir().unwrap();
    let output = clean_command()
        .args(["skill", "path", "--json"])
        .env("CODEX_HOME", root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    let path = PathBuf::from(payload["path"].as_str().unwrap());
    assert!(path.join("SKILL.md").is_file());
    assert!(path.join("agents/openai.yaml").is_file());
}

fn fake_oce_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        for expected in [
            "/find-missing",
            "/batch-upload",
            "/agents/blob-status",
            "/checkpoint-blobs",
            "/agents/codebase-retrieval",
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let (path, request) = read_request(&mut stream);
            assert_eq!(path, expected);
            let response = match expected {
                "/find-missing" => json!({
                    "unknown_memory_names": request["mem_object_names"],
                    "nonindexed_blob_names": []
                }),
                "/batch-upload" => {
                    let blobs = request["blobs"].as_array().unwrap();
                    let names = blobs
                        .iter()
                        .map(|blob| {
                            calculate_blob_identity(
                                blob["path"].as_str().unwrap(),
                                blob["content"].as_str().unwrap(),
                            )
                            .unwrap()
                        })
                        .collect::<Vec<_>>();
                    json!({"blob_names": names})
                }
                "/agents/blob-status" => json!({
                    "unknown_blob_names": [],
                    "nonindexed_blob_names": [],
                    "checkpoint_not_found": false
                }),
                "/checkpoint-blobs" => json!({"new_checkpoint_id": "chain:1"}),
                "/agents/codebase-retrieval" => json!({
                    "formatted_retrieval": "retrieved context",
                    "codebase_retrieval_elapsed_ms": 3
                }),
                _ => unreachable!(),
            };
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

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
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
    let length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() - header_end < length {
        let count = stream.read(&mut buffer).unwrap();
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        path,
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    )
}
