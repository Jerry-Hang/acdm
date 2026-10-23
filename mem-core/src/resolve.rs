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

/// Upper bound on `git rebase --continue` attempts in one resolution run.
///
/// One replayed commit normally needs one round; the bound only exists so that a
/// pathological rebase (or a bug) cannot loop forever.
const MAX_REBASE_ROUNDS: usize = 100;

const CONFLICT_START: &[u8] = b"<<<<<<<";
const CONFLICT_BASE: &[u8] = b"|||||||";
const CONFLICT_SEPARATOR: &[u8] = b"=======";
const CONFLICT_END: &[u8] = b">>>>>>>";

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

/// Kinds of line Git writes inside a conflict block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictMarker {
    /// `<<<<<<<` — start of the "ours" side.
    Start,
    /// `|||||||` — start of the common ancestor (diff3 style).
    Base,
    /// `=======` — separator between "ours" and "theirs".
    Separator,
    /// `>>>>>>>` — end of the "theirs" side.
    End,
}

/// One record line kept for de-duplication and re-ordering.
struct Pending {
    dedup_key: String,
    sort_ts: String,
    sort_key: String,
    /// The original bytes of the line, re-emitted unchanged so unknown fields,
    /// key order, formatting and non-UTF-8 bytes all survive conflict resolution.
    raw: Vec<u8>,
}

struct Resolved {
    content: Vec<u8>,
    markers: usize,
    deduplicated: usize,
    preserved: usize,
}

/// Auto-resolve git conflict markers in all `.jsonl` files under `root`.
///
/// The whole operation holds the store-wide write lock, so it cannot interleave
/// with an append or a projection rebuild.
///
/// When a rebase is in progress, resolution is repeated until the rebase either
/// finishes or stops on something that is not a JSONL event log: a rebase that
/// replays several conflicting commits needs one resolve → `git add` →
/// `git rebase --continue` round per commit, all within this single call.
///
/// Guarantees:
/// * every line that is not a Git conflict marker is either kept verbatim or
///   de-duplicated as a known record — **nothing is silently discarded**;
/// * lines that cannot be interpreted as the file's record type are preserved
///   verbatim (a truncated write, a foreign schema, invalid UTF-8 bytes, …);
/// * only exact Git marker shapes are treated as markers; a data line that merely
///   starts with marker-like characters is preserved.
pub fn resolve_conflicts(root: &Path) -> Result<ResolveReport, Box<dyn Error>> {
    let _write_lock = crate::storage::lock_store(root)?;
    let mut report = ResolveReport::default();
    let mut rebase_rounds = 0usize;

    loop {
        let resolved_this_round = resolve_conflicted_jsonl_files(root, &mut report)?;

        if !rebase_active(root) {
            report.resolved_files.sort();
            report.resolved_files.dedup();
            return Ok(report);
        }

        if rebase_rounds >= MAX_REBASE_ROUNDS {
            let unresolved = unresolved_files(root)?;
            let remaining = if unresolved.is_empty() {
                "unknown files".to_string()
            } else {
                unresolved.join(", ")
            };
            return Err(format!(
                "git rebase did not complete after {} resolution rounds; these files are still \
                 conflicted: {}. Resolve them manually, stage them with `git add <file>`, then run \
                 `git rebase --continue`.",
                MAX_REBASE_ROUNDS, remaining
            )
            .into());
        }
        rebase_rounds += 1;

        match crate::git::git_rebase_continue(root) {
            Ok(()) => continue,
            Err(error) => {
                let unresolved = unresolved_files(root)?;
                if unresolved.is_empty() {
                    // A replayed commit can become an empty patch after
                    // de-duplication: every record it added already exists
                    // upstream, so the resolved content equals HEAD. Git refuses
                    // to commit that ("No changes - did you forget to use
                    // `git add`?") and the commit must be dropped with
                    // `git rebase --skip`.
                    //
                    // Only a round that really resolved JSONL conflicts may skip:
                    // that is the evidence that the empty patch was caused by
                    // this resolver's de-duplication. If nothing was resolved, the
                    // rebase is paused for an unrelated reason (`--exec`, `edit`, a
                    // failed commit signature, ...) and skipping would silently
                    // drop the user's commit — surface Git's original error
                    // instead.
                    //
                    // The empty-patch signature is also an *index* identical to
                    // HEAD (`git diff --cached --quiet`), not a clean working
                    // tree: unstaged edits to unrelated tracked files (or
                    // untracked scratch files) do not change the replayed patch
                    // and must not disable the recovery.
                    if resolved_this_round > 0
                        && rebase_active(root)
                        && crate::git::git_index_clean(root)?
                    {
                        if let Err(skip_error) = crate::git::git_rebase_skip(root) {
                            // `--skip` exits non-zero again when the rebase
                            // immediately stops on the next commit's conflict;
                            // that is progress and the next round resolves it.
                            if unresolved_files(root)?.is_empty() {
                                return Err(skip_error);
                            }
                        }
                        continue;
                    }
                    // Git failed for a reason the resolver cannot act on (a bad
                    // patch, an index problem, ...): surface its own message.
                    return Err(error);
                }
                let all_jsonl = unresolved.iter().all(|file| file.ends_with(".jsonl"));
                if !all_jsonl || resolved_this_round == 0 {
                    return Err(manual_resolution_error(&unresolved, error.as_ref()));
                }
                // Every remaining conflict is a JSONL event log: the next round
                // resolves it and retries the rebase.
            }
        }
    }
}

/// Resolve the conflict markers of every conflicted JSONL file under `root`.
///
/// Returns how many files this round rewrote (zero means the resolver cannot make
/// progress on its own).
fn resolve_conflicted_jsonl_files(
    root: &Path,
    report: &mut ResolveReport,
) -> Result<usize, Box<dyn Error>> {
    let mut jsonl_files = Vec::new();
    for result in WalkBuilder::new(root).hidden(false).build() {
        let entry = result?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") && path.is_file() {
            jsonl_files.push(path.to_path_buf());
        }
    }
    jsonl_files.sort();

    let mut resolved_files = 0;
    let is_git_repo = crate::git::git_is_repo(root);
    for file_path in jsonl_files {
        let raw = fs::read(&file_path)?;
        if !contains_conflict_start(&raw) {
            continue;
        }

        let Some(resolved) = resolve_content(&raw)? else {
            continue;
        };

        write_resolved(&file_path, &resolved.content)?;

        if is_git_repo {
            stage_file(root, &file_path)?;
        }

        report.resolved_files.push(file_path);
        report.total_conflicts_resolved += resolved.markers;
        report.deduplicated_items += resolved.deduplicated;
        report.lines_preserved += resolved.preserved;
        resolved_files += 1;
    }

    Ok(resolved_files)
}

/// Whether the raw bytes contain a potential conflict marker.
fn contains_conflict_start(data: &[u8]) -> bool {
    data.windows(CONFLICT_START.len())
        .any(|window| window == CONFLICT_START)
}

