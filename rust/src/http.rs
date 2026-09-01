use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::VERSION;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobUpload {
    pub path: String,
    pub content: String,
    pub blob_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MissingResult {
    pub unknown_blob_names: Vec<String>,
    pub nonindexed_blob_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlobStatusResult {
    pub unknown_blob_names: Vec<String>,
    pub nonindexed_blob_names: Vec<String>,
    pub checkpoint_not_found: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetrievalResult {
    pub formatted_retrieval: String,
    pub elapsed_ms: u64,
}

pub trait BlobApi: Send + Sync {
    fn find_missing(&self, blob_names: &[String]) -> Result<MissingResult, ApiError>;
    fn batch_upload(&self, blobs: &[BlobUpload]) -> Result<Vec<String>, ApiError>;
    fn blob_status(
        &self,
        blob_names: &[String],
        checkpoint_id: Option<&str>,
    ) -> Result<BlobStatusResult, ApiError>;
    fn checkpoint(
        &self,
        checkpoint_id: Option<&str>,
        added_blobs: &[String],
        deleted_blobs: &[String],
    ) -> Result<String, ApiError>;
    fn retrieve(
        &self,
        query: &str,
        checkpoint_id: Option<&str>,
        added_blobs: &[String],
        deleted_blobs: &[String],
    ) -> Result<RetrievalResult, ApiError>;
}

#[derive(Debug, Clone)]
pub struct OceHttpClient {
    api_url: String,
    client: Client,
}

impl OceHttpClient {
    pub fn new(api_url: &str, api_key: &str) -> Result<Self, ApiError> {
        Self::with_timeout(api_url, api_key, Duration::from_secs(60))
    }

    pub fn with_timeout(api_url: &str, api_key: &str, timeout: Duration) -> Result<Self, ApiError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|source| ApiError::Configuration(source.to_string()))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("oce-client/{VERSION}"))
                .map_err(|source| ApiError::Configuration(source.to_string()))?,
        );
        let client = Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .build()
            .map_err(ApiError::Transport)?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_owned(),
            client,
        })
    }

    fn post<T: DeserializeOwned>(&self, endpoint: &str, body: &Value) -> Result<T, ApiError> {
        let response = self
            .client
            .post(format!("{}/{}", self.api_url, endpoint))
            .json(body)
            .send()
            .map_err(ApiError::Transport)?;
        let status = response.status();
        let text = response.text().map_err(ApiError::Transport)?;
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|body| body.get("detail").cloned())
                .map(|detail| match detail {
                    Value::String(value) => value,
                    value => value.to_string(),
                })
                .unwrap_or(text);
            return Err(ApiError::Http {
                status_code: status.as_u16(),
                detail,
            });
        }
        serde_json::from_str(&text).map_err(|source| ApiError::InvalidResponse(source.to_string()))
    }
}

impl BlobApi for OceHttpClient {
    fn find_missing(&self, blob_names: &[String]) -> Result<MissingResult, ApiError> {
        let response: FindMissingResponse =
            self.post("find-missing", &json!({"mem_object_names": blob_names}))?;
        Ok(MissingResult {
            unknown_blob_names: response.unknown_memory_names,
            nonindexed_blob_names: response.nonindexed_blob_names,
        })
    }

    fn batch_upload(&self, blobs: &[BlobUpload]) -> Result<Vec<String>, ApiError> {
        let response: BatchUploadResponse = self.post(
            "batch-upload",
            &json!({
                "blobs": blobs.iter().map(|blob| UploadBody {
                    path: &blob.path,
                    content: &blob.content,
                }).collect::<Vec<_>>()
            }),
        )?;
        Ok(response.blob_names)
    }

    fn blob_status(
        &self,
        blob_names: &[String],
        checkpoint_id: Option<&str>,
    ) -> Result<BlobStatusResult, ApiError> {
        let response: BlobStatusResponse = self.post(
            "agents/blob-status",
            &json!({"blobs": blobs_payload(checkpoint_id, blob_names, &[])}),
        )?;
        Ok(BlobStatusResult {
            unknown_blob_names: response.unknown_blob_names,
            nonindexed_blob_names: response.nonindexed_blob_names,
            checkpoint_not_found: response.checkpoint_not_found,
        })
    }

    fn checkpoint(
        &self,
        checkpoint_id: Option<&str>,
        added_blobs: &[String],
        deleted_blobs: &[String],
    ) -> Result<String, ApiError> {
        let response: CheckpointResponse = self.post(
            "checkpoint-blobs",
            &json!({"blobs": blobs_payload(checkpoint_id, added_blobs, deleted_blobs)}),
        )?;
        Ok(response.new_checkpoint_id)
    }

    fn retrieve(
        &self,
        query: &str,
        checkpoint_id: Option<&str>,
        added_blobs: &[String],
        deleted_blobs: &[String],
    ) -> Result<RetrievalResult, ApiError> {
        let started = Instant::now();
        let response: RetrievalResponse = self.post(
            "agents/codebase-retrieval",
            &json!({
                "information_request": query,
                "blobs": blobs_payload(checkpoint_id, added_blobs, deleted_blobs),
                "chat_history": [],
            }),
        )?;
        Ok(RetrievalResult {
            formatted_retrieval: response.formatted_retrieval,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }
}

fn blobs_payload(
    checkpoint_id: Option<&str>,
    added_blobs: &[String],
    deleted_blobs: &[String],
) -> Value {
    json!({
        "checkpoint_id": checkpoint_id.unwrap_or(""),
        "added_blobs": added_blobs,
        "deleted_blobs": deleted_blobs,
    })
}

#[derive(Debug, Serialize)]
struct UploadBody<'a> {
    path: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct FindMissingResponse {
    #[serde(default)]
    unknown_memory_names: Vec<String>,
    #[serde(default)]
    nonindexed_blob_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BatchUploadResponse {
    #[serde(default)]
    blob_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BlobStatusResponse {
    #[serde(default)]
    unknown_blob_names: Vec<String>,
    #[serde(default)]
    nonindexed_blob_names: Vec<String>,
    #[serde(default)]
    checkpoint_not_found: bool,
}

#[derive(Debug, Deserialize)]
struct CheckpointResponse {
    new_checkpoint_id: String,
}

#[derive(Debug, Deserialize)]
struct RetrievalResponse {
    formatted_retrieval: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("OCE API request failed ({status_code}): {detail}")]
    Http { status_code: u16, detail: String },
    #[error("OCE API transport failed: {0}")]
    Transport(reqwest::Error),
    #[error("OCE API response is invalid: {0}")]
    InvalidResponse(String),
    #[error("OCE API client configuration is invalid: {0}")]
    Configuration(String),
}

impl ApiError {
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Http { status_code, .. } => Some(*status_code),
            _ => None,
        }
    }
}
