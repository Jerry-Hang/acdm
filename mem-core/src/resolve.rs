use crate::models::{Event, FileRecord};
use crate::util;
use fs2::FileExt;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveReport {
    pub resolved_files: Vec<PathBuf>,
    pub total_conflicts_resolved: usize,
    pub deduplicated_items: usize,
    /// Lines that were not valid records of this file's type and were therefore
    /// preserved verbatim instead of being dropped or rewritten.
    pub lines_preserved: usize,
}

/// Which record type a conflicted file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordKind {
    Event,
    FileRecord,
    Generic,
    Unknown,
}

/// One record line kept for de-duplication and re-ordering.
struct Pending {
    dedup_key: String,
    sort_ts: String,
    sort_key: String,
    /// The original text of the line, re-emitted unchanged so unknown fields,
    /// key order and formatting all survive conflict resolution.
    raw: String,
}

struct Resolved {
    content: String,
    markers: usize,
    deduplicated: usize,
    preserved: usize,
}

/// Auto-resolve git conflict markers in all `.jsonl` files under `root`.
///
/// Guarantees:
/// * every line that is not a conflict marker is either kept verbatim or
///   de-duplicated as a known record — **nothing is silently discarded**;
/// * lines that cannot be interpreted as the file's record type are preserved
///   verbatim (a truncated final write, a foreign schema, …);
/// * a rebase is only continued when one is actually in progress.
pub fn resolve_conflicts(root: &Path) -> Result<ResolveReport, Box<dyn Error>> {
    let mut report = ResolveReport::default();

    let mut jsonl_files = Vec::new();
    for result in WalkBuilder::new(root).hidden(false).build() {
        let entry = result?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") && path.is_file() {
            jsonl_files.push(path.to_path_buf());
        }
    }
    jsonl_files.sort();

    for file_path in jsonl_files {
        let content = fs::read_to_string(&file_path)?;
        if !content.contains("<<<<<<<") {
            continue;
        }

        let Some(resolved) = resolve_content(&content)? else {
            continue;
        };

        write_resolved(&file_path, &resolved.content)?;

        if root.join(".git").exists() {
            stage_file(root, &file_path)?;
        }

        report.resolved_files.push(file_path);
        report.total_conflicts_resolved += resolved.markers;
        report.deduplicated_items += resolved.deduplicated;
        report.lines_preserved += resolved.preserved;
    }

    continue_rebase_if_active(root)?;

    Ok(report)
}

/// Resolve one file's content. Returns `None` when there is nothing to write.
fn resolve_content(content: &str) -> Result<Option<Resolved>, Box<dyn Error>> {
    let mut markers = 0usize;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<<<<<<<") {
            markers += 1;
            continue;
        }
        if trimmed.starts_with("=======")
            || trimmed.starts_with(">>>>>>>")
            || trimmed.starts_with("|||||||")
        {
            continue;
        }
        if !trimmed.is_empty() {
            // Keep the original line, not the trimmed one: a preserved line must
            // survive byte-for-byte, including any surrounding whitespace.
            data_lines.push(line);
        }
    }

    if data_lines.is_empty() {
        if markers == 0 {
            return Ok(None);
        }
        // Everything was conflict markers: the merged result is empty.
        return Ok(Some(Resolved {
            content: String::new(),
            markers,
            deduplicated: 0,
            preserved: 0,
        }));
    }

    let kind = detect_kind(&data_lines);
    let mut pending: Vec<Pending> = Vec::new();
    let mut preserved: Vec<String> = Vec::new();

    for line in data_lines {
        match kind {
            RecordKind::Event => match serde_json::from_str::<Event>(line) {
                Ok(event) => pending.push(Pending {
                    dedup_key: event.event_id.clone(),
                    sort_ts: event.ts,
                    sort_key: event.event_id.clone(),
                    raw: line.to_string(),
                }),
                // Not an Event: keep the bytes instead of losing them.
                Err(_) => preserved.push(line.to_string()),
            },
            RecordKind::FileRecord => match serde_json::from_str::<FileRecord>(line) {
                Ok(record) => pending.push(Pending {
                    dedup_key: record.file_id.clone(),
                    sort_ts: String::new(),
                    sort_key: record.file_id.clone(),
                    raw: line.to_string(),
                }),
                Err(_) => preserved.push(line.to_string()),
            },
            RecordKind::Generic => match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => {
                    let key = value
                        .get("event_id")
                        .or_else(|| value.get("file_id"))
                        .or_else(|| value.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| line.to_string());
                    let ts = value
                        .get("ts")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    pending.push(Pending {
                        dedup_key: key.clone(),
                        sort_ts: ts,
                        sort_key: key,
                        raw: line.to_string(),
                    });
                }
                Err(_) => preserved.push(line.to_string()),
            },
            RecordKind::Unknown => preserved.push(line.to_string()),
        }
    }

    let total_records = pending.len();
    // Last occurrence wins when the same record appears on both sides.
    let mut unique_by_key: BTreeMap<String, Pending> = BTreeMap::new();
    for item in pending {
        unique_by_key.insert(item.dedup_key.clone(), item);
    }
    let mut unique: Vec<Pending> = unique_by_key.into_values().collect();
    let deduplicated = total_records.saturating_sub(unique.len());

    match kind {
        RecordKind::Event | RecordKind::Generic => unique.sort_by(|a, b| {
            util::ts_cmp(&a.sort_ts, &b.sort_ts).then_with(|| a.sort_key.cmp(&b.sort_key))
        }),
        RecordKind::FileRecord => unique.sort_by(|a, b| a.sort_key.cmp(&b.sort_key)),
        RecordKind::Unknown => {}
    }

    // Identical opaque lines (e.g. the same truncated write on both sides) are
    // collapsed; genuinely different ones are all kept.
    let mut seen = BTreeSet::new();
    let preserved_unique: Vec<String> = preserved
        .into_iter()
        .filter(|line| seen.insert(line.clone()))
        .collect();

    if unique.is_empty() && preserved_unique.is_empty() {
        return Ok(None);
    }

    let mut output = String::new();
    for item in &unique {
        output.push_str(&item.raw);
        output.push('\n');
    }
    for line in &preserved_unique {
        output.push_str(line);
        output.push('\n');
    }

    Ok(Some(Resolved {
        content: output,
        markers,
        deduplicated,
        preserved: preserved_unique.len(),
    }))
}

