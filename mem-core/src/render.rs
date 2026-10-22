use crate::models::ProjectMeta;
use crate::storage::MemStore;
use fs2::FileExt;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;

/// Render README.md content for a project based on its meta, tracked files, and recent events.
pub fn render_project_readme(store: &MemStore, project: &str) -> Result<String, Box<dyn Error>> {
    let meta = store.read_project_meta(project).unwrap_or_else(|_| ProjectMeta {
        name: project.to_string(),
        description: format!("Project {}", project),
        github_url: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    });
    let files = store.get_latest_files(project)?;

    // Retrieve all events for this project and take the latest 10
    let mut project_events: Vec<_> = store
        .read_all_events()?
        .into_iter()
        .filter(|e| e.project == project)
        .collect();

    project_events.sort_by(|a, b| a.ts.cmp(&b.ts));
    let latest_10 = if project_events.len() > 10 {
        &project_events[project_events.len() - 10..]
    } else {
        &project_events[..]
    };

    let mut md = String::new();

    // 1. Meta Header
    md.push_str(&format!("# {}\n\n", meta.name));
    if !meta.description.is_empty() {
        md.push_str(&format!("{}\n\n", meta.description));
    }

    md.push_str("### Metadata\n");
    md.push_str(&format!("- **Created At:** {}\n", meta.created_at));
    if let Some(ref url) = meta.github_url {
        md.push_str(&format!("- **GitHub:** [{}]({})\n", url, url));
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
            md.push_str(&format!(
                "| `{}` | {} | {} | `{}` |\n",
                file.file_name, file.action, file.size_bytes, file.hash
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
            md.push_str(&format!(
                "- **{}** [`{}` on `{}`]: {}\n",
                event.ts, event.agent, event.device, event.summary
            ));
        }
    }

    Ok(md)
}

/// Render and write README.md to projects/<name>/README.md with exclusive file lock.
pub fn write_project_readme(store: &MemStore, project: &str) -> Result<(), Box<dyn Error>> {
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
            let unique = format!("acdm_render_test_{}_{}", std::process::id(), crate::generate_id());
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
}
