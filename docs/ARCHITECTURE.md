**English** · [简体中文](zh-CN/ARCHITECTURE.md)

# ACDM Architecture

> Intended for developers who want to understand how ACDM works, plan to contribute code, or troubleshoot storage/sync problems.
> This document describes the actual behavior of the current source (v0.1.4); for the rationale behind the design see [DESIGN.md](DESIGN.md), and for command usage see [CLI.md](CLI.md) and [MCP.md](MCP.md).

## 1. Design goals and constraints

ACDM shares "project memory" across multiple machines and multiple AI agents. It does not aim for strong consistency or real time; instead it bets correctness on a **readable append-only log + Git transport**.

Goals:

- **Cross-device**: the same project sees the same event history on different machines;
- **Offline-capable**: local reads and writes keep working without a network;
- **Serverless**: no database or backend process is required — the Git repository is the transport layer;
- **Auditable**: all data is text JSONL, diffable and human-readable;
- **Multiple entry points**: CLI (humans/scripts) and MCP (agents) share the same core logic;
- **No silent data loss**: corrupt lines, unknown fields, and conflict lines must never be dropped silently.

Constraints (intentional):

- Only a weak-consistency model is supported: each device writes locally and syncs periodically; there are no cross-device transactions;
- Git is the only transport layer; there is no built-in network protocol;
- File contents are not stored, only metadata (path / size / SHA256);
- Writes on one machine are serialized by a store-wide advisory lock; reads are lock-free;
- Inputs are validated before touching the filesystem: project names must be one safe path component (see 5.3);
- Single-machine scale assumption: suitable for repository-level stores (thousands to tens of thousands of events); there is no persistent index, so every query rebuilds it;
- Time ordering depends on device clocks (see the consistency-model section of [DESIGN.md](DESIGN.md)).

## 2. Components and layers

```
┌─────────────────────────┐        ┌─────────────────────────┐
│  mem (mem-cli)          │        │  mem-mcp                │
│  8 subcommands          │        │  stdio JSON-RPC 2.0     │
└────────────┬────────────┘        └────────────┬────────────┘
             │                                  │
             └──────────────┬───────────────────┘
                            ▼
┌─────────────────────────────────────────────────────────────┐
│  mem-core (library)                                         │
│  ┌──────────┬──────────┬───────────┬──────────┬──────────┐  │
│  │ models   │ util     │ storage   │ index    │ id_gen   │  │
│  ├──────────┼──────────┼───────────┼──────────┼──────────┤  │
│  │ git      │ resolve  │ render    │ compact  │          │  │
│  └──────────┴──────────┴───────────┴──────────┴──────────┘  │
└───────────────┬──────────────────────────────┬──────────────┘
                ▼                              ▼
        ┌───────────────┐              ┌───────────────┐
        │ filesystem    │              │ git process   │
        │ (.mem/events/ │              │ status/add/   │
        │  projects/)   │              │ commit/pull/  │
        │               │              │ push/rebase   │
        └───────────────┘              └───────────────┘
```

Responsibilities and dependency direction of the three crates:

| Crate | Type | Responsibility | Dependencies |
| --- | --- | --- | --- |
| `mem-core` | library | Data structures, IDs, storage, index, Git, conflict resolution, rendering, compaction, shared utilities | Does not depend on any binary; calls `git` as a subprocess |
| `mem-cli` | executable `mem` | 8 subcommands: `init` / `add` / `query` / `list` / `compact` / `sync` / `resolve` / `render` | `mem-core`, `clap` |
| `mem-mcp` | executable `mem-mcp` | stdio MCP server (JSON-RPC 2.0) exposing 8 tools | `mem-core`, `serde`, `serde_json` |

Dependencies point in one direction only: `mem-cli` / `mem-mcp` → `mem-core`; `mem-core` knows nothing about any entry point. Both entry points locate the store through [`resolve_store_root`](../mem-core/src/util.rs) and share the same timestamp, ID, and path-normalization functions, which avoids "the CLI and the MCP behave differently".

The MCP entry point is a newline-delimited JSON-RPC 2.0 server over stdio. It speaks one protocol version (`jsonrpc: "2.0"`) and separates two kinds of failure: protocol errors are JSON-RPC error responses (`-32700` malformed JSON, `-32600` invalid request, `-32601` unknown tool/method, `-32602` invalid arguments), while tool-level business failures are returned as a **successful response with `result.isError = true`** so an agent can read the message and retry. A single request line is capped at 16 MiB and discarded in a stream-preserving way when exceeded, so one oversized request cannot take the server down or desynchronize the stream. A message **without** an `id` is a JSON-RPC notification and gets no response; a message **with** an `id` must always be answered, so a `notifications/*` method with an `id` is treated as an ordinary request and falls through to `-32601` (`Method not found`) instead of leaving the client waiting forever.

## 3. Write path

### 3.1 `mem add` (CLI)

The entry point is the `Commands::Add` branch in [`mem-cli/src/main.rs`](../mem-cli/src/main.rs):

