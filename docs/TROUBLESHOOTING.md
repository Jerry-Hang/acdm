**English** · [简体中文](zh-CN/TROUBLESHOOTING.md)

# Troubleshooting

> Audience: users running into problems while using the `mem` CLI or `mem-mcp`. This document is organized by "symptom";
> for design-level trade-offs and boundaries see [DESIGN.md](DESIGN.md), and for upgrade/compatibility issues see [MIGRATION.md](MIGRATION.md).

## Quick index

| Symptom | Most likely cause | Handling |
| --- | --- | --- |
| Records written to another directory / store not found | store resolution differs from expectation | [§1](#1-store-resolves-to-an-unexpected-location) |
| `query -n` cannot find a file that was just recorded | old absolute-path record, or project name/path mismatch | [§2](#2-query-cannot-find-the-record) |
| Timeline order looks wrong | JSONL edited by hand, or invalid ts | [§3](#3-timeline-order-looks-wrong) |
| `mem sync` reports an error | no remote / no upstream / authentication / conflict | [§4](#4-mem-sync-fails) |
| sync reports conflicts | JSONL merge conflict | [§5](#5-handling-conflicts) |
| Fewer records after `compact` | by design (deduplication) | [§6](#6-fewer-records-after-compact) |
| `README.md` is overwritten | it is a generated file | [§7](#7-project-readmemd-is-overwritten) |
| MCP client cannot connect / tools report errors | configuration path, protocol, parameters | [§8](#8-mcp-client-issues) |
| Queries become slow | index rebuilt O(N) every time | [§9](#9-performance-issues) |
| Suspected data corruption | truncated lines caused by power loss/concurrent writes | [§10](#10-data-health-check) |
| A project name is rejected (`invalid project name`) | Windows-reserved device name, trailing `.`, path separator, or other invalid component | [§12](#12-known-limitations) |

## 1. Store resolves to an unexpected location

The CLI and MCP use **the same** resolution rules (`mem-core/src/util.rs`):

1. Explicit `--path <dir>` (MCP also supports `--path <dir>` / `--path=<dir>`)
2. The `ACDM_STORE` environment variable
3. Search upward from the current working directory for an ancestor containing a `.mem` file or an `events/` directory
4. If none of the above: the current working directory

`mem init` is the exception: it only accepts an explicit `--path`; otherwise it initializes in the current directory and does not search upward.

Troubleshooting:

```bash
# Which store does the current directory resolve to? (list prints the contents; with no store it shows empty output/error hints)
mem list
# Specify it explicitly to confirm it is the one you want
mem --path /path/to/store list
# Check whether the marker file and the events directory exist
ls -la /path/to/store/.mem /path/to/store/events
```

Common pitfall: running inside a subdirectory of the store but wanting to write to the "project repository" rather than the store — because the upward search hits the store,
you should pass `--path` explicitly instead, or pin it with `ACDM_STORE`. For MCP clients, it is recommended to always configure `--path` explicitly.

## 2. Query cannot find the record

`mem query -n <file>` matches `file_name` **exactly** (not a substring match, case-sensitive, separator is `/`).
First check what is actually stored in the record:

```bash
mem --path /path/to/store list -p <project>
```

- If it shows an absolute path (for example `/home/alice/repo/src/main.rs`), the data was written by an old version:
  the read side does not rewrite it automatically, so it cannot be found by relative name. Handle it as described in "Migration B" of [MIGRATION.md](MIGRATION.md).
- If it shows `src/main.rs`, confirm that the directory separator and case used in the query are exactly the same.
- Confirm that the `-p` project name matches (the project name is the logical name used at write time, not a directory name).
- If a record written just now cannot be found: confirm that it was written to the same store (§1), and check whether the current month's file under `events/` gained a line.
- If the file lives on a path reached through a symlink: identity is normalized by canonicalizing the longest existing prefix, so a symlinked store/project directory yields the same relative `file_name` as its real path. If an older record still shows an absolute path, treat it as a legacy record and migrate it (see "Migration B" in `MIGRATION.md`).

## 3. Timeline order looks wrong

- ACDM orders time by **parse-then-compare**, not string comparison, so a mix of both old formats is still ordered correctly.
- If `ts` cannot be parsed at all: old versions wrote such events into `events/default.jsonl`. That file is not a valid monthly shard,
  so "latest events" queries ignore it; the `mem render` timeline also only includes events from monthly shards.
- Hand-editing JSONL and corrupting `ts` also causes the entry to be skipped by the read side (reads tolerate bad lines).
- The order of events within the same second is determined by the event timestamp (nanoseconds); if timestamps are equal, they are ordered by `event_id`.

## 4. `mem sync` fails

`mem sync` performs, in order: holding the store-wide write lock, automatic commit of a dirty working tree (commit message `Auto-sync from <device> at <timestamp>`) →
`git pull --rebase` (**without `--autostash`**; skipped when the branch has no upstream) → `git push`. A non-conflict pull failure **aborts** the sync
instead of continuing to push, and a Git conflict ends the command with a **non-zero exit code** (MCP returns `isError`). Any step that fails ends with a non-zero exit code.

| Symptom | Cause | Handling |
| --- | --- | --- |
| `No upstream configured; skipped pull` (not an error) | The local branch has no upstream configured | If you want to pull, run `git -C <store> push -u origin <branch>` once |
| `git push ... failed: ... No configured push destination` | No remote configured | `git -C <store> remote add origin <url>`, or re-initialize with `mem init --remote <url>` |
| `rejected` / `non-fast-forward` | The remote has new commits and the local rebase did not succeed | Run `git -C <store> pull --rebase` first, and handle conflicts per §5 |
| `git pull --rebase failed and the repository is not conflicted` | Network/authentication/index problem; the sync was aborted on purpose | Fix the underlying git problem, then re-run `mem sync` |
| `Permission denied (publickey)` / `Authentication failed` | Git credential problem | Fix the SSH key or HTTPS credentials; ACDM does not manage credentials |
| Conflicts reported (exit code 1) | JSONL merge conflict | See §5 |

Note: if the store is not a Git repository (only `mem init` was used without a remote), `sync` fails when it runs `git status`/`git push`;
in that case switch to local-only usage, or create the repository/remote.

## 5. Handling conflicts

When syncing across devices it is very common for two devices to append to the same `events/YYYY-MM.jsonl` at the same time, and Git merges it into
conflict markers. Handling:

```bash
mem --path /path/to/store sync     # reports conflicts and exits non-zero
mem --path /path/to/store resolve  # automatically resolve JSONL conflicts
mem --path /path/to/store sync     # sync again
```

`mem resolve` behavior (see [ARCHITECTURE.md](ARCHITECTURE.md#7-synchronization-and-conflict-resolution) for details):

- Removes only the **exact** Git marker shapes (`=======` must be the whole line; `<<<<<<<` / `>>>>>>>` / `|||||||` must be followed by end-of-line or a space), so a data line that merely starts with marker-like characters is preserved;
- Works on raw bytes, so non-UTF-8 lines and CRLF endings are preserved verbatim (never silently dropped); records with the same `event_id` / `file_id` are **last-write-wins**, and `FileRecord`s keep their causal order;
- Parseable records are sorted by time and written back; **unparseable lines are preserved verbatim** (placed at the end of the file);
- After writing back it runs `git add`; a failed `git add` is now a **hard error** (previously only a warning), because the following `git rebase --continue` could never succeed.
- Rebase state is resolved with `git rev-parse --git-path` and repository detection with `git rev-parse --git-dir`, independent of the system language and working in `git worktree` / submodule checkouts where `.git` is a **file**.
- It loops through a multi-commit rebase (up to 100 rounds) in one call; when a replayed commit becomes an **empty patch** after de-duplication, it is dropped automatically with `git rebase --skip` instead of aborting on git's "No changes" error.

Troubleshooting:

- A non-zero `Lines preserved` in the output means there are unparseable lines (usually truncated lines produced by an interrupted write, or invalid UTF-8). This is
  **intentional data protection**, not an error. After checking the content, delete them with an editor or leave them as they are.
- `git add failed for ...`: since 0.1.2 this is a **hard error** that stops the run (it would otherwise hide the real cause of a doomed `rebase --continue`). Add the file manually (`git -C <store> add -- <path>`) and continue, or run `mem resolve` again after fixing the cause; if the path is skipped by a `.gitignore` rule, check with `git -C <store> check-ignore -v <path>`.
- Still conflicting after resolve: check `git -C <store> status`; confirm the conflicting file is inside the repository, has a `.jsonl` suffix,
  and that a rebase/merge is currently in progress. JSONL-only conflicts are resolved and the rebase is advanced automatically; the command fails only when a **non-JSONL file conflict** remains,
  most commonly `projects/<project>/meta.json`: when two devices write metadata for the same project for the first time at the same time,
  both create `meta.json`; JSONL can merge automatically, but `meta.json` cannot.
  `mem resolve` prints an **actionable error** listing the paths that are still unresolved. Recovery:

  ```bash
  git -C /path/to/store status --short          # find the meta.json shown as UU
  # The contents are basically identical (same name/description, only created_at differs); either side works:
  git -C /path/to/store checkout --theirs projects/<project>/meta.json
  git -C /path/to/store add projects/<project>/meta.json
  git -C /path/to/store rebase --continue
  ```

  Or edit the file directly, delete the `<<<<<<<` / `=======` / `>>>>>>>` lines, and then run `git add`.
  Only after `git rebase --continue` succeeds and the rebase state directory disappears can `mem sync` continue.

## 6. Fewer records after `compact`

`mem compact -p <project>` **rebuilds `files.jsonl` from the append-only event log**, keeping the last writer per `file_name`; a reduced record count is
expected behavior (the output reports before / after / pruned). It keeps `Deleted` records.

Because the event log is the source of truth, `compact` is also the **repair command for the projection**: if `files.jsonl` was lost, truncated, or left stale
(for example by a crash between the two appends, or by an old concurrent-write race), one `compact` restores the complete latest state — `records before`
then counts the damaged projection and the `after` count is the reconstructed one. It holds the store-wide write lock, so a concurrent `mem add` on the same machine
waits instead of being lost. Still run it in a clean working tree, and `sync` once afterwards.

## 7. Project README.md is overwritten

`projects/<name>/README.md` is a file **generated** by `mem render` and is rewritten on every render. Do not edit it by hand;
if you need extra notes, write them in a different file, or change the rendering logic.

## 8. MCP client issues

- **The server does not respond**: make sure `command` in the client configuration points to a real `mem-mcp` (an absolute path is recommended),
  and `args` looks like `["--path", "/path/to/store"]`. MCP uses stdio + **line-delimited** JSON-RPC 2.0;
  no `Content-Length` header is needed.
- **Exits immediately on startup**: unknown startup arguments print usage and `exit 2` (to prevent a typo from silently using the wrong store);
  make sure you only pass `--path`.
- **The client keeps waiting for a response**: a message **without** `id` is a Notification and is never answered, by design. If your client sends a `notifications/*` method **with** an `id`, 0.1.2 answers it with `-32601 Method not found: notifications/...` rather than dropping it; if an older server left you waiting, upgrade to 0.1.2.
- **Tool call returns an error**: distinguish the two channels:
  - JSON-RPC `error` means a protocol problem: `-32700` the request is not valid JSON; `-32600` the `jsonrpc` field is not `"2.0"` or the request line exceeds 16 MiB; `-32601` unknown method or unknown tool; `-32602` the `tools/call` parameters or the tool's arguments do not match the schema (including an invalid `action` value).
  - A **successful response with `result.isError = true`** means the tool ran but failed logically (Git conflict, failed sync, invalid query dimension, unsafe project name, failed compact/render); read `result.content[0].text` for the reason and retry after fixing it. `-32603` is reserved for genuine internal server faults.
- **Tool execution failed but the server is still alive**: this is expected behavior; fix the arguments or the underlying git state and retry. An over-long request line is discarded and answered with `-32600` without desynchronizing the stream.
- **Store not found / events written to a strange location**: MCP runs `init` at startup (creating a store in the resolved root when it is missing).
  It is recommended to pass `--path` explicitly or set `ACDM_STORE`, to avoid depending on the client process's CWD.
- **Multiple clients in parallel**: each client starts its own `mem-mcp` process; all writes (append, compact, render, init, resolve, sync) take the store-wide `fs2` cross-process lock, so concurrent writes on one machine are serialized, and reads are lock-free (`fs::read`, no mmap).
- **Invalid project names**: `project` must be a single safe path component (no `/`, `\`, `.`/`..`, control characters; ≤200 bytes); Windows-reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, case-insensitive, including `con.txt`) and names ending in `.` are rejected as well. The name is validated **before anything is written**, so the tool returns `isError` with `Invalid project: invalid project name ...` and nothing is created.

## 9. Performance issues

- Every `mem query` **builds the in-memory index from scratch** (scanning all `events/*.jsonl`), with complexity O(number of events).
  With hundreds of thousands of events, a single query becomes noticeably slower; this is a known limitation of the current design (see [DESIGN.md](DESIGN.md)).
- `mem query --fuzzy` is a line-by-line regex scan and is slower than other queries; narrow the regex as much as possible.
- `mem render` only takes the 10 most recent events and is unaffected by the total history size.
- It is recommended to run `mem compact` periodically; `events/YYYY-MM.jsonl` from old months can be archived outside the repository
  (note: after archiving, the `mem render` timeline and latest events will not include those months, which is expected).

## 10. Data health check

Check unparseable lines in JSONL, the total number of events, and the file list:

```bash
python3 - <<'PY'
import json, pathlib
store = pathlib.Path("/path/to/store")
total = bad = 0
for path in sorted((store / "events").glob("*.jsonl")):
    for lineno, line in enumerate(path.read_bytes().split(b"\n"), 1):
        if not line.strip():
            continue
        total += 1
        try:
            json.loads(line)
        except Exception:
            bad += 1
            print(f"bad line: {path}:{lineno}: {line[:80]!r}")
print(f"events: {total}, unparseable: {bad}")
PY
```

- `unparseable > 0`: usually truncated lines produced by an interrupted write, which the read side skips; you can handle them with `mem resolve` (which preserves them verbatim)
  or by deleting the lines by hand.
- The read side's tolerance for bad lines means "can be listed by `list`" does not imply "the data is complete"; keeping a backup is recommended.
- **Projection vs. event log**: `projects/<project>/files.jsonl` is derived data. If `list -p` shows fewer or older records than the events suggest,
  run `mem compact -p <project>` to rebuild the projection from the event log; this also repairs a missing or truncated `files.jsonl` left by a crash.

## 11. Diagnostics to include before filing an issue

Please include:

```bash
mem --version
mem --path /path/to/store list
git -C /path/to/store status --short
ls -la /path/to/store /path/to/store/events
```

As well as the command that failed, the full stderr, the MCP client configuration (with sensitive paths redacted), and the `initialize` response.

## 12. Known limitations

- Writes on one machine are serialized by a single store-wide lock (append / compact / render / init / resolve / sync); there is still no cross-device lock and no multi-writer transaction, so cross-device consistency is "eventual consistency + deduplication".
- The SHA-256 in event records is supplied by the writer (the CLI computes it by streaming; MCP callers may pass an empty value).
- Non-UTF-8 paths: they can be written and read, but the `resolve` staging path is passed to Git in lossy form. (Non-UTF-8 *content* in JSONL is preserved byte-for-byte by `resolve` since 0.1.1.)
- Project names are restricted to one safe path component (no `/`, `\`, `.`/`..`, control characters; ≤200 bytes), and since 0.1.2 Windows-reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, case-insensitive, including `con.txt`), names ending in `.`, and the Windows-invalid characters `<>:"|?*` are rejected by both the CLI and MCP. **If an existing store already contains such a project**, rename `projects/<old>` and update the `project` field of its records (and the name in any event lines), or start a fresh project name, before syncing that store to Windows; otherwise the Windows checkout fails.
- `mem add` records only **regular files**: FIFOs, device nodes and directories are rejected with `Error: cannot record '<path>': not a regular file` (exit 1), and the hash is computed before the store-wide lock is taken, so a blocking/special file cannot stall other writers. A missing path is still recorded as a `Deleted` record; there is no way to point `mem add` at file contents through a FIFO — copy the data into a regular file first.
- No built-in authentication or encryption: the security of the store depends on Git remote permissions and disk permissions.
