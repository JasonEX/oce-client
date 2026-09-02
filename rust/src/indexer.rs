use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::context::{ContextError, WorkspaceContext};
use crate::filesystem::normalize_workspace_event_path;
use crate::ignore_rules::LayeredIgnoreMatcher;
use crate::watcher::{WatchError, WatchHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Readiness {
    Ready,
    Indexing,
    Error,
}

impl Readiness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Indexing => "indexing",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexerStatus {
    pub status: Readiness,
    pub workspace_folder: String,
    pub requested_generation: u64,
    pub synced_generation: u64,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct WorkspaceIndexer {
    inner: Arc<IndexerInner>,
}

impl std::fmt::Debug for WorkspaceIndexer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceIndexer")
            .field("root", &self.inner.root)
            .finish_non_exhaustive()
    }
}

struct IndexerInner {
    root: PathBuf,
    runtime_patterns: Vec<String>,
    debounce: Duration,
    context: Mutex<WorkspaceContext>,
    control: Mutex<Control>,
    condition: Condvar,
    watch: Mutex<Option<WatchHandle>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
struct Control {
    started: bool,
    stop: bool,
    state: Readiness,
    requested_generation: u64,
    synced_generation: u64,
    initialized: bool,
    recovery_required: bool,
    full_pending: bool,
    pending_paths: BTreeSet<PathBuf>,
    last_error: Option<String>,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            started: false,
            stop: false,
            state: Readiness::Indexing,
            requested_generation: 0,
            synced_generation: 0,
            initialized: false,
            recovery_required: false,
            full_pending: false,
            pending_paths: BTreeSet::new(),
            last_error: None,
        }
    }
}

