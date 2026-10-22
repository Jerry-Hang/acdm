//! Shared helpers used by the library, the CLI and the MCP server.
//!
//! These exist so that every entry point agrees on the same three things:
//!
//! 1. **Timestamps** — one canonical, fixed-width format is written, and every
//!    sort is parse-aware so old records written by earlier versions (which used
//!    two different formats) still compare chronologically.
//! 2. **Store root discovery** — the CLI and the MCP server resolve the store in
//!    exactly the same way instead of each inventing its own rule.
//! 3. **File identity** — a path is normalised to a project-relative,
//!    forward-slash form so the same file keeps the same identity on every
//!    device. Recording raw absolute paths would break cross-device matching.

use chrono::{DateTime, SecondsFormat, Utc};
use std::cmp::Ordering;
use std::env;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::models::Event;

/// Marker file that identifies a directory as an ACDM store root.
pub const STORE_MARKER: &str = ".mem";

/// Environment variable that pins the store root explicitly.
pub const STORE_ENV_VAR: &str = "ACDM_STORE";

/// Canonical timestamp written by every entry point: UTC, nanosecond precision,
/// `Z` suffix and *fixed width* (`YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`).
///
/// Fixed width matters: lexicographic order of these strings is chronological
/// order, so plain string comparisons stay correct.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Parse a timestamp written by any ACDM version.
///
/// Accepted forms include `2026-10-22T12:16:21.301566199Z`,
/// `2026-10-22T12:16:21.301566199+00:00` and `2026-10-22T12:16:21Z`.
pub fn parse_ts(ts: &str) -> Option<DateTime<Utc>> {
    let trimmed = ts.trim();
    if trimmed.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Parse-aware timestamp comparison.
///
/// Lexicographic comparison alone is wrong for mixed legacy formats: `"....5Z"`
/// sorts *after* `"....5.301Z"` even though the latter is later. Falling back to
/// string comparison keeps the result deterministic for unparseable values.
pub fn ts_cmp(a: &str, b: &str) -> Ordering {
    match (parse_ts(a), parse_ts(b)) {
        (Some(left), Some(right)) => left.cmp(&right).then_with(|| a.trim().cmp(b.trim())),
        // Unparseable timestamps sort after parseable ones, deterministically.
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.trim().cmp(b.trim()),
    }
}

/// Total order for events: timestamp first, `event_id` as a stable tie-breaker.
pub fn event_cmp(a: &Event, b: &Event) -> Ordering {
    ts_cmp(&a.ts, &b.ts).then_with(|| a.event_id.cmp(&b.event_id))
}

/// Sort events oldest-first.
pub fn sort_events(events: &mut [Event]) {
    events.sort_by(event_cmp);
}

/// Best-effort device name, identical on every platform.
///
/// Tries the `hostname` binary first (Unix/macOS), then the usual environment
/// variables (Windows uses `COMPUTERNAME`), then falls back to `localhost`.
pub fn default_device_name() -> String {
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            if let Ok(raw) = String::from_utf8(output.stdout) {
                let name = raw.trim();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = env::var(key) {
            let name = value.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "localhost".to_string()
}

/// Whether `dir` looks like an ACDM store root.
pub fn is_store_root(dir: &Path) -> bool {
    dir.join(STORE_MARKER).is_file() || dir.join("events").is_dir()
}

/// Walk `start` and its ancestors looking for the nearest store root.
pub fn find_store_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if is_store_root(dir) {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Resolve the store root, using one rule for both the CLI and the MCP server.
///
/// Priority: explicit path → `$ACDM_STORE` → nearest ancestor that is a store
/// root → current working directory.
pub fn resolve_store_root(explicit: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(lexical_absolute(path));
    }
    if let Some(from_env) = env::var_os(STORE_ENV_VAR).filter(|value| !value.is_empty()) {
        return Ok(lexical_absolute(Path::new(&from_env)));
    }
    let cwd = env::current_dir()?;
    Ok(find_store_root(&cwd).unwrap_or(cwd))
}

/// Normalise a user supplied path into a stable, device independent identity.
///
/// The result is relative to the store root when possible, otherwise relative to
/// the enclosing git work tree, otherwise relative to the current directory, and
/// finally the absolute path. Components always use `/` so the same file keeps
/// the same identity on every platform and device.
pub fn normalize_file_name(path: &Path, store_root: &Path) -> String {
    let absolute = lexical_absolute(path);

    let store = lexical_absolute(store_root);
    if let Ok(relative) = absolute.strip_prefix(&store) {
        let name = to_slash(relative);
        if !name.is_empty() {
            return name;
        }
    }

    if let Some(git_root) = find_git_root(&absolute) {
        if let Ok(relative) = absolute.strip_prefix(&git_root) {
            let name = to_slash(relative);
            if !name.is_empty() {
                return name;
            }
        }
    }

    if let Ok(cwd) = env::current_dir() {
        if let Ok(relative) = absolute.strip_prefix(lexical_clean(&cwd)) {
            let name = to_slash(relative);
            if !name.is_empty() {
                return name;
            }
        }
    }

    to_slash(&absolute)
}

/// Find the enclosing git work tree of a (possibly not yet existing) path.
fn find_git_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Make a path absolute against the current working directory, without touching
/// the filesystem (targets may be deleted, so `canonicalize` is not usable).
fn lexical_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return lexical_clean(path);
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    lexical_clean(&cwd.join(path))
}

/// Resolve `.` and `..` lexically.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `pop` returns false at the filesystem root, where `..` is a no-op.
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Render a path using `/` separators on every platform.
fn to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "acdm_util_test_{}_{}",
                std::process::id(),
                crate::generate_id()
            );
            let path = env::temp_dir().join(unique);
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
    fn test_now_rfc3339_is_fixed_width_and_parses() {
        let ts = now_rfc3339();
        assert_eq!(ts.len(), 30, "unexpected timestamp: {}", ts);
        assert!(ts.ends_with('Z'), "timestamp must be UTC: {}", ts);
        assert!(parse_ts(&ts).is_some());
    }

    #[test]
    fn test_parse_ts_accepts_legacy_formats() {
        // Nanoseconds with explicit offset (old CLI format).
        let legacy_cli = "2026-10-22T12:16:21.301566199+00:00";
        // Seconds with Z (old MCP format).
        let legacy_mcp = "2026-10-22T12:16:21Z";
        assert!(parse_ts(legacy_cli).is_some());
        assert!(parse_ts(legacy_mcp).is_some());
        assert!(parse_ts("not-a-timestamp").is_none());
        assert!(parse_ts("").is_none());
    }

    #[test]
    fn test_ts_cmp_is_chronological_across_formats() {
        // Same wall-clock second, different legacy formats: the one with
        // fractional seconds is *later*, and string comparison would get this
        // backwards ('.' < 'Z').
        let whole_second = "2026-10-22T12:16:21Z";
        let with_nanos = "2026-10-22T12:16:21.301566199+00:00";
        assert_eq!(ts_cmp(whole_second, with_nanos), Ordering::Less);
        assert_eq!(ts_cmp(with_nanos, whole_second), Ordering::Greater);
        assert_eq!(ts_cmp(with_nanos, with_nanos), Ordering::Equal);
    }

    #[test]
    fn test_ts_cmp_falls_back_to_string_for_garbage() {
        assert_eq!(ts_cmp("bogus", "bogus"), Ordering::Equal);
        assert_eq!(ts_cmp("aaa", "bbb"), Ordering::Less);
    }

    #[test]
    fn test_find_and_resolve_store_root() {
        let tmp = TestDir::new();
        let root = tmp.path.join("store");
        let nested = root.join("projects").join("demo");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(STORE_MARKER), "{}\n").unwrap();

        assert!(is_store_root(&root));
        assert!(!is_store_root(&tmp.path));
        assert_eq!(find_store_root(&nested), Some(root.clone()));

        assert_eq!(
            resolve_store_root(Some(&nested)).unwrap(),
            lexical_absolute(&nested)
        );
    }

    #[test]
    fn test_normalize_file_name_prefers_store_relative() {
        let tmp = TestDir::new();
        let root = tmp.path.join("store");
        let nested = root.join("projects").join("demo");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(STORE_MARKER), "{}\n").unwrap();

        let name = normalize_file_name(&nested.join("src").join("main.rs"), &root);
        assert_eq!(name, "projects/demo/src/main.rs");

        // `..` and `.` are resolved lexically, even for paths that do not exist.
        let dotted = root.join("projects").join("demo").join("..").join("x.rs");
        assert_eq!(normalize_file_name(&dotted, &root), "projects/x.rs");
    }

    #[test]
    fn test_normalize_file_name_uses_git_root_outside_store() {
        let tmp = TestDir::new();
        let repo = tmp.path.join("repo");
        let store = tmp.path.join("store");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join(STORE_MARKER), "{}\n").unwrap();
        fs::write(repo.join(".git"), "").unwrap();

        let name = normalize_file_name(&repo.join("src").join("lib.rs"), &store);
        assert_eq!(name, "src/lib.rs");
    }

    #[test]
    fn test_default_device_name_is_usable() {
        let name = default_device_name();
        assert!(!name.is_empty());
        assert!(!name.contains('\n'));
    }
}
