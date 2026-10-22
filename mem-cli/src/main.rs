use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};

use mem_core::compact::compact_project_files;
use mem_core::git::{
    git_add_all, git_commit, git_has_conflicts, git_pull_rebase, git_push, git_status_clean,
};
use mem_core::render::write_project_readme;
use mem_core::resolve::resolve_conflicts;
use mem_core::{generate_id, Event, FileRecord, MemIndex, MemStore, ProjectMeta};

#[derive(Parser)]
#[command(
    name = "mem",
    about = "Agent Cross-Device Memory System CLI",
    version
)]
struct Cli {
    /// Root directory for memory store (defaults to finding .mem or events/ upwards, fallback to CWD)
    #[arg(long, global = true)]
    path: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new memory store
    Init {
        /// Git remote URL to initialize with
        #[arg(long)]
        remote: Option<String>,

        /// Root directory (overrides global --path)
        #[arg(long)]
        path: Option<PathBuf>,
    },

    /// Record file changes to project memory
    Add {
        /// Project name
        #[arg(short = 'p', long)]
        project: String,

        /// Summary of changes
        #[arg(short = 's', long)]
        summary: String,

        /// Device name (defaults to hostname)
        #[arg(short = 'd', long)]
        device: Option<String>,

        /// Agent name (defaults to "human")
        #[arg(short = 'a', long, default_value = "human")]
        agent: String,

        /// File paths to record
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },

    /// Query recorded events and file records
    Query {
        /// List events for a project
        #[arg(short = 'p', long)]
        project: Option<String>,

        /// Look up specific file record by ID
        #[arg(long)]
        file_id: Option<String>,

        /// Find events containing this filename
        #[arg(short = 'n', long)]
        filename: Option<String>,

        /// Regex fuzzy search across all events
        #[arg(long)]
        fuzzy: Option<String>,
    },

    /// List projects or latest files within a project
    List {
        /// Project name to list latest file states for
        #[arg(short = 'p', long)]
        project: Option<String>,
    },

    /// Compact project file records by removing historical duplicates
    Compact {
        /// Project name
        #[arg(short = 'p', long)]
        project: String,
    },

    /// Synchronize local events with remote Git repository
    Sync,

    /// Resolve Git conflict markers in JSONL files
    Resolve,

    /// Render project README markdown documentation
    Render {
        /// Project name
        #[arg(short = 'p', long)]
        project: String,
    },
}

fn resolve_root(custom_path: Option<PathBuf>) -> io::Result<PathBuf> {
    if let Some(p) = custom_path {
        return Ok(p);
    }
    let current = std::env::current_dir()?;
    let mut check: &Path = &current;
    loop {
        if check.join(".mem").exists() || check.join("events").is_dir() {
            return Ok(check.to_path_buf());
        }
        match check.parent() {
            Some(parent) => check = parent,
            None => break,
        }
    }
    Ok(current)
}

fn get_device_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "localhost".to_string())
        })
}

