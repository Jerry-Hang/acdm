use std::io::{self, BufRead, Write};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use mem_core::compact::compact_project_files;
use mem_core::git::{
    git_add_all, git_commit, git_has_conflicts, git_pull_rebase, git_push, git_status_clean,
};
use mem_core::render::write_project_readme;
use mem_core::resolve::resolve_conflicts;
use mem_core::{generate_id, Event, FileRecord, MemIndex, MemStore, ProjectMeta};

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

fn success_response(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    }
}

fn error_response(id: Value, code: i32, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
        }),
    }
}

fn mcp_text_content(text: impl Into<String>) -> Value {
    serde_json::json!({
        "content": [
            {
                "type": "text",
                "text": text.into()
            }
        ]
    })
}

fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();

    let mut days = (secs / 86400) as i64;
    let day_secs = (secs % 86400) as u32;
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;

    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hour, minute, second)
}

fn get_default_device() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

fn get_tools_list() -> Value {
    serde_json::json!([
        {
            "name": "mem_get_context",
            "description": "Get project context (metadata, README, recent events) or list all projects if not specified.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Optional project name. If omitted, returns list of all projects."
                    }
                }
            }
        },
        {
            "name": "mem_add_event",
            "description": "Record a new memory event with associated file changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project name"
                    },
                    "summary": {
                        "type": "string",
                        "description": "Summary of what happened"
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name (optional, defaults to current hostname)"
                    },
                    "agent": {
                        "type": "string",
                        "description": "Agent name (optional, defaults to 'mcp-agent')"
                    },
                    "files": {
                        "type": "array",
                        "description": "List of affected files",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file_name": {
                                    "type": "string",
                                    "description": "Relative file path"
                                },
                                "action": {
                                    "type": "string",
                                    "enum": ["Created", "Modified", "Deleted"],
                                    "description": "Action performed on the file"
                                },
                                "size_bytes": {
                                    "type": "integer",
                                    "description": "File size in bytes"
                                },
                                "hash": {
                                    "type": "string",
                                    "description": "SHA256 hex digest"
                                }
                            },
                            "required": ["file_name", "action"]
                        }
                    }
                },
                "required": ["project", "summary"]
            }
        },
        {
            "name": "mem_query",
            "description": "Query events by project, file_id, filename, or regex/fuzzy search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "by": {
                        "type": "string",
                        "enum": ["project", "file_id", "filename", "fuzzy"],
                        "description": "Query index dimension"
                    },
                    "value": {
                        "type": "string",
                        "description": "Search term, identifier, or regex pattern"
                    }
                },
                "required": ["by", "value"]
            }
        },
        {
            "name": "mem_list_projects",
            "description": "List all projects in the memory store.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "mem_compact",
            "description": "Compact project files.jsonl by keeping only the latest record per filename.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project name to compact"
                    }
                },
                "required": ["project"]
            }
        },
        {
            "name": "mem_sync",
            "description": "Synchronize memory store with remote git repository (commit dirty changes, pull with rebase, push).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Optional commit message"
                    }
                }
            }
        },
        {
            "name": "mem_resolve",
            "description": "Run automatic git conflict resolution on all JSONL files.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "mem_render",
            "description": "Generate or update README.md for a project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project name to render README for"
                    }
                },
                "required": ["project"]
            }
        }
    ])
}

// ----------------- Tool Handlers -----------------

#[derive(Debug, Deserialize, Default)]
struct GetContextArgs {
    project: Option<String>,
}