1. clap parses the global `--path` and the `add` arguments; [`validate_project_name`](../mem-core/src/storage.rs) rejects an unsafe project name before any store operation; [`resolve_store_root`](../mem-core/src/util.rs) yields the store root directory.
2. Construct a `MemStore` and call [`record_file_event`](../mem-core/src/storage.rs). It works in two phases: **before** taking the store-wide write lock it normalizes each path and handles **one file handle at a time** — the path is opened exactly once (on Unix with `O_NONBLOCK`, so a FIFO swapped in can never block the command), and that same handle is used for the `is_file()` check and for streaming the SHA-256 hash. Slow file I/O therefore never holds the store lock, and because the object that is checked is the object that is hashed there is no TOCTOU gap. `NotFound` while opening or hashing records the path as `Deleted` (a file may disappear concurrently); a path that is not a regular file — FIFO, device node, directory — aborts with `not a regular file`. Duplicate normalized `file_name`s **within one batch** are collapsed to their first occurrence. Then it takes the lock and keeps the whole read latest → decide `Created`/`Modified` → ensure metadata → append sequence inside one critical section (see 3.3), so a concurrent writer can neither make the decision stale nor race the metadata write.
3. For each file to record:

   | Step | Behavior |
   | --- | --- |
   | Path normalization | [`normalize_file_name(file_path, &store.root)`](../mem-core/src/util.rs); if the result is still an absolute path (the file is neither inside the store, nor inside any Git repository, nor under the CWD), print a warning to stderr. A final symlink to a file keeps its own name (only the parent is canonicalized), so an in-repo symlink pointing outside the work tree stays repository-relative |
   | Existence | Checked against the **original path supplied by the user**, not the normalized name |
   | Missing | `action = Deleted`, `size_bytes = 0`, `hash = ""`; a path that disappears while being opened or hashed is also recorded as `Deleted` instead of failing the command |
   | Present | Must be a **regular file**, checked on the **same open handle** that is then hashed (single-handle: no TOCTOU race); [`hash_reader`](../mem-core/src/models.rs) streams it through SHA-256 in 64 KiB chunks → `size_bytes` = byte count, `hash` = SHA256 hex (never loaded whole). FIFOs, device nodes and directories are rejected with `cannot record '<path>': not a regular file` |
   | action | `Modified` if the normalized name already exists in the "latest file records", otherwise `Created` |
   | duplicates | Paths that normalize to the same `file_name` within one batch are collapsed into one record (the first occurrence wins) |
   | file_id | Generated by [`generate_id()`](../mem-core/src/id_gen.rs) |

4. Build the event fields: `ts = now_rfc3339()`, `event_id = generate_id()`, `device` from `-d/--device` or [`default_device_name()`](../mem-core/src/util.rs), and `agent` defaulting to `human` via clap.
5. Metadata is ensured under the same lock: if `read_project_meta(project)` fails (missing **or corrupt**), a minimal `meta.json` is written (`description = "Project <name>"`, `created_at = ts`). A metadata write failure aborts the whole command instead of being ignored; a project directory created earlier by `mem render` without metadata is healed here.
6. Append the event and its `FileRecord`s to disk (see 3.3) and print the summary; paths whose normalized identity is still absolute get the usual stderr warning.

### 3.2 `mem_add_event` (MCP)

The entry point is the `tools/call` dispatch and `handle_add_event` in [`mem-mcp/src/main.rs`](../mem-mcp/src/main.rs):

1. Deserialize `AddEventArgs` from the JSON-RPC `params.arguments`: `project` and `summary` are required; `device`, `agent`, and `files` are optional.
2. **Validate the project name first** and return a tool-level error immediately (`Invalid project: ...`) — before any ID is generated or any directory could be created; nothing is left on disk for a rejected name.
3. Defaults: `device = default_device_name()`, `agent = "mcp-agent"`; for each file `action = "Modified"`, `size_bytes = 0`, `hash = ""`.
4. Each `file_name` is normalized with `normalize_file_name` as well (MCP clients commonly send absolute paths); `file_id` is generated by `generate_id()`.
5. Call [`append_event_with_meta`](../mem-core/src/storage.rs) with the bootstrap metadata: the metadata check/write and both appends happen under one store-wide lock, and a metadata write failure now aborts the tool call (it is no longer swallowed). Missing or corrupt `meta.json` is healed in the same critical section.
6. Return the text: "`Event <event_id> recorded with <n> files`".

### 3.3 How `append_event` writes to disk

[`MemStore::append_event`](../mem-core/src/storage.rs) is a thin wrapper over [`append_event_with_meta`](../mem-core/src/storage.rs), the only write entry point for events:

1. Validate the project name, then take the **store-wide write lock** and hold it for the optional metadata write and both appends below.
2. Compute the shard name: if `ts` has length ≥ 7 and its 5th byte is `-`, take `ts[..7]` (that is, `YYYY-MM`); otherwise fall back to `default`.
3. Ensure `events/` exists, open `events/YYYY-MM.jsonl` with `create + append`.
4. Take an `fs2` exclusive lock → serialize the event with `serde_json::to_string(event)` into a **single line** of JSON → append with `writeln!` → `flush` → unlock.
5. If `event.files` is non-empty: ensure `projects/<project>/` exists, open `projects/<project>/files.jsonl` the same way, and under an exclusive lock write **one line per `FileRecord`**, then `flush` and unlock.

The two appends (event log + file view) are not in a single transaction: if the process crashes between them, the store can momentarily hold "event written, `files.jsonl` not updated". `events/*.jsonl` is the complete history (source of truth); `files.jsonl` is a derived projection that [`mem compact`](../mem-core/src/compact.rs) rebuilds from the event stream (see 6.4), which also heals this state.

