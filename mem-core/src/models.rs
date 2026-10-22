use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

#[cfg(test)]
mod tests {
    use super::*;

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
