//! End-to-end tests for the `mem` binary.
//!
//! Every test drives the real binary through `CARGO_BIN_EXE_mem` and owns its own
//! temporary directory, so the suite needs no dev-dependencies and is safe to run
//! in parallel. `ACDM_STORE` is stripped from each child environment so a value
//! exported in the developer shell can never redirect a test at the wrong store.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Path of the `mem` binary built by cargo for this test run.
const MEM_BIN: &str = env!("CARGO_BIN_EXE_mem");

/// Monotonic counter so stores created by different tests never collide.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Minimal self-cleaning temporary directory.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = format!(
            "acdm_cli_it_{}_{}_{}",
            label,
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let path = env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create temporary test directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Create a store inside the temporary directory and initialise it.
    fn init_store(&self, name: &str) -> PathBuf {
        let store = self.path.join(name);
        fs::create_dir_all(&store).expect("create store directory");
        let output = run(&store, &["init"]);
        assert_success(&output);
        store
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Build a `mem` invocation with a fixed working directory and a clean environment.
fn mem(cwd: &Path) -> Command {
    let mut command = Command::new(MEM_BIN);
    command.current_dir(cwd);
    command.env_remove("ACDM_STORE");
    command
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    mem(cwd)
        .args(args)
        .output()
        .expect("failed to run the mem binary")
}

/// Run the `mem` binary, failing instead of hanging if it never exits.
///
/// Needed for the non-regular-file test: opening a FIFO with no writer blocks
/// forever, and a blocking child would hang the whole test binary (std has no
/// built-in `wait_timeout` without extra dependencies).
fn run_with_timeout(cwd: &Path, args: &[&str], timeout: Duration) -> Output {
    let mut child = mem(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the mem binary");
    let started = Instant::now();
    loop {
        match child.try_wait().expect("failed to wait for the mem binary") {
            Some(_) => return child.wait_with_output().expect("collect mem output"),
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .expect("collect output of the timed-out mem binary");
                panic!(
                    "mem {:?} did not exit within {:?}\nstdout:\n{}\nstderr:\n{}",
                    args,
                    timeout,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "mem command failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

/// All event lines from `events/*.jsonl`, oldest month first.
fn event_lines(store: &Path) -> Vec<String> {
    let events_dir = store.join("events");
    let mut files: Vec<PathBuf> = fs::read_dir(&events_dir)
        .expect("read events directory")
        .map(|entry| entry.expect("read events directory entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    files.sort();

    let mut lines = Vec::new();
    for path in files {
        let content = fs::read_to_string(&path).expect("read events jsonl file");
        lines.extend(content.lines().map(|line| line.to_string()));
    }
    lines
}

/// All records from `projects/<project>/files.jsonl`.
fn file_record_lines(store: &Path, project: &str) -> Vec<String> {
    let path = store.join("projects").join(project).join("files.jsonl");
    let content = fs::read_to_string(&path).expect("read files.jsonl");
    content
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect()
}

/// Extract the value of the `"ts"` field from one serialized event line.
fn event_ts(line: &str) -> String {
    let marker = "\"ts\":\"";
    let start = line.find(marker).expect("event line has a ts field") + marker.len();
    let rest = &line[start..];
    let end = rest.find('"').expect("ts value is terminated");
    rest[..end].to_string()
}

#[test]
fn init_creates_marker_and_store_layout() {
    let tmp = TempDir::new("init");
    let store = tmp.path().join("store");
    fs::create_dir_all(&store).expect("create store directory");

    let output = run(&store, &["init"]);
    assert_success(&output);

    assert!(store.join(".mem").is_file(), ".mem marker must exist");
    assert!(store.join("events").is_dir(), "events/ must exist");
    assert!(store.join("projects").is_dir(), "projects/ must exist");
    assert!(
        !store.join("nodes").exists(),
        "the legacy nodes/ directory must not be created"
    );

    let out = stdout(&output);
    assert!(
        out.contains(".mem"),
        "init output must mention the marker: {}",
        out
    );
    assert!(
        !out.contains("nodes/"),
        "init output must not advertise nodes/: {}",
        out
    );
}

#[test]
fn add_from_store_subdirectory_records_store_relative_path() {
    let tmp = TempDir::new("subdir");
    let store = tmp.init_store("store");
    let project_dir = store.join("projects").join("demo");
    let src_dir = project_dir.join("src");
    fs::create_dir_all(&src_dir).expect("create source directory");
    fs::write(src_dir.join("app.txt"), "hello from the store\n").expect("write app.txt");

    let output = run(
        &project_dir,
        &[
            "add",
            "-p",
            "demo",
            "-s",
            "add app",
            "-d",
            "test-device",
            "src/app.txt",
        ],
    );
    assert_success(&output);

    let records = file_record_lines(&store, "demo");
    assert_eq!(records.len(), 1);
    assert!(
        records[0].contains("\"file_name\":\"projects/demo/src/app.txt\""),
        "store-relative identity expected, got: {}",
        records[0]
    );
    assert!(
        records[0].contains("\"action\":\"Created\""),
        "the first record of a file must be Created: {}",
        records[0]
    );
}

#[test]
fn add_from_git_repository_uses_repo_relative_identity() {
    let tmp = TempDir::new("gitrepo");
    let store = tmp.init_store("store");
    // A real (empty) git work tree as the identity baseline; no commit is needed.
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("create repo source directory");
    let git_init = Command::new("git")
        .current_dir(&repo)
        .args(["init", "--quiet"])
        .output()
        .expect("run git init");
    assert!(
        git_init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&git_init.stderr)
    );
    let target = repo.join("src").join("lib.rs");
    fs::write(&target, "pub fn answer() -> u32 { 42 }\n").expect("write lib.rs");

    let output = run(
        &store,
        &[
            "add",
            "-p",
            "demo",
            "-s",
            "track repo lib",
            "-d",
            "test-device",
            target.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert_success(&output);

    let records = file_record_lines(&store, "demo");
    assert_eq!(records.len(), 1);
    assert!(
        records[0].contains("\"file_name\":\"src/lib.rs\""),
        "repository-relative identity expected, got: {}",
        records[0]
    );

    // Regression: querying by the recorded, normalised name must find the event.
    let query = run(&store, &["query", "-n", "src/lib.rs"]);
    assert_success(&query);
    let out = stdout(&query);
    assert!(
        out.contains("Found 1 event(s)"),
        "query by normalised name must match: {}",
        out
    );
    assert!(
        !out.contains("No events found"),
        "query must hit the recorded file: {}",
        out
    );
}

#[test]
fn add_writes_fixed_width_utc_timestamp() {
    let tmp = TempDir::new("timestamp");
    let store = tmp.init_store("store");
    let file = store.join("note.txt");
    fs::write(&file, "timestamp test\n").expect("write note.txt");

    let output = run(
        &store,
        &[
            "add",
            "-p",
            "demo",
            "-s",
            "timestamp",
            file.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert_success(&output);

    let lines = event_lines(&store);
    assert_eq!(lines.len(), 1);
    let ts = event_ts(&lines[0]);
    assert_eq!(ts.len(), 30, "timestamp must be fixed width 30: {ts:?}");
    assert!(ts.ends_with('Z'), "timestamp must be UTC: {ts:?}");
    let bytes = ts.as_bytes();
    assert_eq!(bytes[4], b'-');
    assert_eq!(bytes[10], b'T');
    assert_eq!(bytes[19], b'.');
    assert!(
        stdout(&output).contains(&format!("Timestamp: {ts}")),
        "the CLI must echo the canonical timestamp: {}",
        stdout(&output)
    );
}

#[test]
fn add_outside_store_and_git_warns_and_records_absolute_path() {
    let tmp = TempDir::new("outside");
    let store = tmp.init_store("store");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside directory");
    let loose = outside.join("loose.txt");
    fs::write(&loose, "loose file\n").expect("write loose.txt");

    let output = run(
        &store,
        &[
            "add",
            "-p",
            "demo",
            "-s",
            "loose",
            loose.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert_success(&output);

    let err = stderr(&output);
    assert!(
        err.contains("Warning:"),
        "an add outside the store and any git repository must warn on stderr: {}",
        err
    );

    let records = file_record_lines(&store, "demo");
    assert_eq!(records.len(), 1);
    // The recorded identity is the **canonical** absolute path: on Windows
    // `env::temp_dir()` can hand out an 8.3 short-name spelling (JERRY-~1) while
    // the identity is the long form, and the verbatim `\\?\` prefix is stripped
    // before rendering. Compare against canonicalize() with both normalized.
    let canonical = fs::canonicalize(&loose).expect("canonicalize the loose path");
    let absolute = canonical
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_string();
    assert!(
        records[0].contains(&format!("\"file_name\":\"{absolute}\"")),
        "an unnormalisable path is recorded absolutely, got: {}",
        records[0]
    );
}

#[test]
fn root_resolution_uses_path_env_var_and_upward_search() {
    let tmp = TempDir::new("root");
    let store = tmp.init_store("store");
    fs::write(store.join("root-note.txt"), "root resolution\n").expect("write root-note.txt");
    let add = run(
        &store,
        &["add", "-p", "demo", "-s", "root note", "root-note.txt"],
    );
    assert_success(&add);

    let outside = tmp.path().join("elsewhere");
    fs::create_dir_all(&outside).expect("create unrelated directory");

    // 1. An explicit --path always wins.
    let by_path = run(
        &outside,
        &["--path", store.to_str().expect("UTF-8 path"), "list"],
    );
    assert_success(&by_path);
    assert!(
        stdout(&by_path).contains("demo"),
        "--path must select the store: {}",
        stdout(&by_path)
    );

    // 2. $ACDM_STORE is used when --path is absent.
    let by_env = mem(&outside)
        .args(["list"])
        .env("ACDM_STORE", &store)
        .output()
        .expect("run mem with ACDM_STORE");
    assert_success(&by_env);
    assert!(
        stdout(&by_env).contains("demo"),
        "ACDM_STORE must select the store: {}",
        stdout(&by_env)
    );

    // 3. From a nested directory the nearest ancestor store is discovered.
    let nested = store.join("projects").join("demo");
    fs::create_dir_all(&nested).expect("create nested directory");
    let by_search = run(&nested, &["list"]);
    assert_success(&by_search);
    assert!(
        stdout(&by_search).contains("demo"),
        "upward search must find the store: {}",
        stdout(&by_search)
    );

    // Nothing may be created in the unrelated directory.
    assert!(
        fs::read_dir(&outside)
            .expect("read unrelated directory")
            .next()
            .is_none(),
        "no store artifacts may be created in an unrelated directory"
    );
}

#[test]
fn query_list_compact_and_render_work_with_correct_counts() {
    let tmp = TempDir::new("workflow");
    let store = tmp.init_store("store");
    fs::write(store.join("alpha.txt"), "alpha v1\n").expect("write alpha.txt");
    fs::write(store.join("beta.txt"), "beta v1\n").expect("write beta.txt");

    for (summary, file) in [("add alpha", "alpha.txt"), ("add beta", "beta.txt")] {
        let output = run(&store, &["add", "-p", "demo", "-s", summary, file]);
        assert_success(&output);
    }
    fs::write(store.join("alpha.txt"), "alpha v2\n").expect("rewrite alpha.txt");
    let output = run(
        &store,
        &["add", "-p", "demo", "-s", "modify alpha", "alpha.txt"],
    );
    assert_success(&output);

    let records = file_record_lines(&store, "demo");
    assert_eq!(records.len(), 3, "three records must be appended");
    assert!(
        records[2].contains("\"action\":\"Modified\""),
        "the second write of the same file must be Modified: {}",
        records[2]
    );

    let query = run(&store, &["query", "-p", "demo"]);
    assert_success(&query);
    assert!(
        stdout(&query).contains("Found 3 event(s)"),
        "query -p must list all three events: {}",
        stdout(&query)
    );

    let list_projects = run(&store, &["list"]);
    assert_success(&list_projects);
    let list_out = stdout(&list_projects);
    assert!(
        list_out.contains("PROJECT"),
        "list header expected: {}",
        list_out
    );
    assert!(
        list_out.contains("demo"),
        "project row expected: {}",
        list_out
    );

    let list_files = run(&store, &["list", "-p", "demo"]);
    assert_success(&list_files);
    let files_out = stdout(&list_files);
    assert!(
        files_out.contains("alpha.txt") && files_out.contains("beta.txt"),
        "latest file rows expected: {}",
        files_out
    );

    let render = run(&store, &["render", "-p", "demo"]);
    assert_success(&render);
    let readme = store.join("projects").join("demo").join("README.md");
    assert!(readme.is_file(), "render must write README.md");
    let readme_text = fs::read_to_string(&readme).expect("read README.md");
    assert!(
        readme_text.contains("alpha.txt"),
        "README must list tracked files: {}",
        readme_text
    );

    let compact = run(&store, &["compact", "-p", "demo"]);
    assert_success(&compact);
    let compact_out = stdout(&compact);
    assert!(
        compact_out.contains("Records before: 3"),
        "compact must report three records before: {}",
        compact_out
    );
    assert!(
        compact_out.contains("Records after:  2"),
        "compact must report two records after: {}",
        compact_out
    );
    assert!(
        compact_out.contains("Records pruned: 1"),
        "compact must prune one duplicate: {}",
        compact_out
    );
    assert_eq!(
        file_record_lines(&store, "demo").len(),
        2,
        "files.jsonl must hold two records after compaction"
    );
}

#[test]
fn resolve_reports_preserved_lines() {
    let tmp = TempDir::new("resolve");
    let store = tmp.init_store("store");
    let events_dir = store.join("events");
    fs::create_dir_all(&events_dir).expect("create events directory");
    let truncated = r#"{"event_id":"eid_3","ts":"2026-10-22T03:00:00Z","summary":"TRUNCATED"#;
    let conflicted = format!(
        r#"{{"event_id":"eid_1","ts":"2026-10-22T01:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s1","files":[]}}
<<<<<<< HEAD
{{"event_id":"eid_2","ts":"2026-10-22T02:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s2","files":[]}}
=======
{{"event_id":"eid_2","ts":"2026-10-22T02:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s2-theirs","files":[]}}
>>>>>>> branch
{truncated}
"#
    );
    let events_file = events_dir.join("2026-10.jsonl");
    fs::write(&events_file, conflicted).expect("write conflicted event log");

    let output = run(&store, &["resolve"]);
    assert_success(&output);

    let out = stdout(&output);
    assert!(
        out.contains("Resolved files:      1"),
        "resolve must report one resolved file: {}",
        out
    );
    assert!(
        out.contains("Conflicts resolved:  1"),
        "resolve must report one conflict: {}",
        out
    );
    assert!(
        out.contains("Lines preserved:     1"),
        "resolve must report the preserved unparseable line: {}",
        out
    );

    let resolved = fs::read_to_string(&events_file).expect("read resolved event log");
    assert!(
        !resolved.contains("<<<<<<<"),
        "conflict markers must be gone: {}",
        resolved
    );
    assert!(
        resolved.contains(truncated),
        "the unparseable line must survive verbatim: {}",
        resolved
    );
}

/// Regression: `mem render` creates `projects/<name>/`, so `mem add` used to
/// skip writing `meta.json` forever, leaving the project without metadata.
#[test]
fn render_before_add_still_creates_project_metadata() {
    let tmp = TempDir::new("render-then-add");
    let store = tmp.init_store("store");

    // Rendering a project that has no metadata yet must not break, and its
    // fallback "Created At" must use the canonical fixed-width UTC timestamp.
    let out = run(&store, &["render", "-p", "ghost"]);
    assert_success(&out);

    let readme = fs::read_to_string(store.join("projects").join("ghost").join("README.md"))
        .expect("read rendered README");
    let created_at = readme
        .lines()
        .find_map(|line| line.strip_prefix("- **Created At:** "))
        .expect("README must contain a Created At line");
    assert_eq!(
        created_at.len(),
        30,
        "fallback metadata timestamp must be the canonical 30-char form: {}",
        created_at
    );
    assert!(
        created_at.ends_with('Z'),
        "fallback metadata timestamp must be UTC: {}",
        created_at
    );

    fs::write(store.join("hello.txt"), "hello\n").expect("write tracked file");
    let out = run(
        &store,
        &["add", "-p", "ghost", "-s", "after render", "hello.txt"],
    );
    assert_success(&out);

    let meta_path = store.join("projects").join("ghost").join("meta.json");
    assert!(
        meta_path.is_file(),
        "meta.json must be written even though render created the directory first"
    );
    let meta = fs::read_to_string(&meta_path).expect("read meta.json");
    assert!(
        meta.contains("\"name\": \"ghost\""),
        "meta.json must describe the project: {}",
        meta
    );

    // `mem list` must now show a real creation time instead of "-".
    let out = run(&store, &["list"]);
    assert_success(&out);
    assert!(
        !stdout(&out).contains("ghost                     0         -"),
        "project row must use the metadata creation time: {}",
        stdout(&out)
    );
}

// ----------------- Review fixes: sync semantics, validation, streaming hash -----------------

/// Run `git` in `cwd` and return its output.
fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("failed to run git")
}

fn assert_git_ok(output: &Output) {
    assert!(
        output.status.success(),
        "git command failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Review finding #1/#5: a project name must never be usable to escape `projects/`
/// or the store, and the CLI must fail before touching the filesystem.
#[test]
fn project_name_traversal_is_rejected() {
    let tmp = TempDir::new("project-traversal");
    let store = tmp.init_store("store");
    fs::write(store.join("tracked.txt"), "content\n").expect("write tracked file");

    let absolute_outside = tmp.path().join("abs-outside");
    let invalid_names = [
        "../escape",
        "a/b",
        absolute_outside.to_str().expect("UTF-8 path"),
    ];

    for name in invalid_names {
        for args in [
            vec!["add", "-p", name, "-s", "must fail", "tracked.txt"],
            vec!["render", "-p", name],
            vec!["compact", "-p", name],
            vec!["list", "-p", name],
            vec!["query", "-p", name],
        ] {
            let output = run(&store, &args);
            assert!(
                !output.status.success(),
                "invalid project name {:?} must be rejected by {:?}",
                name,
                args
            );
            assert_eq!(
                output.status.code(),
                Some(1),
                "invalid project name must exit 1: {:?}",
                args
            );
            assert!(
                stderr(&output).contains("invalid project name"),
                "error must name the invalid project: {:?} -> {}",
                args,
                stderr(&output)
            );
        }
    }

    assert!(
        !tmp.path().join("escape").exists(),
        "`../escape` must not create a directory next to the store"
    );
    assert!(
        !store.join("escape").exists(),
        "`../escape` must not escape projects/ inside the store"
    );
    assert!(
        !absolute_outside.exists(),
        "an absolute project name must not create a directory outside the store"
    );
    assert!(
        !store.join("projects").join("a").exists(),
        "a nested project name must not create nested directories"
    );
}

/// Review finding #5: `mem sync` used to swallow a failed push and still exit 0.
/// Without an upstream the pull is skipped, but the push must fail loudly.
#[test]
fn sync_without_upstream_reports_push_failure_and_exits_nonzero() {
    let tmp = TempDir::new("sync-no-upstream");
    let store = tmp.init_store("store");
    assert_git_ok(&git(&store, &["init", "--quiet"]));
    assert_git_ok(&git(&store, &["config", "user.email", "test@acdm.local"]));
    assert_git_ok(&git(&store, &["config", "user.name", "ACDM Test"]));

    let output = run(&store, &["sync"]);
    assert!(
        !output.status.success(),
        "sync without a remote must not exit 0: stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed sync must exit with 1"
    );
    let combined = format!("{}\n{}", stdout(&output), stderr(&output)).to_lowercase();
    assert!(
        combined.contains("push"),
        "the error must point at the failed push/remote: {}",
        combined
    );
    assert!(
        !stdout(&output).contains("Sync successful"),
        "a failed sync must not claim success: {}",
        stdout(&output)
    );
}

/// Review finding #5: a pull conflict is not a success. The CLI must exit
/// non-zero and tell the user to run `mem resolve`.
#[test]
fn sync_conflict_returns_nonzero_and_points_to_resolve() {
    let tmp = TempDir::new("sync-conflict");

    // A bare remote seeded with an initialised store.
    let bare = tmp.path().join("origin.git");
    assert_git_ok(&git(
        tmp.path(),
        &["init", "--bare", bare.to_str().expect("UTF-8 path")],
    ));

    let seed = tmp.path().join("seed");
    fs::create_dir_all(&seed).expect("create seed directory");
    assert_success(&run(&seed, &["init"]));
    fs::write(
        seed.join("events").join("2026-10.jsonl"),
        "{\"event_id\":\"base\"}\n",
    )
    .expect("write seed event log");
    assert_git_ok(&git(&seed, &["init", "--quiet"]));
    assert_git_ok(&git(&seed, &["config", "user.email", "test@acdm.local"]));
    assert_git_ok(&git(&seed, &["config", "user.name", "ACDM Test"]));
    assert_git_ok(&git(&seed, &["add", "-A"]));
    assert_git_ok(&git(&seed, &["commit", "-qm", "seed store"]));
    assert_git_ok(&git(
        &seed,
        &[
            "remote",
            "add",
            "origin",
            bare.to_str().expect("UTF-8 path"),
        ],
    ));
    let branch = git_stdout(&git(&seed, &["rev-parse", "--abbrev-ref", "HEAD"]));
    assert_git_ok(&git(&seed, &["push", "-q", "-u", "origin", &branch]));

    let clone_a = tmp.path().join("clone-a");
    let clone_b = tmp.path().join("clone-b");
    for clone in [&clone_a, &clone_b] {
        assert_git_ok(&git(
            tmp.path(),
            &[
                "clone",
                "-q",
                bare.to_str().expect("UTF-8 path"),
                clone.to_str().expect("UTF-8 path"),
            ],
        ));
        assert_git_ok(&git(clone, &["config", "user.email", "test@acdm.local"]));
        assert_git_ok(&git(clone, &["config", "user.name", "ACDM Test"]));
    }

    // Clone B pushes a conflicting change first.
    fs::write(
        clone_b.join("events").join("2026-10.jsonl"),
        "{\"event_id\":\"remote\"}\n",
    )
    .expect("write clone B event log");
    assert_git_ok(&git(&clone_b, &["add", "-A"]));
    assert_git_ok(&git(&clone_b, &["commit", "-qm", "remote change"]));
    assert_git_ok(&git(&clone_b, &["push", "-q"]));

    // Clone A now diverges: the pull --rebase inside `mem sync` must conflict.
    fs::write(
        clone_a.join("events").join("2026-10.jsonl"),
        "{\"event_id\":\"local\"}\n",
    )
    .expect("write clone A event log");
    assert_git_ok(&git(&clone_a, &["add", "-A"]));
    assert_git_ok(&git(&clone_a, &["commit", "-qm", "local change"]));

    let output = run(&clone_a, &["sync"]);
    assert!(
        !output.status.success(),
        "a conflicted sync must exit non-zero: stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "a conflicted sync must exit with 1"
    );
    let combined = format!("{}\n{}", stdout(&output), stderr(&output));
    assert!(
        combined.contains("mem resolve"),
        "conflict output must point at `mem resolve`: {}",
        combined
    );
    assert!(
        combined.to_lowercase().contains("conflict"),
        "conflict output must mention a conflict: {}",
        combined
    );
    assert!(
        !combined.contains("Sync successful"),
        "a conflicted sync must not claim success: {}",
        combined
    );

    // The repository is intentionally left conflicted for the user to resolve.
    let unmerged = git(&clone_a, &["ls-files", "-u"]);
    assert_git_ok(&unmerged);
    assert!(
        !git_stdout(&unmerged).is_empty(),
        "the conflict should be left staged for `mem resolve`"
    );
}

/// Review finding #9: `mem add` must hash files in a streaming fashion and stay
/// correct for large files.
#[test]
fn add_streams_eight_megabyte_file() {
    let tmp = TempDir::new("large-file");
    let store = tmp.init_store("store");
    let file = store.join("large.bin");
    let payload = vec![0xABu8; 8 * 1024 * 1024];
    fs::write(&file, &payload).expect("write 8 MiB file");

    let output = run(
        &store,
        &["add", "-p", "demo", "-s", "large file", "large.bin"],
    );
    assert_success(&output);

    let records = file_record_lines(&store, "demo");
    assert_eq!(records.len(), 1, "expected one file record");
    assert!(
        records[0].contains("\"action\":\"Created\""),
        "first record must be Created: {}",
        records[0]
    );
    assert!(
        records[0].contains("\"size_bytes\":8388608"),
        "the streamed size must be 8 MiB: {}",
        records[0]
    );

    // SHA-256 of 8 MiB of 0xAB, computed independently with Python's hashlib.
    let expected = "831439a0359856291c8e5d9dff6e683970721e5e76cc7a34df5b82f52dc3d01d";
    assert!(
        records[0].contains(&format!("\"hash\":\"{}\"", expected)),
        "the streaming hash must match the independent SHA-256: {}",
        records[0]
    );
}

/// Second-round review: project names that Windows cannot represent (DOS device
/// names, trailing dot) must be rejected on every platform so a store stays
/// portable across devices.
#[test]
fn add_rejects_windows_reserved_and_trailing_dot_project_names() {
    let tmp = TempDir::new("windows-names");
    let store = tmp.init_store("store");
    fs::write(store.join("tracked.txt"), "content\n").expect("write tracked file");

    for name in ["CON", "nul", "com1", "con.txt", "demo."] {
        let output = run(
            &store,
            &["add", "-p", name, "-s", "must fail", "tracked.txt"],
        );
        assert!(
            !output.status.success(),
            "project name {name:?} must be rejected"
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "project name {name:?} must exit 1"
        );
        let err = stderr(&output);
        assert!(
            err.contains("invalid project name"),
            "error must name the invalid project for {name:?}: {err}"
        );
        assert!(
            err.contains("Windows"),
            "the error for {name:?} must explain the Windows rule: {err}"
        );
    }

    // Rejected names must not create any project directory (not even `demo`).
    let projects = fs::read_dir(store.join("projects")).expect("read projects dir");
    assert_eq!(
        projects.count(),
        0,
        "rejected project names must not create directories"
    );
    assert!(!store.join("projects").join("demo").exists());
}

/// v0.1.2 verifier finding: characters Windows forbids in file names
/// (`<>:"|?*`) must be rejected in project names on every platform, otherwise a
/// synchronized store cannot be checked out on Windows.
#[test]
fn add_rejects_windows_invalid_chars_in_project_names() {
    let tmp = TempDir::new("windows-invalid-chars");
    let store = tmp.init_store("store");
    fs::write(store.join("tracked.txt"), "content\n").expect("write tracked file");

    for name in ["a:b", "a*b", "a?b", "a|b", "a\"b", "a<b", "a>b"] {
        let output = run(
            &store,
            &["add", "-p", name, "-s", "must fail", "tracked.txt"],
        );
        assert!(
            !output.status.success(),
            "project name {name:?} must be rejected"
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "project name {name:?} must exit 1"
        );
        let err = stderr(&output);
        assert!(
            err.contains("invalid project name"),
            "error must name the invalid project for {name:?}: {err}"
        );
        assert!(
            err.contains("Windows"),
            "the error for {name:?} must explain the Windows rule: {err}"
        );
    }

    let projects = fs::read_dir(store.join("projects")).expect("read projects dir");
    assert_eq!(
        projects.count(),
        0,
        "rejected project names must not create directories"
    );
}

/// v0.1.2 verifier finding: `mem add` must reject FIFOs/devices (and other
/// non-regular files) instead of trying to hash them — opening a FIFO would
/// block forever — while regular files keep working.
#[test]
fn add_rejects_non_regular_files() {
    let tmp = TempDir::new("non-regular");
    let store = tmp.init_store("store");
    fs::write(store.join("regular.txt"), "regular content\n").expect("write regular file");

    let events_before = event_lines(&store).len();

    // A directory is not a regular file either and must be rejected everywhere.
    fs::create_dir_all(store.join("some-dir")).expect("create directory");
    let dir_output = run_with_timeout(
        &store,
        &["add", "-p", "demo", "-s", "dir", "-a", "test", "some-dir"],
        Duration::from_secs(10),
    );
    assert!(
        !dir_output.status.success(),
        "adding a directory must fail: stdout:\n{}\nstderr:\n{}",
        stdout(&dir_output),
        stderr(&dir_output)
    );
    assert!(
        stderr(&dir_output).contains("not a regular file"),
        "directory error must explain regular files: {}",
        stderr(&dir_output)
    );
    assert_eq!(
        event_lines(&store).len(),
        events_before,
        "a rejected directory add must not record an event"
    );

    // Unix: a FIFO must be rejected *without being opened*, which used to block.
    #[cfg(unix)]
    {
        let fifo_path = store.join("input.fifo");
        let fifo_created = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if fifo_created {
            let fifo_output = run_with_timeout(
                &store,
                &[
                    "add",
                    "-p",
                    "demo",
                    "-s",
                    "fifo",
                    "-a",
                    "test",
                    "input.fifo",
                ],
                Duration::from_secs(10),
            );
            assert!(
                !fifo_output.status.success(),
                "adding a FIFO must fail: stdout:\n{}\nstderr:\n{}",
                stdout(&fifo_output),
                stderr(&fifo_output)
            );
            assert_eq!(
                fifo_output.status.code(),
                Some(1),
                "a rejected FIFO add must exit with 1"
            );
            assert!(
                stderr(&fifo_output).contains("not a regular file"),
                "FIFO error must explain regular files: {}",
                stderr(&fifo_output)
            );
            assert_eq!(
                event_lines(&store).len(),
                events_before,
                "a rejected FIFO add must not record an event"
            );
        }
        // If `mkfifo` is unavailable, the directory assertion above is the
        // documented fallback: both are non-regular files.
    }

    // The rejection must not be over-broad: a regular file still records an event.
    let ok = run(
        &store,
        &[
            "add",
            "-p",
            "demo",
            "-s",
            "regular",
            "-a",
            "test",
            "regular.txt",
        ],
    );
    assert_success(&ok);
    assert_eq!(
        event_lines(&store).len(),
        events_before + 1,
        "a regular file add must still record exactly one event"
    );
}
