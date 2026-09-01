use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::ignore_rules::LayeredIgnoreMatcher;

pub const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    pub relative_path: String,
    pub path: PathBuf,
    pub size: u64,
    pub modified_ns: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct LocalFileSource {
    pub max_file_size: u64,
}

impl Default for LocalFileSource {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

impl LocalFileSource {
    pub fn discover(
        &self,
        root: &Path,
        matcher: &LayeredIgnoreMatcher,
    ) -> Result<Vec<DiscoveredFile>, FileAdmissionError> {
        let root = root
            .canonicalize()
            .map_err(|source| FileAdmissionError::Io {
                path: root.to_path_buf(),
                source,
            })?;
        let mut files = Vec::new();
        self.discover_directory(&root, &root, matcher, &mut files)?;
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(files)
    }

    fn discover_directory(
        &self,
        root: &Path,
        directory: &Path,
        matcher: &LayeredIgnoreMatcher,
        files: &mut Vec<DiscoveredFile>,
    ) -> Result<(), FileAdmissionError> {
        let entries = fs::read_dir(directory).map_err(|source| FileAdmissionError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let mut entries =
            entries
                .collect::<Result<Vec<_>, _>>()
                .map_err(|source| FileAdmissionError::Io {
                    path: directory.to_path_buf(),
                    source,
                })?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|source| FileAdmissionError::Io {
                    path: path.clone(),
                    source,
                })?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let relative = workspace_relative_path(root, &path)?;
            if metadata.is_dir() {
                if matcher.ignores(&relative, true) {
                    continue;
                }
                let canonical = match path.canonicalize() {
                    Ok(value) if value.starts_with(root) => value,
                    _ => continue,
                };
                self.discover_directory(root, &canonical, matcher, files)?;
                continue;
            }
            if !metadata.is_file()
                || matcher.ignores(&relative, false)
                || metadata.len() > self.max_file_size
            {
                continue;
            }
            let canonical = match path.canonicalize() {
                Ok(value) if value.starts_with(root) => value,
                _ => continue,
            };
            files.push(DiscoveredFile {
                relative_path: relative,
                path: canonical,
                size: metadata.len(),
                modified_ns: modified_ns(&metadata),
            });
        }
        Ok(())
    }

    pub fn read_within(&self, root: &Path, path: &Path) -> Result<String, FileAdmissionError> {
        let root = root
            .canonicalize()
            .map_err(|source| FileAdmissionError::Io {
                path: root.to_path_buf(),
                source,
            })?;
        let resolved_before = path
            .canonicalize()
            .map_err(|source| FileAdmissionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if !resolved_before.starts_with(&root) {
            return Err(FileAdmissionError::OutsideWorkspace(path.to_path_buf()));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| FileAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(FileAdmissionError::SymbolicLink(path.to_path_buf()));
        }
        if !metadata.is_file() {
            return Err(FileAdmissionError::NotRegular(path.to_path_buf()));
        }
        if metadata.len() > self.max_file_size {
            return Err(FileAdmissionError::TooLarge {
                path: path.to_path_buf(),
                limit: self.max_file_size,
            });
        }
        let mut file = fs::File::open(path).map_err(|source| FileAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let opened_metadata = file.metadata().map_err(|source| FileAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !opened_metadata.is_file() {
            return Err(FileAdmissionError::NotRegular(path.to_path_buf()));
        }
        if opened_metadata.len() > self.max_file_size {
            return Err(FileAdmissionError::TooLarge {
                path: path.to_path_buf(),
                limit: self.max_file_size,
            });
        }
        let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
        file.by_ref()
            .take(self.max_file_size + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| FileAdmissionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() as u64 > self.max_file_size {
            return Err(FileAdmissionError::TooLarge {
                path: path.to_path_buf(),
                limit: self.max_file_size,
            });
        }
        if bytes.contains(&0) {
            return Err(FileAdmissionError::Binary(path.to_path_buf()));
        }
        let resolved_after = path
            .canonicalize()
            .map_err(|source| FileAdmissionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if resolved_after != resolved_before || !resolved_after.starts_with(&root) {
            return Err(FileAdmissionError::OutsideWorkspace(path.to_path_buf()));
        }
        let current_metadata = fs::metadata(path).map_err(|source| FileAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !same_file(&opened_metadata, &current_metadata) {
            return Err(FileAdmissionError::OutsideWorkspace(path.to_path_buf()));
        }
        String::from_utf8(bytes).map_err(|_| FileAdmissionError::InvalidUtf8(path.to_path_buf()))
    }
}

#[cfg(unix)]
fn same_file(opened: &fs::Metadata, current: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    opened.dev() == current.dev() && opened.ino() == current.ino()
}

#[cfg(not(unix))]
fn same_file(opened: &fs::Metadata, current: &fs::Metadata) -> bool {
    opened.len() == current.len()
        && opened.modified().ok() == current.modified().ok()
        && opened.created().ok() == current.created().ok()
}

pub fn workspace_relative_path(root: &Path, path: &Path) -> Result<String, FileAdmissionError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| FileAdmissionError::OutsideWorkspace(path.to_path_buf()))?;
    if relative.as_os_str().is_empty() {
        return Err(FileAdmissionError::InvalidRelativePath(path.to_path_buf()));
    }
    let normalized = relative.to_string_lossy().replace('\\', "/");
    if normalized == "." || normalized.starts_with("../") {
        return Err(FileAdmissionError::InvalidRelativePath(path.to_path_buf()));
    }
    Ok(normalized)
}

fn modified_ns(metadata: &fs::Metadata) -> Option<i64> {
    let duration = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_nanos()).ok()
}

#[derive(Debug, thiserror::Error)]
pub enum FileAdmissionError {
    #[error("symbolic links are not supported: {0}")]
    SymbolicLink(PathBuf),
    #[error("path is not a regular file: {0}")]
    NotRegular(PathBuf),
    #[error("file exceeds {limit} bytes: {path}")]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("binary file is not supported: {0}")]
    Binary(PathBuf),
    #[error("file is not valid UTF-8: {0}")]
    InvalidUtf8(PathBuf),
    #[error("path is outside workspace: {0}")]
    OutsideWorkspace(PathBuf),
    #[error("invalid workspace-relative path: {0}")]
    InvalidRelativePath(PathBuf),
    #[error("filesystem operation failed for {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}
