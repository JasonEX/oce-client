use std::env;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::context::{ContextError, WorkspaceContext};
use crate::http::OceHttpClient;
use crate::identity::sha256_hex;

pub const DEFAULT_API_URL: &str = "http://127.0.0.1:8986";
pub const DEFAULT_API_KEY: &str = "sk-opencontextengine";
pub const DEFAULT_DEBOUNCE_MS: u64 = 500;
pub const DEFAULT_READY_TIMEOUT_SECONDS: f64 = 3.0;

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
        Ok(Self {
            root: canonical_or_absolute(&root)?,
            api_url: resolve_api_url(api_url)?,
            api_key: resolve_api_key(require_api_key)?,
            state_path: resolve_state_path(state_path)?,
            runtime_patterns: resolve_runtime_patterns(runtime_patterns),
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

/// Explicit MCP settings; `None` fields fall back to environment variables and defaults.
#[derive(Debug, Clone, Default)]
pub struct McpOptions {
    pub workspace_roots: Option<Vec<PathBuf>>,
    pub api_url: Option<String>,
    pub state_path: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub runtime_patterns: Option<Vec<String>>,
    pub debounce_ms: Option<i64>,
    pub initial_sync: Option<String>,
    pub ready_timeout_seconds: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct McpConfiguration {
    pub workspaces: Vec<ClientSettings>,
    pub debounce_ms: u64,
    pub initial_sync: InitialSync,
    pub ready_timeout_seconds: f64,
}

impl McpConfiguration {
    pub fn resolve(options: McpOptions) -> Result<Self, ConfigurationError> {
        let roots = match options.workspace_roots {
            Some(values) => values,
            None => match env::var_os("OCE_WORKSPACES") {
                Some(paths) => env::split_paths(&paths).collect(),
                None => nonempty_env("OCE_WORKSPACE")
                    .map(PathBuf::from)
                    .into_iter()
                    .collect(),
            },
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

        let state_path = resolve_state_path(options.state_path)?;
        let state_dir = options
            .state_dir
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
        let api_url = resolve_api_url(options.api_url)?;
        let api_key = resolve_api_key(true)?;
        let runtime_patterns = resolve_runtime_patterns(options.runtime_patterns);
        let workspaces = canonical_roots
            .into_iter()
            .enumerate()
            .map(|(index, root)| ClientSettings {
                state_path: match &state_dir {
                    Some(directory) => Some(directory.join(workspace_state_name(&root))),
                    None if index == 0 => state_path.clone(),
                    None => None,
                },
                root,
                api_url: api_url.clone(),
                api_key: api_key.clone(),
                runtime_patterns: runtime_patterns.clone(),
            })
            .collect();

        let debounce_ms = match options.debounce_ms {
            Some(value) => value,
            None => parse_env::<i64>("OCE_DEBOUNCE_MS")?
                .unwrap_or(i64::try_from(DEFAULT_DEBOUNCE_MS).expect("default fits i64")),
        };
        let debounce_ms = u64::try_from(debounce_ms).map_err(|_| {
            ConfigurationError::Invalid("debounce-ms must not be negative".to_owned())
        })?;
        let initial_sync_value = options
            .initial_sync
            .or_else(|| nonempty_env("OCE_INITIAL_SYNC"))
            .unwrap_or_else(|| "background".to_owned());
        let initial_sync = InitialSync::parse(&initial_sync_value)?;
        let ready_timeout_seconds = match options.ready_timeout_seconds {
            Some(value) => value,
            None => parse_env::<f64>("OCE_READY_TIMEOUT")?.unwrap_or(DEFAULT_READY_TIMEOUT_SECONDS),
        };
        if ready_timeout_seconds < 0.0 || !ready_timeout_seconds.is_finite() {
            return Err(ConfigurationError::Invalid(
                "ready-timeout must be a finite non-negative number".to_owned(),
            ));
        }
        Ok(Self {
            workspaces,
            debounce_ms,
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

fn resolve_api_url(api_url: Option<String>) -> Result<String, ConfigurationError> {
    let api_url = api_url
        .or_else(|| nonempty_env("OCE_API_URL"))
        .unwrap_or_else(|| DEFAULT_API_URL.to_owned());
    let api_url = api_url.trim().trim_end_matches('/').to_owned();
    if api_url.is_empty() {
        return Err(ConfigurationError::Invalid(
            "OCE API URL must not be empty".to_owned(),
        ));
    }
    Ok(api_url)
}

fn resolve_api_key(required: bool) -> Result<String, ConfigurationError> {
    let api_key = env::var("OCE_API_KEY").unwrap_or_else(|_| DEFAULT_API_KEY.to_owned());
    if required && api_key.is_empty() {
        return Err(ConfigurationError::Invalid(
            "OCE API key is required; set OCE_API_KEY".to_owned(),
        ));
    }
    Ok(api_key)
}

fn resolve_state_path(state_path: Option<PathBuf>) -> Result<Option<PathBuf>, ConfigurationError> {
    state_path
        .or_else(|| nonempty_env("OCE_STATE_PATH").map(PathBuf::from))
        .map(|path| canonical_parent_join(&path))
        .transpose()
}

fn resolve_runtime_patterns(runtime_patterns: Option<Vec<String>>) -> Vec<String> {
    runtime_patterns.unwrap_or_else(|| {
        split_runtime_patterns(env::var("OCE_IGNORE").unwrap_or_default().as_str())
    })
}

fn workspace_state_name(root: &Path) -> String {
    let encoded = sha256_hex(&[normalized_workspace_identity(root).as_bytes()]);
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

/// Canonicalizes an existing path, or makes a missing one absolute against the current
/// directory.
pub(crate) fn canonical_or_absolute(path: &Path) -> Result<PathBuf, ConfigurationError> {
    let path = expand_home(path)?;
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(env::current_dir()
        .map_err(ConfigurationError::CurrentDirectory)?
        .join(path))
}

/// Like `canonical_or_absolute`, but also canonicalizes the parent of a path that does not
/// exist yet, so state files land next to their real directory.
fn canonical_parent_join(path: &Path) -> Result<PathBuf, ConfigurationError> {
    let absolute = canonical_or_absolute(path)?;
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

pub(crate) fn user_home() -> Option<PathBuf> {
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
