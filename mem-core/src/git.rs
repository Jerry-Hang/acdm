use crate::util;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_git(root: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    run_git_with_env(root, args, &[])
}

fn run_git_with_env(
    root: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, Box<dyn Error>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd.output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let err_msg = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        Err(format!("git command 'git {}' failed: {}", args.join(" "), err_msg).into())
    }
}

/// Initialize a git repository at the given root.
pub fn git_init(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["init"])?;
    Ok(())
}

/// Add a remote with given name and URL.
pub fn git_add_remote(root: &Path, name: &str, url: &str) -> Result<(), Box<dyn Error>> {
    run_git(root, &["remote", "add", name, url])?;
    Ok(())
}

/// Check if git working directory is clean (no uncommitted or untracked changes).
pub fn git_status_clean(root: &Path) -> Result<bool, Box<dyn Error>> {
    git_worktree_clean(root)
}

/// Whether the working tree is completely clean (tracked and untracked files).
pub fn git_worktree_clean(root: &Path) -> Result<bool, Box<dyn Error>> {
    let stdout = run_git(root, &["status", "--porcelain"])?;
    Ok(stdout.trim().is_empty())
}

/// Whether the *tracked* working tree is clean; untracked files are ignored.
///
/// Kept for callers that need a worktree view; the empty-patch recovery in
/// [`crate::resolve`] uses [`git_index_clean`] instead, because only the index
/// (not the working tree) decides whether a replayed commit is an empty patch.
pub fn git_worktree_clean_tracked(root: &Path) -> Result<bool, Box<dyn Error>> {
    let stdout = run_git(root, &["status", "--porcelain", "--untracked-files=no"])?;
    Ok(stdout.trim().is_empty())
}

/// Whether the Git index matches `HEAD` (no staged changes).
///
/// This is the authoritative *empty patch* signal for the rebase recovery in
/// [`crate::resolve`]: `git rebase --continue` refuses a replayed commit whose
/// resolved result has no staged changes relative to `HEAD` ("No changes - did
/// you forget to use `git add`?"), so it must be dropped with `git rebase
/// --skip`. Unstaged modifications to unrelated tracked files and untracked
/// scratch files say nothing about the replayed patch and must not disable that
/// recovery.
///
/// `git diff --cached --quiet` exits `0` when the index matches `HEAD`, `1` when
/// staged changes exist (**a normal result, not an error**), and anything else is
/// a real failure.
pub fn git_index_clean(root: &Path) -> Result<bool, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["diff", "--cached", "--quiet"])
        .output()?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "git command 'git diff --cached --quiet' failed: {}",
                stderr.trim()
            )
            .into())
        }
    }
}

/// Stage all changes in the repository (`git add -A`).
pub fn git_add_all(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["add", "-A"])?;
    Ok(())
}

/// Commit staged changes with given commit message.
pub fn git_commit(root: &Path, msg: &str) -> Result<(), Box<dyn Error>> {
    run_git(root, &["commit", "-m", msg])?;
    Ok(())
}

/// Pull from the remote with rebase (`git pull --rebase`).
///
/// Deliberately does **not** pass `--autostash`: [`sync_store`] commits the whole
/// working tree first and holds the store write lock, so there should be nothing
/// to stash. With `--autostash`, a stash pop that conflicts during a rebase
/// leaves a conflicted working tree with no rebase state, which is very hard to
/// recover from; without it, a dirty tree simply fails the pull with a clear
/// error.
pub fn git_pull_rebase(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["pull", "--rebase"])?;
    Ok(())
}

/// Push changes to remote repository (`git push`).
pub fn git_push(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["push"])?;
    Ok(())
}

/// Whether the checked-out branch has an upstream configured.
pub fn git_has_upstream(root: &Path) -> Result<bool, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .output()?;
    Ok(output.status.success())
}

