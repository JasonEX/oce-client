use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::filesystem::{FileAdmissionError, LocalFileSource, normalize_workspace_event_path};
use crate::http::{ApiError, BlobApi, BlobUpload, RetrievalResult};
use crate::identity::{IdentityError, calculate_blob_identity};
use crate::ignore_rules::{IgnoreError, LayeredIgnoreMatcher};
use crate::state::{
    FileRecord, FileSource, FileStatus, StateError, StateStore, WorkspaceSnapshot,
    default_state_path, ensure_legacy_state_is_safe, reject_legacy_schema_at,
};

const DEFAULT_READY_POLL_ATTEMPTS: usize = 20;
const DEFAULT_READY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_MAX_FIND_MISSING: usize = 1000;
const DEFAULT_MAX_UPLOAD_BLOBS: usize = 1000;
const DEFAULT_MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncResult {
    pub uploaded_blob_names: Vec<String>,
    pub checkpoint_id: Option<String>,
    pub added_blobs: Vec<String>,
    pub deleted_blobs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncPlan {
    checkpoint_id: Option<String>,
    added_blobs: Vec<String>,
    deleted_blobs: Vec<String>,
}

pub struct WorkspaceContext {
    root: PathBuf,
    api: Arc<dyn BlobApi>,
    state: StateStore,
    file_source: LocalFileSource,
    runtime_patterns: Vec<String>,
    ready_poll_attempts: usize,
    ready_poll_interval: Duration,
    max_find_missing: usize,
    max_upload_blobs: usize,
    max_upload_bytes: usize,
}

impl std::fmt::Debug for WorkspaceContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceContext")
            .field("root", &self.root)
            .field("state", &self.state.path())
            .finish_non_exhaustive()
    }
}

