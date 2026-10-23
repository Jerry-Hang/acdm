**English** · [简体中文](zh-CN/MIGRATION.md)

# Migration Guide

> Audience: users upgrading an existing ACDM store to 0.1.5, or developers migrating from internal prerelease snapshots.
> This document explains "which changes require action and which are already compatible on the read side", and provides a migration script that has been tested in practice.

## Data format and compatibility ground rules

- **The store directory structure is unchanged**: `events/`, `projects/`, `.mem` (new in 0.1.0), `.gitignore`.
  `.mem` is a marker file whose content looks like `{"format": 1, "created_at": "..."}`, used to discover the store from subdirectories.
- **The read side is backward compatible**: both old timestamp formats in historical data (nanoseconds + `+00:00`, seconds-level `Z`) can be parsed,
  time ordering compares by "parsed time"; unparseable lines are skipped (reads) or preserved verbatim (`mem resolve`).
- **The write side uses a unified format**: newly written timestamps are always fixed-width UTC nanoseconds `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`,
  and file identities are always normalized relative paths.
- **The event log stays append-only**: apart from `mem resolve` handling conflicts and the optional migration script below,
  normal writes never rewrite existing lines.
- **0.1.1 requires no data migration**: the on-disk format is unchanged (`events/`, `files.jsonl`, `meta.json`, `.mem` are all identical).
  0.1.1 hardens semantics (sync failures are surfaced, conflicts exit non-zero, `compact` rebuilds the projection from the event log,
  reads no longer use mmap, project names are validated). After upgrading, it is **recommended to run `mem compact -p <project>` once per project**:
  it rebuilds `files.jsonl` from the authoritative event log and heals a projection that an older crash or concurrent write left stale or truncated.

## Changes in 0.1.0 relative to internal prerelease snapshots

| Change | Impact | Action needed? |
| --- | --- | --- |
| Timestamp format unified to 30-character UTC `Z` | The read side is compatible with old formats, and ordering is correct | No |
| ID format changed to `YYYYMMDDHHMMSS_nanos9_pid5_counter4` (UTC) | Old and new IDs can coexist; ordering is by `ts`, not by ID | Only if downstream scripts parse the ID structure |
| `file_name` normalized to relative paths | Absolute paths in old data are not rewritten automatically | Optional, see "Migration B" below |
| `mem resolve` output gains `Lines preserved` | Adapt if a script parses that command's stdout | Depends |
| MCP `mem_add_event.files[].action` made optional (defaults to `Modified`) | Old clients that pass action still work | No |
| MCP `mem_resolve` response text gains the preserved line count | Adapt if you match the old text in full | Depends |
| Removed `MemStore::scan_files` / `MemStore::read_latest_events` / `nodes/` | Code calling these APIs will fail to compile | Library users need to change, see below |
| Reads tolerate corrupted lines | Stores that previously failed outright now skip bad lines and keep reading | Recommended to run a health check once |

0.1.1 changes (semantics only; no data-format change):