### 3.4 Partial writes and fault tolerance

- A failed or interrupted append can leave a truncated last line; the read side parses each line independently and "skips" lines that fail to parse instead of failing the whole read (`read_events_for_month`, etc.).
- `mem resolve` preserves such unparseable lines verbatim (see section 7), so a single truncated write does not lose data.
- Writes only `flush`, never `fsync`: a power loss may lose the last in-memory append, but it will not corrupt existing content.

## 4. Read path

### 4.1 Building `MemIndex`

[`MemIndex::build`](../mem-core/src/index.rs) is the shared foundation of `query` and MCP `mem_query`; it is **rebuilt on every query**:

1. List all `.jsonl` files under `events/` and sort them (not limited to monthly shards).
2. For each file, read the whole shard with `fs::read` (plain buffered I/O — no memory mapping, so a concurrent truncation cannot cause `SIGBUS`), split on `\n`, and accumulate the byte offset: `offset += line_len + 1`.
3. For each line that parses as an `Event`, build an `EventPointer { event_id, file_path, byte_offset, ts, project }`.
4. Populate three index dimensions:

   | Dimension | Type | Purpose |
   | --- | --- | --- |
   | `by_project` | `HashMap<String, Vec<EventPointer>>` | List all events of a project |
   | `by_file_id` | `BTreeMap<String, EventPointer>` | Look up the owning event by exact `file_id` (later duplicate IDs win) |
   | `by_filename` | `HashMap<String, Vec<EventPointer>>` | Find all events involving a `file_name` |

### 4.2 Reading a single event

`index.read_event(pointer)`: open `pointer.file_path`, `seek` to `pointer.byte_offset`, read one line with `BufReader::read_line`, and deserialize it. The index stores only offsets and never caches event bodies, so memory usage is proportional to the number of events and essentially independent of total event body size.

### 4.3 Regex fuzzy search

`MemIndex::fuzzy_search(store, pattern)`: compile the pattern once with `RegexBuilder` and a 10 MiB compiled-program size limit (so an agent-supplied pathological pattern cannot exhaust memory); scan all `events/*.jsonl` in filename order; for each non-empty line, decode it as UTF-8 and match the regex against the **raw JSON line text**, then parse hits into `Event`. Matching is regex-based rather than tokenized; any field in the line (including `summary`, `project`, and paths) can match.

### 4.4 Latest events: `read_latest_events_for_project`

[`MemStore::read_latest_events_for_project`](../mem-core/src/storage.rs) is designed specifically for "the latest N":

1. `month_files()` lists the month shards under `events/` (`is_month_shard`: the file stem must be `YYYY-MM`, exactly 7 characters in a digits-plus-hyphen structure), sorted ascending.
2. **Iterate from the newest month to the oldest month.**
3. Within each month: `read_events_for_month` loads the file and filters to the given `project` → `sort_events` (parse-aware time ordering) → take from the back and append to the result.
4. Stop early once `n` events are collected; finally `reverse()` the whole result to return **oldest first, newest last**.

This algorithm ensures that, across months, newer-month events are never missed because "older month files sort earlier lexicographically"; and within a month, legacy timestamp formats still sort by real time.

### 4.5 Latest file state

`read_project_files(project)` reads every record from `projects/<project>/files.jsonl`; `get_latest_files(project)` `insert`s them one by one into a `BTreeMap<file_name, FileRecord>` (later records with the same `file_name` win) and finally returns them sorted by `file_name`. `list -p`, `render`, and `compact` are all based on it.

### 4.6 CLI / MCP query mapping

| Query entry | Implementation path |
| --- | --- |
| `mem query -p <project>` / MCP `by=project` | `MemIndex.by_project` → `read_event` one by one |
| `mem query --file-id <id>` / MCP `by=file_id` | `MemIndex.by_file_id` → locate the event, then find the corresponding `FileRecord` |
| `mem query -n <filename>` / MCP `by=filename` | `MemIndex.by_filename` (exact key match; pass the normalized relative path) |
| `mem query --fuzzy <regex>` / MCP `by=fuzzy` | `MemIndex::fuzzy_search` |
| `mem list` | `list_projects()` (`ignore::WalkBuilder` depth 1, respecting `.gitignore`) + each project's `meta.json` and latest file count |
| `mem list -p` | `get_latest_files` |
| MCP `mem_get_context` | `read_project_meta` + read `projects/<name>/README.md` + `read_latest_events_for_project(project, 5)` |

## 5. Storage format specification

### 5.1 Directory tree

```
<store root>/
├── .mem                       # store marker file (JSON)
├── .gitignore                 # generated by mem init (not overwritten if present)
├── events/
│   └── YYYY-MM.jsonl          # event log, sharded by month, append-only
└── projects/
    └── <project>/
        ├── files.jsonl        # file record view, append-only (replaced wholesale by compact)
        ├── meta.json          # project metadata, overwritten wholesale
        └── README.md          # generated by mem render
```

### 5.2 Event shards

