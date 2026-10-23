use crate::models::ProjectMeta;
use crate::storage::MemStore;
use fs2::FileExt;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;

/// Fold CR/LF into spaces so an untrusted value cannot break out of its line.
fn fold_line_breaks(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\r' | '\n' => ' ',
            other => other,
        })
        .collect()
}

/// Make a value safe to interpolate into one line of generated Markdown.
///
/// - CR/LF are folded to spaces, so a summary / title / file name cannot inject
///   extra Markdown lines (a fake heading, list item or table row).
/// - `|` is escaped, so a value cannot terminate a table cell. Outside a table
///   the backslash-escaped pipe still renders as a literal `|`.
fn escape_markdown_inline(value: &str) -> String {
    fold_line_breaks(value).replace('|', "\\|")
}

/// Render README.md content for a project based on its meta, tracked files, and recent events.
pub fn render_project_readme(store: &MemStore, project: &str) -> Result<String, Box<dyn Error>> {
    let meta = store
        .read_project_meta(project)
        .unwrap_or_else(|_| ProjectMeta {
            name: project.to_string(),
            description: format!("Project {}", project),
            github_url: None,
            created_at: crate::util::now_rfc3339(),
        });
    let files = store.get_latest_files(project)?;

    // Only the most recent events are needed: load just those instead of the
    // whole store, and let the store order them newest-last.
    let latest_10 = store.read_latest_events_for_project(project, 10)?;

    let mut md = String::new();

    // 1. Meta Header
    md.push_str(&format!("# {}\n\n", escape_markdown_inline(&meta.name)));
    if !meta.description.is_empty() {
        md.push_str(&format!(
            "{}\n\n",
            escape_markdown_inline(&meta.description)
        ));
    }

    md.push_str("### Metadata\n");
    md.push_str(&format!(
        "- **Created At:** {}\n",
        escape_markdown_inline(&meta.created_at)
    ));
    if let Some(ref url) = meta.github_url {
        // The visible label is escaped like any other inline text; the link
        // destination only needs its line breaks folded so the link stays on
        // one line (escaping it would corrupt the URL).
        md.push_str(&format!(
            "- **GitHub:** [{}]({})\n",
            escape_markdown_inline(url),
            fold_line_breaks(url)
        ));
    }
    md.push('\n');

    // 2. Tracked Files Table
    md.push_str("## Tracked Files\n\n");
    if files.is_empty() {
        md.push_str("_No files tracked yet._\n\n");
    } else {
        md.push_str("| File Name | Action | Size (bytes) | Hash (SHA256) |\n");
        md.push_str("| --- | --- | --- | --- |\n");
        for file in &files {
            // Every interpolated cell is escaped: a `|` or a line break in a
            // recorded file name/action/hash must not split the row.
            md.push_str(&format!(
                "| `{}` | {} | {} | `{}` |\n",
                escape_markdown_inline(&file.file_name),
                escape_markdown_inline(&file.action),
                file.size_bytes,
                escape_markdown_inline(&file.hash)
            ));
        }
        md.push('\n');
    }

    // 3. Timeline
    md.push_str("## Recent Activity Timeline\n\n");
    if latest_10.is_empty() {
        md.push_str("_No events recorded yet._\n");
    } else {
        for event in latest_10.iter().rev() {
            // One timeline entry per event: line breaks in any field are folded
            // so a summary can never spill into a second Markdown line.
            md.push_str(&format!(
                "- **{}** [`{}` on `{}`]: {}\n",
                escape_markdown_inline(&event.ts),
                escape_markdown_inline(&event.agent),
                escape_markdown_inline(&event.device),
                escape_markdown_inline(&event.summary)
            ));
        }
    }

    Ok(md)
}

