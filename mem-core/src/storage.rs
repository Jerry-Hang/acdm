use crate::models::{Event, FileRecord, ProjectMeta};
use fs2::FileExt;
use ignore::WalkBuilder;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

    pub fn nodes_dir(&self) -> PathBuf {
        self.root.join("nodes")
    }

    /// Initialize directories and create default .gitignore if not present.
    pub fn init(&self) -> Result<(), Box<dyn Error>> {
        fs::create_dir_all(self.events_dir())?;
        fs::create_dir_all(self.projects_dir())?;
        fs::create_dir_all(self.nodes_dir())?;

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

    /// Append an event to events/YYYY-MM.jsonl and append its FileRecords to projects/<name>/files.jsonl.
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
            let event: Event = serde_json::from_slice(line)?;
            events.push(event);
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
                let event: Event = serde_json::from_slice(line)?;
                all_events.push(event);
            }
        }
        Ok(all_events)
    }

    /// Read tail of the most recent events file using File::seek(SeekFrom::End) + BufReader::read_line.
    /// Never uses mmap for tail reads.
    pub fn read_latest_events(&self, n: usize) -> Result<Vec<Event>, Box<dyn Error>> {
        if n == 0 {
            return Ok(Vec::new());
        }

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

        let latest_file_path = match files.last() {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let mut file = File::open(latest_file_path)?;
        let file_len = file.metadata()?.len();
        if file_len == 0 {
            return Ok(Vec::new());
        }

        // Determine if file ends with newline
        let mut last_byte = [0u8; 1];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last_byte)?;
        let ends_with_nl = last_byte[0] == b'\n';
        let target_newlines = if ends_with_nl { n + 1 } else { n };

        const CHUNK_SIZE: u64 = 8192;
        let mut pos = file_len;
        let mut newlines_found = 0usize;
        let mut start_pos = 0u64;

        while pos > 0 {
            let to_read = CHUNK_SIZE.min(pos);
            pos -= to_read;
            file.seek(SeekFrom::Start(pos))?;
            let mut buf = vec![0u8; to_read as usize];
            file.read_exact(&mut buf)?;

            for (i, &b) in buf.iter().enumerate().rev() {
                if b == b'\n' {
                    newlines_found += 1;
                    if newlines_found == target_newlines {
                        start_pos = pos + i as u64 + 1;
                        break;
                    }
                }
            }
            if newlines_found >= target_newlines {
                break;
            }
        }

        file.seek(SeekFrom::Start(start_pos))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut events = Vec::new();

        while reader.read_line(&mut line)? > 0 {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if let Ok(event) = serde_json::from_str::<Event>(trimmed) {
                    events.push(event);
                }
            }
            line.clear();
        }

        if events.len() > n {
            events = events.split_off(events.len() - n);
        }

        Ok(events)
    }

    /// Read all records from projects/<project>/files.jsonl.
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
            let rec: FileRecord = serde_json::from_slice(line)?;
            records.push(rec);
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

    /// Read projects/<project>/meta.json.
    pub fn read_project_meta(&self, project: &str) -> Result<ProjectMeta, Box<dyn Error>> {
        let path = self.project_dir(project).join("meta.json");
        let file = File::open(&path)?;
        let meta = serde_json::from_reader(file)?;
        Ok(meta)
    }

    /// Write projects/<project>/meta.json (overwrite).
    pub fn write_project_meta(&self, project: &str, meta: &ProjectMeta) -> Result<(), Box<dyn Error>> {
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
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    projects.push(name.to_string());
                }
            }
        }
        projects.sort();
        Ok(projects)
    }

    /// Scan files in directory respecting .gitignore entries via ignore crate.
    pub fn scan_files(&self, dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
        let mut files = Vec::new();
        for result in WalkBuilder::new(dir).hidden(false).build() {
            let entry = result?;
            if entry.file_type().map_or(false, |ft| ft.is_file()) {
                files.push(entry.into_path());
            }
        }
        Ok(files)
    }
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
        assert!(store.nodes_dir().exists());
        assert!(store.root.join(".gitignore").exists());
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

        let latest_1 = store.read_latest_events(1).unwrap();
        assert_eq!(latest_1.len(), 1);
        assert_eq!(latest_1[0].event_id, "eid_2");

        let latest_5 = store.read_latest_events(5).unwrap();
        assert_eq!(latest_5.len(), 2);

        let proj_files = store.read_project_files("demo").unwrap();
        assert_eq!(proj_files.len(), 2);

        let latest_files = store.get_latest_files("demo").unwrap();
        assert_eq!(latest_files.len(), 1);
        assert_eq!(latest_files[0].action, "Modified");
        assert_eq!(latest_files[0].size_bytes, 120);
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
