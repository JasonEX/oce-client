use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

const INTERNAL_DIRECTORIES: &[&str] = &[".git", ".oce-client"];
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WatchHandle")
            .field("error", &self.error())
            .finish_non_exhaustive()
    }
}

impl WatchHandle {
    pub fn start(
        root: &Path,
        debounce: Duration,
        callback: Arc<dyn Fn(BTreeSet<PathBuf>) + Send + Sync>,
        on_error: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, WatchError> {
        let (sender, receiver) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                let _ = sender.send(event);
            },
            Config::default().with_follow_symlinks(false),
        )
        .map_err(WatchError::Notify)?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(WatchError::Notify)?;

        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_error = Arc::clone(&error);
        let thread = thread::Builder::new()
            .name("oce-client-watcher".to_owned())
            .spawn(move || {
                let _watcher = watcher;
                let stopped = || thread_stop.load(Ordering::Acquire);
                while !stopped() {
                    // Collect events until the debounce window that opened with the first
                    // event closes, waking regularly to observe stop requests.
                    let mut paths = BTreeSet::new();
                    let mut deadline: Option<Instant> = None;
                    loop {
                        let timeout = deadline.map_or(STOP_POLL_INTERVAL, |deadline| {
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(STOP_POLL_INTERVAL)
                        });
                        match receiver.recv_timeout(timeout) {
                            Ok(event) => {
                                if !collect_event(event, &mut paths, &thread_error, &on_error) {
                                    return;
                                }
                                deadline.get_or_insert_with(|| Instant::now() + debounce);
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                if !stopped() {
                                    record_error(
                                        &thread_error,
                                        "filesystem watcher stopped unexpectedly".to_owned(),
                                        &on_error,
                                    );
                                }
                                return;
                            }
                        }
                        if stopped() {
                            return;
                        }
                        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            break;
                        }
                    }
                    paths.retain(|path| is_relevant(path));
                    if !paths.is_empty() {
                        callback(paths);
                    }
                }
            })
            .map_err(WatchError::Thread)?;
        Ok(Self {
            stop,
            error,
            thread: Some(thread),
        })
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|error| error.clone())
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn join(&mut self) -> Result<(), WatchError> {
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| WatchError::Panicked)?;
        }
        Ok(())
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop();
        let _ = self.join();
    }
}

fn collect_event(
    event: notify::Result<Event>,
    paths: &mut BTreeSet<PathBuf>,
    error: &Mutex<Option<String>>,
    on_error: &Arc<dyn Fn() + Send + Sync>,
) -> bool {
    match event {
        Ok(event) => {
            if !event.kind.is_access() {
                paths.extend(event.paths);
            }
            true
        }
        Err(source) => {
            record_error(
                error,
                format!("filesystem watcher failed: {source}"),
                on_error,
            );
            false
        }
    }
}

fn record_error(
    error: &Mutex<Option<String>>,
    message: String,
    on_error: &Arc<dyn Fn() + Send + Sync>,
) {
    if let Ok(mut current) = error.lock() {
        *current = Some(message);
    }
    on_error();
}

fn is_relevant(path: &Path) -> bool {
    !path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|part| INTERNAL_DIRECTORIES.contains(&part))
    })
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("filesystem watcher failed: {0}")]
    Notify(notify::Error),
    #[error("failed to start filesystem watcher thread: {0}")]
    Thread(std::io::Error),
    #[error("filesystem watcher thread panicked")]
    Panicked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use notify::event::AccessKind;

    #[test]
    fn access_events_do_not_schedule_workspace_changes() {
        let mut paths = BTreeSet::new();
        let error = Mutex::new(None);
        let on_error: Arc<dyn Fn() + Send + Sync> = Arc::new(|| panic!("unexpected error"));
        let event = Event::new(EventKind::Access(AccessKind::Read))
            .add_path(PathBuf::from("workspace/file.py"));

        assert!(collect_event(Ok(event), &mut paths, &error, &on_error));
        assert!(paths.is_empty());
    }
}
