**English** · [简体中文](zh-CN/MCP.md)

# `mem-mcp` MCP Reference

> Audience: developers who want to integrate ACDM into Claude Desktop, Cursor, or a custom MCP client. This document covers how `mem-mcp` starts, the JSON-RPC 2.0 session over stdio, and the parameters, return values, edge-case behavior, and error codes of the 8 tools; for the CLI side see [CLI.md](CLI.md), and for the overall design see [ARCHITECTURE.md](ARCHITECTURE.md) and [DESIGN.md](DESIGN.md).

This document treats the `inputSchema` returned by `tools/list` in `mem-mcp/src/main.rs` and the actual behavior of each handler as the **sole authority**. All messages and return texts below were produced by feeding stdin to the real v0.1.0 binary through a pipe; for readability, absolute local paths in the examples are replaced with `/path/to/...` placeholders, while event IDs, timestamps, and hashes are kept as-is. Actual responses are **single-line JSON**; examples that keep the original single-line message quote the measured output directly, and the few structural illustrations (such as the tree-style expansion of `tools/list`) are explicitly labeled as illustrations.

## Table of Contents

- [1. Overview](#1-overview)
- [2. Startup and Configuration](#2-startup-and-configuration)
  - [2.1 Command-Line Arguments](#21-command-line-arguments)
  - [2.2 Store Resolution and Startup Initialization](#22-store-resolution-and-startup-initialization)
  - [2.3 Client Configuration Examples](#23-client-configuration-examples)
  - [2.4 Startup Failure Errors](#24-startup-failure-errors)
- [3. Session Flow](#3-session-flow)
  - [3.1 Transport Format](#31-transport-format)
  - [3.2 initialize](#32-initialize)
  - [3.3 notifications/initialized](#33-notificationsinitialized)
  - [3.4 ping](#34-ping)
  - [3.5 tools/list](#35-toolslist)
  - [3.6 tools/call](#36-toolscall)
- [4. Tools](#4-tools)
  - [4.1 mem_get_context](#41-mem_get_context)
  - [4.2 mem_add_event](#42-mem_add_event)
  - [4.3 mem_query](#43-mem_query)
  - [4.4 mem_list_projects](#44-mem_list_projects)
  - [4.5 mem_compact](#45-mem_compact)
  - [4.6 mem_sync](#46-mem_sync)
  - [4.7 mem_resolve](#47-mem_resolve)
  - [4.8 mem_render](#48-mem_render)
- [5. Error Codes and Semantics](#5-error-codes-and-semantics)
- [6. Recommended Workflow for Sharing a Store with the CLI](#6-recommended-workflow-for-sharing-a-store-with-the-cli)
- [7. Limitations](#7-limitations)
- [8. Related Documentation](#8-related-documentation)

## 1. Overview

`mem-mcp` is ACDM's stdio MCP server (binary name `mem-mcp`). It is launched as a **child process** by an MCP client and exposes the read/write capabilities of the same ACDM memory store as 8 tools, so that AI agents can read project context, record events, query history, and perform sync maintenance. `mem-mcp` and the `mem` CLI share the entire `mem-core` implementation and read and write the same store; the two can be used interchangeably.

Key facts:

- **Transport**: **newline-delimited JSON-RPC 2.0** over stdio. One JSON object per line on stdin; one response object per line on stdout. Not `Content-Length` framing like LSP.
- **Protocol version**: `initialize` always returns `protocolVersion: "2024-11-05"`, with `serverInfo` being `{"name":"mem-mcp","version":"0.1.0"}`.
- **Capabilities**: declares only `capabilities.tools = {}`; no resources, prompts, logging, sampling, or other capabilities.
- **Single-threaded and serial**: the main loop reads stdin line by line, handles one request synchronously, writes one response back and flushes it, and only then reads the next line; there is no concurrent processing.
- **Store initialized at startup**: at startup the store is resolved with the same rules as the CLI, and `MemStore::init()` is called — missing directory structure or `.mem` is created automatically (equivalent to `mem init`).
- **Lifecycle**: the process exits normally on stdin EOF (exit code 0). The client is responsible for launching and terminating the process; it does not need to be kept resident manually.
- **No authentication**: local stdio only; for the capability boundary see [7. Limitations](#7-limitations).

## 2. Startup and Configuration

### 2.1 Command-Line Arguments

```text
mem-mcp [--path <store-root>]
```

| Argument | Required | Default | Description |
| --- | --- | --- | --- |
| `--path <DIR>` / `--path=<DIR>` | No | Auto-resolved (see 2.2) | Pins the store root directory; both forms are supported, and when repeated the **last** one wins |
| `-h` / `--help` | No | — | **Not supported**: rejected as an unknown argument (see 2.4) |

Argument parsing is hand-written (not clap):

- Only `--path` and `--path=` are recognized; any other argument causes an error and exits with code 2;
- `--path` missing a value, or `--path=` with an empty value → error and exit with code 2;
- An empty-string path is invalid.

### 2.2 Store Resolution and Startup Initialization

The resolution rules are exactly the same as for the `mem` CLI (`mem-core/src/util.rs::resolve_store_root`), in priority order:

1. Command-line `--path <DIR>`;
2. Environment variable `ACDM_STORE`;
3. Walk up from the **process's current working directory** looking for the nearest store root (one that has a `.mem` file or an `events/` directory);
4. Fallback: the process's current working directory.

After resolving the target directory, `mem-mcp` immediately calls `MemStore::init()`: it creates `events/`, `projects/`, `.mem`, and `.gitignore` (existing files are not overwritten). When initialization succeeds, both stdout and stderr are empty — the client receives no JSON-RPC message during startup. If initialization fails (for example, the target path is not writable), `Notice: store initialization check: <reason>` is printed to stderr, but the server **keeps running** (later tool calls may fail).

> Note: an MCP client's process working directory is not necessarily your project directory. **Explicitly using `--path` or `ACDM_STORE` is recommended**; otherwise the "walk up" lookup may land in the client's own directory, or even create a store automatically inside the CWD.

### 2.3 Client Configuration Examples

Three common styles (placeholders: `/path/to/mem-mcp` is the absolute path to the binary, `/path/to/store` is the store root directory):

**① Claude Desktop style** (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "acdm": {
      "command": "/path/to/mem-mcp",
      "args": ["--path", "/path/to/store"]
    }
  }
}
```

**② Cursor style** (`~/.cursor/mcp.json` or a project-local `.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "acdm": {
      "command": "mem-mcp",
      "args": ["--path", "/path/to/store"],
      "env": {
        "ACDM_STORE": "/path/to/store"
      }
    }
  }
}
```

Both Cursor and Claude Desktop use the `mcpServers` mapping; `command` must be on the client's `PATH`, otherwise write an absolute path. The example above provides both `--path` and `ACDM_STORE` (they agree, which is redundant insurance).

**③ Generic command + args** (any MCP client / custom client):

```json
{
  "command": "mem-mcp",
  "args": ["--path", "/path/to/store"],
  "env": {}
}
```

If the client is configured in "command line" form, it is equivalent to: `mem-mcp --path /path/to/store`; `mem-mcp --path=/path/to/store` is also supported. If you do not want to pass `--path`, you can specify the store with `"env": {"ACDM_STORE": "/path/to/store"}`; when neither is given, the store is auto-resolved by the rules in 2.2.

### 2.4 Startup Failure Errors

On an argument error, `mem-mcp: ...` is printed to stderr and the process exits with code **2** (this is also the startup failure information the client sees). Real output:

```text
$ mem-mcp --bogus
mem-mcp: unrecognized argument "--bogus"; usage: mem-mcp [--path <store-root>]
[exit code 2]

$ mem-mcp --path
mem-mcp: --path requires a directory argument
[exit code 2]

$ mem-mcp --help
mem-mcp: unrecognized argument "--help"; usage: mem-mcp [--path <store-root>]
[exit code 2]
```

A store resolution/initialization failure at startup (an error from `resolve_store_root`, e.g. the CWD is not readable) prints `Failed to resolve memory store root: <reason>` and exits with code **1**.

## 3. Session Flow

### 3.1 Transport Format

- **One JSON object per line**, UTF-8 encoded, terminated by `\n`. Empty lines are ignored.
- Request: `{"jsonrpc":"2.0","id":<id>,"method":"<method>","params":<object?>}`. `id` may be a number, a string, or `null`.
- Success response: `{"jsonrpc":"2.0","id":<id>,"result":<object>}`.
- Error response: `{"jsonrpc":"2.0","id":<id>,"error":{"code":<int>,"message":"<text>"}}`.
- The `id` in a response echoes the request's `id` as-is (`null` is used only for a Parse error where no request object can be parsed out).
- **A message without `id` is treated as a notification: it produces no response and the method is not executed** (messages whose method name starts with `notifications/` or equals `initialized` are silently dropped even earlier).
- **JSON-RPC batch arrays are not supported**: the whole line must be a single request object; an array is returned as the parse error `-32700`.
- Standard input and output are the protocol channel: only JSON-RPC responses are written to stdout; diagnostics go to stderr.

### 3.2 initialize

The first step of the client handshake. The server **does not validate** the request's `protocolVersion` / `capabilities` / `clientInfo` and always returns the same result.

Request (real message):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"docs-client","version":"0.1.0"}}}
```

Response (real message):

```json
{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"mem-mcp","version":"0.1.0"}}}
```

### 3.3 notifications/initialized

The notification sent after the handshake completes, which **produces no response**. In the real session below, three input lines produce only two output lines (the responses to `initialize` and `ping`), proving that the notification produces no output:

```text
# stdin (3 lines)
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"docs-client","version":"0.1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"ping"}

# stdout (2 lines)
{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"mem-mcp","version":"0.1.0"}}}
{"jsonrpc":"2.0","id":2,"result":{}}
```

Any method with a `notifications/` prefix is handled silently as a notification; in addition, the method name `initialized` is also treated as a notification.

### 3.4 ping

```text
// Request
{"jsonrpc":"2.0","id":2,"method":"ping"}
// Response
{"jsonrpc":"2.0","id":2,"result":{}}
```

`ping` returns an empty object and can be used as a liveness probe; the server keeps running after receiving an unknown method or an unknown tool (see [5. Error Codes and Semantics](#5-error-codes-and-semantics)).

### 3.5 tools/list

Returns the complete definitions of the 8 tools: each element contains `name`, `description`, and `inputSchema` (a JSON Schema whose root type is `object`). The real response is **single-line JSON** (about 2.6 KB), structured as follows (illustrative; see [4. Tools](#4-tools) for the exact `inputSchema` of each tool):

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "tools": [
      {"name": "mem_get_context", "description": "...", "inputSchema": {"type": "object", "properties": {...}}},
      {"name": "mem_add_event", "description": "...", "inputSchema": {"type": "object", "properties": {...}, "required": ["project", "summary"]}},
      {"name": "mem_query", "description": "...", "inputSchema": {"type": "object", "properties": {...}, "required": ["by", "value"]}},
      {"name": "mem_list_projects", "description": "...", "inputSchema": {"type": "object", "properties": {}}},
      {"name": "mem_compact", "description": "...", "inputSchema": {"type": "object", "properties": {...}, "required": ["project"]}},
      {"name": "mem_sync", "description": "...", "inputSchema": {"type": "object", "properties": {...}}},
      {"name": "mem_resolve", "description": "...", "inputSchema": {"type": "object", "properties": {}}},
      {"name": "mem_render", "description": "...", "inputSchema": {"type": "object", "properties": {...}, "required": ["project"]}}
    ]
  }
}
```

The tool order is fixed: `mem_get_context`, `mem_add_event`, `mem_query`, `mem_list_projects`, `mem_compact`, `mem_sync`, `mem_resolve`, `mem_render`. None of the `inputSchema`s declares `additionalProperties: false`, so extra fields are ignored (serde's default behavior).

### 3.6 tools/call

The shape of a tool-call message:

```json
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"<tool>","arguments":{...}}}
```

- `params.name` is required (a string); `params.arguments` may be omitted (treated as `{}` when omitted).
- A success response looks like `{"jsonrpc":"2.0","id":20,"result":{"content":[{"text":"...","type":"text"}]}}`: `result.content` is a content array, and currently each tool returns only **one** `text` item.
- Tool execution failures go through the JSON-RPC `error` (not MCP's `result.isError` field): `{"jsonrpc":"2.0","id":20,"error":{"code":-32603,"message":"..."}}`.
- An unknown tool returns `-32601` and does not terminate the server.

Real example (`mem_render`):

```text
// Request
{"jsonrpc":"2.0","id":26,"method":"tools/call","params":{"name":"mem_render","arguments":{"project":"demo"}}}
// Response
{"jsonrpc":"2.0","id":26,"result":{"content":[{"text":"Successfully rendered README for project 'demo'.","type":"text"}]}}
```

## 4. Tools

| Tool | Required parameters | Purpose |
| --- | --- | --- |
| `mem_get_context` | none | Read project context (metadata + README + latest 5 events); lists all projects when `project` is not passed |
| `mem_add_event` | `project`, `summary` | Record an event and its file changes |
| `mem_query` | `by`, `value` | Query events by project / file_id / filename / fuzzy |
| `mem_list_projects` | none | List all projects |
| `mem_compact` | `project` | Compact `files.jsonl`, keeping only the latest record per file name |
| `mem_sync` | none (`message` optional) | Commit local changes, rebase-pull, push |
| `mem_resolve` | none | Automatically resolve JSONL conflict markers |
| `mem_render` | `project` | Generate `projects/<project>/README.md` |

Each tool below gives: purpose, parameter table, the `inputSchema` verbatim from `tools/list` (formatted), return text format, real return text, example request/response, and edge-case behavior.

### 4.1 mem_get_context

Reads project context; **when `project` is not passed, lists all projects in the store**, and when passed, returns that project's metadata, `README.md`, and latest 5 events.

**Parameters**

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `project` | string | No | — | Project name; when omitted, `null`, or a whitespace-only string, lists all projects |

**inputSchema (verbatim from tools/list)**

```json
{
  "type": "object",
  "properties": {
    "project": {
      "type": "string",
      "description": "Optional project name. If omitted, returns list of all projects."
    }
  }
}
```

**Return text format**

- Without `project` (empty store): `No projects found in memory store.`
- Without `project` (projects exist):
  ```
  Found <N> project(s):
  - <name>: <description> (created: <created_at>)
    GitHub: <github_url>        # only when meta.github_url exists
  ```
- With `project`:
  ```
  === Project: <name> ===
  Description: <description>
  Created At: <created_at>
  GitHub: <url>                 # only when present

  --- README.md ---
  <projects/<name>/README.md contents, or "(No README.md available)">

  --- Recent Events (last 5) ---
  [<ts>] (<event_id>) <summary>
    Device: <device> | Agent: <agent> | Files: <n>
      - <file_name> [<action>] (<size> bytes)
  ```
  Recent events are ordered **newest first**, at most 5; when `meta.json` is missing the second line becomes `Metadata: (not found or error: <reason>)`; when there are no events that section is `(No events found for this project)`.

**Real return text (empty store)**

```text
$ mem-mcp --path /path/to/store   # send mem_get_context
No projects found in memory store.
```

**Real return text (list all projects)**

```json
{"jsonrpc":"2.0","id":22,"result":{"content":[{"text":"Found 1 project(s):\n- demo: Project demo (created: 2026-10-22T13:08:57.814181116Z)","type":"text"}]}}
```

**Real return text (specific project, excerpt)**

```text
=== Project: demo ===
Description: Project demo
Created At: 2026-10-22T13:08:57.814181116Z

--- README.md ---
# demo

Project demo

### Metadata
- **Created At:** 2026-10-22T13:08:57.814181116Z

## Tracked Files

| File Name | Action | Size (bytes) | Hash (SHA256) |
| --- | --- | --- | --- |
| `src/lib.rs` | Modified | 0 | `` |
| `src/main.rs` | Created | 33 | `0955a876...` |

## Recent Activity Timeline

- **2026-10-22T13:08:57.814602658Z** [`mcp-agent` on `localhost`]: touch lib.rs
- **2026-10-22T13:08:57.814181116Z** [`claude` on `laptop`]: add main.rs

--- Recent Events (last 5) ---
[2026-10-22T13:08:57.814602658Z] (20261022130857_814598761_26572_0002) touch lib.rs
  Device: localhost | Agent: mcp-agent | Files: 1
    - src/lib.rs [Modified] (0 bytes)
[2026-10-22T13:08:57.814181116Z] (20261022130857_814125160_26572_0000) add main.rs
  Device: laptop | Agent: claude | Files: 1
    - src/main.rs [Created] (33 bytes)
```

**Edge-case behavior**

- Passing a whitespace-only string as `project` is the same as omitting it (all projects are listed).
- A nonexistent project **does not raise an error**: it returns `=== Project: nope ===` + `Metadata: (not found or error: ...)` + `(No README.md available)` + `(No events found for this project)`.
- Whether `README.md` exists depends on whether `mem_render` / `mem render` has been run; at most 5 events are taken, but the latest are correctly fetched across month files (it does not look only at the newest month file).

### 4.2 mem_add_event

Records one event and its associated file changes. This is the primary entry point for agents to write memory.

**Parameters**

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `project` | string | **Yes** | — | Project name |
| `summary` | string | **Yes** | — | Event summary |
| `device` | string | No | Local hostname | Device name; `null`/whitespace-only is treated as not given and falls back to the hostname (same rules as the CLI) |
| `agent` | string | No | `mcp-agent` | Recorder name; whitespace-only falls back to the default |
| `files` | array | No | `[]` | List of affected files; omitted means this event has no file changes |
| `files[].file_name` | string | **Yes** | — | File path; **absolute paths are normalized** to a store / git repository / CWD relative path |
| `files[].action` | string | No | `Modified` | The schema declares the enum `Created` / `Modified` / `Deleted`, but the handler **does not validate the enum** |
| `files[].size_bytes` | integer | No | `0` | File size in bytes |
| `files[].hash` | string | No | `""` | SHA256 hex digest supplied by the caller; the server **neither computes nor validates it** |

**inputSchema (verbatim from tools/list)**

```json
{
  "type": "object",
  "properties": {
    "project": {"type": "string", "description": "Project name"},
    "summary": {"type": "string", "description": "Summary of what happened"},
    "device": {"type": "string", "description": "Device name (optional, defaults to current hostname)"},
    "agent": {"type": "string", "description": "Agent name (optional, defaults to 'mcp-agent')"},
    "files": {
      "type": "array",
      "description": "List of affected files",
      "items": {
        "type": "object",
        "properties": {
          "file_name": {"type": "string", "description": "Relative file path"},
          "action": {"type": "string", "enum": ["Created", "Modified", "Deleted"], "description": "Action performed on the file"},
          "size_bytes": {"type": "integer", "description": "File size in bytes"},
          "hash": {"type": "string", "description": "SHA256 hex digest"}
        },
        "required": ["file_name"]
      }
    }
  },
  "required": ["project", "summary"]
}
```

**Return text format**: `Event <event_id> recorded with <N> files` (`N` = number of files in this call, 0 when there are no files).

**Real return text**

```json
{"jsonrpc":"2.0","id":20,"result":{"content":[{"text":"Event 20261022130857_814125160_26572_0000 recorded with 1 files","type":"text"}]}}
```

The corresponding (real) request — note that `file_name` is an absolute path, which the server normalizes before storing:

```json
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"mem_add_event","arguments":{"project":"demo","summary":"add main.rs","device":"laptop","agent":"claude","files":[{"file_name":"/path/to/project/src/main.rs","action":"Created","size_bytes":33,"hash":"0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1"}]}}}
```

After writing to `projects/demo/files.jsonl` (real content), `file_name` is `src/main.rs`, so `mem_query by=filename` can match it with a relative path:

```json
{"file_id":"20261022130857_814188379_26572_0001","file_name":"src/main.rs","action":"Created","size_bytes":33,"hash":"0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1"}
```

Real request and response with the optional fields omitted (`action` defaults to `Modified`, `size_bytes` to 0, `hash` to the empty string, `device` falls back to the hostname, `agent` falls back to `mcp-agent`):

```text
// Request
{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"mem_add_event","arguments":{"project":"demo","summary":"touch lib.rs","files":[{"file_name":"/path/to/project/src/lib.rs"}]}}}
// Response
{"jsonrpc":"2.0","id":21,"result":{"content":[{"text":"Event 20261022130857_814598761_26572_0002 recorded with 1 files","type":"text"}]}}
```

**Edge-case behavior**

- When `project` is recorded for the first time and `projects/<project>/meta.json` does not exist, minimal metadata is **written automatically** (`description` = `"Project <name>"`, `github_url` = null, `created_at` = the event time), consistent with the CLI `add`.
- When `files` is omitted or is an empty array, the event is still written (`recorded with 0 files`), and no `files.jsonl` record is appended.
- `file_name` normalization rules are the same as the CLI: relative to the store root → relative to the git repository root → relative to the CWD → absolute path (the MCP side does **not** print the absolute-path warning that the CLI does).
- `action` is not enum-validated: any string such as `"Renamed"` is written to `files.jsonl` as-is; `size_bytes` / `hash` are likewise passed through without checking whether the file exists.
- Event IDs are generated by the server (`YYYYMMDDHHMMSS_nanos9_pid5_counter4`, UTC); `ts` is a fixed-width UTC nanosecond timestamp.
- Failure example: missing `project` / `summary` returns `-32603`, with a message like `Invalid arguments for mem_add_event: missing field \`project\``.

### 4.3 mem_query

Queries events by the specified dimension.

**Parameters**

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `by` | string | **Yes** | — | Query dimension enum: `project` / `file_id` / `filename` / `fuzzy` |
| `value` | string | **Yes** | — | Search term / ID / file name / regex |

**inputSchema (verbatim from tools/list)**

```json
{
  "type": "object",
  "properties": {
    "by": {"type": "string", "enum": ["project", "file_id", "filename", "fuzzy"], "description": "Query index dimension"},
    "value": {"type": "string", "description": "Search term, identifier, or regex pattern"}
  },
  "required": ["by", "value"]
}
```

`by` is case-insensitive (internally matched after `to_lowercase()`).

**Return text format**

- `by=project`: `Found <N> event(s) for project '<value>':` + one event block per event:
  ```
  [<ts>] (<event_id>) Project: <project>
  Summary: <summary>
  Device: <device> | Agent: <agent>
  Files:
    - <file_name> [<action>] size: <n> bytes, hash: <hash>
  ```
- `by=file_id`: `Found event for file_id '<value>':` + one event block.
- `by=filename`: `Found <N> event(s) affecting file '<value>':` + event blocks.
- `by=fuzzy`: `Found <N> matching event(s) for pattern '<value>':` + event blocks.
- No match:
  - project → `No events found for project '<value>'.`
  - file_id → `No event found for file_id '<value>'.`
  - filename → `No events found for filename '<value>'.`
  - fuzzy → `No events matched pattern '<value>'.`
- Invalid `by` → `-32603`, message: `Invalid query dimension 'by': '<value>'. Allowed values: 'project', 'file_id', 'filename', 'fuzzy'.`
- Invalid `fuzzy` regex → `-32603`, message: `Fuzzy search failed: regex parse error: ...`

**Real return text (by=project)**

```text
Found 2 event(s) for project 'demo':

[2026-10-22T13:08:57.814181116Z] (20261022130857_814125160_26572_0000) Project: demo
Summary: add main.rs
Device: laptop | Agent: claude
Files:
  - src/main.rs [Created] size: 33 bytes, hash: 0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1

[2026-10-22T13:08:57.814602658Z] (20261022130857_814598761_26572_0002) Project: demo
Summary: touch lib.rs
Device: localhost | Agent: mcp-agent
Files:
  - src/lib.rs [Modified] size: 0 bytes, hash:
```

**Real return text (by=file_id)**

Request (real message):

```json
{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"mem_query","arguments":{"by":"file_id","value":"20261022130857_814188379_26572_0001"}}}
```

Returned text:

```text
Found event for file_id '20261022130857_814188379_26572_0001':

[2026-10-22T13:08:57.814181116Z] (20261022130857_814125160_26572_0000) Project: demo
Summary: add main.rs
Device: laptop | Agent: claude
Files:
  - src/main.rs [Created] size: 33 bytes, hash: 0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1
```

**Real return text (by=filename)**

```text
Found 1 event(s) affecting file 'src/lib.rs':

[2026-10-22T13:08:57.814602658Z] (20261022130857_814598761_26572_0002) Project: demo
Summary: touch lib.rs
Device: localhost | Agent: mcp-agent
Files:
  - src/lib.rs [Modified] size: 0 bytes, hash:
```

**Real message (raw single-line response for `by=filename`, showing the shape of `id` and `result.content[0].text`)**

```json
{"jsonrpc":"2.0","id":24,"result":{"content":[{"text":"Found 1 event(s) affecting file 'src/lib.rs':\n\n[2026-10-22T13:08:57.814602658Z] (20261022130857_814598761_26572_0002) Project: demo\nSummary: touch lib.rs\nDevice: localhost | Agent: mcp-agent\nFiles:\n  - src/lib.rs [Modified] size: 0 bytes, hash:","type":"text"}]}}
```

**Real return text (by=fuzzy)**

```text
Found 2 matching event(s) for pattern 'main|lib':

[2026-10-22T13:08:57.814181116Z] (20261022130857_814125160_26572_0000) Project: demo
Summary: add main.rs
Device: laptop | Agent: claude
Files:
  - src/main.rs [Created] size: 33 bytes, hash: 0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1

[2026-10-22T13:08:57.814602658Z] (20261022130857_814598761_26572_0002) Project: demo
Summary: touch lib.rs
Device: localhost | Agent: mcp-agent
Files:
  - src/lib.rs [Modified] size: 0 bytes, hash:
```

**Real error returns (invalid by / invalid fuzzy regex)**

```json
{"jsonrpc":"2.0","id":10,"error":{"code":-32603,"message":"Invalid query dimension 'by': 'commit'. Allowed values: 'project', 'file_id', 'filename', 'fuzzy'."}}
{"jsonrpc":"2.0","id":5,"error":{"code":-32603,"message":"Fuzzy search failed: regex parse error:\n    [\n    ^\nerror: unclosed character class"}}
```

**Edge-case behavior**

- `by=filename` is an **exact whole-string match** on `file_name`, not a substring/fuzzy search: `value` must exactly equal the normalized `file_name` in `files.jsonl` (for example `src/hello.txt`); passing `hello.txt` will not match.
- `by=filename` only matches the **normalized relative `file_name`**; records stored with absolute paths cannot be found.
- `by=fuzzy` runs a Rust `regex` match against the raw JSONL lines of events (not full-text tokenization); a matching line must be deserializable as an `Event`.
- As with the CLI `query`, the in-memory index is rebuilt on every call; there are no pagination parameters, and all matches are returned.
- Extra argument fields are ignored; missing `by` / `value` yields `-32603` (`Invalid arguments for mem_query: missing field ...`).

### 4.4 mem_list_projects

Lists all projects in the store.

**Parameters**: none.

**inputSchema (verbatim from tools/list)**

```json
{"type": "object", "properties": {}}
```

**Return text format**

```text
Projects in memory store (<N>):
- <name>: <description>      # with meta.json
- <name>                     # without meta.json
```

Empty store: `No projects found in memory store.`

**Real return text**

```json
{"jsonrpc":"2.0","id":29,"result":{"content":[{"text":"Projects in memory store (1):\n- demo: Project demo","type":"text"}]}}
```

**Edge-case behavior**: projects are sorted lexicographically by name; only first-level subdirectories under `projects/` are read (traversed via the ignore crate: hidden directories are skipped by default, and `.gitignore` is applied when the store is a git repository).

### 4.5 mem_compact

Compacts the project's `files.jsonl`: only the latest record is kept for each `file_name`.

**Parameters**

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `project` | string | **Yes** | — | Project to compact |

**inputSchema (verbatim from tools/list)**

```json
{
  "type": "object",
  "properties": {
    "project": {"type": "string", "description": "Project name to compact"}
  },
  "required": ["project"]
}
```

**Return text format**: `Compacted project '<project>': before: <A> records, after: <B> records (reduced by <C> duplicate/old records).` (`C = A - B`)

**Real return text**

```text
// when duplicate records exist
{"jsonrpc":"2.0","id":32,"result":{"content":[{"text":"Compacted project 'demo': before: 3 records, after: 2 records (reduced by 1 duplicate/old records).","type":"text"}]}}
// when there are no duplicates
{"jsonrpc":"2.0","id":28,"result":{"content":[{"text":"Compacted project 'demo': before: 2 records, after: 2 records (reduced by 0 duplicate/old records).","type":"text"}]}}
```

**Edge-case behavior**: same implementation as the CLI `compact` (temporary file + atomic rename); returns 0/0/0 when the project file does not exist; on failure returns `-32603` with a message like `Compaction failed for project '<p>': ...`.

### 4.6 mem_sync

Syncs the store to a remote git repository: if the working tree has changes, commit → `git pull --rebase --autostash` → `git push`.

**Parameters**

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `message` | string | No | `"Auto-sync memory store"` | Commit message when there are local changes; a whitespace-only string is treated as not given |

**inputSchema (verbatim from tools/list)**

```json
{
  "type": "object",
  "properties": {
    "message": {"type": "string", "description": "Optional commit message"}
  }
}
```

**Return text format**: `Sync completed successfully: <step1>; <step2>; ...`, where the steps may include:

- `Working directory clean` or `Committed local changes ("<message>")`
- `Pulled remote changes with rebase` or `pull note: <git error>`
- `Pushed to remote successfully`

**Real return text**

```text
// with local changes + custom message
{"jsonrpc":"2.0","id":51,"result":{"content":[{"text":"Sync completed successfully: Committed local changes (\"sync from mcp-agent\"); Pulled remote changes with rebase; Pushed to remote successfully","type":"text"}]}}
// working tree clean
{"jsonrpc":"2.0","id":52,"result":{"content":[{"text":"Sync completed successfully: Working directory clean; Pulled remote changes with rebase; Pushed to remote successfully","type":"text"}]}}
```

**Edge-case behavior**

- The store is not a git repository, or `git` is unavailable → `-32603`, message: `Git status check failed: git command 'git status --porcelain' failed: ...` (real example, `LC_ALL=C`):
  ```json
  {"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"Git status check failed: git command 'git status --porcelain' failed: fatal: not a git repository (or any parent up to mount point /)\nStopping at filesystem boundary (GIT_DISCOVERY_ACROSS_FILESYSTEM not set)."}}
  ```
- A conflict discovered during pull: **returns an `-32603` error directly** (unlike the CLI's "exit code 0 + stderr warning"), and the message asks the caller to use `mem_resolve`: `Git conflict detected during pull --rebase: <git error>. Please call the \`mem_resolve\` tool to automatically deduplicate and merge the JSONL records.`
- A non-conflict pull failure (such as no upstream, or network/authentication problems) does not abort: `pull note: ...` appears among the steps and the push continues afterwards; only a push failure returns `-32603` (`git push failed: ...`).
- `git add -A` / `git commit` failures each return `-32603`, with messages beginning `git add -A failed:` / `git commit failed:`.
- The prerequisites are the same as for the CLI: a git repository, a configured remote and upstream, and a configured git user identity.
- A whitespace-only `message` is filtered out and the default message is used.

### 4.7 mem_resolve

Automatically resolves Git conflict markers in all `.jsonl` files under the store.

**Parameters**: none.

**inputSchema (verbatim from tools/list)**

```json
{"type": "object", "properties": {}}
```

**Return text format**: `Conflict resolution completed: <A> files resolved, <B> conflict markers resolved, <C> duplicate items removed, <D> non-record lines preserved.`

**Real return text**

```text
// with conflict (1 duplicate record removed + 1 unparseable line preserved)
{"jsonrpc":"2.0","id":40,"result":{"content":[{"text":"Conflict resolution completed: 1 files resolved, 1 conflict markers resolved, 1 duplicate items removed, 1 non-record lines preserved.","type":"text"}]}}
// without conflict
{"jsonrpc":"2.0","id":34,"result":{"content":[{"text":"Conflict resolution completed: 0 files resolved, 0 conflict markers resolved, 0 duplicate items removed, 0 non-record lines preserved.","type":"text"}]}}
```

**Edge-case behavior**

- Same implementation as the CLI `resolve`: deduplicate (later occurrences win), sort by time/ID, preserve unparseable lines verbatim, re-emit original line text as-is (unknown fields are not lost); when `.git` exists, the rewritten files are `git add`ed; `git rebase --continue` is run only when a rebase is actually in progress.
- No conflict is not an error: the all-zero success text is returned.
- Only `.jsonl` is handled; conflicts in non-JSONL files such as `meta.json` are not resolved and may make `git rebase --continue` fail → returns `-32603` with a message beginning `Conflict resolution failed:`. In that case the file must be resolved manually before continuing the rebase (see [CLI.md](CLI.md#37-resolve) and [TROUBLESHOOTING.md](TROUBLESHOOTING.md)).

### 4.8 mem_render

Generates or updates `projects/<project>/README.md`.

**Parameters**

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `project` | string | **Yes** | — | Name of the project to render |

**inputSchema (verbatim from tools/list)**

```json
{
  "type": "object",
  "properties": {
    "project": {"type": "string", "description": "Project name to render README for"}
  },
  "required": ["project"]
}
```

**Return text format**: `Successfully rendered README for project '<project>'.`

**Real return text**

```json
{"jsonrpc":"2.0","id":26,"result":{"content":[{"text":"Successfully rendered README for project 'demo'.","type":"text"}]}}
```

**Edge-case behavior**

- Same implementation as the CLI `render`: the README contains metadata, the latest file table, and a timeline of the latest 10 events (newest first), overwritten on every run.
- When `meta.json` is missing, in-memory fallback metadata is used (`description` = `Project <name>`, `created_at` = current time), and it is **not written back** to `meta.json`.
- It also succeeds when the project has no files or events; the README then shows `_No files tracked yet._` / `_No events recorded yet._`.
- On failure (e.g. the directory is not writable) it returns `-32603`: `Failed to render README for project '<p>': ...`.

## 5. Error Codes and Semantics

| Error code | Trigger scenario | Example message |
| --- | --- | --- |
| `-32700` | The whole line cannot be parsed as a valid request object (including JSON syntax errors, missing `jsonrpc`/`method` fields, batch arrays) | `Parse error: EOF while parsing a value at line 1 column 34` |
| `-32601` | Unknown method; or `params.name` of `tools/call` is not one of the 8 tools | `Method not found: no/such/method`; `Unknown tool: mem_does_not_exist` |
| `-32602` | The outer `params` of `tools/call` cannot be deserialized: missing `name`, missing `params`, etc. | `Missing parameters for tools/call`; `Invalid tool call parameters: missing field \`name\`` |
| `-32603` | **All** errors during tool execution: missing or wrongly typed handler arguments, business failures, git failures, regex errors, etc. | `Invalid arguments for mem_render: missing field \`project\``; `Invalid query dimension 'by': ...`; `Git status check failed: ...` |

**Real error messages**

```text
// -32700: unparseable input (id is null)
{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error: EOF while parsing a value at line 1 column 34"}}

// -32601: unknown method
{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"Method not found: no/such/method"}}

// -32601: unknown tool
{"jsonrpc":"2.0","id":6,"error":{"code":-32601,"message":"Unknown tool: mem_does_not_exist"}}

// -32602: tools/call missing params
{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"Missing parameters for tools/call"}}

// -32602: invalid params for tools/call
{"jsonrpc":"2.0","id":8,"error":{"code":-32602,"message":"Invalid tool call parameters: missing field `name`"}}

// -32603: handler arguments missing
{"jsonrpc":"2.0","id":9,"error":{"code":-32603,"message":"Invalid arguments for mem_render: missing field `project`"}}

// -32603: business failure (invalid query dimension)
{"jsonrpc":"2.0","id":10,"error":{"code":-32603,"message":"Invalid query dimension 'by': 'commit'. Allowed values: 'project', 'file_id', 'filename', 'fuzzy'."}}
```

**Semantic points**

- **Errors do not terminate the server**: after an unknown method, an unknown tool, or bad JSON, the server continues to handle subsequent requests. Real verification: after sending an unknown tool and receiving `-32601`, an immediately following `ping` still returns normally:
  ```json
  {"jsonrpc":"2.0","id":12,"result":{}}
  {"jsonrpc":"2.0","id":13,"result":{}}
  ```
  (these are the ping after bad JSON and the ping after the unknown tool, respectively.)
- Success and error responses are mutually exclusive: a success contains no `error`, and an error contains no `result`; there is no MCP `isError` field.
- An unparseable line affects only that line: the loop continues after `-32700` (`id` is `null`).
- Tool execution errors use `-32603`; clients should treat `error.message` as displayable error text; the message prefix differs between tools (see each tool section).
- Only errors in the outer parameter shape of `tools/call` are `-32602`; errors in a tool's internal arguments are uniformly `-32603`.

## 6. Recommended Workflow for Sharing a Store with the CLI

Recommended rhythm for the agent side (`mem-mcp` needs no extra initialization at startup):

1. **Start work**: `mem_get_context` (without `project`, first see which projects exist; with `project`, read metadata + README + latest 5 events) to understand the context.
2. **After changes**: `mem_add_event` records a summary and the file changes (`file_name` as a relative path is safest; absolute paths are normalized).
3. **Need history**: `mem_query` (`by=project` for everything, `by=filename` for a specific file, `by=fuzzy` to find keywords).
4. **Periodic maintenance**: `mem_render` refreshes the project README; `mem_compact` compacts `files.jsonl`; `mem_sync` commits and syncs to the remote.
5. **On conflict**: after `mem_sync` returns a conflict error, call `mem_resolve` as the error suggests, then call `mem_sync` once more.

Example session sequence (each line is one request; see the corresponding tool section for actual output):

```text
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mem_get_context","arguments":{"project":"demo"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mem_add_event","arguments":{"project":"demo","summary":"implement parser","files":[{"file_name":"src/parser.rs","action":"Created","size_bytes":1200}]}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"mem_query","arguments":{"by":"filename","value":"src/parser.rs"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"mem_render","arguments":{"project":"demo"}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"mem_sync","arguments":{"message":"agent: add parser"}}}
```

Notes for sharing with the CLI:

- **Same store**: the CLI's `mem list` / `mem query` can immediately see events written by MCP, and vice versa; the two are separate processes and serialize appended writes with `fs2` file locks.
- **Concurrent writes**: multiple MCP processes / the CLI appending at the same time is safe (file locks), but `mem_compact` and sync/append are not a global transaction; avoid running `compact` concurrently on a critical sync path.
- **Sync is not automatic**: writing an event only lands in local files; cross-device visibility requires an explicit `mem_sync` (or CLI `mem sync`).
- **Consistent naming**: `project` maps directly to the `projects/<project>/` directory name and stays consistent with the CLI side; do not give the same project different names on the two sides.
- **Consistent file identity**: both sides normalize `file_name` by the same rules, which is what allows matching across devices and entry points.

## 7. Limitations

- **Tools only**: does not provide resources, prompts, logging, sampling, or other MCP capabilities; `capabilities` contains only `tools`.
- **No authentication**: it is a local stdio process whose permissions are inherited from the client that launches it; **do not** expose `mem-mcp` over the network.
- **Single-process serial**: the next line is read only after the current request is fully handled; potentially slow git operations such as `mem_sync` / `mem_resolve` block subsequent requests.
- **No file verification**: `mem_add_event` does not read files and does not compute SHA256; `size_bytes` / `hash` are entirely supplied by the caller and are not validated on write; `action` is not validated against the enum either.
- **No pagination**: `mem_query` / `mem_get_context` return results in one shot; queries are slower on large stores (the in-memory index is rebuilt every time).
- **Context truncation**: `mem_get_context` takes only the latest 5 events, and the `mem_render` timeline takes only the latest 10.
- **Sync prerequisites**: `mem_sync` requires the store to be a git repository with a usable remote/upstream and a git user identity; otherwise it returns `-32603`.
- **Conflict resolution covers JSONL only**: `mem_resolve` does not handle conflicts in non-JSONL files such as `meta.json` (they may need to be resolved manually followed by `git rebase --continue`).
- **Fixed protocol version**: always returns `2024-11-05`; does not validate the client version, does not support batch arrays, and does not support server-initiated notifications.
- **Project names are not validated**: `project` is concatenated directly as a directory name under `projects/`, with no character allowlist validation; it is suitable only for trusted local callers.

## 8. Related Documentation

- [CLI.md](CLI.md) — complete reference for all `mem` subcommands and arguments
- [ARCHITECTURE.md](ARCHITECTURE.md) — component layering, read/write paths, sync and concurrency
- [DESIGN.md](DESIGN.md) — design trade-offs, consistency model, IDs and file identity
- [MIGRATION.md](MIGRATION.md) — historical data issues such as legacy timestamp formats and legacy absolute-path records
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — common failures and recovery steps
- [README.md](../README.md) — project overview and quick start
