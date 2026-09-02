use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Serialize;

pub const STATE_SCHEMA_VERSION: i32 = 1;
pub const STATE_NAME: &str = "state-v1.sqlite3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Present,
    Deleted,
}

impl FileStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Deleted => "deleted",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "present" => Ok(Self::Present),
            "deleted" => Ok(Self::Deleted),
            value => Err(StateError::InvalidValue(format!(
                "unknown file status {value:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileSource {
    Filesystem,
    Explicit,
}

impl FileSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
            Self::Explicit => "explicit",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "filesystem" => Ok(Self::Filesystem),
            "explicit" => Ok(Self::Explicit),
            value => Err(StateError::InvalidValue(format!(
                "unknown file source {value:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileRecord {
    pub path: String,
    pub blob_name: Option<String>,
    pub committed_blob_name: Option<String>,
    pub status: FileStatus,
    #[serde(skip_serializing)]
    pub content: Option<String>,
    pub size: u64,
    pub modified_ns: Option<i64>,
    pub source: FileSource,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceSnapshot {
    pub files: BTreeMap<String, FileRecord>,
    pub checkpoint_id: Option<String>,
    pub generation: i64,
    pub synced_generation: Option<i64>,
}

#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StateError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let connection = Connection::open(path).map_err(StateError::Sqlite)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(30))
            .map_err(StateError::Sqlite)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(StateError::Sqlite)?;
        connection
            .execute_batch(&format!(
                "
                CREATE TABLE IF NOT EXISTS meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS files (
                    path TEXT PRIMARY KEY,
                    blob_name TEXT,
                    committed_blob_name TEXT,
                    status TEXT NOT NULL,
                    content TEXT,
                    size INTEGER NOT NULL DEFAULT 0,
                    mtime_ns INTEGER,
                    source TEXT NOT NULL DEFAULT 'filesystem',
                    generation INTEGER NOT NULL DEFAULT 0
                );
                PRAGMA user_version = {STATE_SCHEMA_VERSION};
                "
            ))
            .map_err(StateError::Sqlite)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection: Mutex::new(connection),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StateError> {
        self.connection.lock().map_err(|_| StateError::PoisonedLock)
    }

    fn transaction(connection: &mut Connection) -> Result<Transaction<'_>, StateError> {
        connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StateError::Sqlite)
    }

    /// Current workspace generation without loading the file inventory.
    pub fn generation(&self) -> Result<i64, StateError> {
        let connection = self.connection()?;
        parse_meta_i64(get_meta_from(&connection, "generation")?, 0)
    }

    pub fn load_snapshot(&self) -> Result<WorkspaceSnapshot, StateError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT path, blob_name, committed_blob_name, status,
                        CASE WHEN source = 'explicit' THEN content ELSE NULL END,
                        size, mtime_ns, source, generation
                 FROM files ORDER BY path",
            )
            .map_err(StateError::Sqlite)?;
        let rows = statement
            .query_map([], |row| {
                Ok(RawFileRecord {
                    path: row.get(0)?,
                    blob_name: row.get(1)?,
                    committed_blob_name: row.get(2)?,
                    status: row.get(3)?,
                    content: row.get(4)?,
                    size: row.get(5)?,
                    modified_ns: row.get(6)?,
                    source: row.get(7)?,
                    generation: row.get(8)?,
                })
            })
            .map_err(StateError::Sqlite)?;
        let mut files = BTreeMap::new();
        for row in rows {
            let record: FileRecord = row.map_err(StateError::Sqlite)?.try_into()?;
            files.insert(record.path.clone(), record);
        }
        drop(statement);
        let checkpoint_id =
            get_meta_from(&connection, "checkpoint_id")?.filter(|value| !value.is_empty());
        let generation = parse_meta_i64(get_meta_from(&connection, "generation")?, 0)?;
        let synced_generation = get_meta_from(&connection, "synced_generation")?
            .map(|value| parse_i64(&value))
            .transpose()?;
        Ok(WorkspaceSnapshot {
            files,
            checkpoint_id,
            generation,
            synced_generation,
        })
    }

    pub fn load_file_content(&self, path: &str) -> Result<Option<String>, StateError> {
        self.connection()?
            .query_row("SELECT content FROM files WHERE path = ?1", [path], |row| {
                row.get(0)
            })
            .optional()
            .map(|value| value.flatten())
            .map_err(StateError::Sqlite)
    }

    pub fn upsert_file(&self, record: &FileRecord) -> Result<(), StateError> {
        let mut connection = self.connection()?;
        let transaction = Self::transaction(&mut connection)?;
        upsert_record(&transaction, record, record.generation)?;
        set_meta_in(&transaction, "generation", &record.generation.to_string())?;
        transaction.commit().map_err(StateError::Sqlite)
    }

    /// Upserts `records`, marks `deleted_paths` as deleted, and advances the generation in
    /// one transaction.
    pub fn apply_file_changes(
        &self,
        records: &[FileRecord],
        deleted_paths: &[String],
        generation: i64,
    ) -> Result<(), StateError> {
        let mut connection = self.connection()?;
        let transaction = Self::transaction(&mut connection)?;
        for record in records {
            upsert_record(&transaction, record, generation)?;
        }
        for path in deleted_paths {
            transaction
                .execute(
                    "UPDATE files
                     SET status = 'deleted', blob_name = NULL, content = NULL,
                         source = 'filesystem', generation = ?1
                     WHERE path = ?2",
                    params![generation, path],
                )
                .map_err(StateError::Sqlite)?;
        }
        set_meta_in(&transaction, "generation", &generation.to_string())?;
        transaction.commit().map_err(StateError::Sqlite)
    }

    /// Records a successful server checkpoint for the inventory at `synced_generation`.
    pub fn commit_sync(
        &self,
        checkpoint_id: &str,
        deleted_paths: &[String],
        synced_generation: i64,
    ) -> Result<(), StateError> {
        let mut connection = self.connection()?;
        let transaction = Self::transaction(&mut connection)?;
        transaction
            .execute(
                "UPDATE files SET committed_blob_name = blob_name WHERE status = 'present'",
                [],
            )
            .map_err(StateError::Sqlite)?;
        transaction
            .execute(
                "UPDATE files SET content = NULL WHERE source = 'filesystem'",
                [],
            )
            .map_err(StateError::Sqlite)?;
        for path in deleted_paths {
            transaction
                .execute("DELETE FROM files WHERE path = ?1", [path])
                .map_err(StateError::Sqlite)?;
        }
        set_meta_in(&transaction, "checkpoint_id", checkpoint_id)?;
        set_meta_in(
            &transaction,
            "synced_generation",
            &synced_generation.to_string(),
        )?;
        transaction.commit().map_err(StateError::Sqlite)
    }
}

