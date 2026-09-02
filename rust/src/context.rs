use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::filesystem::{
    FileAdmissionError, LocalFileSource, lexical_normalize, modified_ns,
    normalize_workspace_event_path, relative_path_string,
};
use crate::http::{ApiError, BlobApi, BlobUpload, RetrievalResult};
use crate::identity::{IdentityError, calculate_blob_identity};
use crate::ignore_rules::{IgnoreError, LayeredIgnoreMatcher, is_ignore_file};
use crate::state::{
    FileRecord, FileSource, FileStatus, StateError, StateStore, WorkspaceSnapshot,
    default_state_path, ensure_schema_version,
};

const READY_POLL_ATTEMPTS: usize = 20;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_FIND_MISSING: usize = 1000;
const MAX_UPLOAD_BLOBS: usize = 1000;
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncResult {
    pub uploaded_blob_names: Vec<String>,
    pub checkpoint_id: Option<String>,
    pub added_blobs: Vec<String>,
    pub deleted_blobs: Vec<String>,
}

pub struct WorkspaceContext {
    root: PathBuf,
    api: Arc<dyn BlobApi>,
    state: StateStore,
    file_source: LocalFileSource,
    runtime_patterns: Vec<String>,
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
        let state_path = state_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_state_path(&root));
        ensure_schema_version(&state_path)?;
        let state = StateStore::open(&state_path)?;
        Ok(Self {
            root,
            api,
            state,
            file_source: LocalFileSource::default(),
            runtime_patterns,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runtime_patterns(&self) -> &[String] {
        &self.runtime_patterns
    }

    fn matcher(&self) -> Result<LayeredIgnoreMatcher, ContextError> {
        LayeredIgnoreMatcher::new(&self.root, &self.runtime_patterns).map_err(Into::into)
    }

    fn next_generation(&self) -> Result<i64, ContextError> {
        self.state
            .generation()?
            .checked_add(1)
            .ok_or(ContextError::GenerationOverflow)
    }

    /// Converts a user-supplied path into the workspace-relative record key.
    pub fn normalize_path(&self, path: &Path) -> Result<String, ContextError> {
        let invalid = || ContextError::InvalidRelativePath(path.to_path_buf());
        let relative = if path.is_absolute() {
            let normalized = match path.canonicalize() {
                Ok(canonical) => canonical,
                Err(_) => lexical_normalize(path).ok_or_else(invalid)?,
            };
            normalized
                .strip_prefix(&self.root)
                .map(Path::to_path_buf)
                .map_err(|_| ContextError::OutsideWorkspace(path.to_path_buf()))?
        } else {
            if path
                .components()
                .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
            {
                return Err(invalid());
            }
            lexical_normalize(path).ok_or_else(invalid)?
        };
        relative_path_string(&relative).ok_or_else(invalid)
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
        self.state
            .apply_file_changes(&[], &[normalized], generation)?;
        Ok(())
    }

    /// Lists the workspace-relative paths a sync would upload, without contacting the server.
    pub fn admitted_files(&self) -> Result<Vec<String>, ContextError> {
        let matcher = self.matcher()?;
        Ok(self
            .file_source
            .discover(&self.root, &matcher)?
            .into_iter()
            .filter(|file| self.file_source.read_within(&self.root, &file.path).is_ok())
            .map(|file| file.relative_path)
            .collect())
    }

    /// Builds the record for an admitted filesystem path. Returns the explicit overlay when
    /// one exists and differs from disk, and `None` when the file cannot be read and no
    /// overlay protects it.
    fn filesystem_record(
        &self,
        relative: &str,
        source_path: &Path,
        current: &WorkspaceSnapshot,
        generation: i64,
    ) -> Result<Option<FileRecord>, ContextError> {
        let existing = current.files.get(relative);
        let overlay = existing.filter(|record| {
            record.source == FileSource::Explicit && record.status == FileStatus::Present
        });
        let Ok((content, metadata)) = self.file_source.read_within(&self.root, source_path) else {
            return Ok(overlay.cloned());
        };
        let blob_name = calculate_blob_identity(relative, &content)?;
        if let Some(overlay) = overlay
            && overlay.blob_name.as_deref() != Some(&blob_name)
        {
            return Ok(Some(overlay.clone()));
        }
        Ok(Some(FileRecord {
            path: relative.to_owned(),
            blob_name: Some(blob_name),
            committed_blob_name: existing.and_then(|record| record.committed_blob_name.clone()),
            status: FileStatus::Present,
            content: Some(content),
            size: metadata.len(),
            modified_ns: modified_ns(&metadata),
            source: FileSource::Filesystem,
            generation,
        }))
    }

    pub fn reconcile(&self) -> Result<WorkspaceSnapshot, ContextError> {
        let generation = self.next_generation()?;
        let current = self.snapshot()?;
        let matcher = self.matcher()?;
        let mut records = Vec::new();
        let mut known_paths = BTreeSet::new();
        for file in self.file_source.discover(&self.root, &matcher)? {
            let path = file.relative_path;
            if let Some(existing) = current.files.get(&path)
                && existing.source == FileSource::Filesystem
                && existing.status == FileStatus::Present
                && existing.blob_name.is_some()
                && existing.modified_ns == file.modified_ns
                && existing.size == file.size
            {
                // Unchanged on disk; the stored record stays as it is.
                known_paths.insert(path);
                continue;
            }
            if let Some(record) = self.filesystem_record(&path, &file.path, &current, generation)? {
                known_paths.insert(path);
                records.push(record);
            }
        }
        // Explicit overlays without a matching disk file remain durable.
        for (path, record) in &current.files {
            if record.source == FileSource::Explicit
                && record.status == FileStatus::Present
                && !known_paths.contains(path)
            {
                known_paths.insert(path.clone());
                records.push(record.clone());
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
        let resolved = changed_paths
            .into_iter()
            .filter_map(|path| normalize_workspace_event_path(&self.root, &path))
            .collect::<BTreeSet<_>>();
        if resolved.is_empty() {
            return self.snapshot();
        }
        if resolved
            .iter()
            .any(|path| is_ignore_file(path) || path.is_dir())
        {
            return self.reconcile();
        }

        let generation = self.next_generation()?;
        let current = self.snapshot()?;
        let matcher = self.matcher()?;
        let mut records = Vec::new();
        let mut deleted = BTreeSet::new();
        for source_path in resolved {
            let Some(relative) = source_path
                .strip_prefix(&self.root)
                .ok()
                .and_then(relative_path_string)
            else {
                continue;
            };
            // Filesystem records at or below the changed path; explicit overlays stay.
            let prefix = format!("{relative}/");
            let tracked = current
                .files
                .iter()
                .filter(|(path, record)| {
                    record.source != FileSource::Explicit
                        && (**path == relative || path.starts_with(&prefix))
                })
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            let admitted = !has_symlink_component(&self.root, &source_path)
                && source_path.is_file()
                && !matcher.ignores(&relative, false);
            let record = if admitted {
                self.filesystem_record(&relative, &source_path, &current, generation)?
            } else {
                None
            };
            match record {
                Some(record) => {
                    deleted.remove(&relative);
                    records.push(record);
                }
                None => deleted.extend(tracked),
            }
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
        let snapshot = self.snapshot()?;
        let present = snapshot
            .files
            .values()
            .filter(|record| record.status == FileStatus::Present);
        let current_records = present
            .clone()
            .filter_map(|record| record.blob_name.clone().map(|name| (name, record)))
            .collect::<BTreeMap<_, _>>();
        let current_names = current_records.keys().cloned().collect::<Vec<_>>();
        let committed = snapshot
            .files
            .values()
            .filter_map(|record| record.committed_blob_name.clone())
            .collect::<BTreeSet<_>>();
        let added_blobs = current_names
            .iter()
            .filter(|name| !committed.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        let deleted_blobs = committed
            .iter()
            .filter(|name| !current_records.contains_key(*name))
            .cloned()
            .collect::<BTreeSet<_>>();

        let mut checkpoint_id = snapshot.checkpoint_id.clone();
        if let Some(current) = checkpoint_id.as_deref()
            && self
                .api
                .blob_status(&[], Some(current))?
                .checkpoint_not_found
        {
            checkpoint_id = None;
        }
        // Without a usable server checkpoint the whole inventory forms a fresh one.
        let (mut checkpoint_added, mut checkpoint_deleted) = if checkpoint_id.is_none() {
            (current_names.clone(), Vec::new())
        } else {
            (added_blobs, deleted_blobs.iter().cloned().collect())
        };

        let to_upload = self.find_missing(&checkpoint_added)?;
        let mut uploads = Vec::new();
        for name in &to_upload {
            let record = current_records
                .get(name)
                .ok_or_else(|| ContextError::MissingLocalBlob(name.clone()))?;
            let content = match self.state.load_file_content(&record.path)? {
                Some(content) => content,
                None => {
                    self.file_source
                        .read_within(&self.root, &self.root.join(&record.path))?
                        .0
                }
            };
            if calculate_blob_identity(&record.path, &content)? != *name {
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
                && (batch.len() >= MAX_UPLOAD_BLOBS
                    || batch_bytes + upload_bytes > MAX_UPLOAD_BYTES)
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
        self.wait_ready(&to_upload)?;

        if checkpoint_id.is_none() || !checkpoint_added.is_empty() || !checkpoint_deleted.is_empty()
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
        let deleted_paths = snapshot
            .files
            .values()
            .filter(|record| record.status != FileStatus::Present)
            .filter(|record| {
                record
                    .committed_blob_name
                    .as_ref()
                    .is_some_and(|name| deleted_blobs.contains(name))
            })
            .map(|record| record.path.clone())
            .collect::<Vec<_>>();
        self.state
            .commit_sync(&checkpoint_id, &deleted_paths, snapshot.generation)?;
        Ok(SyncResult {
            uploaded_blob_names: uploaded.into_iter().collect(),
            checkpoint_id: Some(checkpoint_id),
            added_blobs: checkpoint_added,
            deleted_blobs: checkpoint_deleted,
        })
    }

    fn find_missing(&self, names: &[String]) -> Result<BTreeSet<String>, ContextError> {
        let mut missing_names = BTreeSet::new();
        for batch in names.chunks(MAX_FIND_MISSING) {
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

    fn wait_ready(&self, names: &BTreeSet<String>) -> Result<(), ContextError> {
        if names.is_empty() {
            return Ok(());
        }
        let mut pending = names.iter().cloned().collect::<Vec<_>>();
        for attempt in 0..READY_POLL_ATTEMPTS {
            let status = self.api.blob_status(&pending, None)?;
            if status.checkpoint_not_found {
                return Err(ContextError::CheckpointResetRequired(
                    "server checkpoint no longer exists".to_owned(),
                ));
            }
            if !status.unknown_blob_names.is_empty() {
                return Err(ContextError::UnknownBlobs(status.unknown_blob_names));
            }
            pending = status.nonindexed_blob_names;
            if pending.is_empty() {
                return Ok(());
            }
            if attempt + 1 < READY_POLL_ATTEMPTS {
                thread::sleep(READY_POLL_INTERVAL);
            }
        }
        Err(ContextError::ReadyTimeout(pending))
    }

    pub fn retrieve(&self, query: &str) -> Result<RetrievalResult, ContextError> {
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
