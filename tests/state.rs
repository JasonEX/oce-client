use std::fs;
use std::path::Path;

use oce_client::state::{
    FileRecord, FileSource, FileStatus, StateError, StateStore, default_state_path,
    ensure_legacy_state_is_safe, legacy_state_path, reject_legacy_schema_at,
};
use rusqlite::Connection;

fn record(path: &str, source: FileSource, generation: i64) -> FileRecord {
    FileRecord {
        path: path.to_owned(),
        blob_name: Some("blob-a".to_owned()),
        committed_blob_name: None,
        status: FileStatus::Present,
        content: Some("content".to_owned()),
        size: 7,
        modified_ns: Some(123),
        source,
        generation,
    }
}

#[test]
fn state_store_persists_inventory_and_commit_barrier() {
    let root = tempfile::tempdir().expect("workspace");
    let path = default_state_path(root.path());
    let state = StateStore::open(&path).expect("open state");
    state
        .apply_file_changes(&[record("src/main.py", FileSource::Filesystem, 1)], &[], 1)
        .expect("store inventory");

    let pending = state.load_snapshot().expect("pending snapshot");
    assert_eq!(pending.generation, 1);
    assert_eq!(pending.synced_generation, None);
    assert_eq!(pending.files["src/main.py"].content, None);

    state
        .commit_sync("chain:1", &[], 1)
        .expect("commit checkpoint");
    let committed = state.load_snapshot().expect("committed snapshot");
    assert_eq!(committed.checkpoint_id.as_deref(), Some("chain:1"));
    assert_eq!(committed.synced_generation, Some(1));
    assert_eq!(
        committed.files["src/main.py"]
            .committed_blob_name
            .as_deref(),
        Some("blob-a")
    );
    assert_eq!(state.load_file_content("src/main.py").unwrap(), None);
}

#[test]
fn explicit_content_remains_durable_after_commit() {
    let root = tempfile::tempdir().expect("workspace");
    let state = StateStore::open(&default_state_path(root.path())).expect("open state");
    state
        .upsert_file(&record("src/main.py", FileSource::Explicit, 1))
        .expect("store explicit overlay and generation");
    assert_eq!(state.load_snapshot().unwrap().generation, 1);
    state
        .commit_sync("chain:1", &[], 1)
        .expect("commit checkpoint");

    assert_eq!(
        state.load_file_content("src/main.py").unwrap().as_deref(),
        Some("content")
    );
}

#[test]
fn rust_state_does_not_reuse_the_legacy_database() {
    let root = tempfile::tempdir().expect("workspace");

    assert_eq!(
        legacy_state_path(root.path()),
        root.path().join(".oce-client/state.sqlite3")
    );
    assert_eq!(
        default_state_path(root.path()),
        root.path().join(".oce-client/state-v1.sqlite3")
    );
}

#[test]
fn migration_rejects_legacy_explicit_overlay() {
    let root = tempfile::tempdir().expect("workspace");
    create_legacy_state(root.path(), true, Some("1"), Some("1"));

    assert!(matches!(
        ensure_legacy_state_is_safe(root.path()),
        Err(StateError::UnsafeLegacyState { .. })
    ));
}

#[test]
fn migration_rejects_uncommitted_legacy_generation() {
    let root = tempfile::tempdir().expect("workspace");
    create_legacy_state(root.path(), false, Some("2"), Some("1"));

    assert!(matches!(
        ensure_legacy_state_is_safe(root.path()),
        Err(StateError::UnsafeLegacyState { .. })
    ));
}

#[test]
fn migration_allows_synced_filesystem_only_legacy_state() {
    let root = tempfile::tempdir().expect("workspace");
    create_legacy_state(root.path(), false, Some("1"), Some("1"));

    ensure_legacy_state_is_safe(root.path()).expect("safe full resync boundary");
    assert!(matches!(
        reject_legacy_schema_at(&legacy_state_path(root.path())),
        Err(StateError::UnsupportedSchema { found: 0, .. })
    ));
}

fn create_legacy_state(
    root: &Path,
    explicit: bool,
    generation: Option<&str>,
    synced_generation: Option<&str>,
) {
    let path = legacy_state_path(root);
    fs::create_dir_all(path.parent().unwrap()).expect("state directory");
    let connection = Connection::open(path).expect("legacy database");
    connection
        .execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE files (
                 path TEXT PRIMARY KEY,
                 blob_name TEXT,
                 committed_blob_name TEXT,
                 status TEXT NOT NULL,
                 content TEXT,
                 size INTEGER NOT NULL,
                 mtime_ns INTEGER,
                 source TEXT NOT NULL,
                 generation INTEGER NOT NULL,
                 skip_reason TEXT
             );",
        )
        .expect("legacy schema");
    connection
        .execute(
            "INSERT INTO files(path, blob_name, status, content, size, source, generation)
             VALUES ('a.py', 'blob-a', 'present', ?1, 1, ?2, 1)",
            rusqlite::params![
                explicit.then_some("unsaved"),
                if explicit { "explicit" } else { "filesystem" }
            ],
        )
        .expect("legacy row");
    for (key, value) in [
        ("generation", generation),
        ("synced_generation", synced_generation),
    ] {
        if let Some(value) = value {
            connection
                .execute("INSERT INTO meta(key, value) VALUES (?1, ?2)", [key, value])
                .expect("legacy metadata");
        }
    }
}