- Filename: `events/YYYY-MM.jsonl`, where `YYYY-MM` comes from the event timestamp (UTC).
- One compact JSON `Event` per line (`serde_json::to_string`, no indentation), terminated by `\n`.
- Shards are append-only; historical months are never rewritten because of new events.
- Edge case: when `ts` is malformed, events are written to `events/default.jsonl`; the month-shard read paths only accept `YYYY-MM`, while `MemIndex` / `read_all_events` scan every `.jsonl` under `events/`.

### 5.3 Data structures

`Event` (one line = one record):

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `event_id` | string | yes | `YYYYMMDDHHMMSS_nanos9_pid5_counter4`, UTC |
| `ts` | string | yes | Fixed-width UTC nanosecond timestamp (30 characters) |
| `device` | string | yes | Device name; CLI/MCP default to the hostname |
| `agent` | string | yes | Recorder; CLI defaults to `human`, MCP defaults to `mcp-agent` |
| `project` | string | yes | Project name (also used as the `projects/<name>/` directory name) |
| `summary` | string | yes | Change summary |
| `files` | `FileRecord[]` | yes (may be an empty array) | File records associated with this event |

`FileRecord`:

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `file_id` | string | yes | ID in the same format as `event_id` |
| `file_name` | string | yes | Normalized store / Git-repository-relative path, `/`-separated |
| `action` | string | yes | `Created` / `Modified` / `Deleted` |
| `size_bytes` | number | yes | File size in bytes; `0` for `Deleted` |
| `hash` | string | yes | SHA256 hex; empty string for `Deleted` |

`ProjectMeta` (`projects/<name>/meta.json`):

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | string | yes | Project name |
| `description` | string | yes | Project description |
| `github_url` | string \| null | no (may be omitted or `null`) | Associated repository URL |
| `created_at` | string | yes | Creation time (fixed-width UTC nanoseconds) |

Every `project` value (and every CLI/MCP project argument) is validated by [`validate_project_name`](../mem-core/src/storage.rs) before it is used as a path: it must be non-empty, exactly one normal path component (no `.`/`..`/absolute paths), must not contain `/`, `\`, or control characters, and must be at most 200 bytes. It must also be representable on Windows: DOS device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, case-insensitive, checked on the part before the first dot so `nul.log` is reserved too), names ending in `.`, and the Windows-invalid characters `<>:"|?*` are rejected on every platform, so a store created on Unix still checks out on Windows. This prevents a project name from escaping `projects/` and from breaking cross-platform checkouts.

### 5.4 The `.mem` marker file

`mem init` / `MemStore::init` writes it at the store root (not overwritten if it already exists). The marker is created **last and atomically**: `init` first creates `events/`, `projects/` and `.gitignore`, then writes a sibling `.mem.tmp` and `fs::rename`s it to `.mem` (atomic within one directory). Any process that observes `.mem` therefore observes a fully initialized store and a complete marker, never a half-written file; after the rename the bootstrap lock file in the runtime/temp directory is removed, and a `.mem.tmp` left by an earlier crashed init is cleaned up. Marker content:

```json
{
  "format": 1,
  "created_at": "2026-10-22T13:02:45.123456789Z"
}
```

Purpose: `is_store_root(dir)` decides that a directory is a store root when "a `.mem` file exists" or "an `events/` directory exists", and [`find_store_root`](../mem-core/src/util.rs) walks upward from any subdirectory to discover the store. CLI and MCP use the same rule, so an MCP client launched from any working directory finds the nearest store.

### 5.5 `.gitignore`

If the store root has no `.gitignore` yet, `init` writes the default contents:

```
target/
*.swp
*.tmp
*.tmp.*
*~
.DS_Store
```

`*.tmp` covers the `files.jsonl.tmp` temporary file used by `compact`, so a leftover file is not committed if the process is killed.

### 5.6 IDs and timestamps

**ID format** ([`id_gen.rs`](../mem-core/src/id_gen.rs)):

```
YYYYMMDDHHMMSS_<nanos:09>_<pid%100000:05>_<counter%10000:04>
```

| Part | Width | Source | Purpose |
| --- | --- | --- | --- |
| Date and time | 14 | `Utc::now().format("%Y%m%d%H%M%S")` | UTC, sortable across time zones |
| Nanoseconds | 9 | `timestamp_subsec_nanos()` | Ordering within a second + cross-device uniqueness |
| pid | 5 | `process::id() % 100_000` | Distinguish processes on the same machine |
| Counter | 4 | Per-process counter seeded from (start time, pid) and advanced per ID, `% 10_000` | De-duplicate bursts within one process |

Fixed width + high-order time → lexicographic order equals chronological order. The process counter starts from a random offset derived from the process start time and pid instead of zero, so two processes that share a pid (common in containers where every process is pid 1) and mint IDs in the same nanosecond are far less likely to produce the same counter; the ID format itself is unchanged. The time embedded in `event_id` and the event `ts` come from two independent clock reads and may differ by hundreds of nanoseconds; ordering uses `ts`.

**Timestamp format** ([`util.rs`](../mem-core/src/util.rs)):

```
YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ     # exactly 30 characters, UTC
```

- Generated by `now_rfc3339()` (`to_rfc3339_opts(SecondsFormat::Nanos, true)`);
- The read side's `parse_ts()` also accepts legacy formats: nanoseconds with `+00:00`, and second-precision `Z`;
- Ordering uses `ts_cmp()`: compare parsed times first, fall back to string comparison when parsing fails; unparseable timestamps sort after parseable ones; `event_cmp` uses `event_id` as a stable tie-breaker for identical timestamps.