/// Resolve one file's raw bytes. Returns `None` when there is nothing to write.
fn resolve_content(data: &[u8]) -> Result<Option<Resolved>, Box<dyn Error>> {
    let mut markers = 0usize;
    let mut data_lines: Vec<&[u8]> = Vec::new();

    for line in data.split(|&byte| byte == b'\n') {
        match conflict_marker(line) {
            Some(ConflictMarker::Start) => markers += 1,
            Some(_) => {}
            None => {
                // Blank lines are separators, not data; everything else must be
                // either parsed or preserved verbatim.
                if !is_blank(line) {
                    data_lines.push(line);
                }
            }
        }
    }

    if markers == 0 {
        // Marker-like bytes inside data lines are not a conflict: do not rewrite.
        return Ok(None);
    }

    if data_lines.is_empty() {
        // Everything was conflict markers: the merged result is empty.
        return Ok(Some(Resolved {
            content: Vec::new(),
            markers,
            deduplicated: 0,
            preserved: 0,
        }));
    }

    let kind = detect_kind(&data_lines);
    let mut pending: Vec<Pending> = Vec::new();
    let mut preserved: Vec<Vec<u8>> = Vec::new();

    for line in data_lines {
        let Some(text) = std::str::from_utf8(line).ok() else {
            // Invalid UTF-8 can never be a record: keep the bytes untouched.
            preserved.push(line.to_vec());
            continue;
        };
        match kind {
            RecordKind::Event => match serde_json::from_str::<Event>(text) {
                Ok(event) => pending.push(Pending {
                    dedup_key: event.event_id.clone(),
                    sort_ts: event.ts,
                    sort_key: event.event_id.clone(),
                    raw: line.to_vec(),
                }),
                // Not an Event: keep the bytes instead of losing them.
                Err(_) => preserved.push(line.to_vec()),
            },
            RecordKind::FileRecord => match serde_json::from_str::<FileRecord>(text) {
                Ok(record) => pending.push(Pending {
                    dedup_key: record.file_id.clone(),
                    sort_ts: String::new(),
                    sort_key: record.file_id.clone(),
                    raw: line.to_vec(),
                }),
                Err(_) => preserved.push(line.to_vec()),
            },
            RecordKind::Generic => match serde_json::from_str::<serde_json::Value>(text) {
                Ok(value) => {
                    let key = value
                        .get("event_id")
                        .or_else(|| value.get("file_id"))
                        .or_else(|| value.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| text.to_string());
                    let ts = value
                        .get("ts")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    pending.push(Pending {
                        dedup_key: key.clone(),
                        sort_ts: ts,
                        sort_key: key,
                        raw: line.to_vec(),
                    });
                }
                Err(_) => preserved.push(line.to_vec()),
            },
            RecordKind::Unknown => preserved.push(line.to_vec()),
        }
    }

    let total_records = pending.len();
    let unique = deduplicate(pending, kind);
    let deduplicated = total_records.saturating_sub(unique.len());

    // Identical opaque lines (e.g. the same truncated write on both sides) are
    // collapsed; genuinely different ones are all kept.
    let mut seen = BTreeSet::new();
    let preserved_unique: Vec<Vec<u8>> = preserved
        .into_iter()
        .filter(|line| seen.insert(line.clone()))
        .collect();

    let mut content = Vec::new();
    for item in &unique {
        content.extend_from_slice(&item.raw);
        content.push(b'\n');
    }
    for line in &preserved_unique {
        content.extend_from_slice(line);
        content.push(b'\n');
    }

    Ok(Some(Resolved {
        content,
        markers,
        deduplicated,
        preserved: preserved_unique.len(),
    }))
}

/// Drop duplicate records, keeping the last occurrence.
///
/// `Event`s (and generic JSON records) are re-ordered by timestamp, as before.
/// `FileRecord`s must stay in their original **causal order**: sorting them by
/// `file_id` scrambles history, so `get_latest_files`/`compact` (last write wins)
/// would then pick an older state. Their order is preserved by keeping only the
/// last occurrence of every key at the position where that occurrence appeared.
fn deduplicate(pending: Vec<Pending>, kind: RecordKind) -> Vec<Pending> {
    match kind {
        RecordKind::Event | RecordKind::Generic => {
            let mut unique_by_key: BTreeMap<String, Pending> = BTreeMap::new();
            for item in pending {
                unique_by_key.insert(item.dedup_key.clone(), item);
            }
            let mut unique: Vec<Pending> = unique_by_key.into_values().collect();
            unique.sort_by(|a, b| {
                util::ts_cmp(&a.sort_ts, &b.sort_ts).then_with(|| a.sort_key.cmp(&b.sort_key))
            });
            unique
        }
        RecordKind::FileRecord => {
            let mut last_occurrence: BTreeMap<String, usize> = BTreeMap::new();
            for (index, item) in pending.iter().enumerate() {
                last_occurrence.insert(item.dedup_key.clone(), index);
            }
            pending
                .into_iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    (last_occurrence.get(&item.dedup_key) == Some(&index)).then_some(item)
                })
                .collect()
        }
        RecordKind::Unknown => Vec::new(),
    }
}

/// Decide which record type a file holds, based on the first parseable line.
fn detect_kind(lines: &[&[u8]]) -> RecordKind {
    for line in lines {
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        if serde_json::from_str::<Event>(text).is_ok() {
            return RecordKind::Event;
        }
        if serde_json::from_str::<FileRecord>(text).is_ok() {
            return RecordKind::FileRecord;
        }
        if serde_json::from_str::<serde_json::Value>(text).is_ok() {
            return RecordKind::Generic;
        }
    }
    RecordKind::Unknown
}

/// Classify a raw line as a Git conflict marker, if it is one.
///
/// Git writes exactly these shapes (a marker is never followed by arbitrary
/// data):
/// * `<<<<<<<`, `|||||||` and `>>>>>>>`, optionally followed by a space and a
///   label such as `HEAD`;
/// * a `=======` separator line that is exactly seven `=` characters.
///
/// Matching anything broader drops data: a truncated JSON line such as
/// `======= broken line` is not a marker and must be preserved.
fn conflict_marker(line: &[u8]) -> Option<ConflictMarker> {
    // A CRLF checkout writes `\r\n`; the `\r` belongs to the line ending, not to
    // the marker, so it is stripped before classification.
    let line = line.strip_suffix(b"\r").unwrap_or(line);

    if line == CONFLICT_SEPARATOR {
        return Some(ConflictMarker::Separator);
    }

    for (prefix, marker) in [
        (CONFLICT_START, ConflictMarker::Start),
        (CONFLICT_BASE, ConflictMarker::Base),
        (CONFLICT_END, ConflictMarker::End),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return match rest.first() {
                // Exactly seven marker characters: no label.
                None => Some(marker),
                // Seven marker characters followed by a space and a label.
                Some(b' ') => Some(marker),
                // Marker-like prefix only: this is data, not a marker.
                Some(_) => None,
            };
        }
    }

    None
}

/// Whether a line contains only ASCII whitespace (blank separator line).
fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|byte| byte.is_ascii_whitespace())
}

/// Overwrite a file while holding an exclusive lock.
fn write_resolved(path: &Path, content: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.lock_exclusive()?;
    file.write_all(content)?;
    file.flush()?;
    file.unlock()?;
    Ok(())
}

