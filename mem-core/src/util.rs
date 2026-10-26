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
use std::fs;
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
///
/// Every shape (explicit path, environment variable, ancestor search, CWD
/// fallback) is returned in canonical form, so the same store never has two
/// string identities — for example when it is reached through a symbolic link.
pub fn resolve_store_root(explicit: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(canonicalize_existing_prefix(&lexical_absolute(path)));
    }
    if let Some(from_env) = env::var_os(STORE_ENV_VAR).filter(|value| !value.is_empty()) {
        return Ok(canonicalize_existing_prefix(&lexical_absolute(Path::new(
            &from_env,
        ))));
    }
    let cwd = env::current_dir()?;
    let cwd = canonicalize_existing_prefix(&lexical_clean(&cwd));
    Ok(match find_store_root(&cwd) {
        Some(root) => canonicalize_existing_prefix(&root),
        None => cwd,
    })
}

/// Normalise a user supplied path into a stable, device independent identity.
///
/// The result is relative to the store root when possible, otherwise relative to
/// the enclosing git work tree, otherwise relative to the current directory, and
/// finally the absolute path. Components always use `/` so the same file keeps
/// the same identity on every platform and device.
///
/// Existing prefixes are canonicalised (symbolic links resolved) on both sides of
/// every comparison. Mixing a symlinked form with a canonical form would make
/// `strip_prefix` fail and silently degrade the identity to an absolute path.
pub fn normalize_file_name(path: &Path, store_root: &Path) -> String {
    let absolute = lexical_absolute(path);

    // Canonicalise the store root as well: a root reached through a symlink must
    // match the canonical absolute path, not fall through to the absolute form.
    let store = canonicalize_existing_prefix(&lexical_absolute(store_root));
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
        let cwd = lexical_absolute(&cwd);
        if let Ok(relative) = absolute.strip_prefix(&cwd) {
            let name = to_slash(relative);
            if !name.is_empty() {
                return name;
            }
        }
    }

    to_slash(&absolute)
}

/// Find the enclosing git work tree of a (possibly not yet existing) path.
///
/// The returned root is canonicalised so it matches the canonicalised absolute
/// path even when the work tree is reached through a symbolic link.
fn find_git_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(canonicalize_existing_prefix(dir));
        }
        current = dir.parent();
    }
    None
}

/// Canonicalise the longest on-disk-existing prefix of `path`, then append the
/// still-missing tail lexically.
///
/// `fs::canonicalize` fails for paths that do not exist yet (a deleted file, the
/// target of `mem init`, ...). Instead of requiring the whole path to exist, this
/// walks up to the deepest ancestor that really is on disk, canonicalises that
/// (following symbolic links), and re-attaches the remaining components. The
/// result stays usable for nonexistent paths while giving every existing part a
/// single stable string identity — even when the current directory or the store
/// root is reached through a symbolic link (for example `/tmp` on some systems,
/// or macOS `/var` → `/private/var`).
///
/// The result is absolute whenever the input (or the current directory used to
/// make a relative input absolute) is available.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        lexical_clean(path)
    } else {
        env::current_dir()
            .map(|cwd| lexical_clean(&cwd.join(path)))
            .unwrap_or_else(|_| lexical_clean(path))
    };

    // Walk from the full path up to the filesystem root until the deepest on-disk
    // prefix is found. `symlink_metadata` is used instead of `fs::canonicalize`
    // because the deepest existing component may itself be a symbolic link file:
    // following it would move the identity outside the work tree.
    let mut existing: &Path = &absolute;
    let canonical = loop {
        if let Ok(metadata) = fs::symlink_metadata(existing) {
            break canonicalize_existing_keep_file_symlink(existing, &metadata);
        }
        match existing.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => existing = parent,
            _ => break None,
        }
    };

    match canonical {
        Some(canonical) => {
            // Re-attach the components that do not exist yet, lexically.
            let tail = absolute.strip_prefix(existing).unwrap_or(Path::new(""));
            if tail.as_os_str().is_empty() {
                canonical
            } else {
                canonical.join(tail)
            }
        }
        // Nothing on disk could be canonicalised (e.g. no usable parent at all):
        // keep the lexical result rather than failing.
        None => absolute,
    }
}

