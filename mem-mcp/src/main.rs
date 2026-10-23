use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use mem_core::compact::compact_project_files;
use mem_core::render::write_project_readme;
use mem_core::resolve::resolve_conflicts;
use mem_core::{
    default_device_name, generate_id, normalize_file_name, now_rfc3339, resolve_store_root,
    sync_store, Event, FileRecord, MemIndex, MemStore, ProjectMeta, SyncOutcome,
};

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    /// JSON-RPC version; validated against `"2.0"` by the dispatcher.
    #[serde(default)]
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

/// A tool-level failure.
///
/// MCP recommends reporting business errors inside a successful JSON-RPC
/// response with `isError: true`, so the calling agent can read the message and
/// correct itself; -32603 is reserved for genuine server faults.
fn mcp_error_content(text: impl Into<String>) -> Value {
    serde_json::json!({
        "content": [
            {
                "type": "text",
                "text": text.into()
            }
        ],
        "isError": true
    })
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
                            "required": ["file_name"]
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

            // 3. Last 5 events. The store returns them oldest-first and already
            // handles month-file boundaries plus legacy timestamp formats.
            out.push_str("\n--- Recent Events (last 5) ---\n");
            let recent = store
                .read_latest_events_for_project(&project, 5)
                .map_err(|e| format!("Failed to read recent events: {}", e))?;

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

/// File action values accepted from MCP clients.
///
/// Deserializing into an enum makes anything but the three exact, case-sensitive
/// values a tool-argument error (-32602) instead of silently storing junk.
#[derive(Debug, Clone, Copy, Deserialize)]
enum FileAction {
    #[serde(rename = "Created")]
    Created,
    #[serde(rename = "Modified")]
    Modified,
    #[serde(rename = "Deleted")]
    Deleted,
}

impl FileAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Modified => "Modified",
            Self::Deleted => "Deleted",
        }
    }
}

#[derive(Debug, Deserialize)]
struct AddEventFileArg {
    file_name: String,
    #[serde(default)]
    action: Option<FileAction>,
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
    // Fail fast on an unsafe project name instead of waiting for the append to
    // reject it: the dispatcher turns this error into an isError tool result.
    if let Err(error) = mem_core::validate_project_name(&args.project) {
        return Err(format!("Invalid project: {}", error));
    }

    let event_id = generate_id();
    let ts = now_rfc3339();
    let device = args
        .device
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(default_device_name);
    let agent = args
        .agent
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mcp-agent".to_string());

