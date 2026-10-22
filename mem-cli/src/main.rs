use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use mem_core::compact::compact_project_files;
use mem_core::git::{
    git_add_all, git_commit, git_has_conflicts, git_pull_rebase, git_push, git_status_clean,
};
use mem_core::render::write_project_readme;
use mem_core::resolve::resolve_conflicts;
use mem_core::{
    compute_sha256, default_device_name, generate_id, normalize_file_name, now_rfc3339,
    resolve_store_root, Event, FileRecord, MemIndex, MemStore, ProjectMeta, STORE_MARKER,
};

#[derive(Parser)]
#[command(name = "mem", about = "Agent Cross-Device Memory System CLI", version)]
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

        /// Find events recorded for this exact file name (use the stored relative path)
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
            println!(
                "  └── {}          (Store marker file; lets the CLI/MCP find the store from any subdirectory)",
                STORE_MARKER
            );
        }

        Commands::Add {
            project,
            summary,
            device,
            agent,
            files,
        } => {
            let root = resolve_store_root(cli.path.as_deref())?;
            let store = MemStore::new(root);

            let existing_files = store.get_latest_files(&project).unwrap_or_default();
            let mut file_records = Vec::new();

            for file_path in &files {
                // File identity is normalised (store-relative, then git-relative) so
                // the same file matches across devices; existence uses the raw path.
                let file_name = normalize_file_name(file_path, &store.root);
                if Path::new(&file_name).is_absolute() {
                    eprintln!(
                        "Warning: '{}' is outside the memory store and outside any git repository; \
                         recording it under its absolute path, which will not match on other devices.",
                        file_path.display()
                    );
                }

                let exists = file_path.exists();
                let (action, size_bytes, hash) = if !exists {
                    ("Deleted".to_string(), 0u64, String::new())
                } else {
                    let bytes = fs::read(file_path).map_err(|e| {
                        format!("Failed to read file '{}': {}", file_path.display(), e)
                    })?;
                    let size = bytes.len() as u64;
                    let hash = compute_sha256(&bytes);
                    let is_known = existing_files.iter().any(|f| f.file_name == file_name);
                    let act = if is_known { "Modified" } else { "Created" };
                    (act.to_string(), size, hash)
                };

                let file_id = generate_id();
                file_records.push(FileRecord {
                    file_id,
                    file_name,
                    action,
                    size_bytes,
                    hash,
                });
            }

            let ts = now_rfc3339();
            let event_id = generate_id();
            let dev_name = device.unwrap_or_else(default_device_name);

            let event = Event {
                event_id,
                ts: ts.clone(),
                device: dev_name,
                agent,
                project: project.clone(),
                summary,
                files: file_records,
            };

            // Check the metadata itself, not the directory: `mem render` may
            // already have created `projects/<name>/`, which would otherwise make
            // `add` believe the project was fully initialised.
            if store.read_project_meta(&project).is_err() {
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
                    if file.hash.is_empty() {
                        "-"
                    } else {
                        &file.hash
                    }
                );
            }
        }

        Commands::Query {
            project,
            file_id,
            filename,
            fuzzy,
        } => {
            let root = resolve_store_root(cli.path.as_deref())?;
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
            let root = resolve_store_root(cli.path.as_deref())?;
            let store = MemStore::new(root);

            match project {
                None => {
                    let projects = store.list_projects()?;
                    if projects.is_empty() {
                        println!("No projects found in memory store.");
                    } else {
                        println!(
                            "{:<24} {:<8} {:<32} DESCRIPTION",
                            "PROJECT", "FILES", "CREATED AT"
                        );
                        println!("{}", "-".repeat(96));
                        for p in projects {
                            let meta = store.read_project_meta(&p).ok();
                            let file_count =
                                store.get_latest_files(&p).map(|f| f.len()).unwrap_or(0);
                            let created_at =
                                meta.as_ref().map(|m| m.created_at.as_str()).unwrap_or("-");
                            let desc = meta.as_ref().map(|m| m.description.as_str()).unwrap_or("-");
                            println!("{:<24} {:<8} {:<32} {}", p, file_count, created_at, desc);
                        }
                    }
                }
                Some(p) => {
                    let files = store.get_latest_files(&p)?;
                    if files.is_empty() {
                        println!("No files recorded for project '{}'.", p);
                    } else {
                        println!(
                            "{:<32} {:<10} {:<14} {:<37} HASH (SHA256)",
                            "FILE NAME", "ACTION", "SIZE (BYTES)", "FILE ID"
                        );
                        println!("{}", "-".repeat(125));
                        for file in files {
                            let hash_short = if file.hash.len() > 16 {
                                format!("{}...", &file.hash[..16])
                            } else if file.hash.is_empty() {
                                "-".to_string()
                            } else {
                                file.hash.clone()
                            };
                            println!(
                                "{:<32} {:<10} {:<14} {:<37} {}",
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
            let root = resolve_store_root(cli.path.as_deref())?;
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
            let root = resolve_store_root(cli.path.as_deref())?;
            println!("Checking Git repository status at: {}", root.display());
            let is_clean = git_status_clean(&root)?;
            if !is_clean {
                let dev = default_device_name();
                let now = now_rfc3339();
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
                    eprintln!(
                        "Please run `mem resolve` to automatically resolve JSONL conflict markers."
                    );
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
            let root = resolve_store_root(cli.path.as_deref())?;
            println!("Resolving conflicts in JSONL files at: {}", root.display());
            let report = resolve_conflicts(&root)?;
            println!("Conflict Resolution Report:");
            println!("  Resolved files:      {}", report.resolved_files.len());
            println!("  Conflicts resolved:  {}", report.total_conflicts_resolved);
            println!("  Deduplicated items:  {}", report.deduplicated_items);
            println!("  Lines preserved:     {}", report.lines_preserved);
            if !report.resolved_files.is_empty() {
                println!("  Files updated:");
                for f in &report.resolved_files {
                    println!("    • {}", f.display());
                }
            }
        }

        Commands::Render { project } => {
            let root = resolve_store_root(cli.path.as_deref())?;
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
