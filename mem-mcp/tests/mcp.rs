//! End-to-end tests for the stdio MCP server.
//!
//! These spawn the real `mem-mcp` binary and speak line-delimited JSON-RPC over
//! its stdin/stdout, using `std::process` only. They therefore cover the same
//! path an MCP client takes: argument parsing, store root resolution, request
//! dispatch and response framing.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2024-11-05";
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

// ----------------- Test store sandbox -----------------

/// A throwaway directory tree with a store root and a separate working directory.
struct TestEnv {
    base: PathBuf,
    store: PathBuf,
    cwd: PathBuf,
}

impl TestEnv {
    fn new(label: &str) -> Self {
        let unique = format!(
            "acdm_mcp_test_{}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            label,
        );
        let base = std::env::temp_dir().join(unique);
        let store = base.join("store");
        let cwd = base.join("cwd");
        fs::create_dir_all(&store).expect("create test store dir");
        fs::create_dir_all(&cwd).expect("create test cwd");
        Self { base, store, cwd }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

// ----------------- MCP client harness -----------------

/// A running `mem-mcp` process plus helpers to speak line-delimited JSON-RPC.
struct McpServer {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Option<String>>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

impl McpServer {
    /// Start the server with `--path <store>` from `cwd`, so tests can prove that
    /// the flag (not the working directory) selects the store.
    fn start(store: &Path, cwd: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mem-mcp"))
            .arg("--path")
            .arg(store)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn mem-mcp");

        let stdin = child.stdin.take().expect("child stdin is piped");
        let stdout = child.stdout.take().expect("child stdout is piped");
        let stderr_pipe = child.stderr.take().expect("child stderr is piped");

        // Drain stdout on a thread so that a missing response fails with a
        // timeout/diagnostics instead of hanging the test suite forever.
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if sender.send(Some(line)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = sender.send(None);
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(mut sink) = stderr_sink.lock() {
                            sink.push_str(&line);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            stdin,
            responses,
            stderr,
            next_id: 1,
        }
    }

    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{}", line).expect("write to mem-mcp stdin");
        self.stdin.flush().expect("flush mem-mcp stdin");
    }

    fn send_notification(&mut self, method: &str) {
        self.send_raw(&json!({"jsonrpc": "2.0", "method": method}).to_string());
    }

    fn send_request(&mut self, method: &str, params: Option<Value>) -> Value {
        let id = self.next_id;
        self.next_id += 1;

        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        self.send_raw(&request.to_string());

        let response = self.read_response();
        assert_eq!(
            response["jsonrpc"],
            json!("2.0"),
            "response is not JSON-RPC 2.0: {}",
            response
        );
        assert_eq!(
            response["id"],
            json!(id),
            "response does not match the request id: {}",
            response
        );
        response
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.send_request(
            "tools/call",
            Some(json!({"name": name, "arguments": arguments})),
        )
    }

    /// Perform the normal MCP handshake: `initialize` followed by the
    /// `notifications/initialized` notification.
    fn initialize(&mut self) {
        let response = self.send_request(
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "acdm-mcp-tests", "version": "0.0.0"},
            })),
        );
        assert!(
            response.get("error").is_none(),
            "initialize failed: {}",
            response
        );
        self.send_notification("notifications/initialized");
    }

    fn read_response(&mut self) -> Value {
        match self.responses.recv_timeout(RESPONSE_TIMEOUT) {
            Ok(Some(line)) => serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("server wrote invalid JSON {:?}: {}", line.trim(), e)),
            Ok(None) => panic!(
                "mem-mcp closed stdout before responding; stderr: {}",
                self.stderr_text()
            ),
            Err(_) => panic!(
                "timed out waiting for a mem-mcp response; stderr: {}",
                self.stderr_text()
            ),
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|sink| sink.trim().to_string())
            .unwrap_or_default()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ----------------- Helpers -----------------

