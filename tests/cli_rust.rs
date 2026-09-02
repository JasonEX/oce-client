use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use oce_client::identity::calculate_blob_identity;
use serde_json::{Value, json};

mod common;

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
    let (api_url, server) = common::fake_oce_server(vec![
        (
            "/find-missing",
            common::respond(|request| {
                json!({
                    "unknown_memory_names": request["mem_object_names"],
                    "nonindexed_blob_names": []
                })
            }),
        ),
        (
            "/batch-upload",
            common::respond(|request| {
                let names = request["blobs"]
                    .as_array()
                    .unwrap()
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
            }),
        ),
        (
            "/agents/blob-status",
            common::json_response(json!({
                "unknown_blob_names": [],
                "nonindexed_blob_names": [],
                "checkpoint_not_found": false
            })),
        ),
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
fn rust_binary_runs_mcp_stdio() {
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
}

#[test]
fn rust_list_files_reports_admitted_files_without_a_server() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/a.py"), "print(1)").unwrap();
    fs::write(root.path().join(".env"), "SECRET=1").unwrap();
    fs::write(root.path().join("blob.bin"), b"\0\x01").unwrap();
    let output = clean_command()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--state-path",
            root.path().join("state.sqlite3").to_str().unwrap(),
            "list-files",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["files"], json!(["src/a.py"]));
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
