use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::VERSION;
use crate::config::{ConfigurationError, InitialSync, McpConfiguration, canonical_or_absolute};
use crate::indexer::{IndexerError, Readiness, WorkspaceIndexer};

const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    LATEST_PROTOCOL_VERSION,
];

pub const TOOL_NAME: &str = "codebase-retrieval";
pub const TOOL_DESCRIPTION: &str = "Retrieve semantically relevant code sections from an indexed workspace.\n\
Use this for broad architectural, behavioral, cross-language, or unknown-location searches.\n\
For known filenames or exact identifiers, native filesystem or text search may be faster.\n\
Results are retrieval candidates and should be verified before editing.\n\
The index reflects files currently on disk and contains no version-control history.";

#[derive(Debug)]
pub struct McpServer {
    indexers: BTreeMap<PathBuf, WorkspaceIndexer>,
    initial_sync: InitialSync,
    ready_timeout: Duration,
}

impl McpServer {
    pub fn new(configuration: McpConfiguration) -> Result<Self, McpError> {
        let mut indexers = BTreeMap::new();
        for settings in configuration.workspaces {
            let context = settings.context()?;
            let root = context.root().to_path_buf();
            indexers.insert(
                root,
                WorkspaceIndexer::new(context, Duration::from_millis(configuration.debounce_ms)),
            );
        }
        Ok(Self {
            indexers,
            initial_sync: configuration.initial_sync,
            ready_timeout: Duration::from_secs_f64(configuration.ready_timeout_seconds),
        })
    }

    pub fn start(&self) -> Result<(), McpError> {
        if self.initial_sync == InitialSync::Off {
            return Ok(());
        }
        for indexer in self.indexers.values() {
            indexer.start(true)?;
        }
        if self.initial_sync == InitialSync::Blocking {
            for indexer in self.indexers.values() {
                if indexer.wait_until_ready(None)? == Readiness::Error {
                    let status = indexer.status()?;
                    return Err(McpError::InitialSync {
                        workspace: status.workspace_folder,
                        detail: status
                            .error
                            .unwrap_or_else(|| "workspace synchronization failed".to_owned()),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn stop(&self) -> Result<(), McpError> {
        let mut first_error = None;
        for indexer in self.indexers.values() {
            if let Err(error) = indexer.stop()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), |error| Err(error.into()))
    }

    pub fn handle_message(&self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);
        id.as_ref()?;
        let id = id.unwrap_or(Value::Null);
        let result = match method {
            Some("initialize") => self.initialize(message.get("params")),
            Some("ping") | Some("shutdown") => Ok(json!({})),
            Some("tools/list") => Ok(json!({"tools": [tool_definition()]})),
            Some("tools/call") => self.call_tool(message.get("params")),
            Some(_) => {
                return Some(error_response(
                    id,
                    -32601,
                    "Method not found",
                    message.get("method").cloned(),
                ));
            }
            None => {
                return Some(error_response(id, -32600, "Invalid Request", None));
            }
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => error_response(id, error.code, &error.message, error.data),
        })
    }

    fn initialize(&self, params: Option<&Value>) -> Result<Value, RpcError> {
        let requested = params
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("protocolVersion is required"))?;
        let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION
        };
        Ok(json!({
            "protocolVersion": protocol_version,
            "capabilities": {
                "tools": {"listChanged": false}
            },
            "serverInfo": {"name": "oce-client", "version": VERSION}
        }))
    }

    fn call_tool(&self, params: Option<&Value>) -> Result<Value, RpcError> {
        let params = params
            .and_then(Value::as_object)
            .ok_or_else(|| RpcError::invalid_params("tools/call params must be an object"))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("tool name is required"))?;
        if name != TOOL_NAME {
            return Err(RpcError::invalid_params(format!("unknown tool: {name}")));
        }
        let arguments = params
            .get("arguments")
            .and_then(Value::as_object)
            .ok_or_else(|| RpcError::invalid_params("tool arguments must be an object"))?;
        let query = arguments
            .get("information_request")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| RpcError::invalid_params("information_request must not be empty"))?;
        let workspace = arguments.get("workspace_folder").and_then(|value| {
            if value.is_null() {
                None
            } else {
                value.as_str()
            }
        });
        let indexer = self
            .indexer_for(workspace)
            .map_err(|error| RpcError::invalid_params(error.to_string()))?;
        let payload = match indexer.retrieve(query, self.ready_timeout) {
            Ok(payload) => payload,
            Err(error) => {
                let payload = json!({
                    "status": "error",
                    "workspace_folder": indexer.root().to_string_lossy(),
                    "error": error.to_string(),
                });
                return Ok(tool_result(payload, true));
            }
        };
        Ok(tool_result(payload, false))
    }

    fn indexer_for(&self, workspace: Option<&str>) -> Result<&WorkspaceIndexer, McpError> {
        let requested = match workspace {
            None if self.indexers.len() == 1 => {
                return self
                    .indexers
                    .values()
                    .next()
                    .ok_or(McpError::MissingWorkspace);
            }
            None => return Err(McpError::WorkspaceRequired),
            Some(value) if value.trim().is_empty() => return Err(McpError::EmptyWorkspace),
            Some(value) => canonical_or_absolute(Path::new(value))?,
        };
        self.indexers
            .get(&requested)
            .ok_or_else(|| McpError::WorkspaceNotConfigured {
                requested,
                allowed: self.indexers.keys().cloned().collect(),
            })
    }

    pub fn tool_definition(&self) -> Value {
        tool_definition()
    }
}