fn read_jsonl(path: &Path) -> Vec<Value> {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("invalid JSONL line {:?} in {}: {}", line, path.display(), e)
            })
        })
        .collect()
}

/// Every event record in `<store>/events/*.jsonl`, oldest month file first.
fn store_events(store: &Path) -> Vec<Value> {
    let events_dir = store.join("events");
    let entries = fs::read_dir(&events_dir).unwrap_or_else(|e| {
        panic!(
            "failed to list events directory {}: {}",
            events_dir.display(),
            e
        )
    });

    let mut files = Vec::new();
    for entry in entries {
        let path = entry.expect("read events directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    files.sort();

    let mut events = Vec::new();
    for path in files {
        events.extend(read_jsonl(&path));
    }
    events
}

fn tool_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool response has no text content: {}", response))
}

fn error_code(response: &Value) -> i64 {
    response["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected a JSON-RPC error response: {}", response))
}

// ----------------- Tests -----------------

#[test]
fn test_initialize_reports_protocol_version_and_capabilities() {
    let env = TestEnv::new("initialize");
    let mut server = McpServer::start(&env.store, &env.cwd);

    let response = server.send_request(
        "initialize",
        Some(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "acdm-mcp-tests", "version": "0.0.0"},
        })),
    );

    assert!(
        response.get("error").is_none(),
        "initialize returned an error: {}",
        response
    );
    let result = &response["result"];
    assert_eq!(
        result["protocolVersion"],
        json!(PROTOCOL_VERSION),
        "unexpected protocol version: {}",
        result
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "initialize must advertise the tools capability: {}",
        result
    );
    assert_eq!(result["serverInfo"]["name"], json!("mem-mcp"));
}

/// `notifications/initialized` is a notification: answering it would desync the
/// client's request/response stream, so the next stdout line must be the reply to
/// the *following* request.
#[test]
fn test_notifications_do_not_produce_a_response() {
    let env = TestEnv::new("notifications");
    let mut server = McpServer::start(&env.store, &env.cwd);

    let response = server.send_request(
        "initialize",
        Some(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "acdm-mcp-tests", "version": "0.0.0"},
        })),
    );
    assert!(
        response.get("error").is_none(),
        "initialize failed: {}",
        response
    );

    server.send_notification("notifications/initialized");

    // If the notification had produced a response, this read would receive that
    // response first and `send_request` would fail on the id mismatch.
    let pong = server.send_request("ping", None);
    assert_eq!(
        pong["result"],
        json!({}),
        "ping should return an empty result: {}",
        pong
    );
}

#[test]
fn test_tools_list_exposes_eight_tools() {
    let env = TestEnv::new("tools-list");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.send_request("tools/list", None);
    assert!(
        response.get("error").is_none(),
        "tools/list returned an error: {}",
        response
    );

    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result has no tools array: {}", response));
    assert_eq!(tools.len(), 8, "unexpected tool count: {}", response);

    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .unwrap_or_else(|| panic!("tool without a name: {}", tool))
        })
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "mem_add_event",
            "mem_compact",
            "mem_get_context",
            "mem_list_projects",
            "mem_query",
            "mem_render",
            "mem_resolve",
            "mem_sync",
        ]
    );

    for tool in tools {
        assert_eq!(
            tool["inputSchema"]["type"],
            json!("object"),
            "tool is missing an object input schema: {}",
            tool
        );
        // The advertised schema must not be stricter than the handler: unknown
        // top-level keys are accepted, and only what the handler truly needs is
        // marked required.
        let required = tool["inputSchema"]["required"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            !required.iter().any(|value| value == "action"),
            "top-level `action` should not be required: {}",
            tool
        );
    }

    // `mem_add_event.files[]` defaults `action` to "Modified", so it must not be
    // advertised as required. (Regression: schema said required while the handler
    // accepted a missing action.)
    let add_event = tools
        .iter()
        .find(|tool| tool["name"] == json!("mem_add_event"))
        .expect("mem_add_event tool");
    let file_required = add_event["inputSchema"]["properties"]["files"]["items"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        file_required,
        vec![json!("file_name")],
        "files[] should require only file_name"
    );
}

