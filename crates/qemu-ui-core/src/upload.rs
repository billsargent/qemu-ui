//! Resumable upload API types (plan.md §9.4, M2-07).
//!
//! The protocol is tus-like: create a session, then send chunks with
//! `PATCH` and an `Upload-Offset` header that must equal the bytes already
//! stored; `HEAD` returns the current offset so a client can resume after a
//! network error, a reload, or a daemon restart.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Largest chunk the daemon accepts in one `PATCH`.
pub const UPLOAD_CHUNK_SIZE: u64 = 16 * 1024 * 1024;

/// Request header carrying the offset a chunk starts at.
pub const OFFSET_HEADER: &str = "upload-offset";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct UploadSession {
    pub id: String,
    pub pool: String,
    pub name: String,
    #[ts(type = "number")]
    pub size: u64,
    /// Bytes stored so far; the next chunk must start here.
    #[ts(type = "number")]
    pub offset: u64,
    /// Opaque client identity of the source file (e.g. name + size +
    /// lastModified) so a reloaded page can find its session again.
    pub client_key: Option<String>,
    /// The job that shows this upload on the Jobs page.
    pub job_id: Option<String>,
    /// Largest accepted chunk.
    #[ts(type = "number")]
    pub chunk_size: u64,
    #[ts(type = "number")]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct UploadProgress {
    #[ts(type = "number")]
    pub offset: u64,
    #[ts(type = "number")]
    pub size: u64,
    /// The file is complete and now an image in its pool.
    pub complete: bool,
    /// Image format detected by `qemu-img info` once complete.
    pub format: Option<String>,
}

/// Is `s` a lowercase or uppercase hex SHA-256?
pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_format() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(!valid_sha256(&"a".repeat(63)));
        assert!(!valid_sha256(&"g".repeat(64)));
    }
}
