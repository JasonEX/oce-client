//! Shared test doubles: an in-memory `BlobApi` and a scripted fake OCE HTTP server.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use oce_client::http::{
    ApiError, BlobApi, BlobStatusResult, BlobUpload, MissingResult, RetrievalResult,
};
use serde_json::Value;

#[derive(Debug, Default)]
pub struct FakeState {
    pub known: BTreeSet<String>,
    pub ready: BTreeSet<String>,
    pub checkpoint_members: BTreeSet<String>,
    pub checkpoint_id: Option<String>,
    pub checkpoint_calls: Vec<(Option<String>, Vec<String>, Vec<String>)>,
    pub find_missing_batch_sizes: Vec<usize>,
    pub fail_upload: bool,
    pub fail_checkpoint: bool,
    pub checkpoint_404_once: bool,
    pub mismatch_upload: bool,
    pub checkpoint_counter: usize,
}

/// In-memory OCE server double. Flags in `FakeState` inject failures; the atomics let
/// concurrent tests pause or observe individual requests without holding the state lock.
#[derive(Debug, Default)]
pub struct FakeApi {
    state: Mutex<FakeState>,
    pub block_find_missing: AtomicBool,
    pub fail_find_missing_once: AtomicBool,
    pub block_retrieve: AtomicBool,
    pub retrieve_started: AtomicBool,
    pub retrieve_calls: AtomicUsize,
}

impl FakeApi {
    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake API lock")
    }

    fn wait_while(flag: &AtomicBool) {
        while flag.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn missing_checkpoint() -> ApiError {
        ApiError::Http {
            status_code: 404,
            detail: "missing checkpoint".to_owned(),
        }
    }
}

impl BlobApi for FakeApi {
    fn find_missing(&self, names: &[String]) -> Result<MissingResult, ApiError> {
        Self::wait_while(&self.block_find_missing);
        if self.fail_find_missing_once.swap(false, Ordering::AcqRel) {
            return Err(ApiError::InvalidResponse(
                "temporary sync failure".to_owned(),
            ));
        }
        let mut state = self.state();
        state.find_missing_batch_sizes.push(names.len());
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
        let mut state = self.state();
        if state.fail_upload {
            return Err(ApiError::InvalidResponse("upload failed".to_owned()));
        }
        let names = blobs
            .iter()
            .map(|blob| blob.blob_name.clone())
            .collect::<Vec<_>>();
        state.known.extend(names.iter().cloned());
        state.ready.extend(names.iter().cloned());
        if state.mismatch_upload {
            Ok(vec!["0".repeat(64)])
        } else {
            Ok(names)
        }
    }

    fn blob_status(
        &self,
        names: &[String],
        checkpoint_id: Option<&str>,
    ) -> Result<BlobStatusResult, ApiError> {
        let state = self.state();
        Ok(BlobStatusResult {
            unknown_blob_names: names
                .iter()
                .filter(|name| !state.known.contains(*name))
                .cloned()
                .collect(),
            nonindexed_blob_names: names
                .iter()
                .filter(|name| !state.ready.contains(*name))
                .cloned()
                .collect(),
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
        let mut state = self.state();
        if state.fail_checkpoint {
            return Err(ApiError::InvalidResponse("checkpoint failed".to_owned()));
        }
        state.checkpoint_calls.push((
            checkpoint_id.map(str::to_owned),
            added.to_vec(),
            deleted.to_vec(),
        ));
        if state.checkpoint_404_once && checkpoint_id.is_some() {
            state.checkpoint_404_once = false;
            state.checkpoint_id = None;
            state.checkpoint_members.clear();
            return Err(Self::missing_checkpoint());
        }
        if checkpoint_id.is_some() && checkpoint_id != state.checkpoint_id.as_deref() {
            return Err(Self::missing_checkpoint());
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
        Self::wait_while(&self.block_retrieve);
        let state = self.state();
        if checkpoint_id.is_some() && checkpoint_id != state.checkpoint_id.as_deref() {
            return Err(Self::missing_checkpoint());
        }
        Ok(RetrievalResult {
            formatted_retrieval: format!("{query}:{}", checkpoint_id.unwrap_or("")),
            elapsed_ms: 1,
        })
    }
}

/// Builds the JSON reply for one expected request.
pub type Responder = Box<dyn Fn(&Value) -> Value + Send>;

pub fn respond(handler: impl Fn(&Value) -> Value + Send + 'static) -> Responder {
    Box::new(handler)
}

pub fn json_response(value: Value) -> Responder {
    Box::new(move |_| value.clone())
}

/// Serves exactly the listed requests in order, asserting each path, then exits.
pub fn fake_oce_server(expected: Vec<(&'static str, Responder)>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake OCE");
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        for (expected_path, responder) in expected {
            let (mut stream, _) = listener.accept().expect("accept OCE request");
            let (path, request) = read_request(&mut stream);
            assert_eq!(path, expected_path);
            let body = serde_json::to_vec(&responder(&request)).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (format!("http://{address}"), thread)
}

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "request ended before headers completed");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut lines = headers.split("\r\n");
    let path = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() - header_end < length {
        let count = stream.read(&mut buffer).unwrap();
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
    };
    (path, body)
}