/// Regression for defect 3: MCP clients send absolute file paths. Storing them
/// raw broke `mem_query by=filename` (which indexes the relative identity) and
/// cross-device matching.
#[test]
fn test_add_event_normalizes_absolute_file_paths() {
    let env = TestEnv::new("normalize");
    // An enclosing git work tree makes the relative identity deterministic: the
    // file is inside the repository but outside the memory store.
    fs::create_dir_all(env.cwd.join(".git")).expect("create fake git dir");
    let absolute = env.cwd.join("proj").join("src").join("main.rs");
    fs::create_dir_all(absolute.parent().expect("file has a parent")).expect("create source dirs");
    fs::write(&absolute, "fn main() {}\n").expect("write source file");

    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.call_tool(
        "mem_add_event",
        json!({
            "project": "demo",
            "summary": "add main.rs",
            "files": [{
                "file_name": absolute.to_string_lossy().into_owned(),
                "action": "Created",
                "size_bytes": 13,
            }],
        }),
    );
    assert!(
        response.get("error").is_none(),
        "mem_add_event failed: {}",
        response
    );

    let files_path = env.store.join("projects").join("demo").join("files.jsonl");
    let records = read_jsonl(&files_path);
    assert_eq!(records.len(), 1, "expected one file record: {:?}", records);
    let stored = records[0]["file_name"]
        .as_str()
        .unwrap_or_else(|| panic!("file record has no file_name: {}", records[0]));
    assert_eq!(
        stored, "proj/src/main.rs",
        "absolute path was not normalised: {:?}",
        stored
    );

    // The event must be reachable by its relative identity.
    let response = server.call_tool(
        "mem_query",
        json!({"by": "filename", "value": "proj/src/main.rs"}),
    );
    assert!(
        response.get("error").is_none(),
        "mem_query failed: {}",
        response
    );
    let text = tool_text(&response);
    assert!(
        text.contains("Found 1 event(s) affecting file 'proj/src/main.rs'"),
        "query by relative filename did not find the event: {}",
        text
    );
    assert!(
        text.contains("Project: demo"),
        "query output misses the event details: {}",
        text
    );
}

/// Regression for defect 4: timestamps must be canonical fixed-width UTC so that
/// lexicographic order stays chronological.
#[test]
fn test_add_event_writes_canonical_timestamps() {
    let env = TestEnv::new("timestamp");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.call_tool(
        "mem_add_event",
        json!({"project": "ts-demo", "summary": "timestamp probe"}),
    );
    assert!(
        response.get("error").is_none(),
        "mem_add_event failed: {}",
        response
    );

    let events = store_events(&env.store);
    assert_eq!(events.len(), 1, "expected exactly one event: {:?}", events);
    let ts = events[0]["ts"]
        .as_str()
        .unwrap_or_else(|| panic!("event has no ts: {}", events[0]));

    assert_eq!(ts.len(), 30, "timestamp is not fixed width: {:?}", ts);
    assert!(ts.ends_with('Z'), "timestamp is not UTC: {:?}", ts);

    let bytes = ts.as_bytes();
    assert_eq!(bytes[4], b'-', "unexpected timestamp: {:?}", ts);
    assert_eq!(bytes[7], b'-', "unexpected timestamp: {:?}", ts);
    assert_eq!(bytes[10], b'T', "unexpected timestamp: {:?}", ts);
    assert_eq!(bytes[13], b':', "unexpected timestamp: {:?}", ts);
    assert_eq!(bytes[16], b':', "unexpected timestamp: {:?}", ts);
    assert_eq!(bytes[19], b'.', "unexpected timestamp: {:?}", ts);
    assert!(
        bytes[20..29].iter().all(|byte| byte.is_ascii_digit()),
        "timestamp nanoseconds are not fixed width: {:?}",
        ts
    );
}

