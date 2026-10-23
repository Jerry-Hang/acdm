use crate::models::{hash_file, Event, FileRecord, ProjectMeta};
use crate::util;
use fs2::FileExt;
use ignore::WalkBuilder;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

/// Cross-process exclusive lock guarding writes to one memory store.
///
/// This is an advisory `fs2`/`flock` lock. It is taken on the store's `.mem`
/// marker file when that marker exists: opening the marker read-only and
/// flocking it mutates nothing, so the synchronized Git repository stays clean,
/// and the lock naturally inherits the store's file permissions. A store that
/// has no marker yet (only the very first [`MemStore::init`] is in that state)
/// falls back to an owner-only lock file in the runtime/temp directory.
///
/// Holding the lock across a multi-file operation (event append, projection
/// rebuild, README render, conflict resolution, sync) removes the interleaving
/// windows that a per-file lock cannot cover.
pub struct StoreWriteLock {
    _file: File,
}

impl Drop for StoreWriteLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

/// Derive a stable, filename-safe identifier for the current user.
///
/// `std` exposes no portable uid, so the identity is derived from the variables
/// every login session sets. Hashing keeps the lock file name safe on every
/// filesystem and avoids leaking the user name into a shared directory.
fn current_user_tag() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let digest = crate::models::compute_sha256(format!("{}@{}", user, home).as_bytes());
    digest[..12].to_string()
}

/// Fallback lock file used while a store has no `.mem` marker yet.
///
/// It lives in the runtime/temp directory so it never pollutes the synchronized
/// Git repository, and its name carries both the user tag and a hash of the
/// canonical store path so users and stores on a shared machine cannot collide.
fn fallback_lock_file_path(root: &Path) -> PathBuf {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let digest = crate::models::compute_sha256(canonical.to_string_lossy().as_bytes());
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!(
        "acdm-store-{}-{}.lock",
        current_user_tag(),
        &digest[..16]
    ))
}

/// Acquire the store-wide write lock (blocks while another writer holds it).
///
/// The `.mem` marker is preferred as the lock target: it is opened **read-only**
/// and flocked, so taking the lock neither writes to the store nor dirty the Git
/// work tree, and permission errors for other users surface naturally. Only a
/// marker-less store (during its first `init`) falls back to a user-scoped,
/// owner-only (`0600` on Unix) file in the runtime/temp directory.
pub fn lock_store(root: &Path) -> Result<StoreWriteLock, Box<dyn Error>> {
    let marker_path = root.join(util::STORE_MARKER);
    if marker_path.is_file() {
        // `flock` (and fs2's Windows equivalent) works on read-only handles.
        let file = File::open(&marker_path)?;
        file.lock_exclusive()?;
        return Ok(StoreWriteLock { _file: file });
    }

    let path = fallback_lock_file_path(root);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Owner-only: on a shared machine another user must not be able to
        // open (and therefore contend on, or delete) this lock file.
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    file.lock_exclusive()?;
    Ok(StoreWriteLock { _file: file })
}

