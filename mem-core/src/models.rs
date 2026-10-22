use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub file_id: String,
    pub file_name: String,
    pub action: String, // Created / Modified / Deleted
    pub size_bytes: u64,
    pub hash: String, // SHA256 hex
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub event_id: String,
    pub ts: String, // RFC3339
    pub device: String,
    pub agent: String,
    pub project: String,
    pub summary: String,
    pub files: Vec<FileRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
    pub description: String,
    pub github_url: Option<String>,
    pub created_at: String,
}

/// Compute SHA-256 hash of byte slice, returned as hex string.
pub fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Hash an arbitrary reader incrementally, returning `(size_bytes, sha256_hex)`.
///
/// Uses a fixed 64 KiB buffer, so hashing a large file never loads it into
/// memory. This is the API the CLI uses for `mem add` instead of reading whole
/// files with `fs::read`.
pub fn hash_reader<R: Read>(mut reader: R) -> io::Result<(u64, String)> {
    const CHUNK: usize = 64 * 1024;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((size, hex::encode(hasher.finalize())))
}

/// Hash a file incrementally, returning `(size_bytes, sha256_hex)`.
pub fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    hash_reader(File::open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_reader_and_file_match_slice_hash() {
        let data = b"streaming hash must match the one-shot hash";
        let (size, digest) = hash_reader(&data[..]).unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(digest, compute_sha256(data));

        let tmp = std::env::temp_dir().join(format!(
            "acdm_hash_test_{}_{}",
            std::process::id(),
            crate::generate_id()
        ));
        std::fs::write(&tmp, data).unwrap();
        let (file_size, file_digest) = hash_file(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(file_size, data.len() as u64);
        assert_eq!(file_digest, compute_sha256(data));
    }

    #[test]
    fn test_models_serialization() {
        let file_rec = FileRecord {
            file_id: "fid_1".to_string(),
            file_name: "test.rs".to_string(),
            action: "Created".to_string(),
            size_bytes: 128,
            hash: compute_sha256(b"hello world"),
        };

        let event = Event {
            event_id: "eid_1".to_string(),
            ts: "2026-10-22T04:14:37Z".to_string(),
            device: "device-1".to_string(),
            agent: "agent-1".to_string(),
            project: "ACDM".to_string(),
            summary: "Initial commit".to_string(),
            files: vec![file_rec.clone()],
        };

        let serialized = serde_json::to_string(&event).expect("serialize event");
        let deserialized: Event = serde_json::from_str(&serialized).expect("deserialize event");
        assert_eq!(event, deserialized);

        let meta = ProjectMeta {
            name: "ACDM".to_string(),
            description: "Agent Cross-Device Memory System".to_string(),
            github_url: Some("https://github.com/example/acdm".to_string()),
            created_at: "2026-10-22T00:00:00Z".to_string(),
        };

        let meta_json = serde_json::to_string(&meta).expect("serialize meta");
        let meta_deser: ProjectMeta = serde_json::from_str(&meta_json).expect("deserialize meta");
        assert_eq!(meta, meta_deser);
    }
}
