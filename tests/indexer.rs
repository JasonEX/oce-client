use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oce_client::context::WorkspaceContext;
use oce_client::http::{
    ApiError, BlobApi, BlobStatusResult, BlobUpload, MissingResult, RetrievalResult,
};
use oce_client::indexer::{Readiness, WorkspaceIndexer};

#[derive(Debug, Default)]
struct FakeState {
    known: BTreeSet<String>,
    checkpoint_id: Option<String>,
    checkpoint_members: BTreeSet<String>,
    checkpoint_counter: usize,
}

#[derive(Debug, Default)]
struct IndexerApi {
    state: Mutex<FakeState>,
    block_find_missing: AtomicBool,
    fail_find_missing_once: AtomicBool,
    block_retrieve: AtomicBool,
    retrieve_started: AtomicBool,
    retrieve_calls: AtomicUsize,
}

impl BlobApi for IndexerApi {
    fn find_missing(&self, names: &[String]) -> Result<MissingResult, ApiError> {
        while self.block_find_missing.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        if self.fail_find_missing_once.swap(false, Ordering::AcqRel) {
            return Err(ApiError::InvalidResponse(
                "temporary sync failure".to_owned(),
            ));
        }
        let state = self.state.lock().unwrap();
        Ok(MissingResult {
            unknown_blob_names: names
                .iter()
                .filter(|name| !state.known.contains(*name))
                .cloned()
                .collect(),
            nonindexed_blob_names: Vec::new(),
        })
    }

    fn batch_upload(&self, blobs: &[BlobUpload]) -> Result<Vec<String>, ApiError> {
        let names = blobs
            .iter()
            .map(|blob| blob.blob_name.clone())
            .collect::<Vec<_>>();
        self.state.lock().unwrap().known.extend(names.clone());
        Ok(names)
    }

    fn blob_status(
        &self,
        names: &[String],
        checkpoint_id: Option<&str>,
    ) -> Result<BlobStatusResult, ApiError> {
        let state = self.state.lock().unwrap();
        Ok(BlobStatusResult {
            unknown_blob_names: names
                .iter()
                .filter(|name| !state.known.contains(*name))
                .cloned()
                .collect(),
            nonindexed_blob_names: Vec::new(),
            checkpoint_not_found: checkpoint_id.is_some()
                && checkpoint_id != state.checkpoint_id.as_deref(),
        })
    }

    fn checkpoint(
        &self,
        checkpoint_id: Option<&str>,
        added: &[String],
        deleted: &[String],
    ) -> Result<String, ApiError> {
        let mut state = self.state.lock().unwrap();
        if checkpoint_id.is_some() && checkpoint_id != state.checkpoint_id.as_deref() {
            return Err(ApiError::Http {
                status_code: 404,
                detail: "missing checkpoint".to_owned(),
            });
        }
        state.checkpoint_members.extend(added.iter().cloned());
        for name in deleted {
            state.checkpoint_members.remove(name);
        }
        state.checkpoint_counter += 1;
        let next = format!("chain:{}", state.checkpoint_counter);
        state.checkpoint_id = Some(next.clone());
        Ok(next)
    }

    fn retrieve(
        &self,
        query: &str,
        checkpoint_id: Option<&str>,
        _added: &[String],
        _deleted: &[String],
    ) -> Result<RetrievalResult, ApiError> {
        self.retrieve_calls.fetch_add(1, Ordering::AcqRel);
        self.retrieve_started.store(true, Ordering::Release);
        while self.block_retrieve.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(RetrievalResult {
            formatted_retrieval: format!("{query}:{}", checkpoint_id.unwrap_or("")),
            elapsed_ms: 7,
        })
    }
}