impl WorkspaceIndexer {
    pub fn new(context: WorkspaceContext, debounce: Duration) -> Self {
        let root = context.root().to_path_buf();
        let runtime_patterns = context.runtime_patterns().to_vec();
        Self {
            inner: Arc::new(IndexerInner {
                root,
                runtime_patterns,
                debounce,
                context: Mutex::new(context),
                control: Mutex::new(Control::default()),
                condition: Condvar::new(),
                watch: Mutex::new(None),
                worker: Mutex::new(None),
            }),
        }
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn start(&self, initial_sync: bool) -> Result<(), IndexerError> {
        {
            let mut control = self.control()?;
            if control.started {
                if initial_sync
                    && !control.initialized
                    && matches!(control.state, Readiness::Indexing | Readiness::Error)
                    && !control.full_pending
                    && control.pending_paths.is_empty()
                {
                    request_full(&mut control)?;
                    self.inner.condition.notify_all();
                }
                return Ok(());
            }
        }

        let weak = Arc::downgrade(&self.inner);
        let callback = Arc::new(move |paths: BTreeSet<PathBuf>| {
            if let Some(inner) = weak.upgrade() {
                inner.notify_changes(paths);
            }
        });
        let weak = Arc::downgrade(&self.inner);
        let on_error = Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.condition.notify_all();
            }
        });
        let watch = WatchHandle::start(&self.inner.root, self.inner.debounce, callback, on_error)?;
        let weak = Arc::downgrade(&self.inner);
        let worker = thread::Builder::new()
            .name(format!(
                "oce-indexer-{}",
                self.inner
                    .root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("workspace")
            ))
            .spawn(move || run_worker(weak))
            .map_err(IndexerError::Thread)?;
        *self
            .inner
            .watch
            .lock()
            .map_err(|_| IndexerError::PoisonedLock)? = Some(watch);
        *self
            .inner
            .worker
            .lock()
            .map_err(|_| IndexerError::PoisonedLock)? = Some(worker);
        let mut control = self.control()?;
        control.started = true;
        if initial_sync {
            request_full(&mut control)?;
        }
        self.inner.condition.notify_all();
        Ok(())
    }

    pub fn stop(&self) -> Result<(), IndexerError> {
        {
            let mut control = self.control()?;
            if !control.started {
                return Ok(());
            }
            control.stop = true;
            self.inner.condition.notify_all();
        }
        if let Some(mut watch) = self
            .inner
            .watch
            .lock()
            .map_err(|_| IndexerError::PoisonedLock)?
            .take()
        {
            watch.stop();
            watch.join()?;
        }
        if let Some(worker) = self
            .inner
            .worker
            .lock()
            .map_err(|_| IndexerError::PoisonedLock)?
            .take()
        {
            worker.join().map_err(|_| IndexerError::WorkerPanicked)?;
        }
        let mut control = self.control()?;
        control.started = false;
        Ok(())
    }

    pub fn request_full_sync(&self) -> Result<(), IndexerError> {
        let mut control = self.control()?;
        request_full(&mut control)?;
        self.inner.condition.notify_all();
        Ok(())
    }

    pub fn notify_changes(&self, paths: BTreeSet<PathBuf>) -> Result<(), IndexerError> {
        self.inner.notify_changes(paths);
        Ok(())
    }

    pub fn wait_until_ready(&self, timeout: Option<Duration>) -> Result<Readiness, IndexerError> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut control = self.control()?;
        if !control.started {
            return Err(IndexerError::NotStarted);
        }
        if !control.initialized
            && matches!(control.state, Readiness::Indexing | Readiness::Error)
            && !control.full_pending
        {
            request_full(&mut control)?;
            self.inner.condition.notify_all();
        }
        loop {
            if self.inner.watch_error()?.is_some() {
                return Ok(Readiness::Error);
            }
            if ready(&control) {
                return Ok(Readiness::Ready);
            }
            if control.state == Readiness::Error
                && !control.full_pending
                && control.pending_paths.is_empty()
            {
                return Ok(Readiness::Error);
            }
            if let Some(deadline) = deadline {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return Ok(Readiness::Indexing);
                };
                if remaining.is_zero() {
                    return Ok(Readiness::Indexing);
                }
                let (next, result) = self
                    .inner
                    .condition
                    .wait_timeout(control, remaining)
                    .map_err(|_| IndexerError::PoisonedLock)?;
                control = next;
                if result.timed_out() && !ready(&control) {
                    return Ok(Readiness::Indexing);
                }
            } else {
                control = self
                    .inner
                    .condition
                    .wait(control)
                    .map_err(|_| IndexerError::PoisonedLock)?;
            }
        }
    }

    pub fn retrieve(&self, query: &str, timeout: Duration) -> Result<Value, IndexerError> {
        self.start(true)?;
        {
            let mut control = self.control()?;
            if control.state == Readiness::Error
                && self.inner.watch_error()?.is_none()
                && !control.full_pending
                && control.pending_paths.is_empty()
            {
                request_full(&mut control)?;
                self.inner.condition.notify_all();
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let status = self.wait_until_ready(Some(remaining))?;
            if status != Readiness::Ready {
                let mut payload = json!({
                    "status": status.as_str(),
                    "workspace_folder": self.inner.root.to_string_lossy(),
                });
                if status == Readiness::Error {
                    payload["error"] = Value::String(
                        self.inner
                            .watch_error()?
                            .or_else(|| self.control().ok()?.last_error.clone())
                            .unwrap_or_else(|| "workspace synchronization failed".to_owned()),
                    );
                } else {
                    payload["message"] = Value::String(
                        "Workspace indexing is still in progress; retry shortly.".to_owned(),
                    );
                }
                return Ok(payload);
            }

            let context = self
                .inner
                .context
                .lock()
                .map_err(|_| IndexerError::PoisonedLock)?;
            let generation = {
                let control = self.control()?;
                if !ready(&control) || self.inner.watch_error()?.is_some() {
                    drop(control);
                    drop(context);
                    continue;
                }
                control.requested_generation
            };
            let result = context.retrieve(query, "workspace");
            drop(context);
            let control = self.control()?;
            if !ready(&control)
                || control.requested_generation != generation
                || self.inner.watch_error()?.is_some()
            {
                drop(control);
                continue;
            }
            match result {
                Ok(result) => {
                    return Ok(json!({
                        "status": "ready",
                        "workspace_folder": self.inner.root.to_string_lossy(),
                        "formatted_retrieval": result.formatted_retrieval,
                        "elapsed_ms": result.elapsed_ms,
                    }));
                }
                Err(error) => {
                    return Ok(json!({
                        "status": "error",
                        "workspace_folder": self.inner.root.to_string_lossy(),
                        "error": error.to_string(),
                    }));
                }
            }
        }
    }

    pub fn status(&self) -> Result<IndexerStatus, IndexerError> {
        let control = self.control()?;
        let watch_error = self.inner.watch_error()?;
        Ok(IndexerStatus {
            status: if watch_error.is_some() {
                Readiness::Error
            } else {
                control.state
            },
            workspace_folder: self.inner.root.to_string_lossy().into_owned(),
            requested_generation: control.requested_generation,
            synced_generation: control.synced_generation,
            error: watch_error.or_else(|| control.last_error.clone()),
        })
    }

    fn control(&self) -> Result<std::sync::MutexGuard<'_, Control>, IndexerError> {
        self.inner
            .control
            .lock()
            .map_err(|_| IndexerError::PoisonedLock)
    }
}