/// `git add` the resolved file (non-UTF-8 paths are handled lossily).
///
/// A failed `git add` is a hard error: during a rebase the following
/// `git rebase --continue` can never succeed, and a warning would hide the real
/// cause (e.g. an index lock, a filter failure or a deleted file).
fn stage_file(root: &Path, file_path: &Path) -> Result<(), Box<dyn Error>> {
    let relative = file_path.strip_prefix(root).unwrap_or(file_path);
    let relative = relative.to_string_lossy().into_owned();
    let output = Command::new("git")
        .current_dir(root)
        .args(["add", "--", &relative])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git add failed for {}: {}",
            relative,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

/// Whether a rebase is actually in progress.
///
/// Detection uses git's own state paths (`rebase-merge`, `rebase-apply`) and
/// deliberately does **not** inspect git's error text: that message is
/// localized, so matching on English ("no rebase in progress") fails on any
/// non-English locale and turned a successful resolution into a hard error.
///
/// The paths are resolved through `git rev-parse --git-path` rather than
/// `root.join(".git")`: in a linked worktree or a submodule `.git` is a *file*,
/// and the rebase state lives in the per-worktree git directory.
fn rebase_active(root: &Path) -> bool {
    ["rebase-merge", "rebase-apply"]
        .into_iter()
        .any(|state| crate::git::git_path(root, state).is_ok_and(|path| path.exists()))
}

/// Paths that are still unmerged in the Git index.
///
/// Uses `git diff --name-only --diff-filter=U -z`: NUL-separated output contains
/// the raw paths, with no status-code prefix and no quoting/escaping. Parsing
/// `git status --porcelain` instead broke on paths with spaces or unusual
/// characters, because Git quotes those (e.g. `UU "events/with space.jsonl"`),
/// which also defeated the `.jsonl` suffix check.
fn unresolved_files(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-only", "--diff-filter=U", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git diff --name-only --diff-filter=U -z failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    let mut files = Vec::new();
    for raw in output.stdout.split(|&byte| byte == 0) {
        if raw.is_empty() {
            continue;
        }
        // Repository paths are UTF-8 in practice; a lossy conversion keeps a
        // non-UTF-8 path reportable instead of hiding it.
        files.push(String::from_utf8_lossy(raw).into_owned());
    }
    Ok(files)
}

/// Build the error shown when a rebase cannot be completed automatically.
///
/// The message must be accurate: a still-conflicted `.jsonl` file is *not*
/// "outside the JSONL event logs"; only the non-JSONL files genuinely require
/// manual work.
fn manual_resolution_error(unresolved: &[String], underlying: &dyn Error) -> Box<dyn Error> {
    let non_jsonl: Vec<&str> = unresolved
        .iter()
        .filter(|file| !file.ends_with(".jsonl"))
        .map(String::as_str)
        .collect();

    if non_jsonl.is_empty() {
        format!(
            "git rebase --continue failed and the JSONL event logs could not be resolved \
             automatically (still conflicted: {}). Resolve them manually, stage them with \
             `git add <file>`, then run `git rebase --continue`. Underlying git error: {}",
            unresolved.join(", "),
            underlying
        )
        .into()
    } else {
        format!(
            "git rebase --continue failed: files that are not JSONL event logs are still \
             conflicted: {}. These files cannot be resolved automatically; edit them (or run \
             `git checkout --ours/--theirs <file>`), then run `git add <file>` and \
             `git rebase --continue`. All still-conflicted files: {}. Underlying git error: {}",
            non_jsonl.join(", "),
            unresolved.join(", "),
            underlying
        )
        .into()
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

    fn event_json(event_id: &str, ts: &str, summary: &str) -> String {
        format!(
            r#"{{"event_id":"{}","ts":"{}","device":"test","agent":"test","project":"demo","summary":"{}","files":[]}}"#,
            event_id, ts, summary
        )
    }

    fn file_record_json(file_id: &str, file_name: &str, action: &str, size_bytes: u64) -> String {
        format!(
            r#"{{"file_id":"{}","file_name":"{}","action":"{}","size_bytes":{},"hash":"hash"}}"#,
            file_id, file_name, action, size_bytes
        )
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git")
    }

    fn git_ok(dir: &Path, args: &[&str]) -> std::process::Output {
        let output = git(dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
            args,
            dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn init_repo(dir: &Path) {
        git_ok(dir, &["init", "-q"]);
        configure_repo(dir);
    }

    /// Pin the repository settings the tests depend on, so that a developer's
    /// global Git configuration cannot change the conflict layout (diff3 style) or
    /// block commits (commit signing).
    fn configure_repo(dir: &Path) {
        git_ok(dir, &["config", "user.email", "test@acdm.local"]);
        git_ok(dir, &["config", "user.name", "ACDM Test"]);
        git_ok(dir, &["config", "merge.conflictStyle", "merge"]);
        git_ok(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn current_branch(dir: &Path) -> String {
        let output = git_ok(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        String::from_utf8(output.stdout)
            .expect("utf8 branch name")
            .trim()
            .to_string()
    }

    fn commit_file(dir: &Path, relative: &str, content: &str, message: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content.as_bytes()).unwrap();
        git_ok(dir, &["add", "-A"]);
        git_ok(dir, &["commit", "-qm", message]);
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

    /// Regression for the P0 finding: only exact Git marker shapes are markers.
    /// A data line that merely *starts* with seven `=` characters (here a broken
    /// line ending in CRLF) must be preserved verbatim.
    #[test]
    fn test_resolve_preserves_lines_starting_with_marker_like_prefix() {
        let tmp = TestDir::new();
        let events_dir = tmp.path.join("events");
        fs::create_dir_all(&events_dir).unwrap();
        let conflicted_file = events_dir.join("2026-10.jsonl");

        let broken = "======= broken line that starts with equals";
        let content = format!(
            "{}\r\n<<<<<<< HEAD\r\n{}\r\n=======\r\n{}\r\n>>>>>>> branch\r\n{}\r\n",
            event_json("eid_1", "2026-10-22T01:00:00.000000000Z", "one"),
            event_json("eid_2", "2026-10-22T02:00:00.000000000Z", "ours"),
            event_json("eid_2", "2026-10-22T03:00:00.000000000Z", "theirs"),
            broken,
        );
        fs::write(&conflicted_file, content.as_bytes()).unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert_eq!(report.total_conflicts_resolved, 1);
        assert_eq!(
            report.lines_preserved, 1,
            "marker-like data line must be preserved, not dropped as a marker"
        );

        let resolved = fs::read(&conflicted_file).unwrap();
        let text = String::from_utf8(resolved.clone()).expect("resolved output is valid UTF-8");
        assert!(
            !text.contains("<<<<<<<"),
            "conflict start left behind:\n{}",
            text
        );
        assert!(
            !text.contains(">>>>>>>"),
            "conflict end left behind:\n{}",
            text
        );
        assert!(
            !text.lines().any(|line| line == "======="),
            "real separator left behind:\n{}",
            text
        );

        // Preserved byte for byte, including its CRLF line ending.
        let mut needle = broken.as_bytes().to_vec();
        needle.extend_from_slice(b"\r\n");
        assert!(
            resolved
                .windows(needle.len())
                .any(|window| window == needle.as_slice()),
            "marker-like data line must survive verbatim, got:\n{}",
            text
        );
        assert!(
            text.contains("theirs"),
            "last occurrence must win:\n{}",
            text
        );
    }

    /// Regression for the non-UTF-8 finding: resolution is byte-level, so a line
    /// with invalid UTF-8 survives verbatim instead of making the file unreadable
    /// or silently disappearing.
    #[test]
    fn test_resolve_preserves_non_utf8_bytes() {
        let tmp = TestDir::new();
        let events_dir = tmp.path.join("events");
        fs::create_dir_all(&events_dir).unwrap();
        let conflicted_file = events_dir.join("2026-10.jsonl");

        let invalid_line: &[u8] = b"raw non-utf8 bytes: \xFF\xFE";
        let mut content = Vec::new();
        content.extend_from_slice(
            event_json("eid_1", "2026-10-22T01:00:00.000000000Z", "one").as_bytes(),
        );
        content.extend_from_slice(b"\r\n<<<<<<< HEAD\r\n");
        content.extend_from_slice(
            event_json("eid_2", "2026-10-22T02:00:00.000000000Z", "ours").as_bytes(),
        );
        content.extend_from_slice(b"\r\n=======\r\n");
        content.extend_from_slice(
            event_json("eid_2", "2026-10-22T03:00:00.000000000Z", "theirs").as_bytes(),
        );
        content.extend_from_slice(b"\r\n>>>>>>> branch\r\n");
        content.extend_from_slice(invalid_line);
        content.extend_from_slice(b"\r\n");
        fs::write(&conflicted_file, &content).unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert_eq!(report.total_conflicts_resolved, 1);
        assert_eq!(
            report.lines_preserved, 1,
            "invalid UTF-8 line must be preserved"
        );

        let resolved = fs::read(&conflicted_file).unwrap();
        let mut needle = invalid_line.to_vec();
        needle.extend_from_slice(b"\r\n");
        assert!(
            resolved
                .windows(needle.len())
                .any(|window| window == needle.as_slice()),
            "non-UTF-8 line must survive byte for byte"
        );

        let text = String::from_utf8_lossy(&resolved);
        assert!(
            text.contains("theirs"),
            "last occurrence must win:\n{}",
            text
        );
        assert!(!text.contains("<<<<<<<"), "conflict start left behind");

        // The valid records remain readable.
        let store = crate::storage::MemStore::new(tmp.path.clone());
        assert_eq!(store.read_events_for_month("2026-10").unwrap().len(), 2);
    }

    /// Regression test for the locale-dependent failure: resolving outside an
    /// active rebase must succeed instead of erroring on git's translated
    /// "no rebase in progress" message.
    #[test]
    fn test_resolve_without_active_rebase_succeeds() {
        let tmp = TestDir::new();
        init_repo(&tmp.path);

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

    /// `FileRecord` de-duplication must preserve causal order: sorting by
    /// `file_id` would put the `Created` record last when its ID sorts higher,
    /// making `get_latest_files` report an older state.
    #[test]
    fn test_resolve_file_records_keep_causal_order() {
        let tmp = TestDir::new();
        let project_dir = tmp.path.join("projects").join("demo");
        fs::create_dir_all(&project_dir).unwrap();
        let files_path = project_dir.join("files.jsonl");

        // Causal order is Created → Modified, but the IDs sort the other way
        // round (e.g. clock skew between two devices). A by-ID sort would flip it.
        let created = file_record_json(
            "20261022999999_000000000_00000_0000",
            "src/main.rs",
            "Created",
            10,
        );
        let modified = file_record_json(
            "20261022000000_000000000_00000_0000",
            "src/main.rs",
            "Modified",
            20,
        );
        let content =
            format!("{created}\n<<<<<<< HEAD\n{modified}\n=======\n{modified}\n>>>>>>> branch\n");
        fs::write(&files_path, content.as_bytes()).unwrap();

        let report = resolve_conflicts(&tmp.path).unwrap();
        assert_eq!(report.total_conflicts_resolved, 1);
        assert_eq!(report.deduplicated_items, 1);

        // On disk the causal order survives.
        let resolved = fs::read_to_string(&files_path).unwrap();
        let records: Vec<FileRecord> = resolved
            .lines()
            .map(|line| serde_json::from_str(line).expect("resolved line is a file record"))
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].action, "Created");
        assert_eq!(records[1].action, "Modified");

        // And last-write-wins sees the Modified state.
        let store = crate::storage::MemStore::new(tmp.path.clone());
        let latest = store.get_latest_files("demo").unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(
            latest[0].action, "Modified",
            "causal order must be preserved, otherwise get_latest_files returns the Created state"
        );
        assert_eq!(latest[0].size_bytes, 20);
    }

    /// Resolution only covers `*.jsonl`. If another file (e.g. a project's
    /// `meta.json`) is also conflicted, resuming the rebase fails — the error must
    /// name the remaining files instead of leaking a bare Git message, and it must
    /// not claim that still-conflicted JSONL files are "outside" the event logs.
    #[test]
    fn test_resolve_reports_remaining_non_jsonl_conflicts() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        init_repo(&root);

        let events_dir = root.join("events");
        let project_dir = root.join("projects").join("demo");
        fs::create_dir_all(&events_dir).unwrap();
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(root.join(".gitignore"), "").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "base"]);

        // Do not assume the default branch is called `master` (it differs per
        // environment); remember the branch the base commit was made on.
        let base_branch = current_branch(&root);

        // Both branches create the same project and append a different event.
        git_ok(&root, &["checkout", "-qb", "other"]);
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
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "other"]);

        git_ok(&root, &["checkout", "-q", base_branch.as_str()]);
        // Git does not track empty directories: the directories created on the
        // `other` branch disappear again when switching back to the base branch.
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
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "main"]);
        let _ = git(&root, &["rebase", "other"]);
        assert!(root.join(".git").join("rebase-merge").exists());

        let error =
            resolve_conflicts(&root).expect_err("rebase cannot continue with a meta.json conflict");
        let message = error.to_string();
        assert!(
            message.contains("meta.json"),
            "error must name the unresolved file, got: {}",
            message
        );
        assert!(
            message.contains("not JSONL event logs"),
            "error must not mislabel non-JSONL files, got: {}",
            message
        );
        assert!(
            !message.contains("outside the JSONL event logs"),
            "error must not claim still-conflicted files are outside the event logs, got: {}",
            message
        );

        // The JSONL conflict itself was resolved and staged.
        let resolved = fs::read_to_string(events_dir.join("2026-10.jsonl")).unwrap();
        assert!(!resolved.contains("<<<<<<<"));

        // Manual resolution unblocks the rebase.
        git_ok(&root, &["checkout", "--theirs", "projects/demo/meta.json"]);
        git_ok(&root, &["add", "projects/demo/meta.json"]);
        crate::git::git_rebase_continue(&root).unwrap();
        assert!(!root.join(".git").join("rebase-merge").exists());
    }

    /// Regression for the multi-commit rebase finding: a rebase that replays
    /// several conflicting commits used to run `git rebase --continue` only once
    /// and stop halfway. One `resolve_conflicts` call must now drive the whole
    /// rebase to the end.
    #[test]
    fn test_resolve_multi_commit_rebase_completes_all_steps() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        let event_file = "events/2026-10.jsonl";

        // A bare remote seeded with one base commit.
        let bare = root.join("remote.git");
        git_ok(&root, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        init_repo(&seed);
        commit_file(
            &seed,
            event_file,
            &format!(
                "{}\n",
                event_json("eid_base", "2026-10-22T00:00:00.000000000Z", "base")
            ),
            "base",
        );
        let branch = current_branch(&seed);
        git_ok(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git_ok(&seed, &["push", "-q", "-u", "origin", &branch]);
        git_ok(
            &bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)],
        );

        // The local clone starts from the base and makes three consecutive commits
        // that all rewrite the same event line: every replay step will conflict.
        let local = root.join("local");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        configure_repo(&local);
        for (event_id, hour) in [("eid_b1", "02"), ("eid_b2", "03"), ("eid_b3", "04")] {
            let ts = format!("2026-10-22T{}:00:00.000000000Z", hour);
            commit_file(
                &local,
                event_file,
                &format!("{}\n", event_json(event_id, &ts, "local change")),
                &format!("local commit {}", event_id),
            );
        }

        // Meanwhile a second clone pushes one commit that rewrites the same line.
        let remote_writer = root.join("remote-writer");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                remote_writer.to_str().unwrap(),
            ],
        );
        configure_repo(&remote_writer);
        commit_file(
            &remote_writer,
            event_file,
            &format!(
                "{}\n",
                event_json("eid_a1", "2026-10-22T01:00:00.000000000Z", "remote change")
            ),
            "remote commit",
        );
        git_ok(&remote_writer, &["push", "-q"]);

        // Divergent history: the pull stops at the first replayed commit.
        let pull = git(&local, &["pull", "--rebase"]);
        assert!(
            !pull.status.success(),
            "pull --rebase must stop on a conflict:\n{}",
            String::from_utf8_lossy(&pull.stdout)
        );
        assert!(
            local.join(".git").join("rebase-merge").exists()
                || local.join(".git").join("rebase-apply").exists(),
            "expected an active rebase after the conflicting pull"
        );
        let conflicted = fs::read_to_string(local.join(event_file)).unwrap();
        assert!(
            conflicted.contains("<<<<<<<"),
            "expected conflict markers, got:\n{}",
            conflicted
        );

        // A single call must resolve the conflicts of every replayed commit.
        let report =
            resolve_conflicts(&local).expect("resolve must complete the whole multi-commit rebase");
        assert_eq!(
            report.total_conflicts_resolved, 3,
            "one conflict per replayed commit, got: {:?}",
            report
        );
        assert!(
            !local.join(".git").join("rebase-merge").exists()
                && !local.join(".git").join("rebase-apply").exists(),
            "rebase must be finished after resolve_conflicts"
        );

        // All four commits' events survive, in chronological order, without markers.
        let resolved = fs::read_to_string(local.join(event_file)).unwrap();
        assert!(
            !resolved.contains("<<<<<<<") && !resolved.contains(">>>>>>>"),
            "markers left behind:\n{}",
            resolved
        );
        let events: Vec<Event> = resolved
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("resolved line is an event"))
            .collect();
        let ids: Vec<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["eid_a1", "eid_b1", "eid_b2", "eid_b3"],
            "all four commits' events must be preserved: {:?}",
            ids
        );

        // The replayed commits are in the rebased history (base + 4), clean tree.
        let log = git_ok(&local, &["log", "--oneline"]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&log).lines().count(),
            5,
            "expected base + 4 commits:\n{}",
            String::from_utf8_lossy(&log)
        );
        let status = git_ok(&local, &["status", "--porcelain"]).stdout;
        assert!(
            String::from_utf8_lossy(&status).trim().is_empty(),
            "working tree must be clean after the rebase:\n{}",
            String::from_utf8_lossy(&status)
        );
    }

    /// Whether a rebase is in progress, resolved the same worktree-aware way as
    /// the production code.
    fn rebase_state_present(root: &Path) -> bool {
        ["rebase-merge", "rebase-apply"]
            .into_iter()
            .any(|state| crate::git::git_path(root, state).is_ok_and(|path| path.exists()))
    }

    /// Regression for the empty-patch failure: a replayed commit can be reduced to
    /// no changes at all by de-duplication (all of its records already exist
    /// upstream). Git then refuses to commit it with "No changes - did you forget
    /// to use `git add`?", leaving a clean worktree and an active rebase. A single
    /// `resolve_conflicts` call must `git rebase --skip` that commit and continue
    /// with the next replayed commit's conflict.
    #[test]
    fn test_resolve_multi_commit_rebase_with_pure_duplicate_commit() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        let event_file = "events/2026-10.jsonl";
        let base_line = format!(
            "{}\n",
            event_json("eid_a", "2026-10-22T00:00:00.000000000Z", "base")
        );
        let x_line = format!(
            "{}\n",
            event_json("eid_x", "2026-10-22T05:00:00.000000000Z", "x")
        );
        let w_line = format!(
            "{}\n",
            event_json("eid_w", "2026-10-22T06:00:00.000000000Z", "w")
        );

        // A bare remote seeded with one base event.
        let bare = root.join("remote.git");
        git_ok(&root, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        init_repo(&seed);
        commit_file(&seed, event_file, &base_line, "base");
        let branch = current_branch(&seed);
        git_ok(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git_ok(&seed, &["push", "-q", "-u", "origin", &branch]);
        git_ok(
            &bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)],
        );

        // The local clone commits event X twice, then a commit that conflicts.
        let local = root.join("local");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        configure_repo(&local);
        // The apply backend still stops on an empty patch after conflict
        // resolution (newer merge backends may drop it by themselves), so pinning
        // it here exercises the `git rebase --skip` recovery deterministically.
        git_ok(&local, &["config", "rebase.backend", "apply"]);
        // Replaying this commit onto the remote change (which already contains X)
        // conflicts; de-duplication collapses the result back to the upstream
        // content, so the commit becomes an empty patch.
        commit_file(
            &local,
            event_file,
            &format!("{base_line}{x_line}{x_line}"),
            "local duplicate",
        );
        // A normal conflicting commit after the empty one, so the rebase becomes
        // multi-step: skip first, then resolve a real conflict.
        commit_file(
            &local,
            event_file,
            &format!("{base_line}{x_line}{x_line}{w_line}"),
            "local add",
        );

        // Meanwhile another clone pushes the change that already contains X.
        let writer = root.join("writer");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                writer.to_str().unwrap(),
            ],
        );
        configure_repo(&writer);
        commit_file(
            &writer,
            event_file,
            &format!("{base_line}{x_line}"),
            "remote add",
        );
        git_ok(&writer, &["push", "-q"]);

        // Divergent history: the pull stops on the duplicate commit's conflict.
        let pull = git(&local, &["pull", "--rebase"]);
        assert!(
            !pull.status.success(),
            "pull --rebase must stop on the duplicate commit's conflict:\n{}",
            String::from_utf8_lossy(&pull.stdout)
        );
        assert!(
            rebase_state_present(&local),
            "expected an active rebase after the conflicting pull"
        );
        let conflicted = fs::read_to_string(local.join(event_file)).unwrap();
        assert!(
            conflicted.contains("<<<<<<<"),
            "expected conflict markers, got:\n{}",
            conflicted
        );

        // One call: skip the empty patch, then resolve the next real conflict.
        let report = resolve_conflicts(&local)
            .expect("resolve must handle a rebase containing an empty patch");
        assert_eq!(
            report.total_conflicts_resolved, 2,
            "one conflict per replayed commit, got: {:?}",
            report
        );
        assert!(
            !rebase_state_present(&local),
            "rebase must be finished after resolve_conflicts"
        );

        let resolved = fs::read_to_string(local.join(event_file)).unwrap();
        assert!(
            !resolved.contains("<<<<<<<") && !resolved.contains(">>>>>>>"),
            "markers left behind:\n{}",
            resolved
        );
        let events: Vec<Event> = resolved
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("resolved line is an event"))
            .collect();
        let ids: Vec<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["eid_a", "eid_x", "eid_w"],
            "the duplicate event must neither be lost nor duplicated: {:?}",
            ids
        );

        // The empty commit was skipped, not committed: base + remote + one replayed.
        let log = git_ok(&local, &["log", "--oneline"]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&log).lines().count(),
            3,
            "expected base + remote + one replayed commit:\n{}",
            String::from_utf8_lossy(&log)
        );
        let status = git_ok(&local, &["status", "--porcelain"]).stdout;
        assert!(
            String::from_utf8_lossy(&status).trim().is_empty(),
            "working tree must be clean after the rebase:\n{}",
            String::from_utf8_lossy(&status)
        );
    }

    /// The empty-patch recovery must not depend on untracked files: a local
    /// scratch file in the store (`?? untracked.txt`) made `git status
    /// --porcelain` non-empty, so the empty patch was never skipped and
    /// `resolve_conflicts` gave up. Only tracked changes may decide it.
    #[test]
    fn test_resolve_empty_patch_succeeds_with_untracked_file_present() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        let event_file = "events/2026-10.jsonl";
        let base_line = format!(
            "{}\n",
            event_json("eid_a", "2026-10-22T00:00:00.000000000Z", "base")
        );
        let x_line = format!(
            "{}\n",
            event_json("eid_x", "2026-10-22T05:00:00.000000000Z", "x")
        );
        let w_line = format!(
            "{}\n",
            event_json("eid_w", "2026-10-22T06:00:00.000000000Z", "w")
        );

        // A bare remote seeded with one base event.
        let bare = root.join("remote.git");
        git_ok(&root, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        init_repo(&seed);
        commit_file(&seed, event_file, &base_line, "base");
        let branch = current_branch(&seed);
        git_ok(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git_ok(&seed, &["push", "-q", "-u", "origin", &branch]);
        git_ok(
            &bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)],
        );

        // The same shape as the pure-duplicate scenario: an empty patch first,
        // then a normal conflict.
        let local = root.join("local");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        configure_repo(&local);
        git_ok(&local, &["config", "rebase.backend", "apply"]);
        commit_file(
            &local,
            event_file,
            &format!("{base_line}{x_line}{x_line}"),
            "local duplicate",
        );
        commit_file(
            &local,
            event_file,
            &format!("{base_line}{x_line}{x_line}{w_line}"),
            "local add",
        );
        // A local, untracked scratch file: it makes `git status --porcelain`
        // non-empty while the tracked worktree stays clean.
        let untracked = local.join("untracked.txt");
        fs::write(&untracked, "local scratch\n").unwrap();

        let writer = root.join("writer");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                writer.to_str().unwrap(),
            ],
        );
        configure_repo(&writer);
        commit_file(
            &writer,
            event_file,
            &format!("{base_line}{x_line}"),
            "remote add",
        );
        git_ok(&writer, &["push", "-q"]);

        // Divergent history: the pull stops on the duplicate commit's conflict,
        // with the untracked file still present.
        let pull = git(&local, &["pull", "--rebase"]);
        assert!(
            !pull.status.success(),
            "pull --rebase must stop on the duplicate commit's conflict:\n{}",
            String::from_utf8_lossy(&pull.stdout)
        );
        assert!(
            rebase_state_present(&local),
            "expected an active rebase after the conflicting pull"
        );
        let status = git_ok(&local, &["status", "--porcelain"]).stdout;
        let status = String::from_utf8_lossy(&status);
        assert!(
            status.contains("UU events/2026-10.jsonl"),
            "the pull must leave a conflicted event log:\n{}",
            status
        );
        assert!(
            status.contains("?? untracked.txt"),
            "the untracked file must be present before resolving:\n{}",
            status
        );

        // One call must succeed even though an untracked file is present.
        let report = resolve_conflicts(&local)
            .expect("an untracked file must not stop the empty-patch (rebase --skip) recovery");
        assert_eq!(
            report.total_conflicts_resolved, 2,
            "one conflict per replayed commit, got: {:?}",
            report
        );
        assert!(
            !rebase_state_present(&local),
            "rebase must be finished after resolve_conflicts"
        );

        let resolved = fs::read_to_string(local.join(event_file)).unwrap();
        assert!(
            !resolved.contains("<<<<<<<") && !resolved.contains(">>>>>>>"),
            "markers left behind:\n{}",
            resolved
        );
        let events: Vec<Event> = resolved
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("resolved line is an event"))
            .collect();
        let ids: Vec<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["eid_a", "eid_x", "eid_w"],
            "the duplicate event must neither be lost nor duplicated: {:?}",
            ids
        );

        // The unresolved scratch file must survive untouched and stay untracked.
        assert_eq!(
            fs::read_to_string(&untracked).unwrap(),
            "local scratch\n",
            "the untracked file must not be modified"
        );
        let status = git_ok(&local, &["status", "--porcelain"]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&status).trim(),
            "?? untracked.txt",
            "only the scratch file may remain untracked:\n{}",
            String::from_utf8_lossy(&status)
        );
        let tracked = git(&local, &["ls-files", "--error-unmatch", "untracked.txt"]);
        assert!(
            !tracked.status.success(),
            "the untracked file must not be committed by the rebase"
        );

        let log = git_ok(&local, &["log", "--oneline"]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&log).lines().count(),
            3,
            "expected base + remote + one replayed commit:\n{}",
            String::from_utf8_lossy(&log)
        );
    }

    /// The empty-patch recovery must not depend on *unrelated tracked* changes:
    /// with the `apply` backend a dirty tracked file makes `git status
    /// --porcelain` non-empty, which used to disable the `git rebase --skip`
    /// recovery. Whether a replayed commit is empty is an index-vs-HEAD question
    /// only.
    #[test]
    fn test_resolve_empty_patch_succeeds_with_dirty_unrelated_tracked_file() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        let event_file = "events/2026-10.jsonl";
        let other_file = "other.txt";
        let base_line = format!(
            "{}\n",
            event_json("eid_a", "2026-10-22T00:00:00.000000000Z", "base")
        );
        let x_line = format!(
            "{}\n",
            event_json("eid_x", "2026-10-22T05:00:00.000000000Z", "x")
        );
        let w_line = format!(
            "{}\n",
            event_json("eid_w", "2026-10-22T06:00:00.000000000Z", "w")
        );

        // A bare remote seeded with one base event.
        let bare = root.join("remote.git");
        git_ok(&root, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        init_repo(&seed);
        commit_file(&seed, event_file, &base_line, "base");
        let branch = current_branch(&seed);
        git_ok(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git_ok(&seed, &["push", "-q", "-u", "origin", &branch]);
        git_ok(
            &bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)],
        );

        // The local clone first commits an unrelated tracked file, then the same
        // pure-duplicate + normal-conflict commits as the other empty-patch
        // tests.
        let local = root.join("local");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        configure_repo(&local);
        // The apply backend still stops on an empty patch after conflict
        // resolution (newer merge backends may drop it by themselves), so pinning
        // it here exercises the `git rebase --skip` recovery deterministically.
        git_ok(&local, &["config", "rebase.backend", "apply"]);
        commit_file(&local, other_file, "tracked base\n", "unrelated file");
        commit_file(
            &local,
            event_file,
            &format!("{base_line}{x_line}{x_line}"),
            "local duplicate",
        );
        commit_file(
            &local,
            event_file,
            &format!("{base_line}{x_line}{x_line}{w_line}"),
            "local add",
        );

        // Meanwhile another clone pushes the change that already contains X.
        let writer = root.join("writer");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                writer.to_str().unwrap(),
            ],
        );
        configure_repo(&writer);
        commit_file(
            &writer,
            event_file,
            &format!("{base_line}{x_line}"),
            "remote add",
        );
        git_ok(&writer, &["push", "-q"]);

        // Divergent history: the pull stops on the duplicate commit's conflict.
        let pull = git(&local, &["pull", "--rebase"]);
        assert!(
            !pull.status.success(),
            "pull --rebase must stop on the duplicate commit's conflict:\n{}",
            String::from_utf8_lossy(&pull.stdout)
        );
        assert!(
            rebase_state_present(&local),
            "expected an active rebase after the conflicting pull"
        );

        // Make `other.txt` dirty without staging it: `git status --porcelain`
        // now reports a tracked ` M other.txt` next to the conflicted JSONL,
        // while the index (which decides the empty patch) is untouched.
        fs::write(local.join(other_file), "tracked base\ndirty edit\n").unwrap();
        let status =
            String::from_utf8_lossy(&git_ok(&local, &["status", "--porcelain"]).stdout).to_string();
        assert!(
            status.contains(" M other.txt"),
            "the unrelated tracked file must be dirty but unstaged before resolving:\n{}",
            status
        );
        assert!(
            status.contains("UU events/2026-10.jsonl"),
            "the event log must still be conflicted before resolving:\n{}",
            status
        );

        // A single call must recover: resolve the first real conflict, skip the
        // empty patch that follows (even though `other.txt` is dirty), then
        // resolve the next conflict and finish the rebase.
        let report = resolve_conflicts(&local)
            .expect("a dirty unrelated tracked file must not stop the empty-patch recovery");
        assert_eq!(
            report.total_conflicts_resolved, 2,
            "one conflict per replayed commit, got: {:?}",
            report
        );
        assert!(
            !rebase_state_present(&local),
            "rebase must be finished after resolve_conflicts"
        );

        // The event log is correct: the duplicate is neither lost nor duplicated.
        let resolved = fs::read_to_string(local.join(event_file)).unwrap();
        assert!(
            !resolved.contains("<<<<<<<") && !resolved.contains(">>>>>>>"),
            "markers left behind:\n{}",
            resolved
        );
        let events: Vec<Event> = resolved
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("resolved line is an event"))
            .collect();
        let ids: Vec<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["eid_a", "eid_x", "eid_w"],
            "the duplicate event must neither be lost nor duplicated: {:?}",
            ids
        );

        // The unrelated edit must never be committed. Note: with the `apply`
        // rebase backend Git's own `rebase --skip` resets the worktree before
        // dropping the empty patch, so the *unstaged* edit itself is discarded by
        // Git (verified on Git 2.56) — the resolver's guarantee is that the
        // recovery completes and that no user change is silently committed.
        let committed = git_ok(&local, &["show", &format!("HEAD:{}", other_file)]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&committed),
            "tracked base\n",
            "the dirty edit must not be committed by the rebase"
        );
        let status = String::from_utf8_lossy(&git_ok(&local, &["status", "--porcelain"]).stdout)
            .trim()
            .to_string();
        assert!(
            status.is_empty() || status == " M other.txt",
            "after the recovery the worktree may only contain the unrelated edit: {:?}",
            status
        );

        // History: upstream base + remote change + replayed unrelated commit +
        // one replayed event commit (the duplicate was skipped).
        let log = git_ok(&local, &["log", "--oneline"]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&log).lines().count(),
            4,
            "expected base + remote + unrelated + one replayed commit:\n{}",
            String::from_utf8_lossy(&log)
        );
    }

    /// Regression for a rebase paused for a reason unrelated to JSONL: when the
    /// resolver did not resolve a single conflict in this round, a "clean index +
    /// no unresolved files" state must not trigger `git rebase --skip` — that
    /// would drop the user's commit (e.g. a paused `--exec`, an `edit` stop or a
    /// failed commit signature). Git's original error must be surfaced instead.
    #[test]
    fn test_resolve_does_not_skip_when_no_jsonl_was_resolved_in_round() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        init_repo(&root);

        let notes = root.join("notes.txt");
        fs::write(&notes, "first\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "first"]);
        fs::write(&notes, "second\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "second"]);

        // Pause a rebase on a failing `--exec` step. `--reschedule-failed-exec`
        // keeps the pause: `git rebase --continue` re-runs (and fails) the exec
        // instead of silently resuming past it. Nothing is conflicted and the
        // index is clean — exactly the empty-patch signature.
        let rebase = git(
            &root,
            &[
                "rebase",
                "--exec",
                "false",
                "--reschedule-failed-exec",
                "HEAD~1",
            ],
        );
        assert!(
            !rebase.status.success(),
            "the exec step must stop the rebase:\n{}",
            String::from_utf8_lossy(&rebase.stdout)
        );
        assert!(rebase_state_present(&root), "expected an active rebase");
        assert!(
            unresolved_files(&root).unwrap().is_empty(),
            "the paused rebase must have no unresolved files"
        );
        assert!(
            crate::git::git_index_clean(&root).unwrap(),
            "the paused rebase must have a clean index"
        );
        let head_before = git_ok(&root, &["rev-parse", "HEAD"]).stdout;
        let log_before = git_ok(&root, &["log", "--format=%s"]).stdout;

        let error = resolve_conflicts(&root)
            .expect_err("a rebase paused for an unrelated reason must not be skipped");
        let message = error.to_string();
        assert!(
            message.contains("rebase --continue"),
            "the Git error must be surfaced instead of being replaced by a failed \
             skip attempt, got: {}",
            message
        );
        assert!(
            !message.contains("rebase --skip"),
            "no automatic `git rebase --skip` may be attempted when nothing was \
             resolved this round, got: {}",
            message
        );

        // The paused commit was neither skipped nor rewritten...
        assert_eq!(
            git_ok(&root, &["rev-parse", "HEAD"]).stdout,
            head_before,
            "HEAD must not move"
        );
        assert_eq!(
            git_ok(&root, &["log", "--format=%s"]).stdout,
            log_before,
            "no commit may be dropped"
        );
        assert!(
            String::from_utf8_lossy(&git_ok(&root, &["log", "--format=%s"]).stdout)
                .contains("second"),
            "the paused commit must still be in the history"
        );

        // ...and the rebase stays paused for the user to fix.
        assert!(
            rebase_state_present(&root),
            "the rebase must stay active for the user to fix"
        );
        assert!(unresolved_files(&root).unwrap().is_empty());
    }

    /// `unresolved_files` must return raw paths: Git's porcelain output quotes
    /// paths with spaces (`UU "docs/notes with space.txt"`), which used to keep the
    /// quotes in the error message and defeat the `.jsonl` suffix check.
    #[test]
    fn test_resolve_non_jsonl_conflict_with_space_in_path() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        init_repo(&root);

        let events_dir = root.join("events");
        let docs_dir = root.join("docs");
        fs::create_dir_all(&events_dir).unwrap();
        fs::create_dir_all(&docs_dir).unwrap();
        let space_path = docs_dir.join("notes with space.txt");
        fs::write(root.join(".gitignore"), "").unwrap();
        fs::write(&space_path, "base\n").unwrap();
        fs::write(
            events_dir.join("2026-10.jsonl"),
            format!(
                "{}\n",
                event_json("eid_base", "2026-10-22T00:00:00.000000000Z", "base")
            ),
        )
        .unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "base"]);
        let base_branch = current_branch(&root);

        // Both branches rewrite the same space-containing file and the same event.
        git_ok(&root, &["checkout", "-qb", "other"]);
        fs::write(&space_path, "theirs\n").unwrap();
        fs::write(
            events_dir.join("2026-10.jsonl"),
            format!(
                "{}\n",
                event_json("eid_other", "2026-10-22T02:00:00.000000000Z", "other")
            ),
        )
        .unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "other"]);

        git_ok(&root, &["checkout", "-q", base_branch.as_str()]);
        fs::write(&space_path, "ours\n").unwrap();
        fs::write(
            events_dir.join("2026-10.jsonl"),
            format!(
                "{}\n",
                event_json("eid_main", "2026-10-22T01:00:00.000000000Z", "main")
            ),
        )
        .unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-qm", "main"]);
        let _ = git(&root, &["rebase", "other"]);
        assert!(root.join(".git").join("rebase-merge").exists());

        let error = resolve_conflicts(&root)
            .expect_err("a non-JSONL conflict must be reported as an error");
        let message = error.to_string();
        assert!(
            message.contains("docs/notes with space.txt"),
            "the error must contain the raw path, got: {}",
            message
        );
        assert!(
            !message.contains("\"docs/notes with space.txt\""),
            "the path must not keep Git's status/porcelain quotes: {}",
            message
        );
        assert!(
            message.matches("docs/notes with space.txt").count() >= 2,
            "the raw path must appear in both the non-JSONL list and the complete list: {}",
            message
        );

        // The JSONL conflict itself was resolved and staged.
        let resolved = fs::read_to_string(events_dir.join("2026-10.jsonl")).unwrap();
        assert!(!resolved.contains("<<<<<<<"));
    }

    /// Regression for `.git` being a *file* (linked worktree / submodule): the old
    /// `root.join(".git").is_dir()` check never detected the rebase there, so the
    /// resolved file was never staged and `git rebase --continue` was never run.
    #[test]
    fn test_resolve_works_with_git_worktree_or_gitfile() {
        let tmp = TestDir::new();
        let root = tmp.path.clone();
        let event_file = "events/2026-10.jsonl";

        // A bare remote seeded with one base event.
        let bare = root.join("remote.git");
        git_ok(&root, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        init_repo(&seed);
        commit_file(
            &seed,
            event_file,
            &format!(
                "{}\n",
                event_json("eid_a", "2026-10-22T00:00:00.000000000Z", "base")
            ),
            "base",
        );
        let branch = current_branch(&seed);
        git_ok(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git_ok(&seed, &["push", "-q", "-u", "origin", &branch]);
        git_ok(
            &bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)],
        );

        // A primary checkout plus a linked worktree: `.git` in the linked worktree
        // is a plain file pointing into the primary repository.
        let primary = root.join("primary");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                primary.to_str().unwrap(),
            ],
        );
        configure_repo(&primary);
        let worktree = root.join("worktree");
        git_ok(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        assert!(
            worktree.join(".git").is_file(),
            "a linked worktree must have a .git file, not a directory"
        );
        assert!(!worktree.join(".git").is_dir());
        assert!(
            crate::git::git_is_repo(&worktree),
            "git_is_repo must work when .git is a file"
        );
        let plain_dir = root.join("not-a-repo");
        fs::create_dir_all(&plain_dir).unwrap();
        assert!(!crate::git::git_is_repo(&plain_dir));

        // The worktree's commit rewrites the base event; another clone pushes a
        // conflicting rewrite of the same line.
        commit_file(
            &worktree,
            event_file,
            &format!(
                "{}\n",
                event_json("eid_a", "2026-10-22T02:00:00.000000000Z", "local")
            ),
            "local change",
        );
        let writer = root.join("writer");
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                writer.to_str().unwrap(),
            ],
        );
        configure_repo(&writer);
        commit_file(
            &writer,
            event_file,
            &format!(
                "{}\n",
                event_json("eid_a", "2026-10-22T01:00:00.000000000Z", "remote")
            ),
            "remote change",
        );
        git_ok(&writer, &["push", "-q"]);

        // Rebase the worktree's feature branch onto the fetched upstream tip.
        git_ok(&worktree, &["fetch", "-q", "origin"]);
        let upstream = format!("origin/{}", branch);
        let rebase = git(&worktree, &["rebase", &upstream]);
        assert!(
            !rebase.status.success(),
            "the worktree rebase must stop on the JSONL conflict:\n{}",
            String::from_utf8_lossy(&rebase.stdout)
        );
        assert!(
            rebase_state_present(&worktree),
            "the worktree's rebase must be found through `git rev-parse --git-path`"
        );
        let conflicted = fs::read_to_string(worktree.join(event_file)).unwrap();
        assert!(
            conflicted.contains("<<<<<<<"),
            "expected conflict markers in the worktree, got:\n{}",
            conflicted
        );

        let report =
            resolve_conflicts(&worktree).expect("resolve must handle a .git-file worktree");
        assert_eq!(report.total_conflicts_resolved, 1);
        assert!(
            !rebase_state_present(&worktree),
            "rebase must be finished after resolve_conflicts"
        );

        // `git add` took effect and the replayed commit was created.
        let resolved = fs::read_to_string(worktree.join(event_file)).unwrap();
        assert!(
            !resolved.contains("<<<<<<<") && !resolved.contains(">>>>>>>"),
            "markers left behind:\n{}",
            resolved
        );
        let events: Vec<Event> = resolved
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("resolved line is an event"))
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, "eid_a");
        assert_eq!(
            events[0].summary, "local",
            "last write wins: the replayed side must be kept"
        );

        let log = git_ok(&worktree, &["log", "--oneline"]).stdout;
        assert_eq!(
            String::from_utf8_lossy(&log).lines().count(),
            3,
            "expected base + remote + one replayed commit:\n{}",
            String::from_utf8_lossy(&log)
        );
        let status = git_ok(&worktree, &["status", "--porcelain"]).stdout;
        assert!(
            String::from_utf8_lossy(&status).trim().is_empty(),
            "worktree must be clean after the rebase:\n{}",
            String::from_utf8_lossy(&status)
        );
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