### 5.7 `file_name` normalization

[`normalize_file_name`](../mem-core/src/util.rs) turns a path into a device-independent identity, in priority order:

| Priority | Rule | Applies when |
| --- | --- | --- |
| 1 | Relative to the **store root** (e.g. `src/index.rs`) | The file is inside the store directory tree |
| 2 | Relative to the **nearest Git repository root** | The file lives in the project repository and the store is elsewhere (most common) |
| 3 | Relative to the **current working directory** | Neither of the above, but a CWD is available |
| 4 | **Absolute path** (the CLI warns) | None of the above; cannot match across devices |

Implementation details:

- Paths are made absolute lexically (`.` / `..` resolved) and the **longest on-disk-existing prefix is canonicalized**, so symbolic links are resolved for store roots and existing files; the still-missing tail is appended lexically (a deleted file keeps a deterministic identity); a final symbolic link **to a file** is a deliberate exception: only its parent directory is canonicalized and the link's own name is kept, so an in-repo symlink pointing outside the work tree is still recorded as a repository-relative path instead of degrading to an absolute path (symlinks to directories, store roots and CWDs keep the full canonicalize behaviour; a dangling file symlink is also kept by name);
- Both sides of every `strip_prefix` comparison are canonicalized the same way, so a store reached through a symlink yields the same relative identity as its real path;
- Components always use `/` (Windows `\` is replaced);
- Non-UTF-8 paths go through `to_string_lossy` and may be lossy.

### 5.8 Project README generation rules

[`render_project_readme`](../mem-core/src/render.rs) emits a fixed structure:

```markdown
# <project>

<description>

### Metadata
- **Created At:** <meta.created_at>
- **GitHub:** [<url>](<url>)          # when github_url is present

## Tracked Files

| File Name | Action | Size (bytes) | Hash (SHA256) |
| --- | --- | --- | --- |
| `<file_name>` | <action> | <size_bytes> | `<hash>` |

## Recent Activity Timeline

- **<ts>** [`<agent>` on `<device>`]: <summary>
```

- When metadata is missing, it falls back to `Project <name>` and `created_at` is the current time;
- The file table comes from `get_latest_files` (the latest record per `file_name`);
- The timeline takes the 10 most recent events, newest first;
- Every interpolated value (`name`, `description`, `created_at`, `github_url` label, file name/action/hash, timeline fields) is escaped before it is written: `|` becomes `\|` and CR/LF are folded to spaces, so recorded data cannot break the Markdown table/timeline structure or inject content;
- `write_project_readme` rewrites the whole file with `create + truncate + exclusive lock`; it does not append.

## 6. Concurrency and consistency

### 6.1 Same-machine concurrency

| Operation | Lock | Notes |
| --- | --- | --- |
| Every store write: `append_event`/`append_event_with_meta`/`record_file_event`, `compact`, `resolve`, `sync`, `write_project_meta`, `write_project_readme`, `MemStore::init` | store-wide advisory lock (`fs2`) | The lock target is the store's `.mem` marker, opened **read-only** and flocked: nothing is written, the Git work tree stays clean, and the store's file permissions scope the lock. A marker-less store (only the very first `init`) falls back to an owner-only (`0600`) lock file under `$XDG_RUNTIME_DIR` / the system temp dir, named from a hash of the user and the canonical store path — never inside the repository, and collision-free across users on a shared machine. |
| `append_event` (event line) | additional `fs2` exclusive lock | Lock after opening `events/<ym>.jsonl`; write one line, `flush`, unlock |
| `append_event` (file lines) | additional `fs2` exclusive lock | Same handling for `files.jsonl` |
| `resolve` write-back | additional `fs2` exclusive lock | Rewrite the whole file, then `git add` |

The store-wide lock is what makes multi-file operations atomic **between processes on one machine**: an append cannot interleave with a projection rebuild, a conflict resolution, or a rebase. The `fs2` locks are **advisory** and do not work across devices; cross-device ordering is established by Git plus `mem resolve` after a sync.

### 6.2 Read side

- Read paths take **no locks**; each read opens the file and reads it with plain `fs::read`, so no long-lived mapping or handle is kept.
- There is no `mmap`: a concurrent `resolve`/`pull` may truncate or replace a file at any moment, and reading through a mapping of a truncated file is undefined behavior (and can raise `SIGBUS`). A normal read is always safe.
- The read side tolerates per-line faults: a line that is not valid JSON is skipped; one corrupt line does not make the store unreadable.
- Empty files are handled explicitly (an empty result is returned).

### 6.3 Cross-device consistency

- Each device writes its own local store independently; **nothing is visible across devices until sync**; consistency is reached only after a successful `mem sync` (weak consistency).
- There is no cross-device lock: two devices modifying the same store concurrently are merged by Git's rebase plus `mem resolve` (see section 7).
- Semantically this is "event log + latest state": events are ordered approximately by time, and conflicting records with the same ID use last-write-wins, not field-level merging.

### 6.4 Projection rebuild in `compact`

[`compact_project_files`](../mem-core/src/compact.rs) reconstructs `files.jsonl` from the event log; it is both a compaction and a self-healing operation:

1. Validate the project name, then take the store-wide write lock and hold it through the rename below.
2. Count the non-empty lines of the existing `files.jsonl` as `records_before` (0 if the file is missing).
3. Read the authoritative event log, keep events of this project, and sort them by `ts` (parse-aware) with `event_id` as the tie-breaker (`sort_events`).
4. Replay every `FileRecord` in that order into a `BTreeMap<file_name, FileRecord>` — the last writer for each `file_name` wins. Note the deliberate asymmetry: incremental appends and `resolve` keep the **physical append order** (causal), while this replay orders by timestamp first; both are deterministic, and `compact` is the operation that regenerates the projection from the authoritative log.
5. If neither a projection nor any event exists, return a zero report without creating a directory.
6. Write the projection to `files.jsonl.tmp`, `flush` and `sync_all`, then `fs::rename` it over `files.jsonl` — an atomic replacement within the same filesystem.

Because the whole read → replay → replace sequence holds the store write lock, an append can no longer slip in between the read and the rename and lose records. Because the replay source is the event log, running it after a crash that left `files.jsonl` missing or truncated restores the complete projection. The `Records before / after / pruned` output is unchanged.

## 7. Synchronization and conflict resolution

### 7.1 Sync flow

**CLI `mem sync`** ([`mem-cli/src/main.rs`](../mem-cli/src/main.rs) → [`sync_store`](../mem-core/src/git.rs)):

```
resolve_store_root
      │
      ▼
acquire the store-wide write lock (held for the whole sync)
      │
      ▼
git status --porcelain  ──clean──▶ step: "Working directory clean"
      │dirty
      ▼
git add -A ; git commit -m "Auto-sync from <device> at <ts>"
      │
      ▼
has upstream?  ──no──▶ step: "No upstream configured; skipped pull"
      │yes
      ▼
git pull --rebase            (no --autostash; the tree was just committed)
      │
      ├─ conflict ────────▶ SyncOutcome::Conflict
      │                     CLI: print steps + tell the user to run mem resolve, exit non-zero
      │                     MCP: isError result with the same instruction
      ├─ other failure ───▶ abort with an error (never push over a failed pull)
      ▼
git push   ──▶ SyncOutcome::Success (steps printed by the caller)
```

**MCP `mem_sync`** ([`mem-mcp/src/main.rs`](../mem-mcp/src/main.rs)) calls the same `sync_store` implementation, with these differences:

- The commit message can be overridden with the `message` parameter; like the CLI, the default is `Auto-sync from <device> at <ts>` (one shared default in `sync_store`);
- On conflict or failure it returns an `isError` result telling the caller to run `mem_resolve` and retry, with the completed steps included in the text;
- On success the returned text lists the result of each step (clean/committed, pulled/skipped, pushed).

`git_has_conflicts` ([`git.rs`](../mem-core/src/git.rs)) relies only on Git's own bookkeeping — unmerged entries from `git ls-files -u`, conflict status codes from `git status --porcelain` (`UU` / `AA` / `UD` / `DU` / `DD` / `AU` / `UA`), and an active rebase/merge state (`rebase-merge`, `rebase-apply`, `MERGE_HEAD`). It deliberately never scans file contents for `<<<<<<<`: a legitimate event summary or document that merely quotes the marker would otherwise make the repository look permanently conflicted and force every sync to fail.

### 7.2 What JSONL conflicts look like

Git performs a three-way merge at the text-line level. When both shards appended different events, the conflict block looks like this:

```
{"event_id":"...","ts":"...","summary":"base","files":[]}
<<<<<<< HEAD
{"event_id":"...","ts":"...","summary":"ours","files":[]}
=======
{"event_id":"...","ts":"...","summary":"theirs","files":[]}
{"event_id":"...","ts":"...","summary":"theirs-2","files":[]}
>>>>>>> origin/main
```

Because the files are append-only, conflicts are usually "duplicate lines with the same ID" or "new lines from both sides interleaved", which suit automatic resolution with a "deduplicate + sort + preserve" strategy instead of semantic merging.

### 7.3 The `mem resolve` algorithm

Steps in [`resolve_conflicts`](../mem-core/src/resolve.rs):

1. Take the store-wide write lock (held for the whole operation) and initialise an aggregated report.
2. **Resolve-and-continue loop** (at most `MAX_REBASE_ROUNDS = 100` rounds):
   1. Recursively scan every `.jsonl` under the store (`WalkBuilder::hidden(false)`, including hidden files), sorted by path.
   2. Process files whose raw bytes contain `<<<<<<<`. Marker recognition is **exact**: CRLF's trailing `\r` is stripped, `=======` counts as a marker only when the line is exactly seven `=`, and `<<<<<<<` / `|||||||` / `>>>>>>>` only when followed by end-of-line or a space. Anything else is data and is preserved.
   3. Classify the file from "the first UTF-8-parseable line": `Event` → `FileRecord` → generic JSON (`Generic`) → `Unknown`. Non-UTF-8 lines can never be a record and are kept as raw bytes.
   4. Classify only non-marker, non-blank lines, keeping their **raw bytes**:

      | Type | Parsed successfully | Failed to parse |
      | --- | --- | --- |
      | `Event` | De-duplication key `event_id`, sort time `ts` | Kept verbatim |
      | `FileRecord` | De-duplication key `file_id` | Kept verbatim |
      | `Generic` | Key is one of `event_id` / `file_id` / `id` (falls back to the whole line) | Kept verbatim |
      | `Unknown` | — | Everything kept verbatim |

   5. De-duplicate: for the same key, later occurrences override earlier ones (last-write-wins). `Event` / `Generic` results are then sorted by `ts_cmp` + key; `FileRecord`s keep their original **causal order** (they are not re-sorted, otherwise a last-write-wins projection could be rolled back to an older state); `Unknown` is not sorted.
   6. Preserved lines (including identical duplicates, collapsed once) are re-emitted **byte-for-byte** and appended at the end of the file.
   7. Write the result back with an exclusive lock and `truncate`; if the store is a Git repository (detected with `git rev-parse --git-dir`, so linked worktrees and submodules whose `.git` is a **file** work too), stage the file with `git add -- <relative path>`. A failed `git add` is now a **hard error** — continuing would only hide the real reason why `rebase --continue` can never succeed. Accumulate the report across rounds.
   8. If no rebase is active, deduplicate the reported file list and return.
   9. Otherwise run `git rebase --continue`: success → next round (the next replayed commit may conflict too). On failure, inspect the still-unresolved files: if every one is a `.jsonl` file and this round actually resolved something, continue with the next round. When nothing is unresolved and the **index is clean** (`git diff --cached --quiet`: no staged difference between the index and `HEAD`, 0.1.4 — neither untracked scratch files nor unstaged edits to unrelated tracked files prevent recovery), the replayed commit became an **empty patch** after de-duplication (every record it added already exists upstream): git refuses to commit it ("No changes"), so the resolver drops it with `git rebase --skip` and continues. Note that `git rebase --skip` itself may reset unstaged changes to unrelated files in the working tree; that is Git's own behaviour and the resolver does not promise to preserve them. Otherwise return an error that lists the **non-JSONL files** that genuinely need manual resolution. The unresolved list comes from `git diff --name-only --diff-filter=U -z` (NUL-separated), so paths containing spaces or special characters are reported verbatim without git's quoting/escaping.
3. After 100 rounds without finishing, return an error listing the still-conflicted files and the manual recovery steps.

`ResolveReport` aggregates `resolved_files`, `total_conflicts_resolved`, `deduplicated_items`, and `lines_preserved` across all rounds.

### 7.4 Why rebase detection uses Git state directories

The old implementation matched the error text from `git rebase --continue` (the English `no rebase in progress`) to decide whether to skip, which broke under non-English locales and turned "no rebase in progress" into a hard error. It now asks Git itself via `git rev-parse --git-path` and checks whether these state paths exist:

- `<git-path>/rebase-merge` (an interactive/ordinary rebase is in progress)
- `<git-path>/rebase-apply` (an `am` / non-interactive rebase is in progress)

Resolving the paths through `--git-path` (rather than assuming `<root>/.git` is a directory) is what makes `git worktree` and submodule checkouts work: in those layouts `.git` is a *file* and the rebase state lives in a per-worktree git directory. The decision is Git's internal state, independent of the language environment; the multi-round loop uses the same bookkeeping (`git status --porcelain` conflict codes, the **index-clean** check for empty patches — `git diff --cached --quiet`, so local scratch files and unstaged unrelated edits do not block recovery, and `git diff --name-only --diff-filter=U -z` for the unresolved list) to decide whether the resolver can still make progress or whether a non-JSONL file is blocking the rebase. The `-z` form also keeps paths with spaces or special characters intact, so the reported/`jsonl` check uses the raw path instead of git's quoted rendering.

### 7.5 After conflicts are resolved

1. `mem resolve` writes the JSONL back and runs `git add`; while a rebase is in progress it keeps resolving and running `git rebase --continue` until the rebase finishes, so one call can advance several conflicting commits (up to 100 rounds).
2. The user runs `mem sync` once more: if there are still uncommitted changes they are committed, then pull/push run; if the working tree is clean, pull/push run directly.
3. During a conflict, **do not** hand-edit the JSONL or run `git checkout`; let `mem resolve` handle everything (otherwise conflict markers may be reintroduced or lines lost).

## 8. Performance characteristics and limits

### 8.1 Complexity

| Operation | Time | Memory | Notes |
| --- | --- | --- | --- |
| `append_event` | O(1) | O(event size) | Store write lock + file append; `flush`, no `fsync` |
| `MemIndex::build` | O(total event bytes) | O(largest shard + index entries) | Rebuilt on every `query` / MCP `mem_query`; each shard is read with `fs::read` |
| `read_event` | O(that line's length) | O(event size) | `seek` to the byte offset, then read one line |
| `fuzzy_search` | O(total event bytes) | O(largest shard + matching events) | Regex scan over every line; compiled program capped at 10 MiB |
| `read_latest_events_for_project` | worst case O(total event bytes) | O(records of the months read) | Searches from the newest month backwards, stops once full |
| `get_latest_files` | O(`files.jsonl` line count) | O(distinct files) | `BTreeMap` de-duplication |
| `compact` | O(total event bytes) | O(events of the project + distinct files) | Replay the event log, then temporary file + atomic rename |
| `list_projects` | O(project count) | O(project count) | `ignore` depth-1 traversal |

### 8.2 Scale assumptions

- The target scenario is **project-level memory**: thousands to tens of thousands of events, and hundreds to thousands of file records.
- `MemIndex` scans everything on every query: at the hundred-thousand-event level, a single query becomes noticeably slower (exact timing depends on event volume, disk, and CPU).
- Git sync cost grows with repository size: events are append-only and never archived, so long-term use will need archival capabilities in the future.
- Per-file size: `events/` is sharded by month to avoid unbounded single-file growth; `files.jsonl` is kept in check by `compact`.

### 8.3 Other limits

- **No persistent index/cache**: rebuilt per query and lost when the process exits;
- **Reads load whole shards**: a read uses `fs::read` per file rather than `mmap`, so memory peaks at roughly the largest month shard plus the index/results — this is the price paid for never mapping a file that another process may truncate;
- **Streaming hash on write**: `mem add` hashes files through a 64 KiB buffer, so hashing a large file does not load it into memory; MCP callers supply `size_bytes` / `hash` themselves, so no content is read there;
- **`resolve` is byte-oriented**: non-UTF-8 bytes and CRLF line endings are preserved verbatim rather than rejected; the trade-off is that unparseable content can only be preserved, not interpreted;
- **Regex resource cap**: the compiled regex program is limited to 10 MiB, so a pathological agent-supplied pattern fails with an error instead of exhausting memory;
- **No fsync on append**: appends `flush` only (the projection rebuild does `sync_all` its temp file); a power loss may lose the last in-memory append, and the store is suitable for "memory", not for financial ledgers;
- **No cross-device lock**: the store-wide lock coordinates processes on one machine only; concurrent edits on different machines still converge through Git and `mem resolve`.

## 9. Extension points

### 9.1 Adding event fields

- The read side deserializes into structs with serde, and unknown fields are **ignored** by default, so old code reading new data does not fail;
- `resolve` re-emits the original line text, so unknown fields, field order, and formatting are **preserved as-is** after conflict resolution;
- When adding a field to a struct, prefer `#[serde(default)]` so that old data remains readable;
- Strictly rejecting unknown fields would require an explicit `deny_unknown_fields` (not enabled today; this is intentional forward compatibility).

### 9.2 Adding MCP tools

1. Add `name` / `description` / `inputSchema` in [`get_tools_list`](../mem-mcp/src/main.rs);
2. Add a branch in the `tools/call` dispatch of `handle_request`, deserialize `arguments` into an argument struct (schema violations become `-32602`), and call a public function of `mem-core`;
3. Return a text result wrapped in `mcp_text_content(...)`; a business failure must be returned as `mcp_error_content(...)` (`result.isError = true`), not as a JSON-RPC `-32603` internal error.

### 9.3 Adding CLI subcommands

Add a variant and its arguments to the [`Commands`](../mem-cli/src/main.rs) enum and implement it in the `match` inside `run()`; always reuse `mem-core` for store resolution, timestamps, device names, hashing, and so on — do not reimplement them inside the CLI.

### 9.4 Incremental index

`MemIndex` currently rebuilds from scratch on every query. Possible extensions:

- Persist the `EventPointer` table (e.g. `events/.index.json`) and update it incrementally after `append_event`;
- Cache an index fragment per file keyed by `(size, mtime)`;
- Rescan only the files that changed before a query.

### 9.5 Other directions

- Event archival / snapshots (compress old events into project-level summaries and move them out of the active shards);
- Content addressing (store file contents as SHA256 blobs for cross-device replay);
- Optional field-level merge strategies (currently the whole record with the same ID uses last-write-wins);
- More transports (intentionally Git-only today).

## 10. Source index

| File | Responsibility |
| --- | --- |
| [`mem-core/src/lib.rs`](../mem-core/src/lib.rs) | Crate export surface |
| [`mem-core/src/models.rs`](../mem-core/src/models.rs) | `Event` / `FileRecord` / `ProjectMeta`, SHA-256 helpers (streaming `hash_file` / `hash_reader`) |
| [`mem-core/src/id_gen.rs`](../mem-core/src/id_gen.rs) | Cross-device unique ID generation |
| [`mem-core/src/util.rs`](../mem-core/src/util.rs) | Timestamps, store discovery, canonicalized file identity |
| [`mem-core/src/storage.rs`](../mem-core/src/storage.rs) | `MemStore`: init / append / read / month shards / store write lock / project-name validation |
| [`mem-core/src/index.rs`](../mem-core/src/index.rs) | `MemIndex`, `EventPointer`, bounded regex fuzzy search |
| [`mem-core/src/git.rs`](../mem-core/src/git.rs) | Git subprocess wrapper, conflict detection, `sync_store` |
| [`mem-core/src/resolve.rs`](../mem-core/src/resolve.rs) | Byte-level JSONL conflict resolution and multi-commit rebase driver |
| [`mem-core/src/render.rs`](../mem-core/src/render.rs) | Project README rendering with Markdown escaping |
| [`mem-core/src/compact.rs`](../mem-core/src/compact.rs) | `files.jsonl` projection rebuild from the event log |
| [`mem-cli/src/main.rs`](../mem-cli/src/main.rs) | CLI's 8 subcommands |
| [`mem-mcp/src/main.rs`](../mem-mcp/src/main.rs) | MCP server and its 8 tools |