| Change | Impact | Action needed? |
| --- | --- | --- |
| `mem compact` rebuilds `files.jsonl` from the event log and holds the store write lock | Stale/torn projections are healed; concurrent appends are not lost | Recommended: run `mem compact -p <project>` once after upgrading |
| Store-wide cross-process write lock; reads use plain `fs::read` instead of mmap | Local writers serialize; no more `SIGBUS` risk under concurrent truncation | No |
| `sync` commits first, uses `git pull --rebase` without `--autostash`, skips the pull without an upstream, aborts on non-conflict pull failures, and **exits non-zero on conflicts** | Scripts that assumed a conflicted `sync` exits 0 must check the exit code now | Update automation |
| MCP: `jsonrpc` validated (`-32600`), tool arguments validated (`-32602`), business errors returned as `result.isError = true`, 16 MiB request-line cap, `action` enum enforced | Clients/monitoring that match on `-32603` or on old text must be updated | Depends |
| Project names validated as a single safe path component | Names containing `/`, `\`, `.`/`..`, control characters, or >200 bytes are rejected | Rename such projects (and their `projects/<name>/` directories) before upgrading |
| `resolve` is byte-level, matches only exact Git markers, advances a multi-commit rebase in one call, and keeps `FileRecord` causal order | Non-UTF-8 content and marker-prefixed data lines are preserved; fewer manual steps | No |

## Upgrading from 0.1.1 to 0.1.2

Nothing to migrate: **0.1.2 changes no data format** (the same `events/`, `files.jsonl`, `meta.json`, `.mem` files are read and written), so an existing store works as-is and every fix takes effect automatically after replacing the binaries.

What you get for free after upgrading:

- **Consistent, complete locking**: `mem render` and `MemStore::init` now also take the store-wide write lock, so rendering can no longer dirty the work tree in the middle of a `sync` rebase; meta.json bootstrap and the `Created`/`Modified` decision are atomic with the append (including metadata self-heal after a corrupt `meta.json`).
- **Conflict resolution keeps going**: a replayed commit that becomes an empty patch is dropped with `git rebase --skip` instead of aborting on "No changes"; `git worktree` / submodule checkouts (`.git` is a file) are supported; a failed `git add` is a hard error.
- **MCP correctness**: a `notifications/*` method sent *with* an `id` is answered with `-32601` instead of leaving the client waiting; unsafe project names are rejected before anything is written; sync's default commit message is the same as the CLI's.

Action items:

1. Optional: run `mem compact -p <project>` once per project to rebuild the `files.jsonl` projection from the authoritative event log (also heals a stale or truncated projection).
2. **Only if you sync to Windows**: rename any project whose name is a Windows-reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, case-insensitive, including `con.txt`) or ends with `.` — 0.1.2 now rejects such names, and Windows cannot check them out. Rename the directory under `projects/` and update the `project` field of that project's records before syncing to Windows.
3. No other action is needed; the 0.1.1 recommendations (running `compact` once) remain valid but are optional.

## Upgrading from 0.1.2 to 0.1.3

Nothing to migrate: **0.1.3 changes no data format** (the same `events/`, `files.jsonl`, `meta.json`, `.mem` files are read and written), so an existing store works as-is and every fix takes effect automatically after replacing the binaries.

What you get for free after upgrading:

- **Hardened `mem add` file handling**: `record_file_event` opens every path exactly once (Unix `O_NONBLOCK`, so a FIFO cannot block the command) and uses that single handle for the regular-file check and the streaming SHA-256 hash — no TOCTOU gap between the object checked and the object hashed. A file that disappears while being opened/hashed is recorded as `Deleted` instead of failing the command, and duplicate paths in one batch are collapsed to a single record.
- **Atomic `init` marker**: `.mem` is written last via `.mem.tmp` + `fs::rename`, so any process that observes `.mem` observes a fully initialized store; the bootstrap fallback lock file and any leftover `.mem.tmp` are cleaned up.
- **More robust `resolve`**: the empty-patch `git rebase --skip` decision now looks only at **tracked** changes (`--untracked-files=no`), so leftover scratch files in the store no longer stop the recovery; the unresolved-file list uses `git diff --name-only --diff-filter=U -z`, so paths with spaces or special characters are reported verbatim.
- **MCP `file_name` validation**: `mem_add_event` rejects empty/whitespace-only file names and paths that normalize to the store root (`isError`, `Invalid file_name: ...`) instead of writing a bogus identity into the projection.
- **Cross-platform warning coverage**: the CLI's absolute-path warning also covers Windows drive paths (`C:/...`).

Action items:

1. Optional: run `mem compact -p <project>` once per project to rebuild the `files.jsonl` projection from the authoritative event log (also heals a stale or truncated projection).
2. No other action is needed; the 0.1.2 recommendations remain valid but are optional.

## Upgrading from 0.1.3 to 0.1.4

Nothing to migrate: **0.1.4 changes no data format** (the same `events/`, `files.jsonl`, `meta.json`, `.mem` files are read and written), so an existing store works as-is and every fix takes effect automatically after replacing the binaries.

What you get for free after upgrading:

- **Directory anchors are rejected by MCP**: `mem_add_event` now refuses a `files[].file_name` that normalizes to a directory root — the store root, the process working directory, or the enclosing Git work tree (`Invalid file_name: must not be a directory root (...)`), and also refuses a path that is itself an existing directory (`Invalid file_name: '<path>' is a directory, not a file`). A whole directory can no longer be recorded as a tracked file.
- **MCP batch de-duplication**: duplicate normalized file names within one event (`b.rs` + `./b.rs`) are collapsed to the first occurrence, matching `mem add`.
- **Repository-relative file symlinks**: `normalize_file_name` canonicalizes only the **parent** of a final symbolic link to a file and keeps the link's own name, so a tracked in-repo symlink pointing outside the work tree (e.g. `repo/src/config.json -> ../dotfiles/config.json`) keeps a repository-relative identity instead of an absolute path that no other device can match; symlinks to directories (a symlinked store root or parent) behave as before.
- **More reliable `resolve` recovery**: the empty-patch test for `git rebase --skip` is now **index-clean** (`git diff --cached --quiet`) instead of requiring a clean working tree, so untracked scratch files and unstaged edits to unrelated files no longer stop the recovery. Note that `git rebase --skip` may reset unstaged changes to unrelated files in the working tree — that is Git's own behaviour; commit or stash unrelated work before running `mem resolve` if you need to keep it.
- **Test suite**: 105 → 109 tests (65 core + 16 CLI integration + 6 MCP unit + 22 MCP integration).

Action items:

1. Optional: run `mem compact -p <project>` once per project to rebuild the `files.jsonl` projection from the authoritative event log (also heals a stale or truncated projection).
2. No other action is needed; the 0.1.3 recommendations remain valid but are optional.

## Upgrading from 0.1.4 to 0.1.5

Nothing to migrate: **0.1.5 changes no data format** (the same `events/`, `files.jsonl`, `meta.json`, `.mem` files are read and written), so an existing store works as-is and every fix takes effect automatically after replacing the binaries.

What you get for free after upgrading:

- **Dangling symlinks are recorded instead of reported as deletions**: when `mem add` opens a path whose target is missing but the path itself is a symlink, the link is recorded with Git's link semantics — content = the link target path (slashes normalised), `size_bytes` = its byte length, `hash` = its SHA-256. The first record is `Created`, a later one is `Modified`, and it only becomes `Deleted` when the link itself is removed; live symlinks are still followed and their target's content is hashed.
- **MCP `file_name` validation is stricter**: `mem_add_event` now rejects values ending in a path separator or trailing `.`/`..` (`Invalid file_name: must not end with a path separator`), and also rejects paths that are directories or store subdirectories (`events`, `projects/<p>`, ...) even when the MCP process CWD is outside the store (`Invalid file_name: '<path>' is a directory, not a file`).
- **Safer `resolve` recovery**: the automatic empty-patch `git rebase --skip` is only taken when the current round actually resolved JSONL conflicts (`resolved_this_round > 0`) *and* the index is clean with nothing unresolved. A rebase paused for an unrelated reason (`git rebase --exec` failure, `edit`, a failed commit signature, ...) therefore returns Git's original error instead of silently discarding the user's commit.
- **Test suite**: 109 → 112 tests (67 core + 16 CLI integration + 6 MCP unit + 23 MCP integration).

Action items:

1. Optional: run `mem compact -p <project>` once per project to rebuild the `files.jsonl` projection from the authoritative event log (also heals a stale or truncated projection). If your store contains dangling symlinks recorded by older versions as `Deleted`, re-running `mem add` on those paths records them with the new link identity.
2. No other action is needed; the 0.1.4 recommendations remain valid but are optional.

## Upgrade steps (single machine)

```bash
# 1) Back up: the entire store directory (including .git)
cp -a /path/to/store /path/to/store.bak-$(date +%Y%m%d)

# 2) Confirm there is no rebase in progress / no unresolved conflicts
git -C /path/to/store status --short
ls /path/to/store/.git/rebase-merge /path/to/store/.git/rebase-apply 2>/dev/null

# 3) Upgrade the binaries (after cargo build --release, replace mem / mem-mcp in PATH)
cargo build --release

# 4) Smoke check
/path/to/mem --path /path/to/store list
/path/to/mem --path /path/to/store resolve     # clean up leftover conflict markers (if any)

# 5) Recommended: rebuild each project's files.jsonl projection once (also heals stale projections)
/path/to/mem --path /path/to/store compact -p <project>

# 6) Sync once to push the local changes made during migration
/path/to/mem --path /path/to/store sync
```

Multi-device upgrade order: **upgrade one device first → complete one `sync` → then upgrade the other devices**. Old versions can still read the new format
(they store the timestamp as a string), but old versions will keep writing the old timestamp format and non-normalized paths, so keep the mixed-version window as short as possible.

## Migration A: timestamp format (usually unnecessary)

Nothing needs to be done. The read side already orders by parsed time, and the old format does not cause ordering errors.

Only consider rewriting in the special scenario where "the string order of the whole JSONL must equal the time order" (for example, external tools compare
`ts` as strings directly); rewriting breaks append-only semantics and hash-based auditing, and is **not recommended**.

## Migration B: rewrite absolute-path `file_name` to relative paths (optional)

**When it is needed**: old versions recorded `file_name` "exactly as the user passed the path". If you have ever passed absolute paths
(for example `/home/alice/repo/src/main.rs`), then:

- `mem query -n src/main.rs` cannot find these old records;
- The same file is treated as different files on different devices; `mem list -p` shows extra entries, and the file table produced by `mem render` contains duplicates.

**Back up first, then dry-run, then execute**. The script only changes the `file_name` field, preserves unknown fields and unparseable lines,
keeps unchanged lines byte-for-byte intact, and writes back using a "temporary file + atomic rename".

```bash
# Preview (no writes); exit code 0 = everything is migratable, 2 = some absolute paths matched no prefix
python3 migrate_absolute_paths.py --store /path/to/store \
    --strip-prefix /home/alice/repo --dry-run

# Execute
python3 migrate_absolute_paths.py --store /path/to/store \
    --strip-prefix /home/alice/repo

# Verify after migration
/path/to/mem --path /path/to/store query -n src/main.rs
/path/to/mem --path /path/to/store list -p <project>
/path/to/mem --path /path/to/store compact -p <project>   # keeps the latest record for each file_name
/path/to/mem --path /path/to/store sync
```

When multiple prefixes are needed (e.g. different project directories), repeat `--strip-prefix`:

```bash
python3 migrate_absolute_paths.py --store /path/to/store \
    --strip-prefix /home/alice/repo --strip-prefix /srv/work/repo
```

Notes:

- If a file has both "absolute-path records" and "relative-path records" in the old data, after migration they are merged into the same
  `file_name`; the history entries are still there, and `mem compact` converges to the latest one.
- The script rewrites both `event.files[].file_name` in `events/*.jsonl` and
  the records in `projects/*/files.jsonl` (queries read the index from events, the file table reads the latter — both must be changed).
- `mem resolve` deduplicates by `file_id` / `event_id`; the migration does not change these two fields, so conflict resolution is unaffected.

Full script (copy as `migrate_absolute_paths.py`):

```python
#!/usr/bin/env python3
"""Rewrite absolute ACDM file identities to relative paths (migration helper).

ACDM versions before 0.1.0 recorded ``file_name`` exactly as the user passed it,
so absolute paths ended up in the append-only event log. Cross-device matching and
``mem query -n`` expect the relative identity, so old records can be normalised.

The script rewrites both:
  * events/*.jsonl              -> event.files[].file_name
  * projects/*/files.jsonl      -> record.file_name

It never drops or reorders records, keeps unknown fields, uses a temporary file
plus atomic rename per file, and changes nothing until you omit --dry-run.

Usage:
  python3 migrate_absolute_paths.py --store /path/to/store \
      --strip-prefix /home/alice/repo --dry-run
  python3 migrate_absolute_paths.py --store /path/to/store \
      --strip-prefix /home/alice/repo

Exit codes: 0 = ok, 2 = some absolute names matched no prefix.
"""