fn handle_get_context(store: &MemStore, args: GetContextArgs) -> Result<Value, String> {
    match args.project.filter(|p| !p.trim().is_empty()) {
        None => {
            let projects = store
                .list_projects()
                .map_err(|e| format!("Failed to list projects: {}", e))?;
            if projects.is_empty() {
                return Ok(mcp_text_content("No projects found in memory store."));
            }
            let mut out = format!("Found {} project(s):\n", projects.len());
            for proj in projects {
                match store.read_project_meta(&proj) {
                    Ok(meta) => {
                        out.push_str(&format!(
                            "- {}: {} (created: {})\n",
                            meta.name, meta.description, meta.created_at
                        ));
                        if let Some(ref url) = meta.github_url {
                            out.push_str(&format!("  GitHub: {}\n", url));
                        }
                    }
                    Err(_) => {
                        out.push_str(&format!("- {} (no metadata found)\n", proj));
                    }
                }
            }
            Ok(mcp_text_content(out.trim_end()))
        }
        Some(project) => {
            let mut out = String::new();

            // 1. Meta
            match store.read_project_meta(&project) {
                Ok(meta) => {
                    out.push_str(&format!("=== Project: {} ===\n", meta.name));
                    if !meta.description.is_empty() {
                        out.push_str(&format!("Description: {}\n", meta.description));
                    }
                    out.push_str(&format!("Created At: {}\n", meta.created_at));
                    if let Some(ref url) = meta.github_url {
                        out.push_str(&format!("GitHub: {}\n", url));
                    }
                }
                Err(e) => {
                    out.push_str(&format!("=== Project: {} ===\n", project));
                    out.push_str(&format!("Metadata: (not found or error: {})\n", e));
                }
            }

            // 2. README
            out.push_str("\n--- README.md ---\n");
            let readme_path = store.project_dir(&project).join("README.md");
            if readme_path.exists() {
                match std::fs::read_to_string(&readme_path) {
                    Ok(content) => out.push_str(&content),
                    Err(e) => out.push_str(&format!("(Error reading README.md: {})\n", e)),
                }
            } else {
                out.push_str("(No README.md available)\n");
            }

            // 3. Last 5 events
            out.push_str("\n--- Recent Events (last 5) ---\n");
            let all_events = store.read_all_events().unwrap_or_default();
            let mut proj_events: Vec<Event> =
                all_events.into_iter().filter(|e| e.project == project).collect();
            proj_events.sort_by(|a, b| a.ts.cmp(&b.ts));

            let recent = if proj_events.len() > 5 {
                proj_events.split_off(proj_events.len() - 5)
            } else {
                proj_events
            };

            if recent.is_empty() {
                out.push_str("(No events found for this project)\n");
            } else {
                for event in recent.into_iter().rev() {
                    out.push_str(&format!(
                        "[{}] ({}) {}\n  Device: {} | Agent: {} | Files: {}\n",
                        event.ts,
                        event.event_id,
                        event.summary,
                        event.device,
                        event.agent,
                        event.files.len()
                    ));
                    for f in &event.files {
                        out.push_str(&format!(
                            "    - {} [{}] ({} bytes)\n",
                            f.file_name, f.action, f.size_bytes
                        ));
                    }
                }
            }

            Ok(mcp_text_content(out.trim_end()))
        }
    }
}

