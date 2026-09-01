use sha2::{Digest, Sha256};

pub fn calculate_blob_identity(path: &str, content: &str) -> Result<String, IdentityError> {
    if path.is_empty() {
        return Err(IdentityError::EmptyPath);
    }
    let mut digest = Sha256::new();
    digest.update(path.as_bytes());
    digest.update(content.as_bytes());
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("path must be a non-empty string")]
    EmptyPath,
}