/// Resolve a path inside the repository's git directory (`git rev-parse --git-path`).
///
/// `git rev-parse --git-path` is worktree-aware: in a linked worktree or a
/// submodule it resolves the path inside the *per-worktree* git directory
/// instead of naively joining `<root>/.git`.
pub(crate) fn git_path(root: &Path, name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let output = run_git(root, &["rev-parse", "--git-path", name])?;
    let path = PathBuf::from(output.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

/// Whether `root` is a Git repository.
///
/// Uses `git rev-parse --git-dir`, which also succeeds when `.git` is a *file*
/// (a linked worktree or a submodule) and therefore works in every checkout
/// layout, unlike checking `root.join(".git").is_dir()`.
pub fn git_is_repo(root: &Path) -> bool {
    match Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--git-dir"])
        .output()
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Whether the repository is in a conflicted state (unmerged entries, conflict
/// status codes, or an active rebase/merge).
///
/// Git's own bookkeeping is authoritative here. The file *contents* are never
/// searched for conflict markers: a legitimate event summary or a document that
/// merely quotes `<<<<<<<` would otherwise make every sync believe the
/// repository is permanently conflicted — and scanning every tracked file also
/// costs O(repository size).
pub fn git_has_conflicts(root: &Path) -> Result<bool, Box<dyn Error>> {
    // Unmerged files in the index.
    let unmerged = run_git(root, &["ls-files", "-u"])?;
    if !unmerged.trim().is_empty() {
        return Ok(true);
    }

    // Porcelain conflict codes (UU, AA, ...).
    let status = run_git(root, &["status", "--porcelain"])?;
    for line in status.lines() {
        let code = line.get(..2).unwrap_or("");
        if matches!(code, "UU" | "AA" | "UD" | "DU" | "DD" | "AU" | "UA") {
            return Ok(true);
        }
    }

    // Active rebase or merge state.
    for state in ["rebase-merge", "rebase-apply", "MERGE_HEAD"] {
        if git_path(root, state)?.exists() {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Resume a rebase after resolving conflicts (`git rebase --continue`).
/// Uses non-interactive editor to prevent blocking.
pub fn git_rebase_continue(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git_with_env(root, &["rebase", "--continue"], &[("GIT_EDITOR", "true")])?;
    Ok(())
}

/// Drop the commit currently being replayed (`git rebase --skip`).
///
/// Needed when conflict resolution produced a patch with no changes: such a
/// commit can never be committed (git says "No changes - did you forget to use
/// `git add`?"), so it has to be skipped for the rebase to finish.
pub fn git_rebase_skip(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git_with_env(root, &["rebase", "--skip"], &[("GIT_EDITOR", "true")])?;
    Ok(())
}

/// Abort an in-progress rebase (`git rebase --abort`).
pub fn git_rebase_abort(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["rebase", "--abort"])?;
    Ok(())
}

/// Outcome of [`sync_store`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Everything that could be done succeeded.
    Success { steps: Vec<String> },
    /// The pull stopped on a conflict; the caller should surface the message and
    /// tell the user/agent to run conflict resolution.
    Conflict { steps: Vec<String>, message: String },
}

impl SyncOutcome {
    /// Human-readable description of every completed step.
    pub fn steps(&self) -> &[String] {
        match self {
            Self::Success { steps } | Self::Conflict { steps, .. } => steps,
        }
    }

    /// Whether the sync stopped because the pull produced conflicts.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

/// Commit local changes, pull with rebase and push — the complete `mem sync` flow.
///
/// Behavioural guarantees (which the CLI and the MCP server both rely on):
///
/// * the store-wide write lock is held for the whole operation, so no other
///   process can append events while the repository is being rebased;
/// * a dirty working tree is committed first, so the pull always starts clean;
/// * a pull failure that is **not** a conflict aborts the sync with an error
///   instead of being swallowed and turning into a confusing push failure;
/// * only a missing upstream (no remote tracking branch) skips the pull.
pub fn sync_store(
    root: &Path,
    commit_message: Option<&str>,
) -> Result<SyncOutcome, Box<dyn Error>> {
    let _write_lock = crate::storage::lock_store(root)?;
    let mut steps = Vec::new();

    if git_status_clean(root)? {
        steps.push("Working directory clean".to_string());
    } else {
        let message = commit_message
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "Auto-sync from {} at {}",
                    util::default_device_name(),
                    util::now_rfc3339()
                )
            });
        git_add_all(root)?;
        git_commit(root, &message)?;
        steps.push(format!("Committed local changes (\"{}\")", message));
    }

    if git_has_upstream(root)? {
        match git_pull_rebase(root) {
            Ok(()) => steps.push("Pulled remote changes with rebase".to_string()),
            Err(error) => {
                if git_has_conflicts(root)? {
                    return Ok(SyncOutcome::Conflict {
                        steps,
                        message: format!(
                            "Git conflict detected during pull --rebase: {}. \
                             Resolve the JSONL conflict (the CLI: `mem resolve`; MCP: the \
                             `mem_resolve` tool), then retry the sync.",
                            error
                        ),
                    });
                }
                return Err(format!(
                    "git pull --rebase failed and the repository is not conflicted; \
                     aborting sync instead of pushing over an inconsistent state: {}",
                    error
                )
                .into());
            }
        }
    } else {
        steps.push("No upstream configured; skipped pull".to_string());
    }

    git_push(root)?;
    steps.push("Pushed to remote successfully".to_string());
    Ok(SyncOutcome::Success { steps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "acdm_git_test_{}_{}",
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

    fn git(dir: &Path, args: &[&str]) {
        let _ = Command::new("git").current_dir(dir).args(args).output();
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@acdm.local"]);
        git(dir, &["config", "user.name", "ACDM Test"]);
    }

    fn current_branch(dir: &Path) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse");
        String::from_utf8(output.stdout)
            .expect("utf8 branch")
            .trim()
            .to_string()
    }

    #[test]
    fn test_git_workflow() {
        let tmp = TestDir::new();
        init_repo(&tmp.path);

        assert!(git_status_clean(&tmp.path).unwrap());

        let file_path = tmp.path.join("test.txt");
        fs::write(&file_path, "initial content\n").unwrap();
        assert!(!git_status_clean(&tmp.path).unwrap());

        git_add_all(&tmp.path).unwrap();
        git_commit(&tmp.path, "Initial commit").unwrap();
        assert!(git_status_clean(&tmp.path).unwrap());

        git_add_remote(&tmp.path, "origin", "https://github.com/example/test.git").unwrap();
        assert!(!git_has_conflicts(&tmp.path).unwrap());
    }

    /// Regression: a legitimate file that merely quotes a conflict marker must
    /// not be reported as a conflict (the old implementation scanned file
    /// contents for `<<<<<<<` and produced permanent false positives).
    #[test]
    fn test_git_has_conflicts_ignores_marker_like_content() {
        let tmp = TestDir::new();
        init_repo(&tmp.path);
        fs::write(
            tmp.path.join("notes.md"),
            "A summary that quotes a marker follows:\n<<<<<<< HEAD\nnot a real conflict\n",
        )
        .unwrap();
        git_add_all(&tmp.path).unwrap();
        git_commit(&tmp.path, "quotes a marker").unwrap();

        assert!(git_status_clean(&tmp.path).unwrap());
        assert!(
            !git_has_conflicts(&tmp.path).unwrap(),
            "marker-like file content must not be treated as a conflict"
        );
    }

    #[test]
    fn test_git_has_conflicts_detects_real_rebase_conflict() {
        let tmp = TestDir::new();
        init_repo(&tmp.path);

        let file_path = tmp.path.join("events.jsonl");
        fs::write(&file_path, "{\"id\":\"base\"}\n").unwrap();
        git_add_all(&tmp.path).unwrap();
        git_commit(&tmp.path, "base").unwrap();
        let base_branch = current_branch(&tmp.path);

        git(&tmp.path, &["checkout", "-qb", "other"]);
        fs::write(&file_path, "{\"id\":\"other\"}\n").unwrap();
        git_add_all(&tmp.path).unwrap();
        git(&tmp.path, &["commit", "-qm", "other"]);

        git(&tmp.path, &["checkout", "-q", &base_branch]);
        fs::write(&file_path, "{\"id\":\"main\"}\n").unwrap();
        git_add_all(&tmp.path).unwrap();
        git(&tmp.path, &["commit", "-qm", "main"]);

        git(&tmp.path, &["rebase", "other"]);
        assert!(
            git_has_conflicts(&tmp.path).unwrap(),
            "an actual rebase conflict must be detected"
        );

        let _ = Command::new("git")
            .current_dir(&tmp.path)
            .args(["rebase", "--abort"])
            .output();
        assert!(!git_has_conflicts(&tmp.path).unwrap());
    }

    #[test]
    fn test_git_has_upstream() {
        let tmp = TestDir::new();
        init_repo(&tmp.path);
        fs::write(tmp.path.join("a.txt"), "a\n").unwrap();
        git_add_all(&tmp.path).unwrap();
        git_commit(&tmp.path, "a").unwrap();

        assert!(
            !git_has_upstream(&tmp.path).unwrap(),
            "a fresh repo has no upstream"
        );

        let remote = TestDir::new();
        let bare = remote.path.join("origin.git");
        let status = Command::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .output()
            .expect("git init --bare");
        assert!(status.status.success());
        git(
            &tmp.path,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(
            &tmp.path,
            &["push", "-q", "-u", "origin", &current_branch(&tmp.path)],
        );

        assert!(
            git_has_upstream(&tmp.path).unwrap(),
            "after push -u the branch has an upstream"
        );
    }

    /// End-to-end sync: commit, pull and push against a local bare remote,
    /// including the conflict outcome (which must not be silently swallowed).
    #[test]
    fn test_sync_store_roundtrip_and_conflict_outcome() {
        let remote = TestDir::new();
        let bare = remote.path.join("origin.git");
        assert!(Command::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .output()
            .unwrap()
            .status
            .success());

        let first = TestDir::new();
        init_repo(&first.path);
        fs::write(first.path.join("events.jsonl"), "{\"id\":\"base\"}\n").unwrap();
        git_add_all(&first.path).unwrap();
        git_commit(&first.path, "base").unwrap();
        git(
            &first.path,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(
            &first.path,
            &["push", "-q", "-u", "origin", &current_branch(&first.path)],
        );

        // Clean-tree sync against the remote succeeds.
        fs::write(first.path.join("events.jsonl"), "{\"id\":\"local-1\"}\n").unwrap();
        let outcome = sync_store(&first.path, Some("local change")).unwrap();
        assert!(!outcome.is_conflict(), "unexpected conflict: {:?}", outcome);
        assert!(outcome
            .steps()
            .iter()
            .any(|step| step.contains("Pushed to remote successfully")));

        // Second clone creates a conflicting change and pushes it.
        let second = TestDir::new();
        assert!(Command::new("git")
            .args([
                "clone",
                "-q",
                bare.to_str().unwrap(),
                second.path.to_str().unwrap(),
            ])
            .output()
            .unwrap()
            .status
            .success());
        git(&second.path, &["config", "user.email", "b@acdm.local"]);
        git(&second.path, &["config", "user.name", "B"]);
        fs::write(second.path.join("events.jsonl"), "{\"id\":\"remote-1\"}\n").unwrap();
        git_add_all(&second.path).unwrap();
        git_commit(&second.path, "remote change").unwrap();
        git(&second.path, &["push", "-q"]);

        // First repo now has a divergent commit: pulling must report a conflict
        // instead of being swallowed or pushed over.
        fs::write(first.path.join("events.jsonl"), "{\"id\":\"local-2\"}\n").unwrap();
        let outcome = sync_store(&first.path, None).unwrap();
        assert!(
            outcome.is_conflict(),
            "divergent history must produce a Conflict outcome, got {:?}",
            outcome
        );

        let _ = Command::new("git")
            .current_dir(&first.path)
            .args(["rebase", "--abort"])
            .output();
    }

    /// A pull failure that is not a conflict must abort the sync (never continue
    /// to `git push` and mask the real cause).
    #[test]
    fn test_sync_store_aborts_on_non_conflict_pull_failure() {
        let temp = TestDir::new();
        init_repo(&temp.path);
        fs::write(temp.path.join("a.txt"), "a\n").unwrap();
        git_add_all(&temp.path).unwrap();
        git_commit(&temp.path, "a").unwrap();
        git(
            &temp.path,
            &["remote", "add", "origin", "/nonexistent/acdm-remote.git"],
        );
        git(
            &temp.path,
            &["push", "-q", "-u", "origin", &current_branch(&temp.path)],
        );

        // The upstream is configured but unreachable: the pull fails with a
        // network/remote error and the sync must return an error, not push.
        let _ = Command::new("git")
            .current_dir(&temp.path)
            .args(["fetch"])
            .output();
        fs::write(temp.path.join("a.txt"), "b\n").unwrap();
        let result = sync_store(&temp.path, None);
        assert!(
            result.is_err(),
            "non-conflict pull failures must abort the sync, got {:?}",
            result
        );
    }
}