#[derive(Debug, Deserialize)]
struct AddEventFileArg {
    file_name: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    size_bytes: Option<u64>,
    #[serde(default)]
    hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddEventArgs {
    project: String,
    summary: String,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    files: Option<Vec<AddEventFileArg>>,
}

fn handle_add_event(store: &MemStore, args: AddEventArgs) -> Result<Value, String> {
    let event_id = generate_id();
    let ts = now_rfc3339();
    let device = args
        .device
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(get_default_device);
    let agent = args
        .agent
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mcp-agent".to_string());

    let raw_files = args.files.unwrap_or_default();
    let mut files = Vec::with_capacity(raw_files.len());
    for f in raw_files {
        let action = f.action.unwrap_or_else(|| "Modified".to_string());
        let file_id = generate_id();
        files.push(FileRecord {
            file_id,
            file_name: f.file_name,
            action,
            size_bytes: f.size_bytes.unwrap_or(0),
            hash: f.hash.unwrap_or_default(),
        });
    }

    // Auto-create minimal project meta if it doesn't exist
    if !store.project_exists(&args.project) {
        let meta = ProjectMeta {
            name: args.project.clone(),
            description: format!("Project {}", args.project),
            github_url: None,
            created_at: ts.clone(),
        };
        let _ = store.write_project_meta(&args.project, &meta);
    }

    let file_count = files.len();
    let event = Event {
        event_id: event_id.clone(),
        ts,
        device,
        agent,
        project: args.project,
        summary: args.summary,
        files,
    };

    store
        .append_event(&event)
        .map_err(|e| format!("Failed to append event: {}", e))?;

    Ok(mcp_text_content(format!(
        "Event {} recorded with {} files",
        event_id, file_count
    )))
}

#[derive(Debug, Deserialize)]
struct QueryArgs {
    by: String,
    value: String,
}

fn format_event_full(out: &mut String, event: &Event) {
    out.push_str(&format!(
        "[{}] ({}) Project: {}\nSummary: {}\nDevice: {} | Agent: {}\n",
        event.ts, event.event_id, event.project, event.summary, event.device, event.agent
    ));
    if !event.files.is_empty() {
        out.push_str("Files:\n");
        for f in &event.files {
            out.push_str(&format!(
                "  - {} [{}] size: {} bytes, hash: {}\n",
                f.file_name, f.action, f.size_bytes, f.hash
            ));
        }
    }
    out.push('\n');
}

fn handle_query(store: &MemStore, args: QueryArgs) -> Result<Value, String> {
    let index = MemIndex::build(store)
        .map_err(|e| format!("Failed to build memory index: {}", e))?;

    match args.by.to_lowercase().as_str() {
        "project" => {
            match index.query_by_project(&args.value) {
                Some(pointers) if !pointers.is_empty() => {
                    let mut out = format!(
                        "Found {} event(s) for project '{}':\n\n",
                        pointers.len(),
                        args.value
                    );
                    for ptr in pointers {
                        if let Ok(ev) = index.read_event(ptr) {
                            format_event_full(&mut out, &ev);
                        } else {
                            out.push_str(&format!(
                                "[{}] ({}) Project: {}\n\n",
                                ptr.ts, ptr.event_id, ptr.project
                            ));
                        }
                    }
                    Ok(mcp_text_content(out.trim_end()))
                }
                _ => Ok(mcp_text_content(format!(
                    "No events found for project '{}'.",
                    args.value
                ))),
            }
        }
        "file_id" => {
            match index.query_by_file_id(&args.value) {
                Some(ptr) => {
                    let mut out = format!("Found event for file_id '{}':\n\n", args.value);
                    if let Ok(ev) = index.read_event(ptr) {
                        format_event_full(&mut out, &ev);
                    } else {
                        out.push_str(&format!(
                            "[{}] ({}) Project: {}\n\n",
                            ptr.ts, ptr.event_id, ptr.project
                        ));
                    }
                    Ok(mcp_text_content(out.trim_end()))
                }
                None => Ok(mcp_text_content(format!(
                    "No event found for file_id '{}'.",
                    args.value
                ))),
            }
        }
        "filename" => {
            match index.query_by_filename(&args.value) {
                Some(pointers) if !pointers.is_empty() => {
                    let mut out = format!(
                        "Found {} event(s) affecting file '{}':\n\n",
                        pointers.len(),
                        args.value
                    );
                    for ptr in pointers {
                        if let Ok(ev) = index.read_event(ptr) {
                            format_event_full(&mut out, &ev);
                        } else {
                            out.push_str(&format!(
                                "[{}] ({}) Project: {}\n\n",
                                ptr.ts, ptr.event_id, ptr.project
                            ));
                        }
                    }
                    Ok(mcp_text_content(out.trim_end()))
                }
                _ => Ok(mcp_text_content(format!(
                    "No events found for filename '{}'.",
                    args.value
                ))),
            }
        }
        "fuzzy" => {
            let events = MemIndex::fuzzy_search(store, &args.value)
                .map_err(|e| format!("Fuzzy search failed: {}", e))?;
            if events.is_empty() {
                Ok(mcp_text_content(format!(
                    "No events matched pattern '{}'.",
                    args.value
                )))
            } else {
                let mut out = format!(
                    "Found {} matching event(s) for pattern '{}':\n\n",
                    events.len(),
                    args.value
                );
                for ev in events {
                    format_event_full(&mut out, &ev);
                }
                Ok(mcp_text_content(out.trim_end()))
            }
        }
        other => Err(format!(
            "Invalid query dimension 'by': '{}'. Allowed values: 'project', 'file_id', 'filename', 'fuzzy'.",
            other
        )),
    }
}

fn handle_list_projects(store: &MemStore) -> Result<Value, String> {
    let projects = store
        .list_projects()
        .map_err(|e| format!("Failed to list projects: {}", e))?;
    if projects.is_empty() {
        return Ok(mcp_text_content("No projects found in memory store."));
    }
    let mut out = format!("Projects in memory store ({}):\n", projects.len());
    for proj in projects {
        match store.read_project_meta(&proj) {
            Ok(meta) => {
                out.push_str(&format!("- {}: {}\n", meta.name, meta.description));
            }
            Err(_) => {
                out.push_str(&format!("- {}\n", proj));
            }
        }
    }
    Ok(mcp_text_content(out.trim_end()))
}

#[derive(Debug, Deserialize)]
struct CompactArgs {
    project: String,
}

fn handle_compact(store: &MemStore, args: CompactArgs) -> Result<Value, String> {
    let report = compact_project_files(store, &args.project)
        .map_err(|e| format!("Compaction failed for project '{}': {}", args.project, e))?;

    let reduced = report.records_before.saturating_sub(report.records_after);
    Ok(mcp_text_content(format!(
        "Compacted project '{}': before: {} records, after: {} records (reduced by {} duplicate/old records).",
        args.project, report.records_before, report.records_after, reduced
    )))
}

#[derive(Debug, Deserialize, Default)]
struct SyncArgs {
    #[serde(default)]
    message: Option<String>,
}

fn handle_sync(store: &MemStore, args: Option<SyncArgs>) -> Result<Value, String> {
    let mut steps = Vec::new();

    let is_clean = git_status_clean(&store.root)
        .map_err(|e| format!("Git status check failed: {}", e))?;

    if !is_clean {
        git_add_all(&store.root)
            .map_err(|e| format!("git add -A failed: {}", e))?;

        let commit_msg = args
            .and_then(|a| a.message)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "Auto-sync memory store".to_string());

        git_commit(&store.root, &commit_msg)
            .map_err(|e| format!("git commit failed: {}", e))?;
        steps.push(format!("Committed local changes (\"{}\")", commit_msg));
    } else {
        steps.push("Working directory clean".to_string());
    }