/// Render and write `README.md` to `projects/<name>/README.md`.
///
/// Holds the store-wide write lock for the whole render + write. The generated
/// README lives inside the Git work tree, and `sync_store` commits then pulls
/// with `--rebase`, so writing it outside that lock could dirty the tree in the
/// middle of a rebase. `render_project_readme` is read-only and never re-acquires
/// the lock, so calling it here cannot dead-lock.
pub fn write_project_readme(store: &MemStore, project: &str) -> Result<(), Box<dyn Error>> {
    let _write_lock = store.lock_writes()?;
    let md = render_project_readme(store, project)?;
    let proj_dir = store.project_dir(project);
    std::fs::create_dir_all(&proj_dir)?;
    let readme_path = proj_dir.join("README.md");

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&readme_path)?;

    file.lock_exclusive()?;
    file.write_all(md.as_bytes())?;
    file.flush()?;
    file.unlock()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Event, FileRecord, ProjectMeta};
    use std::fs;
    use std::path::PathBuf;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "acdm_render_test_{}_{}",
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
    fn test_render_and_write_readme() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();

        let meta = ProjectMeta {
            name: "render-demo".to_string(),
            description: "Rendering demo project".to_string(),
            github_url: Some("https://github.com/example/render-demo".to_string()),
            created_at: "2026-10-22T00:00:00Z".to_string(),
        };
        store.write_project_meta("render-demo", &meta).unwrap();

        let event = Event {
            event_id: "eid_rend".to_string(),
            ts: "2026-10-22T05:00:00Z".to_string(),
            device: "node-x".to_string(),
            agent: "agent-y".to_string(),
            project: "render-demo".to_string(),
            summary: "created readme pipeline".to_string(),
            files: vec![FileRecord {
                file_id: "fid_rend".to_string(),
                file_name: "src/render.rs".to_string(),
                action: "Created".to_string(),
                size_bytes: 2048,
                hash: "abcdef123456".to_string(),
            }],
        };
        store.append_event(&event).unwrap();

        let rendered = render_project_readme(&store, "render-demo").unwrap();
        assert!(rendered.contains("# render-demo"));
        assert!(rendered.contains("Rendering demo project"));
        assert!(rendered.contains("src/render.rs"));
        assert!(rendered.contains("created readme pipeline"));

        write_project_readme(&store, "render-demo").unwrap();
        let readme_file = store.project_dir("render-demo").join("README.md");
        assert!(readme_file.exists());
        let content = fs::read_to_string(readme_file).unwrap();
        assert_eq!(rendered, content);
    }

    /// Count the `|` characters that terminate a Markdown table cell (i.e. are
    /// not backslash-escaped).
    fn count_unescaped_pipes(line: &str) -> usize {
        let mut count = 0;
        let mut escaped = false;
        for character in line.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            match character {
                '\\' => escaped = true,
                '|' => count += 1,
                _ => {}
            }
        }
        count
    }

    /// Untrusted values must not be able to break the rendered Markdown: pipes
    /// are escaped (table columns stay intact) and line breaks are folded (one
    /// timeline bullet per event, single-line title/description).
    #[test]
    fn test_render_escapes_pipes_and_line_breaks() {
        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();
        let project = "render-escape";

        let meta = ProjectMeta {
            name: "meta|name\ninjected heading".to_string(),
            description: "description|with\nline breaks".to_string(),
            github_url: Some("https://github.com/example/render-escape".to_string()),
            created_at: "2026-10-22T00:00:00Z".to_string(),
        };
        store.write_project_meta(project, &meta).unwrap();

        let unusual = Event {
            event_id: "eid_escape_1".to_string(),
            ts: "2026-10-22T05:00:00Z".to_string(),
            device: "node|one".to_string(),
            agent: "agent\none".to_string(),
            project: project.to_string(),
            summary: "first|summary\ninjected second line".to_string(),
            files: vec![FileRecord {
                file_id: "fid_escape_1".to_string(),
                file_name: "src/weird|name\nsecond.rs".to_string(),
                action: "Modified|Renamed".to_string(),
                size_bytes: 10,
                hash: "hash|1".to_string(),
            }],
        };
        let normal = Event {
            event_id: "eid_escape_2".to_string(),
            ts: "2026-10-22T06:00:00Z".to_string(),
            device: "node-two".to_string(),
            agent: "agent-two".to_string(),
            project: project.to_string(),
            summary: "second event".to_string(),
            files: vec![FileRecord {
                file_id: "fid_escape_2".to_string(),
                file_name: "src/main.rs".to_string(),
                action: "Created".to_string(),
                size_bytes: 20,
                hash: "hash2".to_string(),
            }],
        };
        store.append_event(&unusual).unwrap();
        store.append_event(&normal).unwrap();

        let rendered = render_project_readme(&store, project).unwrap();

        // Title and description stay on their own single lines.
        let mut lines = rendered.lines();
        assert_eq!(lines.next(), Some("# meta\\|name injected heading"));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some("description\\|with line breaks"));

        // The table keeps four columns on every row, pipes included.
        let table_rows: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with('|'))
            .collect();
        assert_eq!(table_rows.len(), 4, "header, separator and two file rows");
        for row in table_rows {
            assert_eq!(
                count_unescaped_pipes(row),
                5,
                "table row must keep four columns: {}",
                row
            );
        }
        assert!(rendered.contains("src/weird\\|name second.rs"));
        assert!(rendered.contains("Modified\\|Renamed"));

        // The timeline keeps one bullet line per event and folds the summary's
        // line break instead of letting it start a new Markdown line.
        let timeline = rendered
            .split_once("## Recent Activity Timeline")
            .expect("timeline section")
            .1;
        let bullets: Vec<&str> = timeline
            .lines()
            .filter(|line| line.starts_with("- **"))
            .collect();
        assert_eq!(bullets.len(), 2, "one bullet per event: {:?}", bullets);
        let injected_bullet = bullets
            .iter()
            .find(|line| line.contains("first\\|summary"))
            .expect("escaped summary must stay on its timeline bullet");
        assert!(
            injected_bullet.contains("injected second line"),
            "summary text must be folded onto the same line: {}",
            injected_bullet
        );
        assert!(
            !rendered.lines().any(|line| line == "injected second line"),
            "a summary line break must not inject a new Markdown line"
        );
        assert!(
            bullets.iter().any(|line| line.contains("second event")),
            "normal summaries stay readable: {:?}",
            bullets
        );
    }

    /// `write_project_readme` writes into the Git work tree, so it must hold the
    /// store-wide write lock for the whole operation; the old implementation
    /// only locked the README file itself and could dirty the work tree during a
    /// `sync_store` commit → pull --rebase window.
    #[test]
    fn test_write_project_readme_waits_for_store_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = TestDir::new();
        let store = MemStore::new(tmp.path.clone());
        store.init().unwrap();

        // Thread A holds the store-wide write lock for 400 ms.
        let holder_root = tmp.path.clone();
        let (locked_tx, locked_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let store = MemStore::new(holder_root);
            let guard = store.lock_writes().expect("acquire store lock");
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            drop(guard);
        });

        // Thread B renders and writes only after A has signalled that it holds
        // the lock; it must then wait for A to release it.
        let writer_root = tmp.path.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            locked_rx.recv().expect("holder must signal");
            let store = MemStore::new(writer_root);
            write_project_readme(&store, "blocked-project").expect("write readme");
            done_tx.send(()).unwrap();
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "write_project_readme must wait for the store-wide write lock"
        );
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("write must finish once the lock is released");

        holder.join().expect("holder thread must not panic");
        writer.join().expect("writer thread must not panic");
        assert!(store
            .project_dir("blocked-project")
            .join("README.md")
            .is_file());
    }
}