#[test]
fn retrieval_discards_a_result_if_the_workspace_changes_during_the_request() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("a.py");
    fs::write(&path, "one").unwrap();
    let api = Arc::new(IndexerApi::default());
    let indexer = indexer(root.path(), api.clone());
    indexer.start(true).unwrap();
    assert_eq!(
        indexer
            .wait_until_ready(Some(Duration::from_secs(1)))
            .unwrap(),
        Readiness::Ready
    );

    api.block_retrieve.store(true, Ordering::Release);
    let retrieval_indexer = indexer.clone();
    let retrieval = thread::spawn(move || {
        retrieval_indexer
            .retrieve("find a", Duration::from_secs(2))
            .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !api.retrieve_started.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "retrieval did not start");
        thread::sleep(Duration::from_millis(1));
    }
    fs::write(&path, "two changed").unwrap();

    #[cfg(unix)]
    let (notification_path, _alias_directory) = {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("workspace alias parent");
        let alias = directory.path().join("workspace");
        symlink(root.path(), &alias).expect("workspace alias");
        (alias.join("a.py"), directory)
    };
    #[cfg(not(unix))]
    let notification_path = path;

    indexer.notify_changes(BTreeSet::from([notification_path]));
    api.block_retrieve.store(false, Ordering::Release);

    let result = retrieval.join().unwrap();
    assert_eq!(result["status"], "ready");
    assert_ne!(result["formatted_retrieval"], "find a:chain:1");
    assert!(api.retrieve_calls.load(Ordering::Acquire) >= 2);
    indexer.stop().unwrap();
}

fn indexer(root: &Path, api: Arc<IndexerApi>) -> WorkspaceIndexer {
    let context = WorkspaceContext::open(root, api, None, Vec::new()).unwrap();
    WorkspaceIndexer::new(context, Duration::from_millis(10))
}

#[test]
fn retrieval_starts_indexing_and_returns_only_after_latest_generation() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.py"), "one").unwrap();
    let api = Arc::new(IndexerApi::default());
    let indexer = indexer(root.path(), api);
    indexer.start(false).unwrap();

    let first = indexer.retrieve("find a", Duration::from_secs(1)).unwrap();
    assert_eq!(first["status"], "ready");
    assert_eq!(first["formatted_retrieval"], "find a:chain:1");

    fs::write(root.path().join("a.py"), "two changed").unwrap();
    indexer.notify_changes(BTreeSet::from([root.path().join("a.py")]));
    assert_eq!(
        indexer
            .wait_until_ready(Some(Duration::from_secs(1)))
            .unwrap(),
        Readiness::Ready
    );
    let status = indexer.status().unwrap();
    assert!(status.synced_generation >= status.requested_generation);
    assert_eq!(status.status, Readiness::Ready);
    indexer.stop().unwrap();
}

#[test]
fn retrieval_reports_indexing_without_stale_context_on_timeout() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.py"), "one").unwrap();
    let api = Arc::new(IndexerApi::default());
    api.block_find_missing.store(true, Ordering::Release);
    let indexer = indexer(root.path(), api.clone());
    indexer.start(true).unwrap();

    let result = indexer
        .retrieve("find a", Duration::from_millis(10))
        .unwrap();
    assert_eq!(result["status"], "indexing");
    assert!(result.get("formatted_retrieval").is_none());

    api.block_find_missing.store(false, Ordering::Release);
    assert_eq!(
        indexer
            .wait_until_ready(Some(Duration::from_secs(1)))
            .unwrap(),
        Readiness::Ready
    );
    indexer.stop().unwrap();
}

#[test]
fn failed_sync_is_retried_by_next_retrieval() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.py"), "one").unwrap();
    let api = Arc::new(IndexerApi::default());
    api.fail_find_missing_once.store(true, Ordering::Release);
    let indexer = indexer(root.path(), api);
    indexer.start(true).unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    while indexer.status().unwrap().status != Readiness::Error {
        assert!(
            Instant::now() < deadline,
            "indexer did not expose sync failure"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let result = indexer.retrieve("find a", Duration::from_secs(1)).unwrap();
    assert_eq!(result["status"], "ready");
    assert!(result.get("formatted_retrieval").is_some());
    indexer.stop().unwrap();
}