/// DOS device names that Windows rejects as file or directory names, in any
/// case and even with an extension (`con.txt` is as reserved as `CON`).
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate that `project` is safe to use as a single directory name under
/// `projects/`.
///
/// MCP tool arguments can be produced by an AI agent, so a project name must
/// never be able to escape the store: reject empty/whitespace, control
/// characters, path separators, and anything that is not exactly one normal path
/// component (`.`/`..`/absolute paths/drive prefixes are all rejected).
///
/// The name must also be creatable on every supported platform, so the
/// Windows-specific rules (no trailing dot, no DOS device names) are enforced
/// everywhere — a store synchronized from Linux to Windows must not contain
/// directories Windows cannot represent.
pub fn validate_project_name(project: &str) -> Result<(), Box<dyn Error>> {
    if project.is_empty() {
        return Err("project name must not be empty".into());
    }
    if project != project.trim() {
        return Err(format!(
            "invalid project name {:?}: leading or trailing whitespace",
            project
        )
        .into());
    }
    if project.len() > 200 {
        return Err(format!("invalid project name {:?}: longer than 200 bytes", project).into());
    }
    if project.chars().any(char::is_control) {
        return Err(format!(
            "invalid project name {:?}: contains control characters",
            project
        )
        .into());
    }
    if project.contains('/') || project.contains('\\') {
        return Err(format!(
            "invalid project name {:?}: must not contain path separators",
            project
        )
        .into());
    }
    // Windows also forbids these characters in a file or directory name; a
    // project name containing one would break a Windows checkout after sync.
    if let Some(character) = project
        .chars()
        .find(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(format!(
            "invalid project name {:?}: character {:?} is not allowed (would be invalid on Windows)",
            project, character
        )
        .into());
    }
    let mut components = Path::new(project).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => {
            return Err(format!(
                "invalid project name {:?}: must be a single path component (no '.', '..', absolute paths or prefixes)",
                project
            )
            .into())
        }
    }

    // Cross-platform hardening: a name that Windows cannot turn into a
    // directory must be rejected everywhere, or a store synchronized between
    // platforms breaks on Windows.
    if project.ends_with('.') {
        return Err(format!(
            "invalid project name {:?}: must not end with '.' (would be invalid on Windows)",
            project
        )
        .into());
    }
    // Windows compares only the part before the first dot against the device
    // names, so `nul.log` is reserved too.
    let stem = project.split('.').next().unwrap_or(project);
    if WINDOWS_RESERVED_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err(format!(
            "invalid project name {:?}: reserved Windows device name (would be invalid on Windows)",
            project
        )
        .into());
    }

    Ok(())
}

pub struct MemStore {
    pub root: PathBuf,
}