pub fn default_state_path(root: &Path) -> PathBuf {
    root.join(".oce-client").join(STATE_NAME)
}

/// Rejects an existing state database whose schema version this client cannot read.
pub fn ensure_schema_version(path: &Path) -> Result<(), StateError> {
    if !path.is_file() {
        return Ok(());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(StateError::Sqlite)?;
    let version: i32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(StateError::Sqlite)?;
    if version != STATE_SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: version,
            expected: STATE_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn get_meta_from(connection: &Connection, key: &str) -> Result<Option<String>, StateError> {
    connection
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(StateError::Sqlite)
}

fn set_meta_in(transaction: &Transaction<'_>, key: &str, value: &str) -> Result<(), StateError> {
    transaction
        .execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(StateError::Sqlite)?;
    Ok(())
}

fn upsert_record(
    transaction: &Transaction<'_>,
    record: &FileRecord,
    generation: i64,
) -> Result<(), StateError> {
    let size = i64::try_from(record.size).map_err(|_| {
        StateError::InvalidValue(format!("file size is too large: {}", record.size))
    })?;
    transaction
        .execute(
            "INSERT INTO files
             (path, blob_name, committed_blob_name, status, content, size,
              mtime_ns, source, generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(path) DO UPDATE SET
               blob_name = excluded.blob_name,
               committed_blob_name = COALESCE(excluded.committed_blob_name, files.committed_blob_name),
               status = excluded.status,
               content = excluded.content,
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               source = excluded.source,
               generation = excluded.generation",
            params![
                record.path,
                record.blob_name,
                record.committed_blob_name,
                record.status.as_str(),
                record.content,
                size,
                record.modified_ns,
                record.source.as_str(),
                generation,
            ],
        )
        .map_err(StateError::Sqlite)?;
    Ok(())
}

#[derive(Debug)]
struct RawFileRecord {
    path: String,
    blob_name: Option<String>,
    committed_blob_name: Option<String>,
    status: String,
    content: Option<String>,
    size: i64,
    modified_ns: Option<i64>,
    source: String,
    generation: i64,
}

impl TryFrom<RawFileRecord> for FileRecord {
    type Error = StateError;

    fn try_from(value: RawFileRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            path: value.path,
            blob_name: value.blob_name,
            committed_blob_name: value.committed_blob_name,
            status: FileStatus::parse(&value.status)?,
            content: value.content,
            size: u64::try_from(value.size).map_err(|_| {
                StateError::InvalidValue(format!("negative file size: {}", value.size))
            })?,
            modified_ns: value.modified_ns,
            source: FileSource::parse(&value.source)?,
            generation: value.generation,
        })
    }
}

fn parse_meta_i64(value: Option<String>, default: i64) -> Result<i64, StateError> {
    value.map(|raw| parse_i64(&raw)).unwrap_or(Ok(default))
}

fn parse_i64(value: &str) -> Result<i64, StateError> {
    value
        .parse()
        .map_err(|_| StateError::InvalidValue(format!("invalid integer metadata {value:?}")))
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("SQLite state failed: {0}")]
    Sqlite(rusqlite::Error),
    #[error("failed to create state directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("state lock is poisoned")]
    PoisonedLock,
    #[error("invalid state value: {0}")]
    InvalidValue(String),
    #[error(
        "state database {path} uses schema version {found}; this client requires version {expected}"
    )]
    UnsupportedSchema {
        path: PathBuf,
        found: i32,
        expected: i32,
    },
}
