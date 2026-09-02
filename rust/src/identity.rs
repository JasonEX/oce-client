use sha2::{Digest, Sha256};

pub fn calculate_blob_identity(path: &str, content: &str) -> Result<String, IdentityError> {
    if path.is_empty() {
        return Err(IdentityError::EmptyPath);
    }
    Ok(sha256_hex(&[path.as_bytes(), content.as_bytes()]))
}

/// Hex-encodes the SHA-256 digest of the concatenated parts.
pub fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("path must be a non-empty string")]
    EmptyPath,
}