impl MemStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Acquire the store-wide write lock (blocks while another writer holds it).
    pub fn lock_writes(&self) -> Result<StoreWriteLock, Box<dyn Error>> {
        lock_store(&self.root)
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
        // Serialise concurrent inits and any init racing another writer. The
        // very first init has no `.mem` yet, so it serialises on the fallback
        // lock; every later writer finds the marker and locks that instead. The
        // files written here are idempotent ("write only if missing"), so the
        // brief overlap possible while bootstrapping is harmless.
        let _write_lock = self.lock_writes()?;
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
        // Backwards-compatible wrapper: callers that do not bootstrap project
        // metadata keep the exact previous behaviour.
        self.append_event_with_meta(event, None)
    }

    /// Append an event and, optionally, create the project's `meta.json` in the
    /// same critical section.
    ///
    /// The store-wide write lock is taken *before* the metadata check/write and
    /// held until both the event line and the projection lines are appended, so
    /// concurrent writers cannot interleave the metadata decision with an
    /// append. Callers that used to do "read metadata → write metadata → append"
    /// in separate steps (and could ignore the metadata error) should use this
    /// method instead.
    pub fn append_event_with_meta(
        &self,
        event: &Event,
        init_meta: Option<&ProjectMeta>,
    ) -> Result<(), Box<dyn Error>> {
        validate_project_name(&event.project)?;
        // Held for the optional meta write and both appends: the event log and
        // the files.jsonl projection must not be interleaved with a projection
        // rebuild or a rebase.
        let _write_lock = self.lock_writes()?;
        if let Some(meta) = init_meta {
            self.ensure_project_meta_locked(&event.project, meta)?;
        }
        self.append_event_locked(event)
    }

    /// Record one event describing `paths` and return it.
    ///
    /// This is the lock-safe equivalent of what `mem add` / `mem_add_event` used
    /// to do in several separate steps (read the latest records → decide
    /// `Created`/`Modified` → write metadata → append).
    ///
    /// **File work happens before the store-wide lock is taken.** Metadata
    /// checks and the streaming SHA-256 are pure reads that can be slow (a large
    /// file) or even unbounded, so they run unlocked; holding the store lock
    /// during them would block every other writer. Only regular files are
    /// accepted: a directory, FIFO or device would either fail or never reach
    /// EOF, so it is rejected with `cannot record '<path>': not a regular file`
    /// before it is opened for reading.
    ///
    /// **Only the state transition happens under the lock:** metadata healing,
    /// the latest-records snapshot, the `Created`/`Modified` decision, and the
    /// event/projection append. The decision can therefore never be based on a
    /// stale snapshot, and a metadata write failure aborts the whole operation
    /// instead of being ignored.
    ///
    /// For each path: a missing file records `Deleted` (size 0, empty hash); an
    /// existing regular file is recorded as `Modified` when its normalised name is
    /// already tracked, `Created` otherwise. `file_name` identity is normalised
    /// with [`crate::util::normalize_file_name`], so paths outside the store and
    /// outside any Git work tree keep their absolute form (callers can surface
    /// their own warning for that case).
    pub fn record_file_event(
        &self,
        project: &str,
        summary: &str,
        device: &str,
        agent: &str,
        paths: &[PathBuf],
    ) -> Result<Event, Box<dyn Error>> {
        validate_project_name(project)?;

        // --- Unlocked pre-computation: file names, sizes and hashes. ---
        // `None` content means "the path does not exist" → a Deleted record.
        let mut pending: Vec<(String, Option<(u64, String)>)> = Vec::with_capacity(paths.len());
        for path in paths {
            let file_name = util::normalize_file_name(path, &self.root);
            match fs::metadata(path) {
                Ok(metadata) => {
                    if !metadata.is_file() {
                        return Err(format!(
                            "cannot record '{}': not a regular file",
                            path.display()
                        )
                        .into());
                    }
                    // Streaming hash (64 KiB chunks): a large file is never
                    // loaded into memory and never holds the store lock.
                    let (size, hash) = hash_file(path)?;
                    pending.push((file_name, Some((size, hash))));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    pending.push((file_name, None));
                }
                Err(error) => return Err(error.into()),
            }
        }

        // --- Locked state transition: metadata healing + snapshot + append. ---
        let _write_lock = self.lock_writes()?;

        // One timestamp for both the metadata and the event, exactly like
        // `mem add` did before.
        let timestamp = util::now_rfc3339();
        let meta = ProjectMeta {
            name: project.to_string(),
            description: format!("Project {}", project),
            github_url: None,
            created_at: timestamp.clone(),
        };
        self.ensure_project_meta_locked(project, &meta)?;

        // Snapshot taken under the lock: the Created/Modified decision is made
        // against the same projection the records are appended to.
        let latest = self.get_latest_files(project)?;

        let mut files = Vec::with_capacity(pending.len());
        for (file_name, content) in pending {
            let (action, size_bytes, hash) = match content {
                Some((size, hash)) => {
                    let known = latest.iter().any(|record| record.file_name == file_name);
                    (
                        if known { "Modified" } else { "Created" }.to_string(),
                        size,
                        hash,
                    )
                }
                None => ("Deleted".to_string(), 0, String::new()),
            };
            files.push(FileRecord {
                file_id: crate::generate_id(),
                file_name,
                action,
                size_bytes,
                hash,
            });
        }

        let event = Event {
            event_id: crate::generate_id(),
            ts: timestamp,
            device: device.to_string(),
            agent: agent.to_string(),
            project: project.to_string(),
            summary: summary.to_string(),
            files,
        };
        self.append_event_locked(&event)?;
        Ok(event)
    }

    /// Append an event to the event log and its records to the projection.
    ///
    /// Caller must hold the store-wide write lock and must already have
    /// validated the project name; this is the shared body of
    /// [`Self::append_event_with_meta`] and [`Self::record_file_event`].
    fn append_event_locked(&self, event: &Event) -> Result<(), Box<dyn Error>> {
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

    /// Read historical events for a given YYYY-MM.
    ///
    /// Uses a plain buffered read rather than `mmap`: the store can be rewritten
    /// in place (conflict resolution truncates files, `git pull` may replace
    /// them), and reading through a mapping of a truncated file is undefined
    /// behaviour / `SIGBUS`. One file read is also simpler than mapping and
    /// unmapping a small file.
    pub fn read_events_for_month(&self, year_month: &str) -> Result<Vec<Event>, Box<dyn Error>> {
        let path = self.events_dir().join(format!("{}.jsonl", year_month));
        if !path.exists() {
            return Ok(Vec::new());
        }

        let data = fs::read(&path)?;
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        for line in data.split(|&b| b == b'\n') {
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
            let data = fs::read(&path)?;
            if data.is_empty() {
                continue;
            }
            for line in data.split(|&b| b == b'\n') {
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
        validate_project_name(project)?;
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
        validate_project_name(project)?;
        let path = self.project_dir(project).join("files.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }

        let data = fs::read(&path)?;
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let mut records = Vec::new();
        for line in data.split(|&b| b == b'\n') {
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
        validate_project_name(project)?;
        let records = self.read_project_files(project)?;
        let mut map = std::collections::BTreeMap::new();
        for r in records {
            map.insert(r.file_name.clone(), r);
        }
        Ok(map.into_values().collect())
    }

    /// Read `projects/<project>/meta.json`.
    pub fn read_project_meta(&self, project: &str) -> Result<ProjectMeta, Box<dyn Error>> {
        validate_project_name(project)?;
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
        validate_project_name(project)?;
        let _write_lock = self.lock_writes()?;
        self.write_project_meta_locked(project, meta)
    }

    /// Write `projects/<project>/meta.json` (overwrite) without acquiring the
    /// store-wide lock; the caller must already hold it.
    fn write_project_meta_locked(
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

    /// Create project metadata when the project has no usable `meta.json`.
    ///
    /// "Usable" means *parseable*, matching the previous CLI behaviour: both a
    /// missing and a corrupt `meta.json` are rewritten, so a store damaged by a
    /// partial write self-heals instead of failing forever.
    ///
    /// Caller must hold the store-wide lock: the check and the write must be
    /// atomic with respect to other writers, otherwise concurrent first writers
    /// of a project can race on `meta.json`.
    fn ensure_project_meta_locked(
        &self,
        project: &str,
        meta: &ProjectMeta,
    ) -> Result<(), Box<dyn Error>> {
        if self.read_project_meta(project).is_err() {
            self.write_project_meta_locked(project, meta)?;
        }
        Ok(())
    }

    /// Check if a project exists.
    pub fn project_exists(&self, project: &str) -> bool {
        validate_project_name(project).is_ok() && self.project_dir(project).is_dir()
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

    /// Regression for the two-append write path: eight writers racing on the
    /// same store must not lose, duplicate or tear event/projection lines.
    ///
    /// `flock` locks are tied to open file descriptions, so the store-wide lock
    /// taken by each thread's own `MemStore` serialises them even inside one
    /// process. Without that lock the projection would lose records whenever
    /// two appends interleave.
    #[test]
    fn test_concurrent_append_events_no_lost_lines() {
        let (_tmp, store) = create_temp_store();
        let root = store.root.clone();
        let project = "concurrent_append";
        let writers = 8usize;
        let per_writer = 50usize;
        let total = writers * per_writer;

        let handles: Vec<_> = (0..writers)
            .map(|writer| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let store = MemStore::new(root);
                    for index in 0..per_writer {
                        let event = Event {
                            event_id: format!("eid_{}_{}", writer, index),
                            ts: format!("2026-10-22T00:{:02}:00Z", index),
                            device: format!("device-{}", writer),
                            agent: format!("agent-{}", writer),
                            project: project.to_string(),
                            summary: format!("event {} from writer {}", index, writer),
                            files: vec![FileRecord {
                                file_id: format!("fid_{}_{}", writer, index),
                                file_name: format!("src/w{}/f{}.rs", writer, index),
                                action: "Created".to_string(),
                                size_bytes: index as u64,
                                hash: format!("hash_{}_{}", writer, index),
                            }],
                        };
                        store.append_event(&event).unwrap();
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("concurrent writer must not panic");
        }

        // Every event line in the month shard is complete, parseable JSON.
        let event_data = fs::read(store.events_dir().join("2026-10.jsonl")).unwrap();
        let event_lines: Vec<&[u8]> = event_data
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(event_lines.len(), total, "event lines must not be lost");
        for line in event_lines {
            serde_json::from_slice::<Event>(line).unwrap_or_else(|error| {
                panic!(
                    "torn event line {:?}: {}",
                    String::from_utf8_lossy(line),
                    error
                )
            });
        }

        // Same for the files.jsonl projection: exactly one parsable record per
        // event, and every file_id appears exactly once.
        let projection = fs::read(store.project_dir(project).join("files.jsonl")).unwrap();
        let mut file_ids = std::collections::BTreeSet::new();
        let mut projection_lines = 0usize;
        for line in projection
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: FileRecord = serde_json::from_slice(line).unwrap_or_else(|error| {
                panic!(
                    "torn projection line {:?}: {}",
                    String::from_utf8_lossy(line),
                    error
                )
            });
            file_ids.insert(record.file_id);
            projection_lines += 1;
        }
        assert_eq!(projection_lines, total, "projection lines must not be lost");
        assert_eq!(file_ids.len(), total, "no duplicate or missing file_id");

        assert_eq!(store.read_all_events().unwrap().len(), total);
        assert_eq!(store.read_project_files(project).unwrap().len(), total);
    }

    /// Project names that Windows cannot represent (trailing dot, DOS device
    /// names) must be rejected on every platform, so a store synchronized
    /// between platforms never contains an unrepresentable directory.
    #[test]
    fn test_validate_project_name_rejects_windows_reserved_and_trailing_dot() {
        for invalid in ["CON", "con", "nul", "aux.txt", "com1", "LPT9.log", "demo."] {
            let error = validate_project_name(invalid)
                .expect_err(&format!("{:?} must be rejected", invalid));
            assert!(
                error.to_string().contains("Windows"),
                "error for {:?} must explain the Windows rule: {}",
                invalid,
                error
            );
        }
        for valid in ["demo.v2", "console", "con-artist", "nulls", "com10", "demo"] {
            assert!(
                validate_project_name(valid).is_ok(),
                "{:?} must be accepted: {:?}",
                valid,
                validate_project_name(valid)
            );
        }
    }

    /// With a `.mem` marker the marker file itself is the lock target: it is
    /// opened read-only, so taking the lock never modifies the synchronized Git
    /// repository, and a second writer still blocks on it.
    #[test]
    fn test_lock_store_uses_marker_when_present() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (_tmp, store) = create_temp_store();
        let marker = store.root.join(crate::util::STORE_MARKER);
        assert!(marker.is_file());

        // The very first `init` had no marker yet and therefore used the
        // fallback lock; remove that stale file so we can prove the next lock
        // comes from the marker instead of recreating it.
        let fallback_path = fallback_lock_file_path(&store.root);
        if fallback_path.exists() {
            fs::remove_file(&fallback_path).unwrap();
        }

        let guard = store.lock_writes().unwrap();
        assert!(
            !fallback_path.exists(),
            "an existing .mem marker must be the lock target, not a runtime lock file"
        );

        // A second writer must block while the marker is flocked, and proceed
        // once it is released.
        let second_root = store.root.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let second = std::thread::spawn(move || {
            let store = MemStore::new(second_root);
            started_tx.send(()).unwrap();
            let _second_guard = store.lock_writes().expect("second lock acquisition");
            acquired_tx.send(()).unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second writer must start");
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(150))
                .is_err(),
            "the second writer must block while the marker is exclusively locked"
        );
        drop(guard);
        acquired_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the second writer must acquire the lock after release");
        second.join().unwrap();
    }

    /// A store without a `.mem` marker (only the first `init` is in that state)
    /// falls back to an owner-only, user-scoped lock file outside the store.
    #[test]
    fn test_lock_store_falls_back_without_marker() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        let fallback_path = fallback_lock_file_path(&store.root);
        assert!(!fallback_path.exists());

        let guard = store.lock_writes().unwrap();
        assert!(
            fallback_path.is_file(),
            "a marker-less store must create the fallback lock file"
        );
        let file_name = fallback_path
            .file_name()
            .expect("fallback lock file name")
            .to_string_lossy()
            .into_owned();
        assert!(
            file_name.contains(&current_user_tag()),
            "fallback lock file must carry a per-user tag: {}",
            file_name
        );
        assert!(
            !store.root.join(crate::util::STORE_MARKER).exists(),
            "acquiring the fallback lock must not create the marker"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&fallback_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "fallback lock file must be owner-only");
        }

        drop(guard);
    }

    /// Metadata bootstrapping happens under the same lock as the append, so two
    /// concurrent first writers of a project create exactly one valid
    /// `meta.json` and both events are recorded. A corrupt `meta.json` is
    /// rewritten (self-healing), matching the previous CLI behaviour.
    #[test]
    fn test_append_event_with_meta_creates_meta_once() {
        let (_tmp, store) = create_temp_store();
        let root = store.root.clone();
        let project = "meta_once";
        let meta = ProjectMeta {
            name: project.to_string(),
            description: "Project meta_once".to_string(),
            github_url: None,
            created_at: "2026-10-22T00:00:00Z".to_string(),
        };
        let events = [
            make_event("eid_meta_a", "2026-10-22T01:00:00Z", project),
            make_event("eid_meta_b", "2026-10-22T02:00:00Z", project),
        ];

        let handles: Vec<_> = events
            .into_iter()
            .map(|event| {
                let root = root.clone();
                let meta = meta.clone();
                std::thread::spawn(move || {
                    let store = MemStore::new(root);
                    store
                        .append_event_with_meta(&event, Some(&meta))
                        .expect("append with meta");
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("concurrent writer must not panic");
        }

        let stored_meta = store.read_project_meta(project).unwrap();
        assert_eq!(stored_meta, meta);

        // A corrupt meta.json must be repaired on the next append instead of
        // leaving the project permanently unreadable.
        let meta_path = store.project_dir(project).join("meta.json");
        fs::write(&meta_path, "{not valid json").unwrap();
        assert!(store.read_project_meta(project).is_err());
        store
            .append_event_with_meta(
                &make_event("eid_meta_c", "2026-10-22T03:00:00Z", project),
                Some(&meta),
            )
            .unwrap();
        assert_eq!(store.read_project_meta(project).unwrap(), meta);

        let mut events = store.read_latest_events_for_project(project, 10).unwrap();
        events.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        let ids: Vec<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(ids, vec!["eid_meta_a", "eid_meta_b", "eid_meta_c"]);
    }

    /// One lock-safe `record_file_event` call covers metadata bootstrap, path
    /// normalisation, hashing and the Created/Modified/Deleted decision.
    #[test]
    fn test_record_file_event_actions() {
        let (_tmp, store) = create_temp_store();
        let project = "record_actions";
        let file_path = store.root.join("tracked").join("notes.txt");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, b"v1").unwrap();

        // 1. New file -> Created, project meta bootstrapped in the same call.
        let created = store
            .record_file_event(
                project,
                "add notes",
                "device-x",
                "agent-x",
                std::slice::from_ref(&file_path),
            )
            .unwrap();
        assert_eq!(created.project, project);
        assert_eq!(created.device, "device-x");
        assert_eq!(created.agent, "agent-x");
        assert_eq!(created.summary, "add notes");
        assert_eq!(created.ts.len(), 30);
        assert!(created.ts.ends_with('Z'));
        assert_eq!(created.files.len(), 1);
        let record = &created.files[0];
        assert_eq!(record.action, "Created");
        assert_eq!(record.file_name, "tracked/notes.txt");
        assert_eq!(record.size_bytes, 2);
        assert_eq!(record.hash, crate::models::compute_sha256(b"v1"));
        assert!(!record.file_id.is_empty());

        let meta = store.read_project_meta(project).unwrap();
        assert_eq!(meta.name, project);
        assert_eq!(meta.description, "Project record_actions");

        // 2. Existing tracked file -> Modified with the new size/hash.
        fs::write(&file_path, b"version 2").unwrap();
        let modified = store
            .record_file_event(
                project,
                "edit notes",
                "device-x",
                "agent-x",
                std::slice::from_ref(&file_path),
            )
            .unwrap();
        assert_eq!(modified.files[0].action, "Modified");
        assert_eq!(modified.files[0].file_name, "tracked/notes.txt");
        assert_eq!(modified.files[0].size_bytes, 9);
        assert_eq!(
            modified.files[0].hash,
            crate::models::compute_sha256(b"version 2")
        );

        // 3. Missing path -> Deleted with zero size and an empty hash.
        fs::remove_file(&file_path).unwrap();
        let deleted = store
            .record_file_event(project, "delete notes", "device-x", "agent-x", &[file_path])
            .unwrap();
        assert_eq!(deleted.files[0].action, "Deleted");
        assert_eq!(deleted.files[0].file_name, "tracked/notes.txt");
        assert_eq!(deleted.files[0].size_bytes, 0);
        assert!(deleted.files[0].hash.is_empty());

        // Every call produced exactly one event, and the projection ends at the
        // Deleted state for the tracked file.
        assert_eq!(
            store
                .read_latest_events_for_project(project, 10)
                .unwrap()
                .len(),
            3
        );
        let latest = store.get_latest_files(project).unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].action, "Deleted");
    }

    /// Windows also forbids `< > : " | ? *` in file/directory names; a project
    /// name containing one would break a Windows checkout after sync.
    #[test]
    fn test_validate_project_name_rejects_windows_invalid_chars() {
        for invalid in ["a:b", "a*b", "a\"b", "a?b", "a<b", "a>b", "a|b"] {
            let error = validate_project_name(invalid)
                .expect_err(&format!("{:?} must be rejected", invalid));
            assert!(
                error.to_string().contains("Windows"),
                "error for {:?} must explain the Windows rule: {}",
                invalid,
                error
            );
        }
        for valid in ["a-b", "a_b", "a.b", "a b"] {
            assert!(
                validate_project_name(valid).is_ok(),
                "{:?} must be accepted: {:?}",
                valid,
                validate_project_name(valid)
            );
        }
    }

    /// Directories, FIFOs and devices must be rejected as "not a regular file"
    /// before they are opened: `/dev/zero` or a FIFO would never reach EOF, and
    /// hashing them used to block while holding the store-wide write lock.
    #[test]
    fn test_record_file_event_rejects_non_regular_file() {
        let (_tmp, store) = create_temp_store();
        let project = "non_regular";

        // Platform independent: a directory is not a regular file.
        let dir_path = store.root.join("a-directory");
        fs::create_dir_all(&dir_path).unwrap();
        let error = store
            .record_file_event(project, "add dir", "device-x", "agent-x", &[dir_path])
            .expect_err("a directory must be rejected");
        assert!(
            error.to_string().contains("not a regular file"),
            "unexpected error for a directory: {}",
            error
        );

        #[cfg(unix)]
        {
            // A FIFO never reaches EOF: it must be rejected instead of opened.
            // `mkfifo` is a coreutils/BSD utility; skip if it is unavailable.
            let fifo_path = store.root.join("a.fifo");
            let fifo_created = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if fifo_created {
                let error = store
                    .record_file_event(project, "add fifo", "device-x", "agent-x", &[fifo_path])
                    .expect_err("a FIFO must be rejected");
                assert!(
                    error.to_string().contains("not a regular file"),
                    "unexpected error for a FIFO: {}",
                    error
                );
            }
        }

        // A rejected call must not record anything or create the project.
        assert!(store.read_all_events().unwrap().is_empty());
        assert!(
            !store.project_dir(project).exists(),
            "a rejected call must not create the project directory"
        );
    }

    /// File work (metadata check + streaming hash) happens before the store-wide
    /// lock: pre-lock failures do not wait for a held lock, while the append
    /// itself still waits for it and then succeeds.
    #[test]
    fn test_record_file_event_hashes_before_locking() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let (_tmp, store) = create_temp_store();
        let root = store.root.clone();
        let project = "hash_before_lock";
        let file_path = store.root.join("payload.bin");
        fs::write(&file_path, vec![0u8; 256 * 1024]).unwrap();
        let dir_path = store.root.join("not-a-file");
        fs::create_dir_all(&dir_path).unwrap();

        // Thread A holds the store-wide write lock for 400 ms.
        let holder_root = root.clone();
        let (locked_tx, locked_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let store = MemStore::new(holder_root);
            let guard = store.lock_writes().expect("hold store lock");
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            drop(guard);
        });
        locked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("holder must signal");

        // Pre-lock validation must not wait for the lock: a non-regular path is
        // rejected immediately even though A owns the lock.
        let validation_started = Instant::now();
        let error = store
            .record_file_event(project, "add dir", "device-x", "agent-x", &[dir_path])
            .expect_err("a directory must be rejected");
        assert!(error.to_string().contains("not a regular file"));
        assert!(
            validation_started.elapsed() < Duration::from_millis(200),
            "pre-lock validation must not wait for the store lock, took {:?}",
            validation_started.elapsed()
        );

        // The append itself still serialises: it waits for A's lock, then
        // succeeds with the file hashed outside the critical section.
        let append_started = Instant::now();
        let event = store
            .record_file_event(project, "add payload", "device-x", "agent-x", &[file_path])
            .expect("record after the lock is released");
        assert!(
            append_started.elapsed() >= Duration::from_millis(250),
            "the append must wait for the store lock, took {:?}",
            append_started.elapsed()
        );
        assert_eq!(event.files.len(), 1);
        assert_eq!(event.files[0].action, "Created");
        assert_eq!(event.files[0].file_name, "payload.bin");
        assert_eq!(event.files[0].size_bytes, 256 * 1024);

        holder.join().expect("holder thread must not panic");
    }
}
