use std::fs;
use std::path::Path;
use std::sync::Arc;

use oce_client::context::{ContextError, WorkspaceContext};
use oce_client::state::{FileSource, FileStatus};

mod common;
use common::FakeApi;

fn context(root: &Path, api: Arc<FakeApi>) -> WorkspaceContext {
    WorkspaceContext::open(root, api, None, Vec::new()).expect("workspace context")
}

#[test]
fn rust_sync_tracks_add_modify_and_delete() {
    let root = tempfile::tempdir().expect("workspace");
    fs::create_dir(root.path().join("src")).unwrap();
    let path = root.path().join("src/main.py");
    fs::write(&path, "one").unwrap();
    let api = Arc::new(FakeApi::default());
    let context = context(root.path(), api.clone());

    let first = context.sync().expect("first sync");
    let old_name = first.added_blobs[0].clone();
    assert_eq!(first.checkpoint_id.as_deref(), Some("chain:1"));

    fs::write(&path, "two changed").unwrap();
    let second = context.sync().expect("modified sync");
    assert_eq!(second.deleted_blobs, vec![old_name]);
    assert_eq!(second.added_blobs.len(), 1);

    fs::remove_file(&path).unwrap();
    let third = context.sync().expect("deleted sync");
    assert_eq!(third.deleted_blobs, second.added_blobs);
    assert!(api.state().checkpoint_members.is_empty());
}

#[test]
fn rust_failed_upload_does_not_advance_checkpoint() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("a.py"), "x").unwrap();
    let api = Arc::new(FakeApi::default());
    api.state().fail_upload = true;
    let context = context(root.path(), api.clone());

    assert!(context.sync().is_err());
    let failed = context.snapshot().unwrap();
    assert_eq!(failed.checkpoint_id, None);
    assert_eq!(failed.synced_generation, None);

    api.state().fail_upload = false;
    let retried = context.sync().expect("retry sync");
    assert!(retried.checkpoint_id.is_some());
}

#[test]
fn rust_rebuilds_a_raced_checkpoint_in_bounded_requests() {
    let root = tempfile::tempdir().expect("workspace");
    for index in 0..1001 {
        fs::write(
            root.path().join(format!("file-{index}.py")),
            index.to_string(),
        )
        .unwrap();
    }
    let api = Arc::new(FakeApi::default());
    let context = context(root.path(), api.clone());
    context.sync().expect("initial sync");
    fs::write(root.path().join("file-0.py"), "changed").unwrap();
    {
        let mut state = api.state();
        state.find_missing_batch_sizes.clear();
        state.checkpoint_404_once = true;
    }

    let rebuilt = context.sync().expect("checkpoint rebuild");
    assert_eq!(rebuilt.added_blobs.len(), 1001);
    let state = api.state();
    assert_eq!(state.checkpoint_members.len(), 1001);
    assert!(
        state
            .find_missing_batch_sizes
            .iter()
            .all(|size| *size <= 1000)
    );
    assert!(state.find_missing_batch_sizes.contains(&1000));
}

#[test]
fn rust_retrieve_recovers_checkpoint_lost_after_sync() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("a.py"), "x").unwrap();
    let api = Arc::new(FakeApi::default());
    let context = context(root.path(), api.clone());
    context.sync().expect("initial sync");
    {
        let mut state = api.state();
        state.checkpoint_id = None;
        state.checkpoint_members.clear();
    }

    let result = context.retrieve("find a").expect("retrieve");
    assert!(result.formatted_retrieval.starts_with("find a:chain:"));
    assert_eq!(
        context.snapshot().unwrap().checkpoint_id,
        api.state().checkpoint_id
    );
}

#[test]
fn rust_rejects_server_hash_mismatch() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("a.py"), "x").unwrap();
    let api = Arc::new(FakeApi::default());
    api.state().mismatch_upload = true;
    let context = context(root.path(), api);

    assert!(matches!(
        context.sync(),
        Err(ContextError::UploadMismatch { .. })
    ));
}

#[test]
fn rust_explicit_overlay_survives_disk_reconcile() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("a.py"), "disk").unwrap();
    let api = Arc::new(FakeApi::default());
    let context = context(root.path(), api);

    context
        .observe_file(Path::new("a.py"), "unsaved")
        .expect("observe overlay");
    context.reconcile().expect("reconcile disk");
    let snapshot = context.snapshot().unwrap();
    assert_eq!(snapshot.files["a.py"].source, FileSource::Explicit);
    assert_eq!(snapshot.files["a.py"].content.as_deref(), Some("unsaved"));

    context
        .remove_file(Path::new("a.py"))
        .expect("remove overlay");
    assert_eq!(
        context.snapshot().unwrap().files["a.py"].status,
        FileStatus::Deleted
    );
}

#[test]
fn rust_explicit_overlay_survives_unreadable_disk_file() {
    let root = tempfile::tempdir().expect("workspace");
    let path = root.path().join("a.py");
    fs::write(&path, "disk").unwrap();
    let api = Arc::new(FakeApi::default());
    let context = context(root.path(), api);
    context
        .observe_file(Path::new("a.py"), "unsaved")
        .expect("observe overlay");

    fs::write(&path, b"bin\0ary").unwrap();
    context.reconcile().expect("full reconcile");
    assert_eq!(
        context.snapshot().unwrap().files["a.py"].source,
        FileSource::Explicit
    );
    context
        .reconcile_paths([path])
        .expect("incremental reconcile");
    let record = &context.snapshot().unwrap().files["a.py"];
    assert_eq!(record.source, FileSource::Explicit);
    assert_eq!(record.content.as_deref(), Some("unsaved"));
}

#[test]
fn rust_explicit_paths_are_normalized_without_leaving_the_workspace() {
    let root = tempfile::tempdir().expect("workspace");
    let api = Arc::new(FakeApi::default());
    let context = context(root.path(), api);

    context
        .observe_file(Path::new("src/../a.py"), "unsaved")
        .expect("normalized overlay");
    assert!(context.snapshot().unwrap().files.contains_key("a.py"));

    for path in ["../outside.py", "src/../../outside.py"] {
        assert!(matches!(
            context.observe_file(Path::new(path), "outside"),
            Err(ContextError::InvalidRelativePath(_))
        ));
        assert!(matches!(
            context.remove_file(Path::new(path)),
            Err(ContextError::InvalidRelativePath(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn rust_incremental_sync_drops_outside_symlink_without_reading_it() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("workspace");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    fs::write(root.path().join("linked.py"), "public").unwrap();
    fs::write(outside.path(), "private").unwrap();
    let api = Arc::new(FakeApi::default());
    let context = context(root.path(), api.clone());
    let first = context.sync().expect("initial sync");
    let alias_directory = tempfile::tempdir().expect("workspace alias parent");
    let workspace_alias = alias_directory.path().join("workspace");
    symlink(root.path(), &workspace_alias).expect("workspace alias");
    fs::remove_file(root.path().join("linked.py")).unwrap();
    symlink(outside.path(), root.path().join("linked.py")).unwrap();

    let second = context
        .sync_paths([workspace_alias.join("linked.py")])
        .expect("incremental sync");
    assert!(second.added_blobs.is_empty());
    assert_eq!(second.deleted_blobs, first.added_blobs);
    assert!(api.state().checkpoint_members.is_empty());
}
