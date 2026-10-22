use crate::models::{Event, FileRecord};
use fs2::FileExt;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveReport {
    pub resolved_files: Vec<PathBuf>,
    pub total_conflicts_resolved: usize,
    pub deduplicated_items: usize,
}

/// Auto-resolve JSONL git conflict markers in all .jsonl files under root.
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

    for file_path in jsonl_files {
        let content = fs::read_to_string(&file_path)?;
        if !content.contains("<<<<<<<") {
            continue;
        }

        let mut conflict_markers_count = 0;
        let mut candidate_lines = Vec::new();

        let reader = BufReader::new(content.as_bytes());
        for line_res in reader.lines() {
            let line = line_res?;
            let trimmed = line.trim();
            if trimmed.starts_with("<<<<<<<") {
                conflict_markers_count += 1;
                continue;
            }
            if trimmed.starts_with("=======")
                || trimmed.starts_with(">>>>>>>")
                || trimmed.starts_with("|||||||")
            {
                continue;
            }
            if !trimmed.is_empty() {
                candidate_lines.push(trimmed.to_string());
            }
        }

        if candidate_lines.is_empty() {
            continue;
        }

        // Detect record type: Event or FileRecord or generic JSON
        let first_line = &candidate_lines[0];

        let (resolved_content, dedup_count) = if serde_json::from_str::<Event>(first_line).is_ok() {
            let mut events_map: BTreeMap<String, Event> = BTreeMap::new();
            let mut total_records = 0;
            for line in &candidate_lines {
                if let Ok(event) = serde_json::from_str::<Event>(line) {
                    total_records += 1;
                    events_map.insert(event.event_id.clone(), event);
                }
            }
            let dedup_count = total_records - events_map.len();
            let mut events: Vec<Event> = events_map.into_values().collect();
            // Sort by timestamp, then event_id
            events.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.event_id.cmp(&b.event_id)));

            let mut out = String::new();
            for ev in events {
                out.push_str(&serde_json::to_string(&ev)?);
                out.push('\n');
            }
            (out, dedup_count)
        } else if serde_json::from_str::<FileRecord>(first_line).is_ok() {
            let mut records_map: BTreeMap<String, FileRecord> = BTreeMap::new();
            let mut total_records = 0;
            for line in &candidate_lines {
                if let Ok(record) = serde_json::from_str::<FileRecord>(line) {
                    total_records += 1;
                    records_map.insert(record.file_id.clone(), record);
                }
            }
            let dedup_count = total_records - records_map.len();
            let mut records: Vec<FileRecord> = records_map.into_values().collect();
            // Sort by file_id (starts with YYYYMMDDHHMMSS)
            records.sort_by(|a, b| a.file_id.cmp(&b.file_id));

            let mut out = String::new();
            for rec in records {
                out.push_str(&serde_json::to_string(&rec)?);
                out.push('\n');
            }
            (out, dedup_count)
        } else {
            // Fallback: generic JSON Values
            let mut val_map: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            let mut total_records = 0;
            for line in &candidate_lines {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    total_records += 1;
                    let id = val
                        .get("event_id")
                        .or_else(|| val.get("file_id"))
                        .or_else(|| val.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| line.clone());
                    val_map.insert(id, val);
                }
            }
            let dedup_count = total_records - val_map.len();
            let mut values: Vec<serde_json::Value> = val_map.into_values().collect();
            values.sort_by(|a, b| {
                let ts_a = a.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                let ts_b = b.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                ts_a.cmp(ts_b)
            });

            let mut out = String::new();
            for val in values {
                out.push_str(&serde_json::to_string(&val)?);
                out.push('\n');
            }
            (out, dedup_count)
        };

        // Write resolved content back with exclusive lock
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&file_path)?;
            file.lock_exclusive()?;
            file.write_all(resolved_content.as_bytes())?;
            file.flush()?;
            file.unlock()?;
        }

        // git add the resolved file
        let rel_path = file_path.strip_prefix(root).unwrap_or(&file_path);
        let status = Command::new("git")
            .current_dir(root)
            .args(["add", rel_path.to_str().unwrap()])
            .output();

        if let Ok(output) = status {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("Warning: git add {:?} returned: {}", rel_path, stderr);
            }
        }

        report.resolved_files.push(file_path);
        report.total_conflicts_resolved += conflict_markers_count;
        report.deduplicated_items += dedup_count;
    }

    // After all files resolved, attempt rebase --continue if rebase is active
    if !report.resolved_files.is_empty() && root.join(".git").exists() {
        let rebase_merge = root.join(".git/rebase-merge");
        let rebase_apply = root.join(".git/rebase-apply");
        if rebase_merge.exists() || rebase_apply.exists() {
            crate::git::git_rebase_continue(root)?;
        } else {
            if let Err(e) = crate::git::git_rebase_continue(root) {
                let msg = e.to_string().to_lowercase();
                if !msg.contains("no rebase in progress") {
                    return Err(e);
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!("acdm_resolve_test_{}_{}", std::process::id(), crate::generate_id());
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

        let resolved = fs::read_to_string(&conflicted_file).unwrap();
        assert!(!resolved.contains("<<<<<<<"));
        assert!(!resolved.contains("======="));
        assert!(!resolved.contains(">>>>>>>"));

        let lines: Vec<&str> = resolved.lines().collect();
        assert_eq!(lines.len(), 3); // eid_1, eid_2 (deduped), eid_3
    }
}
