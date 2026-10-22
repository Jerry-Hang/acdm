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

## 3. Timeline order looks wrong

- ACDM orders time by **parse-then-compare**, not string comparison, so a mix of both old formats is still ordered correctly.
- If `ts` cannot be parsed at all: old versions wrote such events into `events/default.jsonl`. That file is not a valid monthly shard,
  so "latest events" queries ignore it; the `mem render` timeline also only includes events from monthly shards.
- Hand-editing JSONL and corrupting `ts` also causes the entry to be skipped by the read side (reads tolerate bad lines).
- The order of events within the same second is determined by the event timestamp (nanoseconds); if timestamps are equal, they are ordered by `event_id`.

## 4. `mem sync` fails

`mem sync` performs, in order: automatic commit of a dirty working tree (commit message `Auto-sync from <device> at <timestamp>`) →
`git pull --rebase --autostash` → `git push`. Any step that fails ends with a non-zero exit code.

| Symptom | Cause | Handling |
| --- | --- | --- |
| `git pull ... failed: ... no tracking information` / `no upstream` | The local branch has no upstream configured | `git -C <store> push -u origin <branch>` (push manually once first) |
| `git push ... failed: ... No configured push destination` | No remote configured | `git -C <store> remote add origin <url>`, or re-initialize with `mem init --remote <url>` |
| `rejected` / `non-fast-forward` | The remote has new commits and the local rebase did not succeed | Run `git -C <store> pull --rebase --autostash` first, and handle conflicts per §5 |
| `Permission denied (publickey)` / `Authentication failed` | Git credential problem | Fix the SSH key or HTTPS credentials; ACDM does not manage credentials |
| Conflicts reported | JSONL merge conflict | See §5 |

Note: if the store is not a Git repository (only `mem init` was used without a remote), `sync` fails at the push stage;
in that case switch to local-only usage, or create the repository/remote.

## 5. Handling conflicts

When syncing across devices it is very common for two devices to append to the same `events/YYYY-MM.jsonl` at the same time, and Git merges it into
conflict markers. Handling:

```bash
mem --path /path/to/store sync     # output reports conflicts
mem --path /path/to/store resolve  # automatically resolve JSONL conflicts
mem --path /path/to/store sync     # sync again
```

`mem resolve` behavior (see [ARCHITECTURE.md](ARCHITECTURE.md#7-synchronization-and-conflict-resolution) for details):

- Discards `<<<<<<<` / `=======` / `>>>>>>>` / `|||||||` marker lines;
- Records with the same `event_id` / `file_id` are **last-write-wins** (when ambiguous, the later version in the file is kept);
- Parseable records are sorted by time and written back; **unparseable lines are preserved verbatim** (placed at the end of the file);
- After writing back it runs `git add`; only when a rebase state actually exists (`.git/rebase-merge` / `rebase-apply`) does it run
  `git rebase --continue`, independent of the system language.

Troubleshooting:

- A non-zero `Lines preserved` in the output means there are unparseable lines (usually truncated lines produced by an interrupted write). This is
  **intentional data protection**, not an error. After checking the content, delete them with an editor or leave them as they are.
- `git add ... failed: warning`: the file may be ignored by `.gitignore` or have a special path.
  Check with `git -C <store> check-ignore -v events/2026-10.jsonl`, and use `git add -f` if necessary.
- Still conflicting after resolve: check `git -C <store> status`; confirm the conflicting file is inside the repository, has a `.jsonl` suffix,
  and that a rebase/merge is currently in progress. Remaining conflicts usually mean there is a **non-JSONL file conflict**,
  most commonly `projects/<project>/meta.json`: when two devices write metadata for the same project for the first time at the same time,
  both create `meta.json`; JSONL can merge automatically, but `meta.json` cannot.
  `mem resolve` resolves JSONL first, then fails at `git rebase --continue` and prints an **actionable error**
  listing the paths that are still unresolved. Recovery:

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

The semantics of `mem compact -p <project>` are "keep only the latest record for each `file_name`", so a reduced record count is
expected behavior (the output reports before / after / pruned). It keeps `Deleted` records. Run it in a clean working tree,
and `sync` once afterwards.

## 7. Project README.md is overwritten

`projects/<name>/README.md` is a file **generated** by `mem render` and is rewritten on every render. Do not edit it by hand;
if you need extra notes, write them in a different file, or change the rendering logic.

## 8. MCP client issues

- **The server does not respond**: make sure `command` in the client configuration points to a real `mem-mcp` (an absolute path is recommended),
  and `args` looks like `["--path", "/path/to/store"]`. MCP uses stdio + **line-delimited** JSON-RPC 2.0;
  no `Content-Length` header is needed.
- **Exits immediately on startup**: unknown startup arguments print usage and `exit 2` (to prevent a typo from silently using the wrong store);
  make sure you only pass `--path`.
- **Tool call returns an error**: look at `error.code`:
  - `-32700` the request is not valid JSON;
  - `-32601` unknown method or unknown tool;
  - `-32602` `tools/call` parameters do not match the schema;
  - `-32603` tool execution failed (see the specific reason in `error.message`).
- **Tool execution failed but the server is still alive**: this is expected behavior; fix the parameters and retry.
- **Store not found / events written to a strange location**: MCP runs `init` at startup (creating a store in the resolved root when it is missing).
  It is recommended to pass `--path` explicitly or set `ACDM_STORE`, to avoid depending on the client process's CWD.
- **Multiple clients in parallel**: each client starts its own `mem-mcp` process; appends are protected by an `fs2` exclusive lock,
  so concurrent writes from multiple processes are safe; queries are lock-free (`mmap` reads a snapshot).

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

- No concurrent multi-writer transactions: there is only one exclusive lock, and cross-device consistency is "eventual consistency + deduplication".
- The SHA-256 in event records is supplied by the writer (the CLI computes it; MCP callers may pass an empty value).
- Non-UTF-8 paths: they can be written and read, but the `resolve` staging path is passed to Git in lossy form.
- No built-in authentication or encryption: the security of the store depends on Git remote permissions and disk permissions.
