use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::context::WorkspaceContext;
use crate::filesystem::{normalize_workspace_event_path, relative_path_string};
use crate::ignore_rules::{LayeredIgnoreMatcher, is_ignore_file};
use crate::watcher::{WatchError, WatchHandle};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Readiness {
    Ready,
    #[default]
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

#[derive(Debug, Default)]
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

/// Work handed from the control state to the worker thread.
struct Batch {
    full: bool,
    paths: BTreeSet<PathBuf>,
    generation: u64,
}

impl Control {
    fn idle(&self) -> bool {
        !self.full_pending && self.pending_paths.is_empty()
    }

    fn ready(&self) -> bool {
        self.initialized
            && self.state == Readiness::Ready
            && self.idle()
            && self.synced_generation >= self.requested_generation
    }

    /// The first full synchronization has neither completed nor been scheduled.
    fn needs_initial_sync(&self) -> bool {
        !self.initialized && !self.full_pending
    }

    /// A failed synchronization has nothing scheduled that would retry it.
    fn needs_recovery(&self) -> bool {
        self.state == Readiness::Error && self.idle()
    }

    fn request_full(&mut self) -> Result<(), IndexerError> {
        self.requested_generation = self
            .requested_generation
            .checked_add(1)
            .ok_or(IndexerError::GenerationOverflow)?;
        self.full_pending = true;
        self.state = Readiness::Indexing;
        self.last_error = None;
        Ok(())
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
                if initial_sync && control.needs_initial_sync() {
                    control.request_full()?;
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
        *lock(&self.inner.watch)? = Some(watch);
        *lock(&self.inner.worker)? = Some(worker);
        let mut control = self.control()?;
        control.started = true;
        if initial_sync {
            control.request_full()?;
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
        if let Some(mut watch) = lock(&self.inner.watch)?.take() {
            watch.stop();
            watch.join()?;
        }
        if let Some(worker) = lock(&self.inner.worker)?.take() {
            worker.join().map_err(|_| IndexerError::WorkerPanicked)?;
        }
        self.control()?.started = false;
        Ok(())
    }

    pub fn notify_changes(&self, paths: BTreeSet<PathBuf>) {
        self.inner.notify_changes(paths);
    }

    pub fn wait_until_ready(&self, timeout: Option<Duration>) -> Result<Readiness, IndexerError> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut control = self.control()?;
        if !control.started {
            return Err(IndexerError::NotStarted);
        }
        if control.needs_initial_sync() {
            control.request_full()?;
            self.inner.condition.notify_all();
        }
        loop {
            if self.inner.watch_error()?.is_some() {
                return Ok(Readiness::Error);
            }
            if control.ready() {
                return Ok(Readiness::Ready);
            }
            if control.needs_recovery() {
                return Ok(Readiness::Error);
            }
            control = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(Readiness::Indexing);
                    }
                    self.inner
                        .condition
                        .wait_timeout(control, remaining)
                        .map_err(|_| IndexerError::PoisonedLock)?
                        .0
                }
                None => self
                    .inner
                    .condition
                    .wait(control)
                    .map_err(|_| IndexerError::PoisonedLock)?,
            };
        }
    }

    pub fn retrieve(&self, query: &str, timeout: Duration) -> Result<Value, IndexerError> {
        self.start(true)?;
        {
            let mut control = self.control()?;
            if control.needs_recovery() && self.inner.watch_error()?.is_none() {
                control.request_full()?;
                self.inner.condition.notify_all();
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let status = self.wait_until_ready(Some(remaining))?;
            if status != Readiness::Ready {
                let mut payload = self.payload(status.as_str());
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

            // Retrieve against the generation observed while ready, then discard the result
            // if the workspace moved on during the request.
            let context = lock(&self.inner.context)?;
            let generation = {
                let control = self.control()?;
                if !control.ready() || self.inner.watch_error()?.is_some() {
                    continue;
                }
                control.requested_generation
            };
            let result = context.retrieve(query);
            drop(context);
            let control = self.control()?;
            if !control.ready()
                || control.requested_generation != generation
                || self.inner.watch_error()?.is_some()
            {
                continue;
            }
            return Ok(match result {
                Ok(result) => {
                    let mut payload = self.payload("ready");
                    payload["formatted_retrieval"] = Value::String(result.formatted_retrieval);
                    payload["elapsed_ms"] = Value::from(result.elapsed_ms);
                    payload
                }
                Err(error) => {
                    let mut payload = self.payload("error");
                    payload["error"] = Value::String(error.to_string());
                    payload
                }
            });
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

    fn payload(&self, status: &str) -> Value {
        json!({
            "status": status,
            "workspace_folder": self.inner.root.to_string_lossy(),
        })
    }

    fn control(&self) -> Result<MutexGuard<'_, Control>, IndexerError> {
        lock(&self.inner.control)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, IndexerError> {
    mutex.lock().map_err(|_| IndexerError::PoisonedLock)
}

impl IndexerInner {
    fn notify_changes(&self, paths: BTreeSet<PathBuf>) {
        let matcher = match LayeredIgnoreMatcher::new(&self.root, &self.runtime_patterns) {
            Ok(matcher) => matcher,
            Err(error) => {
                self.record_error(error.to_string());
                return;
            }
        };
        let mut relevant = BTreeSet::new();
        let mut ignore_rules_changed = false;
        for path in paths {
            let Some(normalized) = normalize_workspace_event_path(&self.root, &path) else {
                continue;
            };
            let Some(relative) = normalized
                .strip_prefix(&self.root)
                .ok()
                .and_then(relative_path_string)
            else {
                continue;
            };
            if is_ignore_file(&normalized) {
                ignore_rules_changed = true;
                relevant.insert(normalized);
            } else if !matcher.ignores(&relative, normalized.is_dir()) {
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
        if !control.initialized || control.recovery_required || ignore_rules_changed {
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
        Ok(lock(&self.watch)?.as_ref().and_then(WatchHandle::error))
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
            while !control.stop && control.idle() {
                let Ok(next) = inner.condition.wait(control) else {
                    return;
                };
                control = next;
            }
            if control.stop {
                return;
            }
            let batch = Batch {
                full: control.full_pending,
                paths: std::mem::take(&mut control.pending_paths),
                generation: control.requested_generation,
            };
            control.full_pending = false;
            control.state = Readiness::Indexing;
            batch
        };
        let result = match inner.context.lock() {
            Err(_) => Err("workspace context lock is poisoned".to_owned()),
            Ok(context) => if batch.full {
                context.sync()
            } else {
                context.sync_paths(batch.paths)
            }
            .map(drop)
            .map_err(|error| error.to_string()),
        };
        let Ok(mut control) = inner.control.lock() else {
            return;
        };
        match result {
            Ok(()) => {
                control.initialized = true;
                control.recovery_required = false;
                control.synced_generation = control.synced_generation.max(batch.generation);
                control.last_error = None;
                control.state = if control.idle() {
                    Readiness::Ready
                } else {
                    Readiness::Indexing
                };
            }
            Err(error) => {
                control.recovery_required = true;
                if !control.idle() {
                    control.full_pending = true;
                    control.pending_paths.clear();
                }
                control.state = Readiness::Error;
                control.last_error = Some(error);
            }
        }
        inner.condition.notify_all();
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