    let raw_files = args.files.unwrap_or_default();
    let mut files = Vec::with_capacity(raw_files.len());
    for f in raw_files {
        let action = f
            .action
            .map(|action| action.as_str().to_string())
            .unwrap_or_else(|| "Modified".to_string());
        let file_id = generate_id();
        // MCP clients usually send absolute paths. Store the canonical,
        // store-relative identity so that `mem_query by=filename` (which indexes
        // relative names) and cross-device matching keep working.
        let file_name = normalize_file_name(Path::new(&f.file_name), &store.root);
        files.push(FileRecord {
            file_id,
            file_name,
            action,
            size_bytes: f.size_bytes.unwrap_or(0),
            hash: f.hash.unwrap_or_default(),
        });
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

    // Bootstrap minimal project metadata in the same critical section as the
    // append: a metadata failure now aborts the tool call instead of being
    // swallowed, while projects whose directory was created by `mem_render`
    // without a meta.json are still healed correctly.
    let meta = ProjectMeta {
        name: event.project.clone(),
        description: format!("Project {}", event.project),
        github_url: None,
        created_at: event.ts.clone(),
    };
    store
        .append_event_with_meta(&event, Some(&meta))
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
    let index =
        MemIndex::build(store).map_err(|e| format!("Failed to build memory index: {}", e))?;

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

fn handle_sync(store: &MemStore, args: SyncArgs) -> Result<Value, String> {
    // Shared with the CLI: locks the store, commits dirty state, skips the pull
    // when there is no upstream, and refuses to push after a failed pull.
    match sync_store(&store.root, args.message.as_deref()) {
        Ok(SyncOutcome::Success { steps }) => Ok(mcp_text_content(format!(
            "Sync completed successfully: {}",
            steps.join("; ")
        ))),
        Ok(SyncOutcome::Conflict { steps, message }) => Err(format!(
            "Sync stopped on a git conflict: {}. Completed steps: {}. \
             Resolve the JSONL conflicts with the `mem_resolve` tool, then retry \
             (run mem_resolve then retry mem_sync).",
            message,
            steps.join("; ")
        )),
        Err(error) => Err(format!(
            "Sync failed: {}. Check the git remote/upstream configuration, then run \
             mem_sync again.",
            error
        )),
    }
}

fn handle_resolve(store: &MemStore) -> Result<Value, String> {
    let report =
        resolve_conflicts(&store.root).map_err(|e| format!("Conflict resolution failed: {}", e))?;

    Ok(mcp_text_content(format!(
        "Conflict resolution completed: {} files resolved, {} conflict markers resolved, {} duplicate items removed, {} non-record lines preserved.",
        report.resolved_files.len(),
        report.total_conflicts_resolved,
        report.deduplicated_items,
        report.lines_preserved
    )))
}

#[derive(Debug, Deserialize)]
struct RenderArgs {
    project: String,
}

fn handle_render(store: &MemStore, args: RenderArgs) -> Result<Value, String> {
    write_project_readme(store, &args.project).map_err(|e| {
        format!(
            "Failed to render README for project '{}': {}",
            args.project, e
        )
    })?;

    Ok(mcp_text_content(format!(
        "Successfully rendered README for project '{}'.",
        args.project
    )))
}

// ----------------- Request Dispatcher -----------------

/// Deserialize one tool's arguments, mapping schema violations to JSON-RPC
/// `-32602` (Invalid params) instead of an internal error.
fn parse_tool_args<T: DeserializeOwned>(
    id: &Value,
    tool: &str,
    arguments: Value,
) -> Result<T, JsonRpcResponse> {
    serde_json::from_value(arguments).map_err(|error| {
        error_response(
            id.clone(),
            -32602,
            format!("Invalid arguments for {}: {}", tool, error),
        )
    })
}

fn handle_request(store: &MemStore, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
    // JSON-RPC 2.0 requires the version field; anything else is an Invalid Request.
    if request.jsonrpc != "2.0" {
        return Some(error_response(
            request.id.unwrap_or(Value::Null),
            -32600,
            format!(
                "Invalid Request: expected jsonrpc \"2.0\", got {:?}",
                request.jsonrpc
            ),
        ));
    }

    // JSON-RPC 2.0: a request without an id is a notification and gets no
    // response, while a request *with* an id must always be answered. Unknown
    // `notifications/*` methods therefore fall through to the -32601 branch
    // instead of leaving the client waiting forever.
    let id = request.id?;

    match request.method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "mem-mcp",
                    "version": "0.1.2"
                }
            });
            Some(success_response(id, result))
        }
        "ping" => Some(success_response(id, serde_json::json!({}))),
        "tools/list" => Some(success_response(
            id,
            serde_json::json!({ "tools": get_tools_list() }),
        )),
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
                "mem_get_context" => {
                    match parse_tool_args::<GetContextArgs>(&id, "mem_get_context", args_val) {
                        Ok(args) => handle_get_context(store, args),
                        Err(response) => return Some(response),
                    }
                }
                "mem_add_event" => {
                    match parse_tool_args::<AddEventArgs>(&id, "mem_add_event", args_val) {
                        Ok(args) => handle_add_event(store, args),
                        Err(response) => return Some(response),
                    }
                }
                "mem_query" => match parse_tool_args::<QueryArgs>(&id, "mem_query", args_val) {
                    Ok(args) => handle_query(store, args),
                    Err(response) => return Some(response),
                },
                "mem_list_projects" => handle_list_projects(store),
                "mem_compact" => match parse_tool_args::<CompactArgs>(&id, "mem_compact", args_val)
                {
                    Ok(args) => handle_compact(store, args),
                    Err(response) => return Some(response),
                },
                "mem_sync" => match parse_tool_args::<SyncArgs>(&id, "mem_sync", args_val) {
                    Ok(args) => handle_sync(store, args),
                    Err(response) => return Some(response),
                },
                "mem_resolve" => handle_resolve(store),
                "mem_render" => match parse_tool_args::<RenderArgs>(&id, "mem_render", args_val) {
                    Ok(args) => handle_render(store, args),
                    Err(response) => return Some(response),
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
                Ok(result) => Some(success_response(id, result)),
                // Business failures (git conflict, invalid query dimension, ...)
                // are reported as isError results in a successful response so the
                // calling agent can read the message and self-correct.
                Err(message) => Some(success_response(id, mcp_error_content(message))),
            }
        }
        other => Some(error_response(
            id,
            -32601,
            format!("Method not found: {}", other),
        )),
    }
}

// ----------------- Command-line arguments -----------------

/// Parse the MCP server's start-up arguments.
///
/// MCP clients configure the server as a command plus arguments. The only flag
/// understood here is `--path <dir>` (also accepted as `--path=<dir>`), which
/// pins the memory store root instead of letting it be discovered from the
/// working directory. Unknown arguments are rejected rather than ignored: a
/// typo silently using the wrong store is worse than refusing to start.
fn parse_store_path_arg<I>(args: I) -> Result<Option<PathBuf>, String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut args = args.into_iter();
    let mut explicit: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        let text = arg.to_str();
        let inline_value = text
            .and_then(|text| text.strip_prefix("--path="))
            .map(PathBuf::from);
        let is_flag = text == Some("--path");

        if !is_flag && inline_value.is_none() {
            return Err(format!(
                "unrecognized argument {:?}; usage: mem-mcp [--path <store-root>]",
                arg
            ));
        }

        let value = match inline_value {
            Some(value) => value,
            None => args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "--path requires a directory argument".to_string())?,
        };
        if value.as_os_str().is_empty() {
            return Err("--path requires a non-empty directory argument".to_string());
        }
        explicit = Some(value);
    }

    Ok(explicit)
}