fn compute_file_hash(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn print_event(event: &Event) {
    println!("Event: {} | {}", event.event_id, event.ts);
    println!(
        "  Project: {} | Device: {} | Agent: {}",
        event.project, event.device, event.agent
    );
    println!("  Summary: {}", event.summary);
    if !event.files.is_empty() {
        println!("  Files ({}):", event.files.len());
        for f in &event.files {
            println!(
                "    • [{}] {} (ID: {}, {} bytes, hash: {})",
                f.action,
                f.file_name,
                f.file_id,
                f.size_bytes,
                if f.hash.is_empty() { "-" } else { &f.hash }
            );
        }
    }
    println!();
}

fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { remote, path } => {
            let target_path = path.or(cli.path).unwrap_or(std::env::current_dir()?);
            let store = MemStore::new(target_path.clone());
            if let Some(ref remote_url) = remote {
                store.init_with_remote(remote_url)?;
                println!(
                    "Successfully initialized ACDM store with remote '{}' at: {}",
                    remote_url,
                    target_path.display()
                );
            } else {
                store.init()?;
                println!(
                    "Successfully initialized ACDM store at: {}",
                    target_path.display()
                );
            }
            println!("Created directory structure:");
            println!("  ├── events/       (Append-only event log)");
            println!("  ├── projects/     (Project metadata and file records)");
            println!("  └── nodes/        (Device node info)");
        }

        Commands::Add {
            project,
            summary,
            device,
            agent,
            files,
        } => {
            let root = resolve_root(cli.path)?;
            let store = MemStore::new(root);

            let existing_files = store.get_latest_files(&project).unwrap_or_default();
            let mut file_records = Vec::new();

            for file_path in &files {
                let file_str = file_path.to_string_lossy().to_string();
                let exists = file_path.exists();
                let (action, size_bytes, hash) = if !exists {
                    ("Deleted".to_string(), 0u64, String::new())
                } else {
                    let bytes = fs::read(file_path).map_err(|e| {
                        format!("Failed to read file '{}': {}", file_path.display(), e)
                    })?;
                    let size = bytes.len() as u64;
                    let hash = compute_file_hash(&bytes);
                    let is_known = existing_files.iter().any(|f| f.file_name == file_str);
                    let act = if is_known { "Modified" } else { "Created" };
                    (act.to_string(), size, hash)
                };

                let file_id = generate_id();
                file_records.push(FileRecord {
                    file_id,
                    file_name: file_str,
                    action,
                    size_bytes,
                    hash,
                });
            }

            let ts = chrono::Utc::now().to_rfc3339();
            let event_id = generate_id();
            let dev_name = device.unwrap_or_else(get_device_name);

            let event = Event {
                event_id,
                ts: ts.clone(),
                device: dev_name,
                agent,
                project: project.clone(),
                summary,
                files: file_records,
            };

            if !store.project_exists(&project) {
                let meta = ProjectMeta {
                    name: project.clone(),
                    description: format!("Project {}", project),
                    github_url: None,
                    created_at: ts,
                };
                store.write_project_meta(&project, &meta)?;
            }

            store.append_event(&event)?;

            println!("Successfully recorded event: {}", event.event_id);
            println!("  Project:   {}", event.project);
            println!("  Timestamp: {}", event.ts);
            println!("  Device:    {}", event.device);
            println!("  Agent:     {}", event.agent);
            println!("  Summary:   {}", event.summary);
            println!("  Files ({}):", event.files.len());
            for file in &event.files {
                println!(
                    "    • [{}] {} ({} bytes, sha256: {})",
                    file.action,
                    file.file_name,
                    file.size_bytes,
                    if file.hash.is_empty() { "-" } else { &file.hash }
                );
            }
        }

        Commands::Query {
            project,
            file_id,
            filename,
            fuzzy,
        } => {
            let root = resolve_root(cli.path)?;
            let store = MemStore::new(root);
            let index = MemIndex::build(&store)?;

            let mut ran_query = false;

            if let Some(ref p) = project {
                ran_query = true;
                println!("--- Events for project '{}' ---", p);
                if let Some(pointers) = index.query_by_project(p) {
                    println!("Found {} event(s):", pointers.len());
                    for ptr in pointers {
                        if let Ok(event) = index.read_event(ptr) {
                            print_event(&event);
                        } else {
                            println!(
                                "• Event ID: {} | Timestamp: {} (offset: {})",
                                ptr.event_id, ptr.ts, ptr.byte_offset
                            );
                        }
                    }
                } else {
                    println!("No events recorded for project '{}'.", p);
                }
                println!();
            }

            if let Some(ref fid) = file_id {
                ran_query = true;
                println!("--- File record lookup: '{}' ---", fid);
                if let Some(ptr) = index.query_by_file_id(fid) {
                    if let Ok(event) = index.read_event(ptr) {
                        if let Some(record) = event.files.iter().find(|f| f.file_id == *fid) {
                            println!("File Record Details:");
                            println!("  File ID:     {}", record.file_id);
                            println!("  File Name:   {}", record.file_name);
                            println!("  Action:      {}", record.action);
                            println!("  Size:        {} bytes", record.size_bytes);
                            println!(
                                "  SHA256:      {}",
                                if record.hash.is_empty() {
                                    "-"
                                } else {
                                    &record.hash
                                }
                            );
                            println!("\nParent Event:");
                            println!("  Event ID:    {}", event.event_id);
                            println!("  Timestamp:   {}", event.ts);
                            println!("  Project:     {}", event.project);
                            println!("  Device:      {}", event.device);
                            println!("  Agent:       {}", event.agent);
                            println!("  Summary:     {}", event.summary);
                        } else {
                            print_event(&event);
                        }
                    } else {
                        println!(
                            "File ID '{}' found in Event '{}' (Project: {}, Timestamp: {})",
                            fid, ptr.event_id, ptr.project, ptr.ts
                        );
                    }
                } else {
                    println!("No file record found with ID '{}'.", fid);
                }
                println!();
            }

            if let Some(ref fname) = filename {
                ran_query = true;
                println!("--- Events containing filename '{}' ---", fname);
                if let Some(pointers) = index.query_by_filename(fname) {
                    println!("Found {} event(s):", pointers.len());
                    for ptr in pointers {
                        if let Ok(event) = index.read_event(ptr) {
                            print_event(&event);
                        } else {
                            println!("• Event ID: {} | Timestamp: {}", ptr.event_id, ptr.ts);
                        }
                    }
                } else {
                    println!("No events found containing filename '{}'.", fname);
                }
                println!();
            }

            if let Some(ref pat) = fuzzy {
                ran_query = true;
                println!("--- Regex fuzzy search: '{}' ---", pat);
                let matched = MemIndex::fuzzy_search(&store, pat)?;
                if matched.is_empty() {
                    println!("No events matched pattern '{}'.", pat);
                } else {
                    println!("Found {} matching event(s):", matched.len());
                    for event in &matched {
                        print_event(event);
                    }
                }
                println!();
            }

            if !ran_query {
                eprintln!("Error: No query filter specified.");
                eprintln!(
                    "Please specify at least one filter: --project (-p), --file-id, --filename (-n), or --fuzzy."
                );
                std::process::exit(1);
            }
        }

        Commands::List { project } => {
            let root = resolve_root(cli.path)?;
            let store = MemStore::new(root);

            match project {
                None => {
                    let projects = store.list_projects()?;
                    if projects.is_empty() {
                        println!("No projects found in memory store.");
                    } else {
                        println!(
                            "{:<24} {:<8} {:<26} {}",
                            "PROJECT", "FILES", "CREATED AT", "DESCRIPTION"
                        );
                        println!("{}", "-".repeat(90));
                        for p in projects {
                            let meta = store.read_project_meta(&p).ok();
                            let file_count =
                                store.get_latest_files(&p).map(|f| f.len()).unwrap_or(0);
                            let created_at =
                                meta.as_ref().map(|m| m.created_at.as_str()).unwrap_or("-");
                            let desc = meta.as_ref().map(|m| m.description.as_str()).unwrap_or("-");
                            println!("{:<24} {:<8} {:<26} {}", p, file_count, created_at, desc);
                        }
                    }
                }
                Some(p) => {
                    let files = store.get_latest_files(&p)?;
                    if files.is_empty() {
                        println!("No files recorded for project '{}'.", p);
                    } else {
                        println!(
                            "{:<32} {:<10} {:<14} {:<22} {}",
                            "FILE NAME", "ACTION", "SIZE (BYTES)", "FILE ID", "HASH (SHA256)"
                        );
                        println!("{}", "-".repeat(110));
                        for file in files {
                            let hash_short = if file.hash.len() > 16 {
                                format!("{}...", &file.hash[..16])
                            } else if file.hash.is_empty() {
                                "-".to_string()
                            } else {
                                file.hash.clone()
                            };
                            println!(
                                "{:<32} {:<10} {:<14} {:<22} {}",
                                file.file_name,
                                file.action,
                                file.size_bytes,
                                file.file_id,
                                hash_short
                            );
                        }
                    }
                }
            }
        }

        Commands::Compact { project } => {
            let root = resolve_root(cli.path)?;
            let store = MemStore::new(root);
            println!("Compacting file records for project '{}'...", project);
            let report = compact_project_files(&store, &project)?;
            println!("Compaction completed for project '{}':", report.project);
            println!("  Records before: {}", report.records_before);
            println!("  Records after:  {}", report.records_after);
            println!(
                "  Records pruned: {}",
                report.records_before.saturating_sub(report.records_after)
            );
        }

        Commands::Sync => {
            let root = resolve_root(cli.path)?;
            println!("Checking Git repository status at: {}", root.display());
            let is_clean = git_status_clean(&root)?;
            if !is_clean {
                let dev = get_device_name();
                let now = chrono::Utc::now().to_rfc3339();
                let commit_msg = format!("Auto-sync from {} at {}", dev, now);
                println!(
                    "Repository is dirty. Staging and committing changes: \"{}\"",
                    commit_msg
                );
                git_add_all(&root)?;
                git_commit(&root, &commit_msg)?;
            } else {
                println!("Repository working directory is clean.");
            }

            println!("Pulling remote changes with rebase...");
            let pull_res = git_pull_rebase(&root);

            if pull_res.is_err() {
                let has_conflicts = git_has_conflicts(&root).unwrap_or(false);
                if has_conflicts {
                    eprintln!("\n⚠️  Warning: Git conflicts detected during pull --rebase!");
                    eprintln!("Please run `mem resolve` to automatically resolve JSONL conflict markers.");
                    return Ok(());
                }
                // pull failed but no conflicts — could be "no remote tracking branch" etc.
                // Log warning but continue to push
                eprintln!("⚠️  git pull --rebase returned an error (possibly no remote changes). Continuing...");
            }

            println!("Pushing changes to remote...");
            git_push(&root)?;
            println!("✅ Sync successful: repository is up-to-date and pushed to remote.");
        }

        Commands::Resolve => {
            let root = resolve_root(cli.path)?;
            println!("Resolving conflicts in JSONL files at: {}", root.display());
            let report = resolve_conflicts(&root)?;
            println!("Conflict Resolution Report:");
            println!("  Resolved files:      {}", report.resolved_files.len());
            println!("  Conflicts resolved:  {}", report.total_conflicts_resolved);
            println!("  Deduplicated items:  {}", report.deduplicated_items);
            if !report.resolved_files.is_empty() {
                println!("  Files updated:");
                for f in &report.resolved_files {
                    println!("    • {}", f.display());
                }
            }
        }

        Commands::Render { project } => {
            let root = resolve_root(cli.path)?;
            let store = MemStore::new(root);
            println!("Rendering README.md for project '{}'...", project);
            write_project_readme(&store, &project)?;
            let readme_path = store.project_dir(&project).join("README.md");
            println!(
                "Successfully generated README at: {}",
                readme_path.display()
            );
        }
    }

    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {}", err);
        std::process::exit(1);
    }
}