/// Decide which record type a file holds, based on the first parseable line.
fn detect_kind(lines: &[&str]) -> RecordKind {
    for line in lines {
        if serde_json::from_str::<Event>(line).is_ok() {
            return RecordKind::Event;
        }
        if serde_json::from_str::<FileRecord>(line).is_ok() {
            return RecordKind::FileRecord;
        }
        if serde_json::from_str::<serde_json::Value>(line).is_ok() {
            return RecordKind::Generic;
        }
    }
    RecordKind::Unknown
}

/// Overwrite a file while holding an exclusive lock.
fn write_resolved(path: &Path, content: &str) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.lock_exclusive()?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.unlock()?;
    Ok(())
}

/// `git add` the resolved file (non-UTF-8 paths are handled lossily).
fn stage_file(root: &Path, file_path: &Path) -> Result<(), Box<dyn Error>> {
    let relative = file_path.strip_prefix(root).unwrap_or(file_path);
    let relative = relative.to_string_lossy().into_owned();
    let output = Command::new("git")
        .current_dir(root)
        .args(["add", "--", &relative])
        .output()?;
    if !output.status.success() {
        eprintln!(
            "Warning: git add {} failed: {}",
            relative,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Resume a rebase only when one is actually in progress.
///
/// Detection uses git's own state directories (`.git/rebase-merge`,
/// `.git/rebase-apply`). It deliberately does **not** inspect git's error text:
/// that message is localized, so matching on English ("no rebase in progress")
/// fails on any non-English locale and turned a successful resolution into a
/// hard error.
fn continue_rebase_if_active(root: &Path) -> Result<(), Box<dyn Error>> {
    let git_dir = root.join(".git");
    if !git_dir.is_dir() {
        return Ok(());
    }
    let rebase_active =
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
    if !rebase_active {
        return Ok(());
    }
    if let Err(error) = crate::git::git_rebase_continue(root) {
        let unresolved = unresolved_files(root)?;
        if unresolved.is_empty() {
            return Err(error);
        }
        return Err(format!(
            "git rebase --continue failed: files outside the JSONL event logs are still conflicted: {}. \
             Resolve them manually (edit the file or `git checkout --ours/--theirs <file>`), \
             then run `git add <file>` and `git rebase --continue`.",
            unresolved.join(", ")
        )
        .into());
    }
    Ok(())
}

/// Paths that are still unmerged in the Git index.
fn unresolved_files(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain"])
        .output()?;
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let code = line.get(..2).unwrap_or("");
        if matches!(code, "UU" | "AA" | "UD" | "DU" | "DD" | "AU" | "UA") {
            files.push(line.get(3..).unwrap_or("").trim().to_string());
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "acdm_resolve_test_{}_{}",
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
    fn test_resolve_conflicts_events() {
        let tmp = TestDir::new();
        let events_dir = tmp.path.join("events");
        fs::create_dir_all(&events_dir).unwrap();

        let conflicted_file = events_dir.join("2026-10.jsonl");
        let conflict_content = r#"{"event_id":"eid_1","ts":"2026-10-22T01:00:00Z","device":"dev1","agent":"a1","project":"p1","summary":"s1","files":[]}
<<<<<<< HEAD
{"event_id":"eid_2","ts":"2026-10-22T02:00:00Z","device":"dev1","agent":"a1","project":"p1","summary":"s2","files":[]}
=======
{"event_id":"eid_2","ts":"2026-10-22T02:00:00Z","device":"dev1","agent":"a1","project":"p1","summary":"s2-theirs","files":[]}
{"event_id":"eid_3","ts":"2026-10-22T03:00:00Z","device":"dev2","agent":"a2","project":"p1","summary":"s3","files":[]}
>>>>>>> branch
"#;
        fs::write(&conflicted_file, conflict_content).unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert_eq!(report.resolved_files.len(), 1);
        assert_eq!(report.total_conflicts_resolved, 1);
        assert_eq!(report.deduplicated_items, 1);
        assert_eq!(report.lines_preserved, 0);

        let resolved = fs::read_to_string(&conflicted_file).unwrap();
        assert!(!resolved.contains("<<<<<<<"));
        assert!(!resolved.contains("======="));
        assert!(!resolved.contains(">>>>>>>"));

        let lines: Vec<&str> = resolved.lines().collect();
        assert_eq!(lines.len(), 3); // eid_1, eid_2 (deduped), eid_3
                                    // Last occurrence wins.
        let eid_2: Event = serde_json::from_str(
            lines
                .iter()
                .find(|line| line.contains("eid_2"))
                .expect("eid_2 line"),
        )
        .unwrap();
        assert_eq!(eid_2.summary, "s2-theirs");
    }

    /// Regression test for silent data loss: a line that is not valid JSON (for
    /// example a truncated final write) used to be dropped by the resolver.
    #[test]
    fn test_resolve_preserves_unparseable_lines() {
        let tmp = TestDir::new();
        let events_dir = tmp.path.join("events");
        fs::create_dir_all(&events_dir).unwrap();

        let conflicted_file = events_dir.join("2026-10.jsonl");
        // Leading/trailing whitespace must survive too: preservation is byte-exact.
        let truncated =
            r#"  {"event_id":"eid_4","ts":"2026-10-22T04:00:00Z","summary":"TRUNCATED"   "#;
        let content = format!(
            r#"{{"event_id":"eid_1","ts":"2026-10-22T01:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s1","files":[]}}
<<<<<<< HEAD
{{"event_id":"eid_2","ts":"2026-10-22T02:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s2","files":[]}}
=======
{{"event_id":"eid_3","ts":"2026-10-22T03:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s3","files":[]}}
>>>>>>> branch
{}
"#,
            truncated
        );
        fs::write(&conflicted_file, content).unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert_eq!(report.total_conflicts_resolved, 1);
        assert_eq!(report.lines_preserved, 1);

        let resolved = fs::read_to_string(&conflicted_file).unwrap();
        assert!(!resolved.contains("<<<<<<<"));
        assert!(
            resolved.contains(truncated),
            "unparseable line must be preserved verbatim, got:\n{}",
            resolved
        );

        // Preserved line must not make the store unreadable.
        let store = crate::storage::MemStore::new(tmp.path.clone());
        assert_eq!(store.read_all_events().unwrap().len(), 3);
    }

    /// Regression test for the locale-dependent failure: resolving outside an
    /// active rebase must succeed instead of erroring on git's translated
    /// "no rebase in progress" message.
    #[test]
    fn test_resolve_without_active_rebase_succeeds() {
        let tmp = TestDir::new();
        crate::git::git_init(&tmp.path).unwrap();
        let _ = Command::new("git")
            .current_dir(&tmp.path)
            .args(["config", "user.email", "test@acdm.local"])
            .output();
        let _ = Command::new("git")
            .current_dir(&tmp.path)
            .args(["config", "user.name", "ACDM Test"])
            .output();

        let events_dir = tmp.path.join("events");
        fs::create_dir_all(&events_dir).unwrap();
        let conflicted_file = events_dir.join("2026-10.jsonl");
        fs::write(
            &conflicted_file,
            r#"{"event_id":"eid_1","ts":"2026-10-22T01:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s1","files":[]}
<<<<<<< HEAD
{"event_id":"eid_2","ts":"2026-10-22T02:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s2","files":[]}
=======
{"event_id":"eid_2","ts":"2026-10-22T02:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s2-theirs","files":[]}
>>>>>>> branch
"#,
        )
        .unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert_eq!(report.resolved_files.len(), 1);
        assert!(!fs::read_to_string(&conflicted_file)
            .unwrap()
            .contains("<<<<<<<"));
    }

    /// Unknown fields must survive resolution: re-emitting the original line
    /// means a newer writer's schema is never silently downgraded.
    #[test]
    fn test_resolve_preserves_unknown_fields() {
        let tmp = TestDir::new();
        let events_dir = tmp.path.join("events");
        fs::create_dir_all(&events_dir).unwrap();
        let conflicted_file = events_dir.join("2026-10.jsonl");

        let future_line = r#"{"event_id":"eid_9","ts":"2026-10-22T09:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s9","files":[],"future_field":{"nested":42}}"#;
        let content = format!(
            "{}\n<<<<<<< HEAD\n{}\n=======\n>>>>>>> branch\n",
            future_line, future_line
        );
        fs::write(&conflicted_file, content).unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert_eq!(report.deduplicated_items, 1);
        let resolved = fs::read_to_string(&conflicted_file).unwrap();
        assert!(resolved.contains("\"future_field\""));
        assert_eq!(resolved.lines().count(), 1);
    }

    /// Resolution only covers `*.jsonl`. If another file (e.g. a project's
    /// `meta.json`) is also conflicted, resuming the rebase fails — the error must
    /// name the remaining files instead of leaking a bare Git message.
    #[test]
    fn test_resolve_reports_remaining_non_jsonl_conflicts() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        let git = |args: &[&str]| {
            let _ = Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .expect("run git");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@acdm.local"]);
        git(&["config", "user.name", "ACDM Test"]);

        let events_dir = root.join("events");
        let project_dir = root.join("projects").join("demo");
        fs::create_dir_all(&events_dir).unwrap();
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(root.join(".gitignore"), "").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);

        // Do not assume the default branch is called `master` (it differs per
        // environment); remember the branch the base commit was made on.
        let base_branch = String::from_utf8(
            Command::new("git")
                .current_dir(&root)
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .expect("git rev-parse")
                .stdout,
        )
        .expect("utf8 branch name")
        .trim()
        .to_string();

        // Both branches create the same project and append a different event.
        git(&["checkout", "-qb", "other"]);
        fs::write(
            project_dir.join("meta.json"),
            "{\"name\":\"demo\",\"description\":\"theirs\",\"github_url\":null,\"created_at\":\"2026-10-22T02:00:00.000000000Z\"}\n",
        )
        .unwrap();
        fs::write(
            events_dir.join("2026-10.jsonl"),
            "{\"event_id\":\"eid_other\",\"ts\":\"2026-10-22T02:00:00.000000000Z\",\"device\":\"d\",\"agent\":\"a\",\"project\":\"demo\",\"summary\":\"other\",\"files\":[]}\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "other"]);

        git(&["checkout", "-q", base_branch.as_str()]);
        // Git does not track empty directories: the directories created on the
        // `other` branch disappear again when switching back to `master`.
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(&events_dir).unwrap();
        fs::write(
            project_dir.join("meta.json"),
            "{\"name\":\"demo\",\"description\":\"ours\",\"github_url\":null,\"created_at\":\"2026-10-22T01:00:00.000000000Z\"}\n",
        )
        .unwrap();
        fs::write(
            events_dir.join("2026-10.jsonl"),
            "{\"event_id\":\"eid_main\",\"ts\":\"2026-10-22T01:00:00.000000000Z\",\"device\":\"d\",\"agent\":\"a\",\"project\":\"demo\",\"summary\":\"main\",\"files\":[]}\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "main"]);
        let _ = Command::new("git")
            .current_dir(&root)
            .args(["rebase", "other"])
            .output();
        assert!(root.join(".git").join("rebase-merge").exists());

        let error =
            resolve_conflicts(&root).expect_err("rebase cannot continue with a meta.json conflict");
        let message = error.to_string();
        assert!(
            message.contains("meta.json"),
            "error must name the unresolved file, got: {}",
            message
        );

        // The JSONL conflict itself was resolved and staged.
        let resolved = fs::read_to_string(events_dir.join("2026-10.jsonl")).unwrap();
        assert!(!resolved.contains("<<<<<<<"));

        // Manual resolution unblocks the rebase.
        git(&["checkout", "--theirs", "projects/demo/meta.json"]);
        git(&["add", "projects/demo/meta.json"]);
        crate::git::git_rebase_continue(&root).unwrap();
        assert!(!root.join(".git").join("rebase-merge").exists());
    }

    #[test]
    fn test_resolve_leaves_clean_files_untouched() {
        let tmp = TestDir::new();
        let events_dir = tmp.path.join("events");
        fs::create_dir_all(&events_dir).unwrap();
        let clean = events_dir.join("2026-10.jsonl");
        let body = r#"{"event_id":"eid_1","ts":"2026-10-22T01:00:00Z","device":"d1","agent":"a1","project":"p1","summary":"s1","files":[]}
"#;
        fs::write(&clean, body).unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert!(report.resolved_files.is_empty());
        assert_eq!(fs::read_to_string(&clean).unwrap(), body);
    }
}
