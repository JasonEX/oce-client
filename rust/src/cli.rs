use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;

use crate::VERSION;
use crate::config::{ClientSettings, ConfigurationError, McpConfiguration, split_runtime_patterns};
use crate::context::{ContextError, WorkspaceContext};
use crate::indexer::{IndexerError, Readiness, WorkspaceIndexer};
use crate::mcp::{McpError, McpServer, run_stdio};

#[derive(Debug, Parser)]
#[command(
    name = "oce-client",
    version = VERSION,
    about = "Synchronize a local workspace with OpenContextEngine."
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        help = "workspace directory (default: OCE_WORKSPACE or .)"
    )]
    root: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "OCE API URL (default: OCE_API_URL or http://127.0.0.1:8986)"
    )]
    api_url: Option<String>,
    #[arg(
        long,
        global = true,
        help = "SQLite state path (default: OCE_STATE_PATH or workspace/.oce-client)"
    )]
    state_path: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        action = clap::ArgAction::Append,
        help = "runtime ignore pattern; can be repeated or comma-separated (OCE_IGNORE)"
    )]
    ignore: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Reconcile and upload the workspace.
    Sync(JsonOutput),
    /// Show local inventory and checkpoint state.
    Status(JsonOutput),
    /// Retrieve formatted code context.
    Retrieve {
        query: String,
        #[arg(long, value_enum, default_value_t = RetrievalScope::Workspace)]
        scope: RetrievalScope,
        #[arg(long = "json")]
        as_json: bool,
    },
    /// Stage explicit file content.
    Observe {
        path: PathBuf,
        #[arg(long)]
        content: Option<String>,
        #[arg(long = "file", help = "read content from a file")]
        content_file: Option<PathBuf>,
        #[arg(long = "json")]
        as_json: bool,
    },
    /// Stage a file deletion.
    Remove {
        path: PathBuf,
        #[arg(long = "json")]
        as_json: bool,
    },
    /// Watch the workspace and sync on changes.
    Watch {
        #[arg(long, default_value_t = 300)]
        debounce_ms: u64,
    },
    /// Locate or install the bundled Codex skill.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Run the MCP server over stdio.
    Mcp(McpArgs),
}

#[derive(Debug, Args)]
struct JsonOutput {
    #[arg(long = "json")]
    as_json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RetrievalScope {
    Workspace,
    #[value(name = "working_set")]
    WorkingSet,
}

impl RetrievalScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::WorkingSet => "working_set",
        }
    }
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// Print the materialized bundled skill path.
    Path(JsonOutput),
    /// Install the skill into Codex skills.
    Install {
        #[arg(long)]
        target: Option<PathBuf>,
        #[arg(long)]
        force: bool,
        #[arg(long = "json")]
        as_json: bool,
    },
}

#[derive(Debug, Args)]
struct McpArgs {
    #[arg(long, action = clap::ArgAction::Append)]
    workspace: Vec<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    debounce_ms: Option<i64>,
    #[arg(long)]
    initial_sync: Option<String>,
    #[arg(long)]
    ready_timeout: Option<f64>,
}

pub fn run_from<I, T>(arguments: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::parse_from(arguments);
    run(cli)
}

pub fn run(cli: Cli) -> Result<(), CliError> {
    let Cli {
        root,
        api_url,
        state_path,
        ignore,
        command,
    } = cli;
    match command {
        Command::Skill { command } => run_skill(command),
        Command::Mcp(arguments) => run_mcp(arguments, root, api_url, state_path, ignore),
        command => {
            let require_api_key = matches!(
                command,
                Command::Sync(_) | Command::Retrieve { .. } | Command::Watch { .. }
            );
            let patterns = (!ignore.is_empty()).then(|| split_runtime_patterns(&ignore.join(",")));
            let settings =
                ClientSettings::resolve(root, api_url, state_path, patterns, require_api_key)?;
            let context = settings.context()?;
            run_workspace(command, context)
        }
    }
}