/// JSON-RPC error codes must stay exactly as specified: -32601 for unknown
/// methods/tools and -32700 for unparsable input.
#[test]
fn test_error_codes_for_unknown_methods_tools_and_bad_json() {
    let env = TestEnv::new("errors");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.send_request("no/such/method", None);
    assert_eq!(
        error_code(&response),
        -32601,
        "unknown method must use -32601: {}",
        response
    );

    let response = server.call_tool("mem_does_not_exist", json!({}));
    assert_eq!(
        error_code(&response),
        -32601,
        "unknown tool must use -32601: {}",
        response
    );

    server.send_raw("{ this is not valid json");
    let response = server.read_response();
    assert_eq!(
        error_code(&response),
        -32700,
        "bad JSON must use -32700: {}",
        response
    );
    assert!(
        response["id"].is_null(),
        "parse errors must use a null id: {}",
        response
    );
}

/// Regression for defect 2: `mem_resolve` used to fail when no rebase was in
/// progress (git's error message is localised). It must succeed and report how
/// many non-record lines were preserved.
#[test]
fn test_resolve_without_active_rebase_succeeds() {
    let env = TestEnv::new("resolve");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.call_tool("mem_resolve", json!({}));
    assert!(
        response.get("error").is_none(),
        "mem_resolve returned an error: {}",
        response
    );

    let text = tool_text(&response);
    assert!(
        text.contains("Conflict resolution completed"),
        "unexpected mem_resolve output: {}",
        text
    );
    assert!(
        text.contains("0 files resolved"),
        "unexpected mem_resolve output: {}",
        text
    );
    assert!(
        text.contains("0 non-record lines preserved"),
        "lines_preserved is missing from mem_resolve output: {}",
        text
    );
}

/// Regression for defect 5: the store root must honour `--path` instead of being
/// hard-wired to the process working directory.
#[test]
fn test_path_flag_pins_the_store_root() {
    let env = TestEnv::new("path-flag");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.call_tool(
        "mem_add_event",
        json!({"project": "pinned", "summary": "stored in --path"}),
    );
    assert!(
        response.get("error").is_none(),
        "mem_add_event failed: {}",
        response
    );

    let events = store_events(&env.store);
    assert_eq!(
        events.len(),
        1,
        "--path store should hold the event: {:?}",
        events
    );
    assert_eq!(events[0]["project"], json!("pinned"));

    // Nothing may be created in the working directory.
    assert!(
        !env.cwd.join("events").exists(),
        "event log leaked into the working directory"
    );
    assert!(
        !env.cwd.join(".mem").exists(),
        "store marker leaked into the working directory"
    );
    assert!(
        !env.cwd.join("projects").exists(),
        "project records leaked into the working directory"
    );
}

/// Item 4: context rendering must use the store's month-aware latest-events query
/// and show the most recent events of the requested project.
#[test]
fn test_get_context_lists_recent_events() {
    let env = TestEnv::new("context");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    for summary in ["first change", "second change"] {
        let response = server.call_tool(
            "mem_add_event",
            json!({"project": "ctx-demo", "summary": summary}),
        );
        assert!(
            response.get("error").is_none(),
            "mem_add_event failed: {}",
            response
        );
    }

    let response = server.call_tool("mem_get_context", json!({"project": "ctx-demo"}));
    assert!(
        response.get("error").is_none(),
        "mem_get_context failed: {}",
        response
    );

    let text = tool_text(&response);
    assert!(
        text.contains("--- Recent Events (last 5) ---"),
        "context output misses the recent events section: {}",
        text
    );
    assert!(
        text.contains("first change") && text.contains("second change"),
        "context output misses recorded events: {}",
        text
    );
}