import argparse
import json
import os
import sys
from pathlib import Path

EVENT_KEYS = ("event_id", "ts", "device", "agent", "project", "summary", "files")


def rewrite_name(name, prefixes):
    """Return (new_name, matched) for one file_name value."""
    if not isinstance(name, str) or not os.path.isabs(name):
        return name, True
    normalised = os.path.normpath(name)
    for prefix in prefixes:
        if normalised == prefix or normalised.startswith(prefix + os.sep):
            relative = os.path.relpath(normalised, prefix)
            return relative.replace(os.sep, "/"), True
    return name, False


def rewrite_line(line, prefixes):
    """Return (new_line, changed_count, unmatched_count) for one JSONL line."""
    try:
        record = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return line, 0, 0

    changed = 0
    unmatched = 0

    if isinstance(record, dict) and "event_id" in record and isinstance(record.get("files"), list):
        for entry in record["files"]:
            if isinstance(entry, dict) and "file_name" in entry:
                new_name, matched = rewrite_name(entry["file_name"], prefixes)
                if new_name != entry["file_name"]:
                    entry["file_name"] = new_name
                    changed += 1
                elif not matched:
                    unmatched += 1
    elif isinstance(record, dict) and "file_name" in record:
        new_name, matched = rewrite_name(record["file_name"], prefixes)
        if new_name != record["file_name"]:
            record["file_name"] = new_name
            changed += 1
        elif not matched:
            unmatched += 1
    else:
        return line, 0, 0

    if changed == 0:
        # Byte-identical for untouched lines: only rewritten records are
        # re-serialised (and only for the file_name field).
        return line, 0, unmatched

    return json.dumps(record, ensure_ascii=False, separators=(",", ":")).encode("utf-8"), changed, unmatched