    if let Err(pull_err) = git_pull_rebase(&store.root) {
        if git_has_conflicts(&store.root).unwrap_or(false) {
            return Err(format!(
                "Git conflict detected during pull --rebase: {}. Please call the `mem_resolve` tool to automatically deduplicate and merge the JSONL records.",
                pull_err
            ));
        }
        // Non-conflict error (e.g. no upstream changes or network blip), log note and proceed
        steps.push(format!("pull note: {}", pull_err));
    } else {
        steps.push("Pulled remote changes with rebase".to_string());
    }

    git_push(&store.root)
        .map_err(|e| format!("git push failed: {}", e))?;
    steps.push("Pushed to remote successfully".to_string());

    Ok(mcp_text_content(format!(
        "Sync completed successfully: {}",
        steps.join("; ")
    )))
}

fn handle_resolve(store: &MemStore) -> Result<Value, String> {
    let report = resolve_conflicts(&store.root)
        .map_err(|e| format!("Conflict resolution failed: {}", e))?;

    Ok(mcp_text_content(format!(
        "Conflict resolution completed: {} files resolved, {} conflict markers resolved, {} duplicate items removed.",
        report.resolved_files.len(),
        report.total_conflicts_resolved,
        report.deduplicated_items
    )))
}

#[derive(Debug, Deserialize)]
struct RenderArgs {
    project: String,
}

fn handle_render(store: &MemStore, args: RenderArgs) -> Result<Value, String> {
    write_project_readme(store, &args.project)
        .map_err(|e| format!("Failed to render README for project '{}': {}", args.project, e))?;

    Ok(mcp_text_content(format!(
        "Successfully rendered README for project '{}'.",
        args.project
    )))
}

// ----------------- Request Dispatcher -----------------