pub fn run_stdio(server: &McpServer) -> Result<(), McpError> {
    server.start()?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut run_result = Ok(());
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                run_result = Err(McpError::Input(error));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle_message(message),
            Err(error) => Some(error_response(
                Value::Null,
                -32700,
                "Parse error",
                Some(Value::String(error.to_string())),
            )),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut stdout, &response).map_err(McpError::Json)?;
            stdout.write_all(b"\n").map_err(McpError::Output)?;
            stdout.flush().map_err(McpError::Output)?;
        }
    }
    let stop_result = server.stop();
    run_result.and(stop_result)
}

fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": TOOL_DESCRIPTION,
        "inputSchema": {
            "properties": {
                "information_request": {
                    "description": "A description of the information you need.",
                    "title": "Information Request",
                    "type": "string"
                },
                "workspace_folder": {
                    "default": null,
                    "description": "Path to the workspace folder to search. Required when multiple workspace folders are open. Use the folder paths shown in your system prompt.",
                    "title": "Workspace Folder",
                    "type": "string"
                }
            },
            "required": ["information_request"],
            "title": "codebase_retrievalArguments",
            "type": "object"
        },
        "outputSchema": {
            "additionalProperties": true,
            "title": "codebase_retrievalDictOutput",
            "type": "object"
        }
    })
}

fn tool_result(payload: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": payload,
        "isError": is_error,
    })
}

fn error_response(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = Map::from_iter([
        ("code".to_owned(), Value::Number(code.into())),
        ("message".to_owned(), Value::String(message.to_owned())),
    ]);
    if let Some(data) = data {
        error.insert("data".to_owned(), data);
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),
    #[error(transparent)]
    Indexer(#[from] IndexerError),
    #[error("initial workspace synchronization failed for {workspace}: {detail}")]
    InitialSync { workspace: String, detail: String },
    #[error("MCP has no configured workspace")]
    MissingWorkspace,
    #[error("workspace_folder is required when multiple workspaces are configured")]
    WorkspaceRequired,
    #[error("workspace_folder must not be empty")]
    EmptyWorkspace,
    #[error("workspace_folder is not configured: {requested}; allowed: {allowed:?}")]
    WorkspaceNotConfigured {
        requested: PathBuf,
        allowed: Vec<PathBuf>,
    },
    #[error("failed to read MCP stdio input: {0}")]
    Input(io::Error),
    #[error("failed to write MCP stdio output: {0}")]
    Output(io::Error),
    #[error("failed to encode MCP response: {0}")]
    Json(serde_json::Error),
}