/// Regression: `mem_render` can create `projects/<name>/` before any event is
/// recorded, which used to stop `mem_add_event` from ever writing `meta.json`.
#[test]
fn test_render_then_add_event_creates_project_metadata() {
    let env = TestEnv::new("render-then-add");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let rendered = server.call_tool("mem_render", json!({ "project": "ghost" }));
    assert!(
        rendered.get("error").is_none(),
        "mem_render failed: {}",
        rendered
    );

    let added = server.call_tool(
        "mem_add_event",
        json!({ "project": "ghost", "summary": "after render" }),
    );
    assert!(
        added.get("error").is_none(),
        "mem_add_event failed: {}",
        added
    );

    let meta_path = env.store.join("projects").join("ghost").join("meta.json");
    assert!(
        meta_path.is_file(),
        "meta.json must exist after render + add_event"
    );
    let meta = fs::read_to_string(&meta_path).expect("read meta.json");
    assert!(
        meta.contains("\"name\": \"ghost\""),
        "meta.json must describe the project: {}",
        meta
    );
}

// ----------------- Review fixes: protocol consistency, sync errors, input caps -----------------

/// Review finding #1: a traversal-like project name must be rejected as a tool
/// business error (isError) and must never create directories.
#[test]
fn project_name_traversal_is_rejected() {
    let env = TestEnv::new("traversal");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    for project in ["../escape", "a/b"] {
        let response = server.call_tool(
            "mem_add_event",
            json!({"project": project, "summary": "must fail"}),
        );
        assert!(
            response.get("error").is_none(),
            "invalid project is a tool business error, not a JSON-RPC error: {}",
            response
        );
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "invalid project {:?} must set isError: {}",
            project,
            response
        );
        assert!(
            tool_text(&response).contains("invalid project name"),
            "the error must name the invalid project, got: {}",
            tool_text(&response)
        );
    }

    assert!(
        !env.base.join("escape").exists(),
        "`../escape` must not create a directory next to the store"
    );
    assert!(
        !env.store.join("escape").exists(),
        "`../escape` must not escape projects/ inside the store"
    );
    assert!(
        !env.store.join("projects").join("a").exists(),
        "`a/b` must not create nested project directories"
    );
}

/// Review finding #15: `jsonrpc` must be validated as `"2.0"` (-32600), even when
/// the field is missing; the connection must keep working afterwards.
#[test]
fn invalid_jsonrpc_version_returns_32600() {
    let env = TestEnv::new("jsonrpc-version");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    server.send_raw(&json!({"jsonrpc": "1.0", "id": 77, "method": "ping"}).to_string());
    let response = server.read_response();
    assert_eq!(
        error_code(&response),
        -32600,
        "a wrong jsonrpc version must be Invalid Request: {}",
        response
    );
    assert_eq!(response["id"], json!(77));

    // A missing version field is an Invalid Request, not a JSON parse error.
    server.send_raw(&json!({"id": 78, "method": "ping"}).to_string());
    let response = server.read_response();
    assert_eq!(
        error_code(&response),
        -32600,
        "a missing jsonrpc version must be Invalid Request: {}",
        response
    );
    assert_eq!(response["id"], json!(78));

    let pong = server.send_request("ping", None);
    assert_eq!(
        pong["result"],
        json!({}),
        "the server must still answer valid requests"
    );
}

/// Review finding #15: tool business errors are `isError` results inside a
/// successful response (so the agent can self-correct), not -32603 errors.
#[test]
fn business_errors_use_is_error_result() {
    let env = TestEnv::new("business-error");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.call_tool("mem_query", json!({"by": "bogus", "value": "anything"}));
    assert!(
        response.get("error").is_none(),
        "business errors must not be JSON-RPC errors: {}",
        response
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "an invalid query dimension must be an isError result: {}",
        response
    );
    assert!(
        tool_text(&response).contains("Invalid query dimension"),
        "the isError text must explain the problem: {}",
        tool_text(&response)
    );

    // Unknown tools remain protocol errors.
    let response = server.call_tool("mem_does_not_exist", json!({}));
    assert_eq!(
        error_code(&response),
        -32601,
        "unknown tools must stay -32601: {}",
        response
    );
}

