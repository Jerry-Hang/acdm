use crate::models::Event;
use crate::storage::MemStore;
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPointer {
    pub event_id: String,
    pub file_path: PathBuf, // Which JSONL file
    pub byte_offset: u64,   // Byte offset in file
    pub ts: String,
    pub project: String,
}

#[derive(Debug, Default)]
pub struct MemIndex {
    pub by_project: HashMap<String, Vec<EventPointer>>,
    pub by_file_id: BTreeMap<String, EventPointer>,
    pub by_filename: HashMap<String, Vec<EventPointer>>,
}

impl MemIndex {
    /// Build in-memory index by scanning all events/*.jsonl files.
    pub fn build(store: &MemStore) -> Result<Self, Box<dyn Error>> {
        let mut index = MemIndex::default();
        let events_dir = store.events_dir();
        if !events_dir.exists() {
            return Ok(index);
        }

        let mut files = Vec::new();
        for entry in fs::read_dir(&events_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
        files.sort();

        for file_path in files {
            let file = File::open(&file_path)?;
            if file.metadata()?.len() == 0 {
                continue;
            }
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            let mut offset = 0u64;

            for line in mmap.split(|&b| b == b'\n') {
                let line_len = line.len() as u64;
                if !line.is_empty() {
                    if let Ok(event) = serde_json::from_slice::<Event>(line) {
                        let pointer = EventPointer {
                            event_id: event.event_id.clone(),
                            file_path: file_path.clone(),
                            byte_offset: offset,
                            ts: event.ts.clone(),
                            project: event.project.clone(),
                        };

                        index
                            .by_project
                            .entry(event.project.clone())
                            .or_default()
                            .push(pointer.clone());

                        for file_record in &event.files {
                            index
                                .by_file_id
                                .insert(file_record.file_id.clone(), pointer.clone());

                            index
                                .by_filename
                                .entry(file_record.file_name.clone())
                                .or_default()
                                .push(pointer.clone());
                        }
                    }
                }
                offset += line_len + 1;
            }
        }

        Ok(index)
    }

    /// Query event pointers for a project.
    pub fn query_by_project(&self, project: &str) -> Option<&Vec<EventPointer>> {
        self.by_project.get(project)
    }

    /// Query event pointer for a specific file ID.
    pub fn query_by_file_id(&self, file_id: &str) -> Option<&EventPointer> {
        self.by_file_id.get(file_id)
    }

    /// Query event pointers by file name.
    pub fn query_by_filename(&self, filename: &str) -> Option<&Vec<EventPointer>> {
        self.by_filename.get(filename)
    }

    /// Read an Event from an EventPointer.
    pub fn read_event(&self, pointer: &EventPointer) -> Result<Event, Box<dyn Error>> {
        let mut file = File::open(&pointer.file_path)?;
        file.seek(SeekFrom::Start(pointer.byte_offset))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let event: Event = serde_json::from_str(line.trim())?;
        Ok(event)
    }

    /// Fuzzy search through JSONL files using regular expressions and memmap2.
    pub fn fuzzy_search(store: &MemStore, pattern: &str) -> Result<Vec<Event>, Box<dyn Error>> {
        let regex = Regex::new(pattern)?;
        let events_dir = store.events_dir();
        if !events_dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        for entry in fs::read_dir(&events_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
        files.sort();

        let mut matched_events = Vec::new();
        for file_path in files {
            let file = File::open(&file_path)?;
            if file.metadata()?.len() == 0 {
                continue;
            }
            let mmap = unsafe { memmap2::Mmap::map(&file)? };

            for line in mmap.split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                if let Ok(line_str) = std::str::from_utf8(line) {
                    if regex.is_match(line_str) {
                        if let Ok(event) = serde_json::from_str::<Event>(line_str) {
                            matched_events.push(event);
                        }
                    }
                }
            }
        }

        Ok(matched_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FileRecord;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!("acdm_index_test_{}_{}", std::process::id(), crate::generate_id());
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
    fn test_index_build_and_query() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();

        let event = Event {
            event_id: "eid_100".to_string(),
            ts: "2026-10-22T08:00:00Z".to_string(),
            device: "laptop".to_string(),
            agent: "agent-a".to_string(),
            project: "proj-alpha".to_string(),
            summary: "refactor storage system".to_string(),
            files: vec![FileRecord {
                file_id: "fid_100".to_string(),
                file_name: "src/storage.rs".to_string(),
                action: "Modified".to_string(),
                size_bytes: 4096,
                hash: "dummyhash".to_string(),
            }],
        };

        store.append_event(&event).unwrap();

        let index = MemIndex::build(&store).unwrap();

        // Query by project
        let by_proj = index.query_by_project("proj-alpha").unwrap();
        assert_eq!(by_proj.len(), 1);
        assert_eq!(by_proj[0].event_id, "eid_100");

        // Query by file ID
        let by_fid = index.query_by_file_id("fid_100").unwrap();
        assert_eq!(by_fid.event_id, "eid_100");

        // Query by filename
        let by_fn = index.query_by_filename("src/storage.rs").unwrap();
        assert_eq!(by_fn.len(), 1);
        assert_eq!(by_fn[0].event_id, "eid_100");

        // Read event using pointer
        let read_ev = index.read_event(&by_proj[0]).unwrap();
        assert_eq!(read_ev.event_id, "eid_100");
        assert_eq!(read_ev.summary, "refactor storage system");

        // Fuzzy search
        let found = MemIndex::fuzzy_search(&store, "refactor.*storage").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].event_id, "eid_100");

        let not_found = MemIndex::fuzzy_search(&store, "nonexistent").unwrap();
        assert!(not_found.is_empty());
    }
}
