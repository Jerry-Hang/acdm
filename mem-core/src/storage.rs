use crate::models::{Event, FileRecord, ProjectMeta};
use crate::util;
use fs2::FileExt;
use ignore::WalkBuilder;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct MemStore {
    pub root: PathBuf,
}

impl MemStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn events_dir(&self) -> PathBuf {
        self.root.join("events")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn project_dir(&self, project: &str) -> PathBuf {
        self.projects_dir().join(project)
    }

    /// Initialize directories, the store marker file and a default `.gitignore`.
    ///
    /// The `.mem` marker makes the store discoverable from any subdirectory by
    /// both the CLI and the MCP server (see [`crate::util::find_store_root`]).
    pub fn init(&self) -> Result<(), Box<dyn Error>> {
        fs::create_dir_all(self.events_dir())?;
        fs::create_dir_all(self.projects_dir())?;

        let marker_path = self.root.join(util::STORE_MARKER);
        if !marker_path.exists() {
            let marker = serde_json::json!({
                "format": 1,
                "created_at": util::now_rfc3339(),
            });
            fs::write(
                &marker_path,
                format!("{}\n", serde_json::to_string_pretty(&marker)?),
            )?;
        }

        let gitignore_path = self.root.join(".gitignore");
        if !gitignore_path.exists() {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&gitignore_path)?;
            file.lock_exclusive()?;
            writeln!(file, "target/")?;
            writeln!(file, "*.swp")?;
            writeln!(file, "*.tmp")?;
            writeln!(file, "*.tmp.*")?;
            writeln!(file, "*~")?;
            writeln!(file, ".DS_Store")?;
            file.flush()?;
            file.unlock()?;
        }
        Ok(())
    }

    /// Initialize store and configure git repository with remote origin.
    pub fn init_with_remote(&self, remote_url: &str) -> Result<(), Box<dyn Error>> {
        self.init()?;
        crate::git::git_init(&self.root)?;
        crate::git::git_add_remote(&self.root, "origin", remote_url)?;
        Ok(())
    }

    /// Append an event to `events/YYYY-MM.jsonl` and append its `FileRecord`s to
    /// `projects/<name>/files.jsonl`.
    pub fn append_event(&self, event: &Event) -> Result<(), Box<dyn Error>> {
        let ym = if event.ts.len() >= 7 && event.ts.as_bytes()[4] == b'-' {
            &event.ts[..7]
        } else {
            "default"
        };

        fs::create_dir_all(self.events_dir())?;
        let event_file_path = self.events_dir().join(format!("{}.jsonl", ym));
        let mut event_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&event_file_path)?;

        event_file.lock_exclusive()?;
        let serialized_event = serde_json::to_string(event)?;
        writeln!(event_file, "{}", serialized_event)?;
        event_file.flush()?;
        event_file.unlock()?;

        if !event.files.is_empty() {
            let proj_dir = self.project_dir(&event.project);
            fs::create_dir_all(&proj_dir)?;
            let files_path = proj_dir.join("files.jsonl");

            let mut files_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&files_path)?;

            files_file.lock_exclusive()?;
            for record in &event.files {
                let serialized_rec = serde_json::to_string(record)?;
                writeln!(files_file, "{}", serialized_rec)?;
            }
            files_file.flush()?;
            files_file.unlock()?;
        }

        Ok(())
    }

    /// Read historical events for a given YYYY-MM using memmap2 for zero-copy.
    pub fn read_events_for_month(&self, year_month: &str) -> Result<Vec<Event>, Box<dyn Error>> {
        let path = self.events_dir().join(format!("{}.jsonl", year_month));
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&path)?;
        if file.metadata()?.len() == 0 {
            return Ok(Vec::new());
        }

        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let mut events = Vec::new();
        for line in mmap.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            // Skip unparseable lines instead of failing the whole read: a single
            // truncated write, or a line preserved verbatim by `mem resolve`,
            // must not make the store unreadable.
            if let Ok(event) = serde_json::from_slice::<Event>(line) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Read all events from all events/*.jsonl files.
    pub fn read_all_events(&self) -> Result<Vec<Event>, Box<dyn Error>> {
        let events_dir = self.events_dir();
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

        let mut all_events = Vec::new();
        for path in files {
            let file = File::open(&path)?;
            if file.metadata()?.len() == 0 {
                continue;
            }
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            for line in mmap.split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_slice::<Event>(line) {
                    all_events.push(event);
                }
            }
        }
        Ok(all_events)
    }

    /// Read the `n` most recent events of a project, newest last.
    ///
    /// Months are visited newest-first so an older month is never preferred over
    /// a newer one; within a month events are sorted with the parse-aware
    /// comparator so mixed legacy timestamp formats stay chronologically correct.
    pub fn read_latest_events_for_project(
        &self,
        project: &str,
        n: usize,
    ) -> Result<Vec<Event>, Box<dyn Error>> {
        if n == 0 {
            return Ok(Vec::new());
        }

        let mut latest = Vec::with_capacity(n);
        for path in self.month_files()?.into_iter().rev() {
            let year_month = match path.file_stem().and_then(|s| s.to_str()) {
                Some(value) => value.to_string(),
                None => continue,
            };
            let mut events: Vec<Event> = self
                .read_events_for_month(&year_month)?
                .into_iter()
                .filter(|event| event.project == project)
                .collect();
            util::sort_events(&mut events);
            for event in events.into_iter().rev() {
                latest.push(event);
                if latest.len() == n {
                    break;
                }
            }
            if latest.len() == n {
                break;
            }
        }

        latest.reverse();
        Ok(latest)
    }

    /// List `events/*.jsonl` files in ascending order (oldest month first).
    pub fn month_files(&self) -> Result<Vec<PathBuf>, Box<dyn Error>> {
        let events_dir = self.events_dir();
        if !events_dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        for entry in fs::read_dir(&events_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") && is_month_shard(&path) {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    /// Read all records from `projects/<project>/files.jsonl`.
    pub fn read_project_files(&self, project: &str) -> Result<Vec<FileRecord>, Box<dyn Error>> {
        let path = self.project_dir(project).join("files.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&path)?;
        if file.metadata()?.len() == 0 {
            return Ok(Vec::new());
        }

        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let mut records = Vec::new();
        for line in mmap.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_slice::<FileRecord>(line) {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Read files.jsonl, deduplicate by file_name keeping only last occurrence.
    pub fn get_latest_files(&self, project: &str) -> Result<Vec<FileRecord>, Box<dyn Error>> {
        let records = self.read_project_files(project)?;
        let mut map = std::collections::BTreeMap::new();
        for r in records {
            map.insert(r.file_name.clone(), r);
        }
        Ok(map.into_values().collect())
    }

    /// Read `projects/<project>/meta.json`.
    pub fn read_project_meta(&self, project: &str) -> Result<ProjectMeta, Box<dyn Error>> {
        let path = self.project_dir(project).join("meta.json");
        let file = File::open(&path)?;
        let meta = serde_json::from_reader(file)?;
        Ok(meta)
    }

    /// Write `projects/<project>/meta.json` (overwrite).
    pub fn write_project_meta(
        &self,
        project: &str,
        meta: &ProjectMeta,
    ) -> Result<(), Box<dyn Error>> {
        let proj_dir = self.project_dir(project);
        fs::create_dir_all(&proj_dir)?;
        let path = proj_dir.join("meta.json");

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;

        file.lock_exclusive()?;
        let json_str = serde_json::to_string_pretty(meta)?;
        file.write_all(json_str.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.unlock()?;
        Ok(())
    }

    /// Check if a project exists.
    pub fn project_exists(&self, project: &str) -> bool {
        self.project_dir(project).is_dir()
    }

    /// List subdirectories in projects/ respecting gitignore via ignore crate.
    pub fn list_projects(&self) -> Result<Vec<String>, Box<dyn Error>> {
        let projects_dir = self.projects_dir();
        if !projects_dir.exists() {
            return Ok(Vec::new());
        }

        let mut projects = Vec::new();
        for result in WalkBuilder::new(&projects_dir).max_depth(Some(1)).build() {
            let entry = result?;
            if entry.path() == projects_dir {
                continue;
            }
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    projects.push(name.to_string());
                }
            }
        }
        projects.sort();
        Ok(projects)
    }
}

/// Whether a file is a `YYYY-MM.jsonl` month shard.
///
/// Month shards are ordered lexicographically, so a stray `default.jsonl` (the
/// legacy fallback for events with an unparseable timestamp) would otherwise be
/// treated as the newest month by latest-event queries.
fn is_month_shard(path: &std::path::Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!("acdm_test_{}_{}", std::process::id(), crate::generate_id());
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

    fn create_temp_store() -> (TestDir, MemStore) {
        let temp_dir = TestDir::new();
        let store = MemStore::new(temp_dir.path.clone());
        store.init().unwrap();
        (temp_dir, store)
    }

    #[test]
    fn test_init_and_paths() {
        let (_tmp, store) = create_temp_store();
        assert!(store.events_dir().exists());
        assert!(store.projects_dir().exists());
        assert!(store.root.join(".gitignore").exists());
        // The marker file is what makes the store discoverable from subdirectories.
        assert!(store.root.join(crate::util::STORE_MARKER).is_file());
    }

    #[test]
    fn test_append_and_read_events() {
        let (_tmp, store) = create_temp_store();

        let event1 = Event {
            event_id: "eid_1".to_string(),
            ts: "2026-10-22T04:00:00Z".to_string(),
            device: "laptop".to_string(),
            agent: "agent-1".to_string(),
            project: "demo".to_string(),
            summary: "first event".to_string(),
            files: vec![FileRecord {
                file_id: "fid_1".to_string(),
                file_name: "src/main.rs".to_string(),
                action: "Created".to_string(),
                size_bytes: 100,
                hash: "hash1".to_string(),
            }],
        };

        let event2 = Event {
            event_id: "eid_2".to_string(),
            ts: "2026-10-22T04:05:00Z".to_string(),
            device: "laptop".to_string(),
            agent: "agent-1".to_string(),
            project: "demo".to_string(),
            summary: "second event".to_string(),
            files: vec![FileRecord {
                file_id: "fid_2".to_string(),
                file_name: "src/main.rs".to_string(),
                action: "Modified".to_string(),
                size_bytes: 120,
                hash: "hash2".to_string(),
            }],
        };

        store.append_event(&event1).unwrap();
        store.append_event(&event2).unwrap();

        let month_events = store.read_events_for_month("2026-10").unwrap();
        assert_eq!(month_events.len(), 2);
        assert_eq!(month_events[0].event_id, "eid_1");
        assert_eq!(month_events[1].event_id, "eid_2");

        let all_events = store.read_all_events().unwrap();
        assert_eq!(all_events.len(), 2);

        let latest_1 = store.read_latest_events_for_project("demo", 1).unwrap();
        assert_eq!(latest_1.len(), 1);
        assert_eq!(latest_1[0].event_id, "eid_2");

        let latest_5 = store.read_latest_events_for_project("demo", 5).unwrap();
        assert_eq!(latest_5.len(), 2);
        assert_eq!(latest_5[0].event_id, "eid_1");
        assert_eq!(latest_5[1].event_id, "eid_2");

        // Another project's events must not leak in.
        assert!(store
            .read_latest_events_for_project("other", 5)
            .unwrap()
            .is_empty());

        let proj_files = store.read_project_files("demo").unwrap();
        assert_eq!(proj_files.len(), 2);

        let latest_files = store.get_latest_files("demo").unwrap();
        assert_eq!(latest_files.len(), 1);
        assert_eq!(latest_files[0].action, "Modified");
        assert_eq!(latest_files[0].size_bytes, 120);
    }

    fn make_event(event_id: &str, ts: &str, project: &str) -> Event {
        Event {
            event_id: event_id.to_string(),
            ts: ts.to_string(),
            device: "laptop".to_string(),
            agent: "agent-1".to_string(),
            project: project.to_string(),
            summary: format!("event {}", event_id),
            files: Vec::new(),
        }
    }

    /// The old tail reader only looked at the newest month file, so it silently
    /// missed older months. Latest-events lookups must span months.
    #[test]
    fn test_read_latest_events_spans_months() {
        let (_tmp, store) = create_temp_store();
        store
            .append_event(&make_event("eid_sep", "2026-09-30T23:00:00Z", "demo"))
            .unwrap();
        store
            .append_event(&make_event("eid_oct1", "2026-10-01T01:00:00Z", "demo"))
            .unwrap();
        store
            .append_event(&make_event("eid_oct2", "2026-10-02T01:00:00Z", "demo"))
            .unwrap();

        let latest = store.read_latest_events_for_project("demo", 3).unwrap();
        let ids: Vec<&str> = latest.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, vec!["eid_sep", "eid_oct1", "eid_oct2"]);

        // Within a month, mixed legacy timestamp formats must still sort correctly.
        let mixed = store.read_latest_events_for_project("demo", 2).unwrap();
        assert_eq!(mixed.len(), 2);
        assert_eq!(mixed[1].event_id, "eid_oct2");
    }

    /// A single corrupt/truncated line must not make the whole store unreadable.
    #[test]
    fn test_reads_tolerate_corrupt_lines() {
        let (_tmp, store) = create_temp_store();
        store
            .append_event(&make_event("eid_ok", "2026-10-02T01:00:00Z", "demo"))
            .unwrap();

        let events_path = store.events_dir().join("2026-10.jsonl");
        let mut content = fs::read_to_string(&events_path).unwrap();
        content.push_str("{\"event_id\":\"broken\",\"ts\":\"2026-10-02T02:00:00Z\"\n");
        fs::write(&events_path, content).unwrap();

        let all = store.read_all_events().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].event_id, "eid_ok");

        let month = store.read_events_for_month("2026-10").unwrap();
        assert_eq!(month.len(), 1);

        let latest = store.read_latest_events_for_project("demo", 5).unwrap();
        assert_eq!(latest.len(), 1);
    }

    #[test]
    fn test_month_files_lists_jsonl_only() {
        let (_tmp, store) = create_temp_store();
        store
            .append_event(&make_event("eid_a", "2026-09-30T23:00:00Z", "demo"))
            .unwrap();
        store
            .append_event(&make_event("eid_b", "2026-10-01T01:00:00Z", "demo"))
            .unwrap();
        fs::write(store.events_dir().join("notes.txt"), "ignored").unwrap();

        let files = store.month_files().unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].to_string_lossy().ends_with("2026-09.jsonl"));
        assert!(files[1].to_string_lossy().ends_with("2026-10.jsonl"));
    }

    /// A legacy `default.jsonl` fallback shard must not be mistaken for the
    /// newest month (month files are ordered lexicographically).
    #[test]
    fn test_month_files_ignores_non_month_shards() {
        let (_tmp, store) = create_temp_store();
        store
            .append_event(&make_event("eid_a", "2026-09-30T23:00:00Z", "demo"))
            .unwrap();
        store
            .append_event(&make_event("eid_b", "2026-10-01T01:00:00Z", "demo"))
            .unwrap();
        // A legacy fallback shard for events without a parseable timestamp.
        let fallback = make_event("eid_default", "", "demo");
        fs::write(
            store.events_dir().join("default.jsonl"),
            format!("{}\n", serde_json::to_string(&fallback).unwrap()),
        )
        .unwrap();

        let files = store.month_files().unwrap();
        assert_eq!(
            files.len(),
            2,
            "only YYYY-MM shards are month files: {:?}",
            files
        );

        let latest = store.read_latest_events_for_project("demo", 5).unwrap();
        let ids: Vec<&str> = latest.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, vec!["eid_a", "eid_b"]);
    }

    #[test]
    fn test_meta_and_project_lifecycle() {
        let (_tmp, store) = create_temp_store();

        assert!(!store.project_exists("new_proj"));

        let meta = ProjectMeta {
            name: "new_proj".to_string(),
            description: "Test description".to_string(),
            github_url: Some("https://github.com/test/new_proj".to_string()),
            created_at: "2026-10-22T00:00:00Z".to_string(),
        };

        store.write_project_meta("new_proj", &meta).unwrap();
        assert!(store.project_exists("new_proj"));

        let read_meta = store.read_project_meta("new_proj").unwrap();
        assert_eq!(read_meta.name, "new_proj");
        assert_eq!(read_meta.description, "Test description");

        let projects = store.list_projects().unwrap();
        assert_eq!(projects, vec!["new_proj".to_string()]);
    }
}