/// Review finding #15: schema violations (missing/wrong/enum-invalid arguments)
/// must be Invalid params (-32602), not internal errors.
#[test]
fn invalid_tool_arguments_return_32602() {
    let env = TestEnv::new("invalid-args");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    // Missing a required field.
    let response = server.call_tool("mem_compact", json!({}));
    assert_eq!(
        error_code(&response),
        -32602,
        "a missing `project` must be Invalid params: {}",
        response
    );

    // Unknown/incorrectly-cased action enum value.
    let response = server.call_tool(
        "mem_add_event",
        json!({
            "project": "demo",
            "summary": "bad action",
            "files": [{"file_name": "x.txt", "action": "Renamed"}],
        }),
    );
    assert_eq!(
        error_code(&response),
        -32602,
        "an unknown files[].action must be Invalid params: {}",
        response
    );
    assert!(
        store_events(&env.store).is_empty(),
        "invalid arguments must not record an event"
    );

    // Wrong argument type.
    let response = server.call_tool("mem_sync", json!({"message": 42}));
    assert_eq!(
        error_code(&response),
        -32602,
        "a wrong argument type must be Invalid params: {}",
        response
    );
}

/// Review finding #5: a sync failure (here: no remote to push to) is surfaced as
/// an isError tool result instead of a silent success.
#[test]
fn mem_sync_without_remote_reports_is_error() {
    let env = TestEnv::new("sync-no-remote");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let init = Command::new("git")
        .current_dir(&env.store)
        .args(["init", "--quiet"])
        .output()
        .expect("run git init");
    assert!(init.status.success(), "git init failed");
    for args in [
        ["config", "user.email", "test@acdm.local"],
        ["config", "user.name", "ACDM Test"],
    ] {
        let output = Command::new("git")
            .current_dir(&env.store)
            .args(args)
            .output()
            .expect("run git config");
        assert!(output.status.success(), "git config failed");
    }

    let response = server.call_tool("mem_sync", json!({}));
    assert!(
        response.get("error").is_none(),
        "a sync failure is a tool business error: {}",
        response
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "a sync without a remote must be an isError result: {}",
        response
    );
    let text = tool_text(&response);
    assert!(
        text.to_lowercase().contains("failed") || text.to_lowercase().contains("push"),
        "the isError text must explain the sync failure: {}",
        text
    );
}

/// Review finding #9: a single oversized input line must not exhaust memory; the
/// server answers -32600 and then keeps serving.
#[test]
fn oversized_request_line_is_rejected_and_server_recovers() {
    let env = TestEnv::new("oversized");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let oversized = "x".repeat(16 * 1024 * 1024 + 1);
    server.send_raw(&oversized);
    let response = server.read_response();
    assert_eq!(
        error_code(&response),
        -32600,
        "an oversized line must be Invalid Request: {}",
        response
    );
    assert!(
        response["id"].is_null(),
        "an oversized line has no usable request id: {}",
        response
    );

    let pong = server.send_request("ping", None);
    assert_eq!(
        pong["result"],
        json!({}),
        "the server must recover after discarding the oversized line"
    );
}

/// Run `git` in `cwd` and return its output.
fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("failed to run git")
}

