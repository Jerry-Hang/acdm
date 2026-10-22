use crate::models::FileRecord;
use crate::storage::MemStore;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactReport {
    pub project: String,
    pub records_before: usize,
    pub records_after: usize,
}

/// Compact a project's files.jsonl by keeping only the latest record per file_name.
/// Writes to a temporary file and atomically renames it over the original.
pub fn compact_project_files(
    store: &MemStore,
    project: &str,
) -> Result<CompactReport, Box<dyn Error>> {
    let proj_dir = store.project_dir(project);
    let files_path = proj_dir.join("files.jsonl");

    if !files_path.exists() {
        return Ok(CompactReport {
            project: project.to_string(),
            records_before: 0,
            records_after: 0,
        });
    }

    // 1. Read all records
    let records = store.read_project_files(project)?;
    let records_before = records.len();

    // 2. Keep only latest record per file_name (last occurrence wins)
    let mut map: BTreeMap<String, FileRecord> = BTreeMap::new();
    for rec in records {
        map.insert(rec.file_name.clone(), rec);
    }

    let compacted: Vec<FileRecord> = map.into_values().collect();
    let records_after = compacted.len();

    // 3. Write to a temporary file in the same directory for atomic rename.
    //    The name is intentionally `files.jsonl.tmp` (no pid suffix) so the
    //    store's own `.gitignore` `*.tmp` pattern covers it if a run is killed
    //    before the rename.
    let temp_path = proj_dir.join("files.jsonl.tmp");
    {
        let mut temp_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)?;

        temp_file.lock_exclusive()?;
        for rec in &compacted {
            let json_line = serde_json::to_string(rec)?;
            writeln!(temp_file, "{}", json_line)?;
        }
        temp_file.flush()?;
        temp_file.unlock()?;
    }

    // 4. Atomically rename temp file over original, cleaning up on failure.
    if let Err(error) = fs::rename(&temp_path, &files_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error.into());
    }

    Ok(CompactReport {
        project: project.to_string(),
        records_before,
        records_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Event;
    use std::path::PathBuf;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "acdm_compact_test_{}_{}",
                std::process::id(),
                crate::generate_id()
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_compact_project_files() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();

        // Add 3 versions of file A, 2 versions of file B
        let event1 = Event {
            event_id: "eid_c1".to_string(),
            ts: "2026-10-22T01:00:00Z".to_string(),
            device: "d1".to_string(),
            agent: "a1".to_string(),
            project: "compact_test".to_string(),
            summary: "v1".to_string(),
            files: vec![
                FileRecord {
                    file_id: "fid_a1".to_string(),
                    file_name: "a.txt".to_string(),
                    action: "Created".to_string(),
                    size_bytes: 10,
                    hash: "h_a1".to_string(),
                },
                FileRecord {
                    file_id: "fid_b1".to_string(),
                    file_name: "b.txt".to_string(),
                    action: "Created".to_string(),
                    size_bytes: 20,
                    hash: "h_b1".to_string(),
                },
            ],
        };

        let event2 = Event {
            event_id: "eid_c2".to_string(),
            ts: "2026-10-22T02:00:00Z".to_string(),
            device: "d1".to_string(),
            agent: "a1".to_string(),
            project: "compact_test".to_string(),
            summary: "v2".to_string(),
            files: vec![
                FileRecord {
                    file_id: "fid_a2".to_string(),
                    file_name: "a.txt".to_string(),
                    action: "Modified".to_string(),
                    size_bytes: 15,
                    hash: "h_a2".to_string(),
                },
                FileRecord {
                    file_id: "fid_b2".to_string(),
                    file_name: "b.txt".to_string(),
                    action: "Deleted".to_string(),
                    size_bytes: 0,
                    hash: "h_b2".to_string(),
                },
            ],
        };

        let event3 = Event {
            event_id: "eid_c3".to_string(),
            ts: "2026-10-22T03:00:00Z".to_string(),
            device: "d1".to_string(),
            agent: "a1".to_string(),
            project: "compact_test".to_string(),
            summary: "v3".to_string(),
            files: vec![FileRecord {
                file_id: "fid_a3".to_string(),
                file_name: "a.txt".to_string(),
                action: "Modified".to_string(),
                size_bytes: 30,
                hash: "h_a3".to_string(),
            }],
        };

        store.append_event(&event1).unwrap();
        store.append_event(&event2).unwrap();
        store.append_event(&event3).unwrap();

        let before_files = store.read_project_files("compact_test").unwrap();
        assert_eq!(before_files.len(), 5);

        let report = compact_project_files(&store, "compact_test").unwrap();
        assert_eq!(report.records_before, 5);
        assert_eq!(report.records_after, 2);

        let after_files = store.read_project_files("compact_test").unwrap();
        assert_eq!(after_files.len(), 2);

        // a.txt should have hash h_a3 and size 30
        let a_rec = after_files.iter().find(|r| r.file_name == "a.txt").unwrap();
        assert_eq!(a_rec.file_id, "fid_a3");
        assert_eq!(a_rec.size_bytes, 30);

        // b.txt should have action "Deleted"
        let b_rec = after_files.iter().find(|r| r.file_name == "b.txt").unwrap();
        assert_eq!(b_rec.file_id, "fid_b2");
        assert_eq!(b_rec.action, "Deleted");
    }
}
