use std::env;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::context::{ContextError, WorkspaceContext};
use crate::http::OceHttpClient;

pub const DEFAULT_API_URL: &str = "http://127.0.0.1:8986";
pub const DEFAULT_API_KEY: &str = "sk-opencontextengine";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSettings {
    pub root: PathBuf,
    pub api_url: String,
    pub api_key: String,
    pub state_path: Option<PathBuf>,
    pub runtime_patterns: Vec<String>,
}

impl ClientSettings {
    pub fn resolve(
        root: Option<PathBuf>,
        api_url: Option<String>,
        state_path: Option<PathBuf>,
        runtime_patterns: Option<Vec<String>>,
        require_api_key: bool,
    ) -> Result<Self, ConfigurationError> {
        let root = root
            .or_else(|| nonempty_env("OCE_WORKSPACE").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let root = canonical_or_absolute(&root)?;
        let api_url = api_url
            .or_else(|| nonempty_env("OCE_API_URL"))
            .unwrap_or_else(|| DEFAULT_API_URL.to_owned());
        let api_url = api_url.trim().trim_end_matches('/').to_owned();
        if api_url.is_empty() {
            return Err(ConfigurationError::Invalid(
                "OCE API URL must not be empty".to_owned(),
            ));
        }
        let api_key = env::var("OCE_API_KEY").unwrap_or_else(|_| DEFAULT_API_KEY.to_owned());
        if require_api_key && api_key.is_empty() {
            return Err(ConfigurationError::Invalid(
                "OCE API key is required; set OCE_API_KEY".to_owned(),
            ));
        }
        let state_path = state_path
            .or_else(|| nonempty_env("OCE_STATE_PATH").map(PathBuf::from))
            .map(|path| canonical_parent_join(&path))
            .transpose()?;
        let runtime_patterns = runtime_patterns.unwrap_or_else(|| {
            split_runtime_patterns(env::var("OCE_IGNORE").unwrap_or_default().as_str())
        });
        Ok(Self {
            root,
            api_url,
            api_key,
            state_path,
            runtime_patterns,
        })
    }

    pub fn context(&self) -> Result<WorkspaceContext, ConfigurationError> {
        let api = Arc::new(
            OceHttpClient::new(&self.api_url, &self.api_key)
                .map_err(|error| ConfigurationError::Invalid(error.to_string()))?,
        );
        WorkspaceContext::open(
            &self.root,
            api,
            self.state_path.as_deref(),
            self.runtime_patterns.clone(),
        )
        .map_err(ConfigurationError::Context)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialSync {
    Background,
    Blocking,
    Off,
}

impl InitialSync {
    pub fn parse(value: &str) -> Result<Self, ConfigurationError> {
        match value.trim().to_lowercase().as_str() {
            "background" => Ok(Self::Background),
            "blocking" => Ok(Self::Blocking),
            "off" => Ok(Self::Off),
            _ => Err(ConfigurationError::Invalid(
                "initial-sync must be one of: background, blocking, off".to_owned(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpConfiguration {
    pub workspaces: Vec<ClientSettings>,
    pub debounce_ms: u64,
    pub initial_sync: InitialSync,
    pub ready_timeout_seconds: f64,
}

impl McpConfiguration {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        workspace_roots: Option<Vec<PathBuf>>,
        api_url: Option<String>,
        state_path: Option<PathBuf>,
        state_dir: Option<PathBuf>,
        runtime_patterns: Option<Vec<String>>,
        debounce_ms: Option<i64>,
        initial_sync: Option<String>,
        ready_timeout_seconds: Option<f64>,
    ) -> Result<Self, ConfigurationError> {
        let roots = match workspace_roots {
            Some(values) => values,
            None => {
                if let Some(paths) = env::var_os("OCE_WORKSPACES") {
                    env::split_paths(&paths).collect()
                } else {
                    nonempty_env("OCE_WORKSPACE")
                        .map(PathBuf::from)
                        .into_iter()
                        .collect()
                }
            }
        };
        let mut canonical_roots = Vec::new();
        for root in roots {
            let root = canonical_or_absolute(&root)?;
            if !canonical_roots.contains(&root) {
                canonical_roots.push(root);
            }
        }
        if canonical_roots.is_empty() {
            return Err(ConfigurationError::Invalid(
                "MCP requires at least one workspace; pass --workspace or set OCE_WORKSPACE/OCE_WORKSPACES"
                    .to_owned(),
            ));
        }

        let state_path = state_path
            .or_else(|| nonempty_env("OCE_STATE_PATH").map(PathBuf::from))
            .map(|path| canonical_parent_join(&path))
            .transpose()?;
        let state_dir = state_dir
            .or_else(|| nonempty_env("OCE_STATE_DIR").map(PathBuf::from))
            .map(|path| canonical_parent_join(&path))
            .transpose()?;
        if state_path.is_some() && state_dir.is_some() {
            return Err(ConfigurationError::Invalid(
                "MCP state configuration is ambiguous; choose OCE_STATE_PATH/--state-path or OCE_STATE_DIR/--state-dir"
                    .to_owned(),
            ));
        }
        if canonical_roots.len() > 1 && state_path.is_some() {
            return Err(ConfigurationError::Invalid(
                "multiple MCP workspaces cannot share OCE_STATE_PATH; use --state-dir or OCE_STATE_DIR"
                    .to_owned(),
            ));
        }
        let patterns = runtime_patterns.unwrap_or_else(|| {
            split_runtime_patterns(env::var("OCE_IGNORE").unwrap_or_default().as_str())
        });
        let mut workspaces = Vec::new();
        for (index, root) in canonical_roots.into_iter().enumerate() {
            let selected_state = if let Some(directory) = state_dir.as_ref() {
                Some(directory.join(workspace_state_name(&root)))
            } else if index == 0 {
                state_path.clone()
            } else {
                None
            };
            workspaces.push(ClientSettings::resolve(
                Some(root),
                api_url.clone(),
                selected_state,
                Some(patterns.clone()),
                true,
            )?);
        }

        let debounce_ms = match debounce_ms {
            Some(value) => value,
            None => parse_env::<i64>("OCE_DEBOUNCE_MS")?.unwrap_or(500),
        };
        if debounce_ms < 0 {
            return Err(ConfigurationError::Invalid(
                "debounce-ms must not be negative".to_owned(),
            ));
        }
        let initial_sync_value = initial_sync
            .or_else(|| nonempty_env("OCE_INITIAL_SYNC"))
            .unwrap_or_else(|| "background".to_owned());
        let initial_sync = InitialSync::parse(&initial_sync_value)?;
        let ready_timeout_seconds = match ready_timeout_seconds {
            Some(value) => value,
            None => parse_env::<f64>("OCE_READY_TIMEOUT")?.unwrap_or(3.0),
        };
        if ready_timeout_seconds < 0.0 || !ready_timeout_seconds.is_finite() {
            return Err(ConfigurationError::Invalid(
                "ready-timeout must be a finite non-negative number".to_owned(),
            ));
        }
        Ok(Self {
            workspaces,
            debounce_ms: u64::try_from(debounce_ms).expect("non-negative debounce"),
            initial_sync,
            ready_timeout_seconds,
        })
    }
}

pub fn split_runtime_patterns(value: &str) -> Vec<String> {
    value
        .replace('\r', "\n")
        .split(',')
        .flat_map(str::lines)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn workspace_state_name(root: &Path) -> String {
    let identity = normalized_workspace_identity(root);
    let mut digest = Sha256::new();
    digest.update(identity.as_bytes());
    let encoded = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{}.sqlite3", &encoded[..16])
}

#[cfg(windows)]
fn normalized_workspace_identity(root: &Path) -> String {
    root.to_string_lossy().replace('/', "\\").to_lowercase()
}

#[cfg(not(windows))]
fn normalized_workspace_identity(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

pub(crate) fn canonical_or_absolute(path: &Path) -> Result<PathBuf, ConfigurationError> {
    let path = expand_home(path)?;
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map_err(ConfigurationError::CurrentDirectory)?
            .join(&path)
    };
    Ok(absolute)
}

fn canonical_parent_join(path: &Path) -> Result<PathBuf, ConfigurationError> {
    let path = expand_home(path)?;
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map_err(ConfigurationError::CurrentDirectory)?
            .join(&path)
    };
    if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name())
        && let Ok(parent) = parent.canonicalize()
    {
        return Ok(parent.join(name));
    }
    Ok(absolute)
}

fn expand_home(path: &Path) -> Result<PathBuf, ConfigurationError> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(part)) if part == OsStr::new("~")) {
        return Ok(path.to_path_buf());
    }
    let home = user_home().ok_or_else(|| {
        ConfigurationError::Invalid(
            "cannot expand '~': user home directory is unavailable".to_owned(),
        )
    })?;
    Ok(components.fold(home, |resolved, component| {
        resolved.join(component.as_os_str())
    }))
}

fn user_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            (!drive.is_empty() && !path.is_empty()).then(|| PathBuf::from(drive).join(path))
        })
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_env<T>(name: &str) -> Result<Option<T>, ConfigurationError>
where
    T: std::str::FromStr,
{
    let Some(value) = nonempty_env(name) else {
        return Ok(None);
    };
    value
        .parse()
        .map(Some)
        .map_err(|_| ConfigurationError::Invalid(format!("{name} has an invalid numeric value")))
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("{0}")]
    Invalid(String),
    #[error("failed to read current directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error(transparent)]
    Context(ContextError),
}