impl WorkspaceContext {
    pub fn open(
        root: &Path,
        api: Arc<dyn BlobApi>,
        state_path: Option<&Path>,
        runtime_patterns: Vec<String>,
    ) -> Result<Self, ContextError> {
        let root = root
            .canonicalize()
            .map_err(|source| ContextError::Workspace {
                path: root.to_path_buf(),
                source,
            })?;
        if !root.is_dir() {
            return Err(ContextError::NotDirectory(root));
        }
        ensure_legacy_state_is_safe(&root)?;
        let state_path = state_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_state_path(&root));
        reject_legacy_schema_at(&state_path)?;
        let state = StateStore::open(&state_path)?;
        Ok(Self {
            root,
            api,
            state,
            file_source: LocalFileSource::default(),
            runtime_patterns,
            ready_poll_attempts: DEFAULT_READY_POLL_ATTEMPTS,
            ready_poll_interval: DEFAULT_READY_POLL_INTERVAL,
            max_find_missing: DEFAULT_MAX_FIND_MISSING,
            max_upload_blobs: DEFAULT_MAX_UPLOAD_BLOBS,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
        })
    }

    #[doc(hidden)]
    pub fn with_polling(mut self, attempts: usize, interval: Duration) -> Self {
        self.ready_poll_attempts = attempts;
        self.ready_poll_interval = interval;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_path(&self) -> &Path {
        self.state.path()
    }

    pub fn runtime_patterns(&self) -> &[String] {
        &self.runtime_patterns
    }

    fn matcher(&self) -> Result<LayeredIgnoreMatcher, ContextError> {
        LayeredIgnoreMatcher::new(&self.root, self.runtime_patterns.clone()).map_err(Into::into)
    }

    fn next_generation(&self) -> Result<i64, ContextError> {
        self.snapshot()?
            .generation
            .checked_add(1)
            .ok_or(ContextError::GenerationOverflow)
    }

    pub fn normalize_path(&self, path: &Path) -> Result<String, ContextError> {
        let relative = if path.is_absolute() {
            let normalized = normalize_absolute(path)?;
            normalized
                .strip_prefix(&self.root)
                .map(Path::to_path_buf)
                .map_err(|_| ContextError::OutsideWorkspace(path.to_path_buf()))?
        } else {
            normalize_lexical_relative(path)?
        };
        let mut normalized = relative.to_string_lossy().replace('\\', "/");
        while let Some(value) = normalized.strip_prefix("./") {
            normalized = value.to_owned();
        }
        if normalized.is_empty() || normalized == "." || normalized.starts_with("../") {
            return Err(ContextError::InvalidRelativePath(path.to_path_buf()));
        }
        Ok(normalized)
    }

    pub fn observe_file(&self, path: &Path, content: &str) -> Result<(), ContextError> {
        let normalized = self.normalize_path(path)?;
        if self.matcher()?.ignores(&normalized, false) {
            return Err(ContextError::IgnoredPath(normalized));
        }
        let size = content.len() as u64;
        if size > self.file_source.max_file_size {
            return Err(ContextError::File(FileAdmissionError::TooLarge {
                path: path.to_path_buf(),
                limit: self.file_source.max_file_size,
            }));
        }
        if content.as_bytes().contains(&0) {
            return Err(ContextError::File(FileAdmissionError::Binary(
                path.to_path_buf(),
            )));
        }
        let generation = self.next_generation()?;
        self.state.upsert_file(&FileRecord {
            path: normalized.clone(),
            blob_name: Some(calculate_blob_identity(&normalized, content)?),
            committed_blob_name: None,
            status: FileStatus::Present,
            content: Some(content.to_owned()),
            size,
            modified_ns: None,
            source: FileSource::Explicit,
            generation,
        })?;
        Ok(())
    }

    pub fn remove_file(&self, path: &Path) -> Result<(), ContextError> {
        let normalized = self.normalize_path(path)?;
        let generation = self.next_generation()?;
        self.state.mark_deleted_paths(&[normalized], generation)?;
        Ok(())
    }

    pub fn reconcile(&self) -> Result<WorkspaceSnapshot, ContextError> {
        let generation = self.next_generation()?;
        let current = self.snapshot()?;
        let explicit = current
            .files
            .iter()
            .filter(|(_, record)| {
                record.source == FileSource::Explicit && record.status == FileStatus::Present
            })
            .map(|(path, record)| (path.clone(), record.clone()))
            .collect::<BTreeMap<_, _>>();
        let matcher = self.matcher()?;
        let discovered = self.file_source.discover(&self.root, &matcher)?;
        let mut records = Vec::new();
        let mut known_paths = BTreeSet::new();
        let mut scanned_paths = BTreeSet::new();

        for file in discovered {
            let path = file.relative_path;
            scanned_paths.insert(path.clone());
            if let Some(existing) = current.files.get(&path)
                && existing.source == FileSource::Filesystem
                && existing.status == FileStatus::Present
                && existing.blob_name.is_some()
                && existing.modified_ns == file.modified_ns
                && existing.size == file.size
            {
                known_paths.insert(path.clone());
                records.push(FileRecord {
                    path,
                    blob_name: existing.blob_name.clone(),
                    committed_blob_name: existing.committed_blob_name.clone(),
                    status: FileStatus::Present,
                    content: None,
                    size: existing.size,
                    modified_ns: existing.modified_ns,
                    source: FileSource::Filesystem,
                    generation,
                });
                continue;
            }

            let content = match self.file_source.read_within(&self.root, &file.path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            let metadata = match fs::metadata(&file.path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let blob_name = calculate_blob_identity(&path, &content)?;
            if let Some(overlay) = explicit.get(&path)
                && overlay.blob_name.as_deref() != Some(&blob_name)
            {
                records.push(overlay.clone());
                known_paths.insert(path);
                continue;
            }
            known_paths.insert(path.clone());
            let committed_blob_name = current
                .files
                .get(&path)
                .and_then(|record| record.committed_blob_name.clone());
            records.push(FileRecord {
                path,
                blob_name: Some(blob_name),
                committed_blob_name,
                status: FileStatus::Present,
                content: Some(content),
                size: metadata.len(),
                modified_ns: metadata_modified_ns(&metadata),
                source: FileSource::Filesystem,
                generation,
            });
        }

        for (path, overlay) in explicit {
            if !scanned_paths.contains(&path) {
                known_paths.insert(path);
                records.push(overlay);
            }
        }
        let missing_paths = current
            .files
            .keys()
            .filter(|path| !known_paths.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        self.state
            .apply_file_changes(&records, &missing_paths, generation)?;
        self.snapshot()
    }

    pub fn reconcile_paths(
        &self,
        changed_paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<WorkspaceSnapshot, ContextError> {
        let mut resolved = BTreeSet::new();
        for path in changed_paths {
            if let Some(normalized) = normalize_workspace_event_path(&self.root, &path) {
                resolved.insert(normalized);
            }
        }
        if resolved.is_empty() {
            return self.snapshot();
        }
        for path in &resolved {
            if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some(".gitignore" | ".oceignore")
            ) || path.is_dir()
            {
                return self.reconcile();
            }
        }

        let generation = self.next_generation()?;
        let current = self.snapshot()?;
        let matcher = self.matcher()?;
        let mut records = Vec::new();
        let mut deleted = BTreeSet::new();
        for source_path in resolved {
            let relative = match source_path.strip_prefix(&self.root) {
                Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            let tracked = current
                .files
                .keys()
                .filter(|path| {
                    *path == &relative
                        || path.starts_with(&format!("{}/", relative.trim_end_matches('/')))
                })
                .cloned()
                .collect::<Vec<_>>();
            let admitted = !has_symlink_component(&self.root, &source_path)
                && source_path.is_file()
                && !matcher.ignores(&relative, false);
            if !admitted {
                deleted.extend(
                    tracked
                        .into_iter()
                        .filter(|path| current.files[path].source != FileSource::Explicit),
                );
                continue;
            }
            let metadata = match fs::metadata(&source_path) {
                Ok(metadata) if metadata.len() <= self.file_source.max_file_size => metadata,
                _ => {
                    deleted.extend(tracked);
                    continue;
                }
            };
            let content = match self.file_source.read_within(&self.root, &source_path) {
                Ok(content) => content,
                Err(_) => {
                    deleted.extend(tracked);
                    continue;
                }
            };
            let metadata = fs::metadata(&source_path).unwrap_or(metadata);
            let blob_name = calculate_blob_identity(&relative, &content)?;
            if let Some(existing) = current.files.get(&relative)
                && existing.source == FileSource::Explicit
                && existing.status == FileStatus::Present
                && existing.blob_name.as_deref() != Some(&blob_name)
            {
                records.push(existing.clone());
                continue;
            }
            records.push(FileRecord {
                path: relative.clone(),
                blob_name: Some(blob_name),
                committed_blob_name: current
                    .files
                    .get(&relative)
                    .and_then(|record| record.committed_blob_name.clone()),
                status: FileStatus::Present,
                content: Some(content),
                size: metadata.len(),
                modified_ns: metadata_modified_ns(&metadata),
                source: FileSource::Filesystem,
                generation,
            });
            deleted.remove(&relative);
        }
        self.state.apply_file_changes(
            &records,
            &deleted.into_iter().collect::<Vec<_>>(),
            generation,
        )?;
        self.snapshot()
    }

    pub fn snapshot(&self) -> Result<WorkspaceSnapshot, ContextError> {
        self.state.load_snapshot().map_err(Into::into)
    }

    fn plan_sync(&self) -> Result<SyncPlan, ContextError> {
        let snapshot = self.snapshot()?;
        let current = snapshot
            .files
            .values()
            .filter(|record| record.status == FileStatus::Present)
            .filter_map(|record| record.blob_name.clone())
            .collect::<BTreeSet<_>>();
        let committed = snapshot
            .files
            .values()
            .filter_map(|record| record.committed_blob_name.clone())
            .collect::<BTreeSet<_>>();
        Ok(SyncPlan {
            checkpoint_id: snapshot.checkpoint_id,
            added_blobs: current.difference(&committed).cloned().collect(),
            deleted_blobs: committed.difference(&current).cloned().collect(),
        })
    }

    pub fn sync(&self) -> Result<SyncResult, ContextError> {
        self.reconcile()?;
        self.sync_reconciled()
    }

    pub fn sync_paths(
        &self,
        changed_paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<SyncResult, ContextError> {
        self.reconcile_paths(changed_paths)?;
        self.sync_reconciled()
    }

    fn sync_reconciled(&self) -> Result<SyncResult, ContextError> {
        let plan = self.plan_sync()?;
        let records = self.state.load_records()?;
        let current_records = records
            .iter()
            .filter(|record| record.status == FileStatus::Present)
            .filter_map(|record| record.blob_name.clone().map(|name| (name, record)))
            .collect::<BTreeMap<_, _>>();
        let current_names = current_records.keys().cloned().collect::<Vec<_>>();
        let mut checkpoint_id = plan.checkpoint_id.clone();
        if let Some(current) = checkpoint_id.as_deref() {
            let status = self.api.blob_status(&[], Some(current))?;
            if status.checkpoint_not_found {
                checkpoint_id = None;
            }
        }
        let names_to_check = if checkpoint_id.is_none() {
            current_names.clone()
        } else {
            plan.added_blobs.clone()
        };
        let to_upload = self.find_missing(&names_to_check)?;

        let mut uploads = Vec::new();
        for name in &to_upload {
            let record = current_records
                .get(name)
                .ok_or_else(|| ContextError::MissingLocalBlob(name.clone()))?;
            let content = match self.state.load_file_content(&record.path)? {
                Some(content) => content,
                None => self
                    .file_source
                    .read_within(&self.root, &self.root.join(&record.path))?,
            };
            let actual = calculate_blob_identity(&record.path, &content)?;
            if &actual != name {
                return Err(ContextError::FileChanged(record.path.clone()));
            }
            uploads.push(BlobUpload {
                path: record.path.clone(),
                content,
                blob_name: name.clone(),
            });
        }
        let mut uploaded = BTreeSet::new();
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;
        for upload in uploads {
            let upload_bytes = upload.path.len() + upload.content.len();
            if !batch.is_empty()
                && (batch.len() >= self.max_upload_blobs
                    || batch_bytes + upload_bytes > self.max_upload_bytes)
            {
                self.upload_batch(&batch, &mut uploaded)?;
                batch.clear();
                batch_bytes = 0;
            }
            batch_bytes += upload_bytes;
            batch.push(upload);
        }
        if !batch.is_empty() {
            self.upload_batch(&batch, &mut uploaded)?;
        }
        self.wait_ready(&to_upload.into_iter().collect::<Vec<_>>(), None)?;

        let mut checkpoint_added = if checkpoint_id.is_none() {
            current_names.clone()
        } else {
            plan.added_blobs.clone()
        };
        let mut checkpoint_deleted = if checkpoint_id.is_none() {
            Vec::new()
        } else {
            plan.deleted_blobs.clone()
        };
        if !checkpoint_added.is_empty() || !checkpoint_deleted.is_empty() || checkpoint_id.is_none()
        {
            match self.api.checkpoint(
                checkpoint_id.as_deref(),
                &checkpoint_added,
                &checkpoint_deleted,
            ) {
                Ok(value) => checkpoint_id = Some(value),
                Err(error) if error.status_code() == Some(404) => {
                    if !self.find_missing(&current_names)?.is_empty() {
                        return Err(ContextError::CheckpointResetRequired(
                            "server checkpoint and blob state changed; retry sync".to_owned(),
                        ));
                    }
                    checkpoint_added = current_names.clone();
                    checkpoint_deleted.clear();
                    checkpoint_id = Some(self.api.checkpoint(None, &current_names, &[])?);
                }
                Err(error) => return Err(error.into()),
            }
        }
        let checkpoint_id = checkpoint_id.ok_or(ContextError::MissingCheckpoint)?;
        let deleted_names = plan.deleted_blobs.iter().cloned().collect::<BTreeSet<_>>();
        let deleted_paths = records
            .iter()
            .filter(|record| record.status != FileStatus::Present)
            .filter(|record| {
                record
                    .committed_blob_name
                    .as_ref()
                    .is_some_and(|name| deleted_names.contains(name))
            })
            .map(|record| record.path.clone())
            .collect::<Vec<_>>();
        let generation = self.snapshot()?.generation;
        self.state
            .commit_sync(&checkpoint_id, &deleted_paths, generation)?;
        Ok(SyncResult {
            uploaded_blob_names: uploaded.into_iter().collect(),
            checkpoint_id: Some(checkpoint_id),
            added_blobs: checkpoint_added,
            deleted_blobs: checkpoint_deleted,
        })
    }

    fn find_missing(&self, names: &[String]) -> Result<BTreeSet<String>, ContextError> {
        let mut missing_names = BTreeSet::new();
        for batch in names.chunks(self.max_find_missing) {
            let missing = self.api.find_missing(batch)?;
            missing_names.extend(missing.unknown_blob_names);
            missing_names.extend(missing.nonindexed_blob_names);
        }
        Ok(missing_names)
    }

    fn upload_batch(
        &self,
        batch: &[BlobUpload],
        uploaded: &mut BTreeSet<String>,
    ) -> Result<(), ContextError> {
        let received = self
            .api
            .batch_upload(batch)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let expected = batch
            .iter()
            .map(|upload| upload.blob_name.clone())
            .collect::<BTreeSet<_>>();
        if received != expected {
            return Err(ContextError::UploadMismatch {
                expected: expected.into_iter().collect(),
                received: received.into_iter().collect(),
            });
        }
        uploaded.extend(received);
        Ok(())
    }

    fn wait_ready(
        &self,
        names: &[String],
        checkpoint_id: Option<&str>,
    ) -> Result<(), ContextError> {
        if names.is_empty() {
            return Ok(());
        }
        let mut pending = names.iter().cloned().collect::<BTreeSet<_>>();
        for attempt in 0..self.ready_poll_attempts {
            let status = self
                .api
                .blob_status(&pending.iter().cloned().collect::<Vec<_>>(), checkpoint_id)?;
            if status.checkpoint_not_found {
                return Err(ContextError::CheckpointResetRequired(
                    "server checkpoint no longer exists".to_owned(),
                ));
            }
            if !status.unknown_blob_names.is_empty() {
                return Err(ContextError::UnknownBlobs(status.unknown_blob_names));
            }
            pending = status.nonindexed_blob_names.into_iter().collect();
            if pending.is_empty() {
                return Ok(());
            }
            if attempt + 1 < self.ready_poll_attempts {
                thread::sleep(self.ready_poll_interval);
            }
        }
        Err(ContextError::ReadyTimeout(pending.into_iter().collect()))
    }

    pub fn retrieve(&self, query: &str, scope: &str) -> Result<RetrievalResult, ContextError> {
        if !matches!(scope, "workspace" | "working_set") {
            return Err(ContextError::InvalidScope(scope.to_owned()));
        }
        let snapshot = self.snapshot()?;
        if snapshot.synced_generation != Some(snapshot.generation) {
            self.sync()?;
        }
        match self.retrieve_current(query) {
            Err(ContextError::Api(error)) if error.status_code() == Some(404) => {
                self.sync()?;
                self.retrieve_current(query)
            }
            result => result,
        }
    }

    fn retrieve_current(&self, query: &str) -> Result<RetrievalResult, ContextError> {
        let snapshot = self.snapshot()?;
        let names = snapshot
            .files
            .values()
            .filter(|record| record.status == FileStatus::Present)
            .filter_map(|record| record.blob_name.clone())
            .collect::<Vec<_>>();
        self.api
            .retrieve(
                query,
                snapshot.checkpoint_id.as_deref(),
                if snapshot.checkpoint_id.is_none() {
                    &names
                } else {
                    &[]
                },
                &[],
            )
            .map_err(Into::into)
    }
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, ContextError> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    normalize_lexical_absolute(path)
}

fn normalize_lexical_absolute(path: &Path) -> Result<PathBuf, ContextError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ContextError::InvalidRelativePath(path.to_path_buf()));
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

fn normalize_lexical_relative(path: &Path) -> Result<PathBuf, ContextError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ContextError::InvalidRelativePath(path.to_path_buf()));
                }
            }
            Component::Normal(value) => normalized.push(value),
            Component::Prefix(_) | Component::RootDir => {
                return Err(ContextError::InvalidRelativePath(path.to_path_buf()));
            }
        }
    }
    Ok(normalized)
}