impl IndexerInner {
    fn notify_changes(&self, paths: BTreeSet<PathBuf>) {
        let matcher = match LayeredIgnoreMatcher::new(&self.root, self.runtime_patterns.clone()) {
            Ok(matcher) => matcher,
            Err(error) => {
                self.record_error(error.to_string());
                return;
            }
        };
        let mut relevant = BTreeSet::new();
        for path in paths {
            let Some(normalized) = normalize_workspace_event_path(&self.root, &path) else {
                continue;
            };
            let relative = normalized
                .strip_prefix(&self.root)
                .expect("normalized event path must remain within the workspace")
                .to_string_lossy()
                .replace('\\', "/");
            let is_ignore_file = matches!(
                normalized.file_name().and_then(|name| name.to_str()),
                Some(".gitignore" | ".oceignore")
            );
            if is_ignore_file || !matcher.ignores(&relative, normalized.is_dir()) {
                relevant.insert(normalized);
            }
        }
        if relevant.is_empty() {
            return;
        }
        let Ok(mut control) = self.control.lock() else {
            return;
        };
        if control.stop {
            return;
        }
        let Some(generation) = control.requested_generation.checked_add(1) else {
            control.state = Readiness::Error;
            control.last_error = Some("workspace generation overflow".to_owned());
            self.condition.notify_all();
            return;
        };
        control.requested_generation = generation;
        let needs_full = !control.initialized
            || control.recovery_required
            || relevant.iter().any(|path| {
                matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".gitignore" | ".oceignore")
                )
            });
        if needs_full {
            control.full_pending = true;
        } else {
            control.pending_paths.extend(relevant);
        }
        control.state = Readiness::Indexing;
        control.last_error = None;
        self.condition.notify_all();
    }

    fn record_error(&self, message: String) {
        if let Ok(mut control) = self.control.lock() {
            control.recovery_required = true;
            control.state = Readiness::Error;
            control.last_error = Some(message);
            self.condition.notify_all();
        }
    }

    fn watch_error(&self) -> Result<Option<String>, IndexerError> {
        Ok(self
            .watch
            .lock()
            .map_err(|_| IndexerError::PoisonedLock)?
            .as_ref()
            .and_then(WatchHandle::error))
    }
}

fn run_worker(weak: Weak<IndexerInner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let batch = {
            let Ok(mut control) = inner.control.lock() else {
                return;
            };
            while !control.stop && !control.full_pending && control.pending_paths.is_empty() {
                let Ok(next) = inner.condition.wait(control) else {
                    return;
                };
                control = next;
            }
            if control.stop {
                return;
            }
            let full = control.full_pending;
            let paths = std::mem::take(&mut control.pending_paths);
            let generation = control.requested_generation;
            control.full_pending = false;
            control.state = Readiness::Indexing;
            (full, paths, generation)
        };
        let result = inner
            .context
            .lock()
            .map_err(|_| ContextErrorText("workspace context lock is poisoned".to_owned()))
            .and_then(|context| {
                if batch.0 {
                    context.sync().map(|_| ()).map_err(ContextErrorText::from)
                } else {
                    context
                        .sync_paths(batch.1)
                        .map(|_| ())
                        .map_err(ContextErrorText::from)
                }
            });
        let Ok(mut control) = inner.control.lock() else {
            return;
        };
        match result {
            Ok(()) => {
                control.initialized = true;
                control.recovery_required = false;
                control.synced_generation = control.synced_generation.max(batch.2);
                control.last_error = None;
                control.state = if control.full_pending || !control.pending_paths.is_empty() {
                    Readiness::Indexing
                } else {
                    Readiness::Ready
                };
            }
            Err(error) => {
                control.recovery_required = true;
                if control.full_pending || !control.pending_paths.is_empty() {
                    control.full_pending = true;
                    control.pending_paths.clear();
                }
                control.state = Readiness::Error;
                control.last_error = Some(error.0);
            }
        }
        inner.condition.notify_all();
    }
}

fn request_full(control: &mut Control) -> Result<(), IndexerError> {
    control.requested_generation = control
        .requested_generation
        .checked_add(1)
        .ok_or(IndexerError::GenerationOverflow)?;
    control.full_pending = true;
    control.state = Readiness::Indexing;
    control.last_error = None;
    Ok(())
}

fn ready(control: &Control) -> bool {
    control.initialized
        && control.state == Readiness::Ready
        && !control.full_pending
        && control.pending_paths.is_empty()
        && control.synced_generation >= control.requested_generation
}

struct ContextErrorText(String);

impl From<ContextError> for ContextErrorText {
    fn from(error: ContextError) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error(transparent)]
    Watch(#[from] WatchError),
    #[error("failed to start workspace indexer thread: {0}")]
    Thread(std::io::Error),
    #[error("workspace indexer thread panicked")]
    WorkerPanicked,
    #[error("workspace indexer lock is poisoned")]
    PoisonedLock,
    #[error("workspace indexer has not been started")]
    NotStarted,
    #[error("workspace generation overflow")]
    GenerationOverflow,
}