fn handle_request(store: &MemStore, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
    // Protocol notifications: do not send any response
    if request.method == "notifications/initialized"
        || request.method == "initialized"
        || request.method.starts_with("notifications/")
    {
        return None;
    }

    let id = match request.id {
        Some(id) => id,
        None => return None,
    };

    match request.method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "mem-mcp",
                    "version": "0.1.0"
                }
            });
            Some(success_response(id, result))
        }
        "ping" => Some(success_response(id, serde_json::json!({}))),
        "tools/list" => {
            Some(success_response(id, serde_json::json!({ "tools": get_tools_list() })))
        }
        "tools/call" => {
            let call_params: ToolCallParams = match request.params {
                Some(p) => match serde_json::from_value(p) {
                    Ok(cp) => cp,
                    Err(e) => {
                        return Some(error_response(
                            id,
                            -32602,
                            format!("Invalid tool call parameters: {}", e),
                        ));
                    }
                },
                None => {
                    return Some(error_response(
                        id,
                        -32602,
                        "Missing parameters for tools/call".to_string(),
                    ));
                }
            };

            let args_val = call_params
                .arguments
                .unwrap_or_else(|| Value::Object(Default::default()));

            let call_result = match call_params.name.as_str() {
                "mem_get_context" => match serde_json::from_value::<GetContextArgs>(args_val) {
                    Ok(args) => handle_get_context(store, args),
                    Err(e) => Err(format!("Invalid arguments for mem_get_context: {}", e)),
                },
                "mem_add_event" => match serde_json::from_value::<AddEventArgs>(args_val) {
                    Ok(args) => handle_add_event(store, args),
                    Err(e) => Err(format!("Invalid arguments for mem_add_event: {}", e)),
                },
                "mem_query" => match serde_json::from_value::<QueryArgs>(args_val) {
                    Ok(args) => handle_query(store, args),
                    Err(e) => Err(format!("Invalid arguments for mem_query: {}", e)),
                },
                "mem_list_projects" => handle_list_projects(store),
                "mem_compact" => match serde_json::from_value::<CompactArgs>(args_val) {
                    Ok(args) => handle_compact(store, args),
                    Err(e) => Err(format!("Invalid arguments for mem_compact: {}", e)),
                },
                "mem_sync" => {
                    let sync_args = serde_json::from_value::<SyncArgs>(args_val).ok();
                    handle_sync(store, sync_args)
                }
                "mem_resolve" => handle_resolve(store),
                "mem_render" => match serde_json::from_value::<RenderArgs>(args_val) {
                    Ok(args) => handle_render(store, args),
                    Err(e) => Err(format!("Invalid arguments for mem_render: {}", e)),
                },
                unknown => {
                    return Some(error_response(
                        id,
                        -32601,
                        format!("Unknown tool: {}", unknown),
                    ));
                }
            };

            match call_result {
                Ok(val) => Some(success_response(id, val)),
                Err(err_msg) => Some(error_response(id, -32603, err_msg)),
            }
        }
        other => Some(error_response(
            id,
            -32601,
            format!("Method not found: {}", other),
        )),
    }
}

fn main() {
    let current_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("Failed to get current working directory: {}", e);
            std::process::exit(1);
        }
    };

    let store = MemStore::new(current_dir);
    if let Err(e) = store.init() {
        eprintln!("Notice: store initialization check: {}", e);
    }

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error reading line from stdin: {}", e);
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(e) => {
                let err_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                    }),
                };
                if let Ok(out) = serde_json::to_string(&err_resp) {
                    let mut lock = stdout.lock();
                    let _ = writeln!(lock, "{}", out);
                    let _ = lock.flush();
                }
                continue;
            }
        };

        if let Some(response) = handle_request(&store, request) {
            match serde_json::to_string(&response) {
                Ok(out) => {
                    let mut lock = stdout.lock();
                    let _ = writeln!(lock, "{}", out);
                    let _ = lock.flush();
                }
                Err(e) => {
                    eprintln!("Failed to serialize JSON-RPC response: {}", e);
                }
            }
        }
    }
}