fn run_workspace(command: Command, context: WorkspaceContext) -> Result<(), CliError> {
    match command {
        Command::Sync(output) => {
            let result = context.sync()?;
            if output.as_json {
                print_json(&result)?;
            } else {
                println!(
                    "synced checkpoint={} uploaded={}",
                    result.checkpoint_id.as_deref().unwrap_or("-"),
                    result.uploaded_blob_names.len()
                );
            }
        }
        Command::Status(output) => {
            let snapshot = context.snapshot()?;
            if output.as_json {
                let files = snapshot
                    .files
                    .into_iter()
                    .map(|(path, record)| {
                        (
                            path,
                            json!({
                                "blob_name": record.blob_name,
                                "committed_blob_name": record.committed_blob_name,
                                "status": record.status,
                                "size": record.size,
                                "source": record.source,
                                "generation": record.generation,
                            }),
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                print_json(&json!({
                    "checkpoint_id": snapshot.checkpoint_id,
                    "generation": snapshot.generation,
                    "files": files,
                }))?;
            } else {
                let present = snapshot
                    .files
                    .values()
                    .filter(|record| record.status == crate::state::FileStatus::Present)
                    .count();
                println!("root={}", context.root().display());
                println!(
                    "checkpoint={} generation={} present={present}",
                    snapshot.checkpoint_id.as_deref().unwrap_or("-"),
                    snapshot.generation
                );
            }
        }
        Command::Retrieve {
            query,
            scope,
            as_json,
        } => {
            let result = context.retrieve(&query, scope.as_str())?;
            if as_json {
                print_json(&result)?;
            } else {
                println!("{}", result.formatted_retrieval);
            }
        }
        Command::Observe {
            path,
            content,
            content_file,
            as_json,
        } => {
            if content.is_some() && content_file.is_some() {
                return Err(CliError::Invalid(
                    "use either --content or --file, not both".to_owned(),
                ));
            }
            let content = match (content, content_file) {
                (Some(content), None) => content,
                (None, Some(path)) => {
                    fs::read_to_string(&path).map_err(|source| CliError::ReadContent {
                        path: path.clone(),
                        source,
                    })?
                }
                (None, None) => {
                    let mut content = String::new();
                    io::stdin().read_to_string(&mut content)?;
                    content
                }
                (Some(_), Some(_)) => unreachable!(),
            };
            context.observe_file(&path, &content)?;
            if as_json {
                print_json(&json!({"path": path.to_string_lossy(), "status": "present"}))?;
            } else {
                println!("observed {}", path.display());
            }
        }
        Command::Remove { path, as_json } => {
            context.remove_file(&path)?;
            if as_json {
                print_json(&json!({"path": path.to_string_lossy(), "status": "deleted"}))?;
            } else {
                println!("removed {}", path.display());
            }
        }
        Command::Watch { debounce_ms } => {
            let root = context.root().to_path_buf();
            let indexer = WorkspaceIndexer::new(context, Duration::from_millis(debounce_ms));
            indexer.start(false)?;
            eprintln!("watching {}; press Ctrl-C to stop", root.display());
            loop {
                thread::sleep(Duration::from_millis(250));
                let status = indexer.status()?;
                if status.status == Readiness::Error {
                    let _ = indexer.stop();
                    return Err(CliError::Invalid(
                        status
                            .error
                            .unwrap_or_else(|| "filesystem watcher failed".to_owned()),
                    ));
                }
            }
        }
        Command::Skill { .. } | Command::Mcp(_) => unreachable!(),
    }
    Ok(())
}

fn run_mcp(
    arguments: McpArgs,
    root: Option<PathBuf>,
    api_url: Option<String>,
    state_path: Option<PathBuf>,
    ignore: Vec<String>,
) -> Result<(), CliError> {
    let workspace_roots = if arguments.workspace.is_empty() {
        root.map(|root| vec![root])
    } else {
        Some(arguments.workspace)
    };
    let configuration = McpConfiguration::resolve(
        workspace_roots,
        api_url,
        state_path,
        arguments.state_dir,
        (!ignore.is_empty()).then(|| split_runtime_patterns(&ignore.join(","))),
        arguments.debounce_ms,
        arguments.initial_sync,
        arguments.ready_timeout,
    )?;
    let server = McpServer::new(configuration)?;
    run_stdio(&server)?;
    Ok(())
}

fn run_skill(command: SkillCommand) -> Result<(), CliError> {
    match command {
        SkillCommand::Path(output) => {
            let path = materialize_skill()?;
            if output.as_json {
                print_json(&json!({"path": path.to_string_lossy()}))?;
            } else {
                println!("{}", path.display());
            }
        }
        SkillCommand::Install {
            target,
            force,
            as_json,
        } => {
            let target = target.unwrap_or(default_skill_target()?);
            if target.exists() && !force {
                return Err(CliError::Invalid(format!(
                    "skill target already exists: {}; pass --force to replace it",
                    target.display()
                )));
            }
            write_embedded_skill(&target)?;
            if as_json {
                print_json(&json!({
                    "path": target.to_string_lossy(),
                    "status": "installed"
                }))?;
            } else {
                println!("installed skill at {}", target.display());
            }
        }
    }
    Ok(())
}

fn materialize_skill() -> Result<PathBuf, CliError> {
    let path = codex_home()?.join("cache").join("oce-client").join(VERSION);
    write_embedded_skill(&path)?;
    Ok(path)
}

fn default_skill_target() -> Result<PathBuf, CliError> {
    Ok(codex_home()?.join("skills").join("oce-client"))
}

fn codex_home() -> Result<PathBuf, CliError> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    let user_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| CliError::Invalid("cannot resolve the user home directory".to_owned()))?;
    Ok(PathBuf::from(user_home).join(".codex"))
}

fn write_embedded_skill(target: &Path) -> Result<(), CliError> {
    let agents = target.join("agents");
    fs::create_dir_all(&agents).map_err(|source| CliError::WriteSkill {
        path: agents,
        source,
    })?;
    for (path, contents) in [
        (
            target.join("SKILL.md"),
            include_str!("../../skills/oce-client/SKILL.md"),
        ),
        (
            target.join("agents/openai.yaml"),
            include_str!("../../skills/oce-client/agents/openai.yaml"),
        ),
    ] {
        fs::write(&path, contents).map_err(|source| CliError::WriteSkill { path, source })?;
    }
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),
    #[error(transparent)]
    Context(#[from] ContextError),
    #[error(transparent)]
    Indexer(#[from] IndexerError),
    #[error(transparent)]
    Mcp(#[from] McpError),
    #[error("failed to read content file {path}: {source}")]
    ReadContent { path: PathBuf, source: io::Error },
    #[error("failed to write bundled skill at {path}: {source}")]
    WriteSkill { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn legacy_mcp_arguments<I>(arguments: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let executable = arguments
        .next()
        .unwrap_or_else(|| OsString::from("oce-client"));
    let is_legacy = Path::new(&executable)
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("oce-client-mcp"));
    let mut normalized = vec![executable];
    if is_legacy {
        normalized.push(OsString::from("mcp"));
    }
    normalized.extend(arguments);
    normalized
}