fn has_symlink_component(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let mut candidate = root.to_path_buf();
    for component in relative.components() {
        candidate.push(component.as_os_str());
        if fs::symlink_metadata(&candidate)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn metadata_modified_ns(metadata: &fs::Metadata) -> Option<i64> {
    let duration = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    i64::try_from(duration.as_nanos()).ok()
}

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("workspace path failed for {path}: {source}")]
    Workspace {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("workspace is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("path is outside workspace: {0}")]
    OutsideWorkspace(PathBuf),
    #[error("invalid workspace-relative path: {0}")]
    InvalidRelativePath(PathBuf),
    #[error("path is ignored: {0}")]
    IgnoredPath(String),
    #[error("workspace generation overflow")]
    GenerationOverflow,
    #[error("missing local record for blob {0}")]
    MissingLocalBlob(String),
    #[error("file changed during sync: {0}")]
    FileChanged(String),
    #[error("batch-upload returned {received:?}; expected {expected:?}")]
    UploadMismatch {
        expected: Vec<String>,
        received: Vec<String>,
    },
    #[error("unknown blobs: {0:?}")]
    UnknownBlobs(Vec<String>),
    #[error("blobs did not become ready: {0:?}")]
    ReadyTimeout(Vec<String>),
    #[error("server checkpoint recovery is required: {0}")]
    CheckpointResetRequired(String),
    #[error("server did not return a checkpoint")]
    MissingCheckpoint,
    #[error("unknown scope: {0}")]
    InvalidScope(String),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    File(#[from] FileAdmissionError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Ignore(#[from] IgnoreError),
    #[error(transparent)]
    State(#[from] StateError),
}
