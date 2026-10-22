//! End-to-end tests for the stdio MCP server.
//!
//! These spawn the real `mem-mcp` binary and speak line-delimited JSON-RPC over
//! its stdin/stdout, using `std::process` only. They therefore cover the same
//! path an MCP client takes: argument parsing, store root resolution, request
//! dispatch and response framing.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
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
