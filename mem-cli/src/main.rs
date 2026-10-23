use std::error::Error;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use mem_core::compact::compact_project_files;
use mem_core::render::write_project_readme;
use mem_core::resolve::resolve_conflicts;
use mem_core::{
    default_device_name, resolve_store_root, sync_store, validate_project_name, Event, MemIndex,
    MemStore, SyncOutcome, STORE_MARKER,
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
            // Reject traversal/invalid names before touching the store so no
            // directory can ever be created outside `projects/`.
            validate_project_name(&project)?;

            let root = resolve_store_root(cli.path.as_deref())?;
            let store = MemStore::new(root);
            let dev = device.unwrap_or_else(default_device_name);

            // One lock-protected core call replaces the old
            // "read latest → decide Created/Modified → write meta → append"
            // sequence: the decision can no longer be based on a stale snapshot
            // and a metadata write failure aborts instead of being ignored.
            let event = store.record_file_event(&project, &summary, &dev, &agent, &files)?;

            // Absolute identity is kept only for paths outside both the store and
            // any git work tree; keep warning the user about those.
            for file in &event.files {
                if file.file_name.starts_with('/') {
                    eprintln!(
                        "Warning: '{}' is outside the memory store and outside any git repository; \
                         recording it under its absolute path, which will not match on other devices.",
                        file.file_name
                    );
                }
            }

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
            // Validate before doing any work so traversal-like names fail fast.
            if let Some(ref p) = project {
                validate_project_name(p)?;
            }

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
            if let Some(ref p) = project {
                validate_project_name(p)?;
            }

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
            validate_project_name(&project)?;

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
            println!("Synchronizing memory store at: {}", root.display());

            // One shared implementation with the MCP server: it holds the store
            // write lock, commits dirty state, skips the pull without an upstream
            // and refuses to push over a failed/inconsistent pull.
            match sync_store(&root, None)? {
                SyncOutcome::Success { steps } => {
                    for step in &steps {
                        println!("  • {}", step);
                    }
                    println!("✅ Sync successful: repository is up-to-date and pushed to remote.");
                }
                SyncOutcome::Conflict { steps, message } => {
                    for step in &steps {
                        println!("  • {}", step);
                    }
                    eprintln!(
                        "⚠️  git conflict detected; run `mem resolve`, then retry `mem sync`"
                    );
                    // Must exit non-zero: a conflicted sync is not a success.
                    return Err(format!("sync stopped on a git conflict: {}", message).into());
                }
            }
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
            validate_project_name(&project)?;

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
    // Rust ignores SIGPIPE, so a consumer closing the pipe early (`mem ... | head`)
    // would make `println!` panic with a broken-pipe error. Restoring the default
    // handler gives the conventional Unix behaviour: quiet termination.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    if let Err(err) = run() {
        eprintln!("Error: {}", err);
        std::process::exit(1);
    }
}