def process_file(path, prefixes, dry_run):
    raw = path.read_bytes()
    had_trailing_newline = raw.endswith(b"\n")
    lines = raw.split(b"\n")
    if had_trailing_newline:
        lines = lines[:-1]

    changed = 0
    unmatched = 0
    output = []
    for line in lines:
        if not line:
            output.append(line)
            continue
        new_line, line_changed, line_unmatched = rewrite_line(line, prefixes)
        output.append(new_line)
        changed += line_changed
        unmatched += line_unmatched

    if changed and not dry_run:
        data = b"\n".join(output)
        if had_trailing_newline and output:
            data += b"\n"
        tmp = path.with_name(path.name + ".migrate.tmp")
        tmp.write_bytes(data)
        os.replace(tmp, path)

    return changed, unmatched


def main():
    parser = argparse.ArgumentParser(description="Rewrite absolute ACDM file identities to relative paths")
    parser.add_argument("--store", required=True, help="ACDM store root (directory containing .mem)")
    parser.add_argument(
        "--strip-prefix",
        action="append",
        required=True,
        help="absolute directory prefix to strip, e.g. /home/alice/repo (repeatable)",
    )
    parser.add_argument("--dry-run", action="store_true", help="report what would change without writing")
    args = parser.parse_args()

    store = os.path.abspath(args.store)
    if not os.path.isdir(store):
        print(f"error: store not found: {store}", file=sys.stderr)
        return 2
    prefixes = [os.path.normpath(os.path.abspath(p)) for p in args.strip_prefix]

    targets = []
    events_dir = os.path.join(store, "events")
    if os.path.isdir(events_dir):
        targets += [
            os.path.join(events_dir, name) for name in sorted(os.listdir(events_dir)) if name.endswith(".jsonl")
        ]
    projects_dir = os.path.join(store, "projects")
    if os.path.isdir(projects_dir):
        for project in sorted(os.listdir(projects_dir)):
            candidate = os.path.join(projects_dir, project, "files.jsonl")
            if os.path.isfile(candidate):
                targets.append(candidate)

    total_changed = 0
    total_unmatched = 0
    for target in targets:
        changed, unmatched = process_file(Path(target), prefixes, args.dry_run)
        if changed or unmatched:
            note = "would change" if args.dry_run else "changed"
            print(f"{target}: {note} {changed} file_name(s), {unmatched} unmatched absolute name(s)")
        total_changed += changed
        total_unmatched += unmatched

    mode = "DRY RUN, nothing written" if args.dry_run else "migrated"
    print(f"{mode}: {total_changed} file_name(s) rewritten across {len(targets)} file(s)")
    if total_unmatched:
        print(
            f"warning: {total_unmatched} absolute file_name(s) matched no --strip-prefix; "
            "add the missing prefix and re-run",
            file=sys.stderr,
        )
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