/// Canonicalise an on-disk path while keeping a final symbolic link **to a file**
/// as a name instead of following it.
///
/// A tracked symlink file (e.g. `repo/src/config.json -> ../dotfiles/config.json`)
/// is part of the repository's identity: resolving it would point outside the work
/// tree and `strip_prefix(git_root)` would fail, degrading the record to an
/// absolute path that no other device can match. Symlinks **to directories** are
/// ordinary path indirection (a symlinked store root, a symlinked parent
/// directory), so they are still resolved; dangling file symlinks are kept by name
/// as well, which is also what resolving them used to fall back to.
fn canonicalize_existing_keep_file_symlink(
    path: &Path,
    metadata: &fs::Metadata,
) -> Option<PathBuf> {
    if !metadata.file_type().is_symlink() {
        return fs::canonicalize(path).ok();
    }
    if fs::metadata(path).is_ok_and(|target| target.is_dir()) {
        return fs::canonicalize(path).ok();
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())?;
    match fs::canonicalize(parent) {
        Ok(canonical_parent) => path.file_name().map(|name| canonical_parent.join(name)),
        // The parent is on disk (the link exists), so this only happens on an I/O
        // error: fall back to the previous "follow the link" behaviour.
        Err(_) => fs::canonicalize(path).ok(),
    }
}