fn assert_git_ok(output: &Output) {
    assert!(
        output.status.success(),
        "git command failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Review finding #5: a pull conflict must become an isError tool result whose
/// text tells the agent to run `mem_resolve` and retry (never a silent success).
#[test]
fn mem_sync_conflict_reports_is_error_and_mentions_resolve() {
    let env = TestEnv::new("sync-conflict");

    let bare = env.base.join("origin.git");
    assert_git_ok(&git(
        &env.base,
        &["init", "--bare", bare.to_str().expect("UTF-8 path")],
    ));

    // Seed the remote with a store-like repository.
    let seed = env.base.join("seed");
    fs::create_dir_all(seed.join("events")).expect("create seed events dir");
    fs::write(
        seed.join("events").join("2026-10.jsonl"),
        "{\"event_id\":\"base\"}\n",
    )
    .expect("write seed event log");
    assert_git_ok(&git(&seed, &["init", "--quiet"]));
    assert_git_ok(&git(&seed, &["config", "user.email", "test@acdm.local"]));
    assert_git_ok(&git(&seed, &["config", "user.name", "ACDM Test"]));
    assert_git_ok(&git(&seed, &["add", "-A"]));
    assert_git_ok(&git(&seed, &["commit", "-qm", "seed store"]));
    assert_git_ok(&git(
        &seed,
        &[
            "remote",
            "add",
            "origin",
            bare.to_str().expect("UTF-8 path"),
        ],
    ));
    let branch = git_stdout(&git(&seed, &["rev-parse", "--abbrev-ref", "HEAD"]));
    assert_git_ok(&git(&seed, &["push", "-q", "-u", "origin", &branch]));

    // The server's store is clone A; clone B pushes a conflicting change first.
    let clone_b = env.base.join("clone-b");
    for repo in [&env.store, &clone_b] {
        assert_git_ok(&git(
            &env.base,
            &[
                "clone",
                "-q",
                bare.to_str().expect("UTF-8 path"),
                repo.to_str().expect("UTF-8 path"),
            ],
        ));
        assert_git_ok(&git(repo, &["config", "user.email", "test@acdm.local"]));
        assert_git_ok(&git(repo, &["config", "user.name", "ACDM Test"]));
    }
    fs::write(
        clone_b.join("events").join("2026-10.jsonl"),
        "{\"event_id\":\"remote\"}\n",
    )
    .expect("write clone B event log");
    assert_git_ok(&git(&clone_b, &["add", "-A"]));
    assert_git_ok(&git(&clone_b, &["commit", "-qm", "remote change"]));
    assert_git_ok(&git(&clone_b, &["push", "-q"]));

    fs::write(
        env.store.join("events").join("2026-10.jsonl"),
        "{\"event_id\":\"local\"}\n",
    )
    .expect("write clone A event log");
    assert_git_ok(&git(&env.store, &["add", "-A"]));
    assert_git_ok(&git(&env.store, &["commit", "-qm", "local change"]));

    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.call_tool("mem_sync", json!({}));
    assert!(
        response.get("error").is_none(),
        "a conflicted sync is a tool business error: {}",
        response
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "a conflicted sync must set isError: {}",
        response
    );
    let text = tool_text(&response);
    assert!(
        text.contains("run mem_resolve then retry"),
        "the isError text must tell the agent to run mem_resolve and retry: {}",
        text
    );
    assert!(
        text.to_lowercase().contains("conflict"),
        "the isError text must mention the conflict: {}",
        text
    );
}

/// Second-round review: a `notifications/*` method *with* an id is an ordinary
/// request (JSON-RPC 2.0) and must be answered with -32601, otherwise the client
/// waits forever. Only id-less notifications are silent.
#[test]
fn notifications_with_id_are_answered() {
    let env = TestEnv::new("notification-with-id");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    server
        .send_raw(&json!({"jsonrpc": "2.0", "id": 99, "method": "notifications/foo"}).to_string());
    let response = server.read_response();
    assert_eq!(
        error_code(&response),
        -32601,
        "a notifications/* request with an id must be answered with -32601: {}",
        response
    );
    assert_eq!(response["id"], json!(99));

    server.send_raw(
        &json!({"jsonrpc": "2.0", "id": 100, "method": "notifications/initialized"}).to_string(),
    );
    let response = server.read_response();
    assert_eq!(
        error_code(&response),
        -32601,
        "even the standard initialized notification must answer -32601 when it has an id: {}",
        response
    );
    assert_eq!(response["id"], json!(100));

    // A notification without an id is still silent: the next response the client
    // reads must belong to the following ping.
    server.send_notification("notifications/initialized");
    let pong = server.send_request("ping", None);
    assert_eq!(
        pong["result"],
        json!({}),
        "an id-less notification must not produce a response: {}",
        pong
    );
}

/// Second-round review: an invalid project name must be rejected immediately by
/// the handler (isError), before any directory or record is created.
#[test]
fn invalid_project_name_returns_is_error_immediately() {
    let env = TestEnv::new("invalid-project");
    let mut server = McpServer::start(&env.store, &env.cwd);
    server.initialize();

    let response = server.call_tool(
        "mem_add_event",
        json!({"project": "../etc", "summary": "must be rejected"}),
    );
    assert!(
        response.get("error").is_none(),
        "an invalid project is a tool business error, not a JSON-RPC error: {}",
        response
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "an invalid project must set isError: {}",
        response
    );
    let text = tool_text(&response);
    assert!(
        text.contains("invalid project name"),
        "the error text must name the invalid project: {}",
        text
    );

    // Nothing may be recorded and no project directory may be created.
    assert!(
        store_events(&env.store).is_empty(),
        "an invalid project must not record an event"
    );
    let projects_dir = env.store.join("projects");
    assert!(projects_dir.is_dir(), "store init creates projects/");
    assert_eq!(
        fs::read_dir(&projects_dir)
            .expect("read projects dir")
            .count(),
        0,
        "an invalid project must not create a project directory"
    );
}

/// v0.1.3 review finding: `files[].file_name` must not be empty/whitespace and
/// must not normalise to the store root (e.g. `"."`), which would record a
/// directory as a tracked file. A normal file name still works.
#[test]
fn mem_add_event_rejects_empty_file_name() {
    let env = TestEnv::new("empty-file-name");
    // Run the server from the store root so `"."` normalises to the store itself.
    let mut server = McpServer::start(&env.store, &env.store);
    server.initialize();

    let response = server.call_tool(
        "mem_add_event",
        json!({
            "project": "demo",
            "summary": "empty name",
            "files": [{"file_name": ""}],
        }),
    );
    assert!(
        response.get("error").is_none(),
        "an invalid file_name is a tool business error: {}",
        response
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "an empty file_name must set isError: {}",
        response
    );
    let text = tool_text(&response);
    assert!(
        text.contains("file_name") && text.contains("empty"),
        "the error must mention the empty file_name: {}",
        text
    );

    let response = server.call_tool(
        "mem_add_event",
        json!({
            "project": "demo",
            "summary": "store root name",
            "files": [{"file_name": "."}],
        }),
    );
    assert!(
        response.get("error").is_none(),
        "a store-root file_name is a tool business error: {}",
        response
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "`.` must set isError instead of recording the store root: {}",
        response
    );
    assert!(
        tool_text(&response).contains("store root"),
        "the error must explain the store-root rejection: {}",
        tool_text(&response)
    );

    // Rejected calls must not have recorded anything.
    assert!(
        store_events(&env.store).is_empty(),
        "rejected file names must not record events"
    );

    // A normal file name still works.
    fs::write(env.store.join("regular.txt"), "content\n").expect("write regular file");
    let response = server.call_tool(
        "mem_add_event",
        json!({
            "project": "demo",
            "summary": "regular file",
            "files": [{"file_name": "regular.txt"}],
        }),
    );
    assert!(
        response.get("error").is_none(),
        "a normal add failed: {}",
        response
    );
    assert!(
        response["result"].get("isError").is_none(),
        "a normal add must not be an error result: {}",
        response
    );
    assert_eq!(
        store_events(&env.store).len(),
        1,
        "the normal add must record exactly one event"
    );
    let records = read_jsonl(&env.store.join("projects").join("demo").join("files.jsonl"));
    assert_eq!(records.len(), 1, "expected one file record: {:?}", records);
    assert_eq!(
        records[0]["file_name"],
        json!("regular.txt"),
        "the store-relative identity must be preserved: {}",
        records[0]
    );
}