## Code migration (callers using the `mem-core` library directly)

| Old usage | New usage |
| --- | --- |
| `MemStore::read_latest_events(n)` | `MemStore::read_latest_events_for_project(project, n)` (correct across months; for globally latest events use `read_all_events` + your own filtering) |
| `MemStore::scan_files(dir)` | Removed; use `ignore::WalkBuilder` to traverse yourself |
| `MemStore::nodes_dir()` | Removed; device information is in each event's `device` field |
| `render_project_readme` requires a pre-existing `meta.json` | Missing metadata is now backfilled automatically |
| `ResolveReport { resolved_files, total_conflicts_resolved, deduplicated_items }` | The struct gains a `lines_preserved` field; add it when using `..Default::default()` or constructing field by field |
| Building `Event.ts` by hand | Use `mem_core::now_rfc3339()`; compare times with `mem_core::ts_cmp()` / `sort_events()` |
| Building device names / hashes by hand | `mem_core::default_device_name()` / `mem_core::compute_sha256()` |
| Implementing store upward search by hand | `mem_core::resolve_store_root(None)`; to write `file_name` use `mem_core::normalize_file_name(path, &store.root)` |

## Rollback

- **Code**: `git checkout <previous commit>` and rebuild.
- **Data**: prefer restoring the pre-upgrade backup (`store.bak-*`). Old versions can read the new-format data, but ordering by string comparison of timestamps
  may be inaccurate, so if you plan to keep using the old version for a long time after rollback, also restore the data backup.
- The migration script has no automatic rollback switch; a backup is mandatory before running it.

## Data format version policy

- The `format` field of `.mem` is currently `1`. If an incompatible data format change happens in the future, this value will be bumped and the
  migration steps will be documented here; the read side checks the marker first and then decides whether to upgrade the data.