/// Make a path absolute against the current working directory and resolve the
/// on-disk-existing part of it (following symlinks).
///
/// The whole path is not required to exist: a deleted file keeps a deterministic
/// absolute identity while its existing ancestors are canonicalised.
fn lexical_absolute(path: &Path) -> PathBuf {
    let lexical = if path.is_absolute() {
        lexical_clean(path)
    } else {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        lexical_clean(&cwd.join(path))
    };
    canonicalize_existing_prefix(&lexical)
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
///
/// On Windows `Path::canonicalize` returns a verbatim path (`\\?\C:\...`, or
/// `\\?\UNC\server\share`); those prefixes are stripped so a recorded
/// `file_name` stays readable and matches the identity the same file gets on
/// other devices.
fn to_slash(path: &Path) -> String {
    strip_verbatim_prefix(path.to_string_lossy().replace('\\', "/"))
}

/// Remove the Windows verbatim (`\\?\`) prefix from an already slash-normalized
/// path string. Other paths are returned unchanged.
fn strip_verbatim_prefix(slash_path: String) -> String {
    if let Some(rest) = slash_path.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = slash_path.strip_prefix("//?/") {
        rest.to_string()
    } else {
        slash_path
    }
}

/// Render a path as a stable `/`-separated identity string.
///
/// This is the canonical rendering used for both recorded file identities and
/// the directory-root identities they are compared against. On Windows
/// `Path::canonicalize` returns a verbatim path (`\\?\C:\...`); the prefix is
/// stripped here so both sides of such a comparison agree, and so a recorded
/// `file_name` stays readable and matches on other devices.
pub fn path_identity(path: &Path) -> String {
    to_slash(path)
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

    /// A path reached through a symlinked directory must keep a relative file
    /// identity instead of degrading to an absolute path.
    #[cfg(unix)]
    #[test]
    fn test_normalize_resolves_symlinked_cwd() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let real = tmp.path.join("real");
        fs::create_dir_all(real.join("src")).unwrap();
        let link = tmp.path.join("link");
        symlink(&real, &link).unwrap();

        // Both the file path and the store root spelled through the symlink.
        assert_eq!(
            normalize_file_name(&link.join("src").join("a.rs"), &link),
            "src/a.rs"
        );

        // The interesting regression: the file is reached through the symlink
        // while the store root is the canonical path (or vice versa). Before the
        // existing prefixes were canonicalised this fell back to an absolute
        // path and broke cross-device identity.
        assert_eq!(
            normalize_file_name(&link.join("src").join("a.rs"), &real),
            "src/a.rs"
        );
    }

    /// A symlinked store root must be canonicalised before `strip_prefix`, so a
    /// file given by its real path still gets the same relative identity.
    #[cfg(unix)]
    #[test]
    fn test_normalize_canonicalizes_store_root() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let real = tmp.path.join("real");
        fs::create_dir_all(real.join("src")).unwrap();
        let link = tmp.path.join("link");
        symlink(&real, &link).unwrap();

        assert_eq!(
            normalize_file_name(&real.join("src").join("a.rs"), &link),
            "src/a.rs"
        );
        assert_eq!(
            normalize_file_name(&link.join("src").join("a.rs"), &real),
            "src/a.rs"
        );
    }

    /// A symbolic link *file* tracked inside a repository (for example
    /// `repo/src/link.txt -> <outside>/shared-target.txt`) must keep its
    /// repository-relative identity. Canonicalising the final component used to
    /// follow the link outside the work tree and degrade the identity to an
    /// absolute path that no other device can match.
    #[cfg(unix)]
    #[test]
    fn test_normalize_file_name_preserves_symlink_file_inside_repo() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let repo = tmp.path.join("repo");
        let store = tmp.path.join("store");
        let outside = tmp.path.join("outside");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(store.join(STORE_MARKER), "{}\n").unwrap();
        // `find_git_root` only needs a `.git` entry; a full repository is not needed.
        fs::write(repo.join(".git"), "").unwrap();

        let target = outside.join("shared-target.txt");
        fs::write(&target, "shared\n").unwrap();
        let link = repo.join("src").join("link.txt");
        symlink(&target, &link).unwrap();

        let name = normalize_file_name(&link, &store);
        assert_eq!(
            name, "src/link.txt",
            "a tracked symlink file must keep its repository-relative identity"
        );
        assert!(
            !name.starts_with('/'),
            "identity degraded to an absolute path: {}",
            name
        );

        // A regular file in the same repository keeps its relative identity too.
        let regular = repo.join("src").join("regular.rs");
        fs::write(&regular, "fn main() {}\n").unwrap();
        assert_eq!(normalize_file_name(&regular, &store), "src/regular.rs");

        // Symlinked directories are still path indirection: the existing
        // symlinked-directory behaviour is unchanged.
        let real = tmp.path.join("real");
        fs::create_dir_all(real.join("src")).unwrap();
        let dir_link = tmp.path.join("dir-link");
        symlink(&real, &dir_link).unwrap();
        assert_eq!(
            normalize_file_name(&dir_link.join("src").join("a.rs"), &real),
            "src/a.rs",
            "symlinked-directory normalisation must stay as before"
        );

        // A file that exists below a symlinked directory is still resolved
        // through the link (it is not a symlink itself).
        let outside_dir = tmp.path.join("outside-dir");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("f.txt"), "f\n").unwrap();
        let link_dir = repo.join("linkdir");
        symlink(&outside_dir, &link_dir).unwrap();
        let through_dir = normalize_file_name(&link_dir.join("f.txt"), &store);
        assert!(
            !through_dir.contains("linkdir") && through_dir.ends_with("outside-dir/f.txt"),
            "a file below a symlinked directory must still be resolved through the link, got: {}",
            through_dir
        );
    }

    /// `resolve_store_root` must return one canonical path for a store, even when
    /// it is addressed through a symlink or does not exist yet.
    #[cfg(unix)]
    #[test]
    fn test_resolve_store_root_canonicalizes_explicit_path() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let real = tmp.path.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = tmp.path.join("link");
        symlink(&real, &link).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();

        assert_eq!(
            resolve_store_root(Some(&link)).unwrap(),
            canonical_real,
            "an explicit symlinked --path must resolve to the canonical root"
        );
        // The same store must not have two string identities.
        assert_eq!(
            resolve_store_root(Some(&real)).unwrap(),
            canonical_real,
            "canonical and symlinked spellings must agree"
        );

        // A path that does not exist yet must still be accepted: only its
        // existing prefix is canonicalised and the tail is re-attached.
        let missing = link.join("not-created-yet");
        assert_eq!(
            resolve_store_root(Some(&missing)).unwrap(),
            canonical_real.join("not-created-yet"),
            "a nonexistent explicit path must not fail"
        );
    }

    #[test]
    fn test_default_device_name_is_usable() {
        let name = default_device_name();
        assert!(!name.is_empty());
        assert!(!name.contains('\n'));
    }

    #[test]
    fn test_verbatim_prefix_is_stripped_from_recorded_paths() {
        // Windows canonicalize() yields verbatim paths; recording them would make
        // the same file look different from another device.
        assert_eq!(
            strip_verbatim_prefix("//?/C:/data/file.txt".into()),
            "C:/data/file.txt"
        );
        assert_eq!(
            strip_verbatim_prefix("//?/UNC/server/share/file.txt".into()),
            "//server/share/file.txt"
        );
        // Non-verbatim paths (all Unix paths, and already-normalized Windows ones)
        // must pass through untouched.
        assert_eq!(
            strip_verbatim_prefix("/home/u/file.txt".into()),
            "/home/u/file.txt"
        );
        assert_eq!(
            strip_verbatim_prefix("C:/data/file.txt".into()),
            "C:/data/file.txt"
        );
    }
}
