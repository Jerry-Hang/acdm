use std::error::Error;
use std::fs;
use std::path::Path;
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
    let stdout = run_git(root, &["status", "--porcelain"])?;
    Ok(stdout.trim().is_empty())
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

/// Pull from remote with rebase and autostash (`git pull --rebase --autostash`).
pub fn git_pull_rebase(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["pull", "--rebase", "--autostash"])?;
    Ok(())
}

/// Push changes to remote repository (`git push`).
pub fn git_push(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["push"])?;
    Ok(())
}

/// Check if there are active git merge/rebase conflicts or conflict markers in tracked files.
pub fn git_has_conflicts(root: &Path) -> Result<bool, Box<dyn Error>> {
    // Check unmerged files via git ls-files -u
    let unmerged = run_git(root, &["ls-files", "-u"])?;
    if !unmerged.trim().is_empty() {
        return Ok(true);
    }

    // Check porcelain status for conflict codes (UU, AA, etc.)
    let status = run_git(root, &["status", "--porcelain"])?;
    for line in status.lines() {
        let code = if line.len() >= 2 { &line[..2] } else { "" };
        if matches!(code, "UU" | "AA" | "UD" | "DU" | "DD" | "AU" | "UA") {
            return Ok(true);
        }
    }

    // Check tracked files for conflict markers
    let tracked = run_git(root, &["ls-files"])?;
    for file_rel in tracked.lines() {
        let trimmed = file_rel.trim();
        if trimmed.is_empty() {
            continue;
        }
        let full_path = root.join(trimmed);
        if full_path.is_file() {
            if let Ok(content) = fs::read_to_string(&full_path) {
                if content.contains("<<<<<<<") {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

/// Continue rebase after resolving conflicts (`git rebase --continue`).
/// Uses non-interactive editor to prevent blocking.
pub fn git_rebase_continue(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git_with_env(root, &["rebase", "--continue"], &[("GIT_EDITOR", "true")])?;
    Ok(())
}

/// Abort an in-progress rebase (`git rebase --abort`).
pub fn git_rebase_abort(root: &Path) -> Result<(), Box<dyn Error>> {
    run_git(root, &["rebase", "--abort"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!("acdm_git_test_{}_{}", std::process::id(), crate::generate_id());
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
    fn test_git_workflow() {
        let tmp = TestDir::new();
        git_init(&tmp.path).unwrap();

        // Configure git identity for testing
        let _ = run_git(&tmp.path, &["config", "user.email", "test@acdm.local"]);
        let _ = run_git(&tmp.path, &["config", "user.name", "ACDM Test"]);

        assert!(git_status_clean(&tmp.path).unwrap());

        // Create a test file
        let file_path = tmp.path.join("test.txt");
        fs::write(&file_path, "initial content\n").unwrap();
        assert!(!git_status_clean(&tmp.path).unwrap());

        git_add_all(&tmp.path).unwrap();
        git_commit(&tmp.path, "Initial commit").unwrap();
        assert!(git_status_clean(&tmp.path).unwrap());

        // Test remote addition
        git_add_remote(&tmp.path, "origin", "https://github.com/example/test.git").unwrap();

        // Test conflict detection
        assert!(!git_has_conflicts(&tmp.path).unwrap());

        fs::write(&file_path, "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>>\n").unwrap();
        assert!(git_has_conflicts(&tmp.path).unwrap());
    }
}
