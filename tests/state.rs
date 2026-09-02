use oce_client::state::{
    FileRecord, FileSource, FileStatus, StateError, StateStore, default_state_path,
    ensure_schema_version,
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
    assert_eq!(path, root.path().join(".oce-client/state-v1.sqlite3"));
    let state = StateStore::open(&path).expect("open state");
    state
        .apply_file_changes(&[record("src/main.py", FileSource::Filesystem, 1)], &[], 1)
        .expect("store inventory");

    let pending = state.load_snapshot().expect("pending snapshot");
    assert_eq!(pending.generation, 1);
    assert_eq!(state.generation().unwrap(), 1);
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
fn schema_version_mismatch_is_rejected_before_opening() {
    let root = tempfile::tempdir().expect("workspace");
    let path = default_state_path(root.path());
    ensure_schema_version(&path).expect("missing database is acceptable");

    StateStore::open(&path).expect("create state");
    ensure_schema_version(&path).expect("current schema is accepted");

    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 7)
        .unwrap();
    assert!(matches!(
        ensure_schema_version(&path),
        Err(StateError::UnsupportedSchema {
            found: 7,
            expected: 1,
            ..
        })
    ));
}
