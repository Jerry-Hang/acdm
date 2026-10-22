use crate::models::{Event, FileRecord};
use crate::storage::{validate_project_name, MemStore};
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactReport {
    pub project: String,
    pub records_before: usize,
    pub records_after: usize,
}

/// Rebuild a project's `files.jsonl` projection from its event log.
///
/// The event log is the source of truth; `files.jsonl` is a derived,
/// last-write-wins view of it. A crash between the two appends of
/// [`MemStore::append_event`] leaves the projection missing or stale (`Created`
/// records where the newest state is `Modified`/`Deleted`), so compaction
/// replays every `FileRecord` in causal order and rewrites the projection
/// atomically — compacting duplicates and healing the projection in one pass.
///
/// The store-wide write lock is held for the whole read → replay → replace
/// sequence. The previous implementation read the projection, wrote a temp file
/// and renamed it over the original without a lock, so records appended
/// concurrently between the read and the rename were silently dropped from the
/// projection (the events survived, but `files.jsonl` no longer reflected them).
pub fn compact_project_files(
    store: &MemStore,
    project: &str,
) -> Result<CompactReport, Box<dyn Error>> {
    validate_project_name(project)?;

    // Serialise with every other store writer (`append_event`, `resolve`,
    // `sync`, `write_project_meta`) and hold the lock until the rename below.
    let _write_lock = store.lock_writes()?;

    let proj_dir = store.project_dir(project);
    let files_path = proj_dir.join("files.jsonl");

    // `records_before` is the line count of the *existing* projection, i.e. how
    // many records the report is compacting (duplicates included).
    let (records_before, has_projection) = match fs::read(&files_path) {
        Ok(data) => (count_nonempty_lines(&data), true),
        Err(error) if error.kind() == ErrorKind::NotFound => (0, false),
        Err(error) => return Err(error.into()),
    };

    // Replay the authoritative event log in causal order. `read_all_events`
    // walks every month shard; `sort_events` is parse-aware, so legacy
    // timestamp formats still order chronologically.
    let mut events: Vec<Event> = store
        .read_all_events()?
        .into_iter()
        .filter(|event| event.project == project)
        .collect();
    util::sort_events(&mut events);

    let mut latest: BTreeMap<String, FileRecord> = BTreeMap::new();
    for event in &events {
        for record in &event.files {
            // Last write wins in event order: a later event's record for the
            // same file_name replaces the earlier one.
            latest.insert(record.file_name.clone(), record.clone());
        }
    }
    let compacted: Vec<FileRecord> = latest.into_values().collect();
    let records_after = compacted.len();

    // Neither a projection nor any event exists: do not create
    // `projects/<name>/` as a side effect of compacting an unknown project.
    if !has_projection && compacted.is_empty() {
        return Ok(CompactReport {
            project: project.to_string(),
            records_before: 0,
            records_after: 0,
        });
    }

    fs::create_dir_all(&proj_dir)?;
    let temp_path = proj_dir.join("files.jsonl.tmp");
    if let Err(error) = write_projection(&temp_path, &compacted) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    // Atomic replace: readers see either the old projection or the complete new
    // one, never a partially written file. The `*.tmp` suffix is git-ignored.
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

/// Count the non-empty lines of a JSONL blob (one record per line).
fn count_nonempty_lines(data: &[u8]) -> usize {
    data.split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .count()
}

/// Write `records` to `temp_path` and flush them to stable storage, so the
/// following rename cannot publish a torn projection after a crash.
fn write_projection(temp_path: &Path, records: &[FileRecord]) -> Result<(), Box<dyn Error>> {
    let mut temp_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(temp_path)?;
    for record in records {
        serde_json::to_writer(&mut temp_file, record)?;
        temp_file.write_all(b"\n")?;
    }
    temp_file.flush()?;
    temp_file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn test_record(
        file_id: &str,
        file_name: &str,
        action: &str,
        size_bytes: u64,
        hash: &str,
    ) -> FileRecord {
        FileRecord {
            file_id: file_id.to_string(),
            file_name: file_name.to_string(),
            action: action.to_string(),
            size_bytes,
            hash: hash.to_string(),
        }
    }

    fn test_event(event_id: &str, ts: &str, project: &str, files: Vec<FileRecord>) -> Event {
        Event {
            event_id: event_id.to_string(),
            ts: ts.to_string(),
            device: "device-1".to_string(),
            agent: "agent-1".to_string(),
            project: project.to_string(),
            summary: format!("event {}", event_id),
            files,
        }
    }

    /// Parse every raw line of `projects/<project>/files.jsonl`, failing on any
    /// malformed/torn line, and return the records keyed by file_name.
    fn read_raw_projection(store: &MemStore, project: &str) -> BTreeMap<String, FileRecord> {
        let raw =
            fs::read(store.project_dir(project).join("files.jsonl")).expect("read files.jsonl");
        let mut records = BTreeMap::new();
        for line in raw
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: FileRecord = serde_json::from_slice(line).unwrap_or_else(|error| {
                panic!(
                    "corrupt files.jsonl line {:?}: {}",
                    String::from_utf8_lossy(line),
                    error
                )
            });
            records.insert(record.file_name.clone(), record);
        }
        records
    }

    #[test]
    fn test_compact_project_files() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();
        let project = "compact_test";

        // Three events record five FileRecords: a.txt is written three times,
        // b.txt twice.
        store
            .append_event(&test_event(
                "eid_c1",
                "2026-10-22T01:00:00Z",
                project,
                vec![
                    test_record("fid_a1", "a.txt", "Created", 10, "h_a1"),
                    test_record("fid_b1", "b.txt", "Created", 20, "h_b1"),
                ],
            ))
            .unwrap();
        store
            .append_event(&test_event(
                "eid_c2",
                "2026-10-22T02:00:00Z",
                project,
                vec![
                    test_record("fid_a2", "a.txt", "Modified", 15, "h_a2"),
                    test_record("fid_b2", "b.txt", "Deleted", 0, "h_b2"),
                ],
            ))
            .unwrap();
        store
            .append_event(&test_event(
                "eid_c3",
                "2026-10-22T03:00:00Z",
                project,
                vec![test_record("fid_a3", "a.txt", "Modified", 30, "h_a3")],
            ))
            .unwrap();

        assert_eq!(store.read_project_files(project).unwrap().len(), 5);

        let report = compact_project_files(&store, project).unwrap();
        assert_eq!(report.project, project);
        assert_eq!(report.records_before, 5);
        assert_eq!(report.records_after, 2);

        let records = read_raw_projection(&store, project);
        assert_eq!(records.len(), 2);
        assert_eq!(records["a.txt"].file_id, "fid_a3");
        assert_eq!(records["a.txt"].size_bytes, 30);
        assert_eq!(records["b.txt"].file_id, "fid_b2");
        assert_eq!(records["b.txt"].action, "Deleted");

        // Compacting an already compact projection is a no-op count-wise.
        let second = compact_project_files(&store, project).unwrap();
        assert_eq!(second.records_before, 2);
        assert_eq!(second.records_after, 2);
    }

    /// A crash between the two appends of `append_event` can leave the
    /// projection missing, empty or truncated even though every event survived.
    /// Compaction must rebuild the projection from the event log.
    #[test]
    fn test_compact_heals_missing_or_truncated_projection() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();
        let project = "heal_test";

        store
            .append_event(&test_event(
                "eid_h1",
                "2026-10-22T01:00:00Z",
                project,
                vec![
                    test_record("fid_a1", "a.txt", "Created", 10, "h_a1"),
                    test_record("fid_b1", "b.txt", "Created", 20, "h_b1"),
                ],
            ))
            .unwrap();
        store
            .append_event(&test_event(
                "eid_h2",
                "2026-10-22T02:00:00Z",
                project,
                vec![test_record("fid_a2", "a.txt", "Modified", 15, "h_a2")],
            ))
            .unwrap();

        let files_path = store.project_dir(project).join("files.jsonl");

        // (1) Projection wiped to an empty file.
        fs::write(&files_path, "").unwrap();
        let report = compact_project_files(&store, project).unwrap();
        assert_eq!(report.records_before, 0);
        assert_eq!(report.records_after, 2);
        let records = read_raw_projection(&store, project);
        assert_eq!(records.len(), 2);
        assert_eq!(records["a.txt"].file_id, "fid_a2");
        assert_eq!(records["b.txt"].file_id, "fid_b1");

        // (2) Projection deleted entirely.
        fs::remove_file(&files_path).unwrap();
        let report = compact_project_files(&store, project).unwrap();
        assert_eq!(report.records_before, 0);
        assert_eq!(report.records_after, 2);
        let records = read_raw_projection(&store, project);
        assert_eq!(records.len(), 2);
        assert_eq!(records["a.txt"].file_id, "fid_a2");

        // (3) Projection truncated mid-line: the partial line parses as no
        // record, but every event survived, so the rebuild restores everything.
        let complete = fs::read(&files_path).unwrap();
        fs::write(&files_path, &complete[..10]).unwrap();
        let report = compact_project_files(&store, project).unwrap();
        assert_eq!(report.records_before, 1, "one partial line remains");
        assert_eq!(report.records_after, 2);
        let records = read_raw_projection(&store, project);
        assert_eq!(records.len(), 2);
        assert_eq!(records["a.txt"].file_id, "fid_a2");
    }

    /// The incremental projection follows *append* order, but causality follows
    /// the timestamp. An out-of-order append (clock skew, replayed merge) must
    /// not win during a rebuild.
    #[test]
    fn test_compact_keeps_latest_event_not_latest_append() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();
        let project = "order_test";

        // Appended newest-first: the projection's last line for a.txt is the
        // oldest version, and its last line for b.txt is not the newest one.
        store
            .append_event(&test_event(
                "eid_newest",
                "2026-10-22T03:00:00Z",
                project,
                vec![
                    test_record("fid_a3", "a.txt", "Modified", 30, "h_a3"),
                    test_record("fid_b1", "b.txt", "Created", 20, "h_b1"),
                ],
            ))
            .unwrap();
        store
            .append_event(&test_event(
                "eid_oldest",
                "2026-10-22T01:00:00Z",
                project,
                vec![test_record("fid_a1", "a.txt", "Created", 10, "h_a1")],
            ))
            .unwrap();
        store
            .append_event(&test_event(
                "eid_middle",
                "2026-10-22T02:00:00Z",
                project,
                vec![test_record("fid_b2", "b.txt", "Modified", 25, "h_b2")],
            ))
            .unwrap();

        // Before compaction the projection reflects append order.
        let before = store.get_latest_files(project).unwrap();
        let a_before = before
            .iter()
            .find(|record| record.file_name == "a.txt")
            .unwrap();
        assert_eq!(a_before.file_id, "fid_a1");

        let report = compact_project_files(&store, project).unwrap();
        assert_eq!(report.records_before, 4);
        assert_eq!(report.records_after, 2);

        let records = read_raw_projection(&store, project);
        assert_eq!(records.len(), 2);
        assert_eq!(records["a.txt"].file_id, "fid_a3");
        assert_eq!(records["a.txt"].size_bytes, 30);
        assert_eq!(records["b.txt"].file_id, "fid_b1");
        assert_eq!(records["b.txt"].action, "Created");
    }

    /// Compaction and appends must serialise on the store-wide write lock; a
    /// rebuild that raced an append used to drop the concurrent records.
    #[test]
    fn test_concurrent_compact_and_append_preserves_records() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        let store = MemStore::new(root.clone());
        store.init().unwrap();

        let project = "concurrent_compact";
        let appends = 60usize;
        let compactions = 30usize;

        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            let store = MemStore::new(writer_root);
            for index in 0..appends {
                let event = test_event(
                    &format!("eid_w_{}", index),
                    // Distinct timestamps so "latest" is well defined.
                    &format!("2026-10-22T00:{:02}:00Z", index),
                    project,
                    vec![test_record(
                        &format!("fid_w_{}", index),
                        &format!("src/file_{}.rs", index % 5),
                        if index < 5 { "Created" } else { "Modified" },
                        index as u64,
                        &format!("hash_{}", index),
                    )],
                );
                store.append_event(&event).unwrap();
            }
        });

        let compactor_root = root.clone();
        let compactor = std::thread::spawn(move || {
            let store = MemStore::new(compactor_root);
            for _ in 0..compactions {
                compact_project_files(&store, project).unwrap();
            }
        });

        writer.join().expect("writer thread must not fail");
        compactor.join().expect("compactor thread must not fail");

        // One final rebuild with no writer running: the projection must equal
        // the last-write-wins replay of the event log.
        let report = compact_project_files(&store, project).unwrap();
        assert_eq!(report.records_after, 5);

        let mut events: Vec<Event> = store
            .read_all_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.project == project)
            .collect();
        assert_eq!(events.len(), appends);
        crate::util::sort_events(&mut events);
        let mut expected: BTreeMap<String, FileRecord> = BTreeMap::new();
        for event in &events {
            for record in &event.files {
                expected.insert(record.file_name.clone(), record.clone());
            }
        }

        // `read_raw_projection` also asserts that no line is torn/corrupt.
        let actual = read_raw_projection(&store, project);
        assert_eq!(actual, expected);
    }
}