/// Maximum size of a single JSON-RPC request line (16 MiB).
///
/// Without a cap a client that never sends a newline could make the server
/// buffer unbounded input and exhaust its memory.
const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024 * 1024;

/// One result of reading a request line from stdin.
enum LimitedLine {
    /// A complete line, without its trailing newline.
    Line(Vec<u8>),
    /// The line exceeded the cap; it was discarded up to and including its newline.
    TooLong,
    /// End of input.
    Eof,
}

/// Read one newline-terminated line without ever buffering more than `limit` bytes.
fn read_limited_line<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<LimitedLine> {
    let mut line = Vec::new();
    {
        // One byte more than the cap is enough to detect a line that does not fit.
        let mut limited = std::io::Read::take(&mut *reader, limit as u64 + 1);
        limited.read_until(b'\n', &mut line)?;
    }

    if line.is_empty() {
        return Ok(LimitedLine::Eof);
    }

    let newline_terminated = line.ends_with(b"\n");
    if !newline_terminated && line.len() > limit {
        // Discard the rest of the oversized line so the stream stays in sync.
        discard_until_newline(reader)?;
        return Ok(LimitedLine::TooLong);
    }
    if newline_terminated {
        line.pop();
    }
    Ok(LimitedLine::Line(line))
}

/// Consume stdin up to and including the next newline (or EOF).
fn discard_until_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        match available.iter().position(|&byte| byte == b'\n') {
            Some(position) => {
                reader.consume(position + 1);
                return Ok(());
            }
            None => {
                let consumed = available.len();
                reader.consume(consumed);
            }
        }
    }
}

/// Write one JSON-RPC response line and flush it.
fn write_response(stdout: &io::Stdout, response: &JsonRpcResponse) {
    match serde_json::to_string(response) {
        Ok(out) => {
            let mut lock = stdout.lock();
            let _ = writeln!(lock, "{}", out);
            let _ = lock.flush();
        }
        Err(e) => eprintln!("Failed to serialize JSON-RPC response: {}", e),
    }
}

fn main() {
    let explicit_root = match parse_store_path_arg(std::env::args_os().skip(1)) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("mem-mcp: {}", message);
            std::process::exit(2);
        }
    };

    // Resolve the store root exactly like the CLI: explicit `--path` first, then
    // $ACDM_STORE, then the nearest enclosing `.mem`/`events/` directory, then
    // the current working directory.
    let store_root = match resolve_store_root(explicit_root.as_deref()) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("Failed to resolve memory store root: {}", e);
            std::process::exit(1);
        }
    };

    let store = MemStore::new(store_root);
    if let Err(e) = store.init() {
        eprintln!("Notice: store initialization check: {}", e);
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();

    loop {
        let line_bytes = match read_limited_line(&mut reader, MAX_REQUEST_LINE_BYTES) {
            Ok(LimitedLine::Eof) => break,
            Ok(LimitedLine::Line(line)) => line,
            Ok(LimitedLine::TooLong) => {
                // The oversized line (including its newline) was already
                // discarded, so the server can keep serving later requests.
                write_response(
                    &stdout,
                    &error_response(
                        Value::Null,
                        -32600,
                        format!(
                            "Invalid Request: request line exceeds {} bytes",
                            MAX_REQUEST_LINE_BYTES
                        ),
                    ),
                );
                continue;
            }
            Err(e) => {
                eprintln!("Error reading line from stdin: {}", e);
                break;
            }
        };

        let line = String::from_utf8_lossy(&line_bytes);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(e) => {
                write_response(
                    &stdout,
                    &error_response(Value::Null, -32700, format!("Parse error: {}", e)),
                );
                continue;
            }
        };

        if let Some(response) = handle_request(&store, request) {
            write_response(&stdout, &response);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_store_path_arg;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> Result<Option<PathBuf>, String> {
        parse_store_path_arg(args.iter().copied().map(OsString::from))
    }

    #[test]
    fn test_parse_path_flag_with_separate_value() {
        assert_eq!(
            parse(&["--path", "/tmp/store"]).unwrap(),
            Some(PathBuf::from("/tmp/store"))
        );
    }

    #[test]
    fn test_parse_path_flag_with_inline_value() {
        assert_eq!(
            parse(&["--path=/tmp/store"]).unwrap(),
            Some(PathBuf::from("/tmp/store"))
        );
    }

    #[test]
    fn test_parse_without_arguments_uses_discovery() {
        assert_eq!(parse(&[]).unwrap(), None);
    }

    #[test]
    fn test_parse_keeps_last_path() {
        assert_eq!(
            parse(&["--path=/one", "--path", "/two"]).unwrap(),
            Some(PathBuf::from("/two"))
        );
    }

    #[test]
    fn test_parse_rejects_missing_or_empty_value() {
        assert!(parse(&["--path"]).is_err());
        assert!(parse(&["--path="]).is_err());
    }

    #[test]
    fn test_parse_rejects_unknown_argument() {
        assert!(parse(&["--bogus"]).is_err());
    }
}
