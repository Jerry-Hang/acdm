**English** · [简体中文](zh-CN/DESIGN.md)

# ACDM Design Notes

> Audience: developers and reviewers who want to understand why ACDM is designed this way and what these choices cost.
> This document discusses the "why"; for implementation details (data structures, read/write paths, file formats), see [ARCHITECTURE.md](ARCHITECTURE.md).

## 1. Design goals and non-goals

### Goals

1. **Let multiple agents share project memory**: events recorded by agents of the same project on different machines become visible to each other after sync.
2. **Serverless**: no database to deploy, no daemon to run; one local directory + one Git remote is the entire infrastructure.
3. **Offline-first**: reads and writes keep working locally while the network is down; sync again once connectivity returns.
4. **Auditable and recoverable**: data is text, so it can be diffed and inspected by humans; conflicts have a deterministic automatic resolution strategy and never silently lose data.
5. **Low barrier to entry**: a single `mem init --remote` is enough to get started; humans use the CLI, agents use MCP.

### Non-goals

- No real-time collaboration or strongly consistent database;
- No general-purpose event store/query engine (no SQL, no persistent secondary indexes);
- No file contents stored (only path, size, SHA256);
- No multi-tenancy, authentication, or encryption service;
- No semantic retrieval/vector search (`--fuzzy` is only regex);
- No cross-device transactions or causal ordering.

## 2. Why Git + append-only JSONL

The core question: **how can agents on multiple machines share a continuously growing memory, usable today, without any operations work?**

| Option | Pros | Costs and risks | Verdict |
| --- | --- | --- | --- |
| SQLite single file | Strong queries, full transactions, simple on a single machine | Binary page-level conflicts are nearly impossible to merge automatically; cross-device use needs an extra sync/locking scheme; schema migrations need a toolchain | Conflicts are uncontrollable — rejected |
| Self-hosted service + cloud database (Postgres, etc.) | Strong consistency, good concurrency | Requires a server and network, operations work, unusable offline, introduces an account system | Violates "serverless, offline-capable" — rejected |
| Redis / KV store | Fast | In-memory, weak persistence, lacks history/version semantics | Unsuitable for long-term memory — rejected |
| CRDT / sync engine (e.g. Automerge) | Automatic merging, strong offline | High complexity, state bloat, needs a library and maintenance of merge semantics; over-engineered for "event append" | Too heavy — rejected |
| Cloud drive sync (Dropbox/Syncthing/OneDrive) | Zero development cost | Conflicts produce conflicted copies with no structured merge; not auditable, no version rollback | Conflicts are uncontrollable — rejected |
| **Git + append-only JSONL (chosen)** | Free hosting, native version history, offline-capable, auditable diffs, line-level merging, mature permissions/backup | Manual conflict resolution needed (now automated by `mem resolve`), repository grows with history, not real-time | A natural fit for the shape of an "event log" |

Another reason for choosing Git: **it is already part of the developer workflow**. Cross-device sync, authentication (SSH/HTTPS), backup, history browsing, and rollback are all already provided by Git, so ACDM does not have to reinvent them.

Reasons for choosing append-only JSONL:

| Property | Why it matters |
| --- | --- |
| Each line is an independent record | Git's conflict scope is limited to the "line" level; conflicts are usually duplicate lines or interleaved appends, and can be handled deterministically |
| Append-only, no in-place modification | No cross-device locks needed; history can be replayed; the conflict surface of `git pull --rebase` is small |
| Text is readable and diffable | You can inspect files directly when something goes wrong; compatible with old data, unknown fields preserved as-is |
| Monthly shards | Bounds single-file size and the cost of each Git diff |
| No schema migration tool needed | serde's forward compatibility (unknown fields ignored) + `resolve` preserving original lines verbatim |

Costs (accepted and documented):

- No index files → every query does a full scan (see section 8);
- Only line/record-level deduplication, no field-level semantic merge;
- Repository size grows monotonically with events;
- Depends on Git's merge behavior; users need to understand "resolve conflicts before syncing".

## 3. Consistency model

### 3.1 Single-writer lock

On one machine, every write operation — event append (including the atomic `Created`/`Modified` decision and metadata bootstrap), projection rebuild (`compact`), README rendering, `init`, conflict resolution, `sync`, and the overwrite writes of `meta.json` — takes a **store-wide cross-process advisory lock** (`fs2`).

The lock target is the store's own `.mem` marker, opened **read-only** and flocked: taking the lock writes nothing, so the synchronized Git repository never gets dirtied by lock bookkeeping, and the store's file permissions naturally scope who can contend. Only a marker-less store (the very first `init`) falls back to a lock file in `$XDG_RUNTIME_DIR` (or the system temp directory), named from a hash of the user and the canonical store path and created owner-only (`0600`) — so multiple users on a shared machine neither collide nor get `EACCES` on each other's files. One lock held across a whole multi-file operation removes the interleaving windows that per-file locks cannot cover: an append cannot slip into the middle of a projection rebuild, a rebase, or a conflict resolution.

The lock only works on the local machine: it is not a distributed lock; cross-device writes converge through conflict resolution after Git sync. Reads take no lock at all.

### 3.2 Weak consistency across devices

- Each device has its own local store copy, **strongly isolated until sync**;
- Only after `mem sync` (commit → pull --rebase → push) succeeds do the devices reach agreement;
- At any moment two devices may see different histories; this is deliberately accepted weak consistency.
- Sync fails loudly instead of faking success: a non-conflict pull failure aborts the sync (no push over an inconsistent state), and a conflict ends the CLI command with a non-zero exit code (MCP: an `isError` result) that tells the caller to run conflict resolution. A machine without an upstream simply skips the pull and tries to push.

### 3.3 Scope of last-write-wins

"Later write overwrites" is used in exactly two places, both limited to **the same deduplication key**:

1. During conflict resolution, multiple lines with the same `event_id` / `file_id` keep only the last one (see section 6);
2. In `get_latest_files` and `compact`, only the last file record for the same `file_name` is kept.

It is not a general CRDT: events with different `event_id`s never overwrite each other; records of the same file in different events keep their history, and only the "latest-state view" takes the last one.

### 3.4 Clocks and ordering

- The event timestamp `ts` is generated by each device's local clock (`Utc::now()`), uniformly UTC and fixed-width nanoseconds;
- Ordering uses parse-aware comparison: parse into time and compare, falling back to string comparison when parsing fails; for equal timestamps, `event_id` acts as the tie-breaker;
- **Clock skew is not corrected**: if one device's clock is fast/slow, event order reflects that clock rather than the real causal order;
- Therefore ACDM promises "approximately ordered by recorded time", not a causally consistent total order. For the "project memory" use case, this approximation is acceptable.

Two orderings coexist on purpose (0.1.2 documented this trade-off explicitly):

- **Incremental projection writes** (`append_event` → `files.jsonl`) and **`resolve`** follow the **physical append order**. That is the causal order of what each device actually recorded, and it is what `get_latest_files` / `compact` use for last-write-wins, so a later state can never be replaced by an older one.
- **`compact` replay** starts from the append-only event log and orders events by `ts` first, with `event_id` as the tie-breaker, then replays them. This makes the regenerated projection deterministic and gives the timestamp priority across months and devices.

The asymmetry is deliberate, not a bug: incremental paths preserve causality, while rebuilding a projection needs a total order, and the fixed-width UTC `ts` is the closest available approximation of "when it happened".

> Related source: [`now_rfc3339` / `parse_ts` / `ts_cmp` / `event_cmp`](../mem-core/src/util.rs)

## 4. Identity design

### 4.1 Format

```
YYYYMMDDHHMMSS_<nanos:09>_<pid%100000:05>_<counter%10000:04>
```

| Design point | Choice | Rationale |
| --- | --- | --- |
| Time base | UTC | IDs written by devices in different time zones sort consistently; local time would sort the same instant differently on different devices |
| Time precision | Nanoseconds | Same-second bursts are distinguishable; the probability of two devices hitting the same nanosecond is also extremely low |
| Fixed width | 14 / 9 / 5 / 4 | Lexicographic order = chronological order (under the same prefix), convenient for sorting, comparing, and human reading |
| pid | `process::id() % 100_000` | Distinguishes different processes on the same machine |
| Counter | Per-process counter seeded from the process start time + pid, `% 10_000` | Deduplicates same-nanosecond bursts within one process without starting every process at zero |

Choosing a "sortable composite ID" instead of UUID v4: UUIDs are random, unreadable, and unsortable; ACDM needs "time-ordered + printable + no extra dependencies". The cost is dependence on the device clock and pid semantics (see 4.3). Starting the counter from a time+pid-derived random offset (0.1.1) keeps the format while making it much less likely that two container processes with the same pid mint the same ID in the same nanosecond.

### 4.2 Flaws of the old format (why it had to change)

The old implementation was "second-level timestamp + pid + counter (max 100)":

```
YYYYMMDDHHMMSS_<pid(5)>_<counter%100>
```

When two devices generate IDs within the same second, as long as the pid is the same (very common: process numbers start at 1 and are heavily reused in containers/scripts) and the counter is the same, they can produce **exactly identical IDs**. Events with the same ID are treated as "the same record" and deduplicated during conflict resolution → **later writes overwrite earlier ones, silently losing data**.

The new format distinguishes with three layers — "nanoseconds + pid + counter": for two devices to collide, they would have to share the same nanosecond, the same pid, and the same counter at the same time, which is practically impossible. Fixed width and UTC are preserved to maintain sortability.

### 4.3 Boundaries and costs

- **Clock rollback** (NTP correction, manual time change) may make a new event's `ts` earlier than an old one; ordering still works by parsed time, but the intuition that "new events always come last" is broken;
- The time embedded in the ID and the event `ts` are two independent clock reads and may differ by hundreds of nanoseconds — both are real times, and ordering is based on `ts`;
- When the pid exceeds 5 digits it is taken modulo (`% 100_000`), and the counter is also taken modulo beyond 4 digits (`% 10_000`); this only affects extreme same-nanosecond, same-process bursts, and does not affect the cross-device uniqueness goal.

## 5. File identity design

### 5.1 Why it must be a relative path

The same code has different absolute paths on different devices (usernames, mount points, `/home/a` vs `/Users/b`). If `file_name` recorded absolute paths:

- They would never match across devices;
- `query -n` / MCP `by=filename` could not find files recorded by other devices;
- The history of "the same file" would be split into multiple unrelated identities.

Therefore `file_name` must be a path **relative to a stable anchor**, uniformly separated by `/`.

### 5.2 Why the priority is store → git → CWD → absolute

| Priority | Anchor | Rationale |
| --- | --- | --- |
| 1 | store root | The anchor syncs together with the store, so all devices see the same relative structure; the most stable option |
| 2 | nearest git repository root | Root cause: the store is usually separate from the project repository. Files live in the project workspace, and anchoring to the repository root yields a cross-device consistent relative path |
| 3 | CWD | Fallback: the file is neither in the store nor in a git repository, but relative to the current directory it is at least more likely to be consistent than an absolute path |
| 4 | absolute path | Last resort. The CLI prints a warning: such records cannot be matched on other devices |

The implementation does not require the file to exist: it canonicalizes the longest on-disk-existing prefix (following symbolic links) and appends the missing tail lexically. Both the store root and the file side are canonicalized the same way, so a store or file reached through a symlink (`/tmp` on some systems, macOS `/var` → `/private/var`, a symlinked project directory) yields the same relative identity as its real path — an absolute-path fallback caused by mismatched symlink forms was itself a cross-device identity bug fixed in 0.1.1. Since 0.1.4 a final symbolic link **to a file** is treated differently: only its parent directory is canonicalized and the link's own name is kept, because resolving it would leave the work tree (e.g. `repo/src/config.json -> ../dotfiles/config.json`) and `strip_prefix` against the repository root would fail, degrading a tracked file identity to an absolute path. Symlinks to directories (including a symlinked store root or parent directory) still use the full canonicalize behaviour. A **dangling** file symlink is a present identity under Git's link model rather than a deletion: the recorded content is the link target path as a slash-normalised string, with `size_bytes` = its byte length and `hash` = its SHA-256 (matching the blob Git would store); a live symlink is still followed and the target content is hashed.

### 5.3 Assumptions and boundaries

- **Assumption**: project repositories/stores on each device keep the same relative directory layout (Git guarantees this by nature);
- **Boundary 1**: a file that is moved or renamed → its identity changes, and the history splits into two `file_name`s; this is deliberately accepted (no filesystem inode is usable across devices);
- **Boundary 2**: on case-insensitive filesystems (macOS/Windows), `Src/A.rs` and `src/a.rs` may be the same file but are recorded as two identities;
- **Boundary 3**: non-UTF-8 paths go through `to_string_lossy` and may be lossy;
- **Boundary 4**: absolute-path fallback records do not participate in cross-device matching (see the migration notes in [MIGRATION.md](MIGRATION.md)).

> Related source: [`normalize_file_name`](../mem-core/src/util.rs)

## 6. Conflict resolution semantics

The order of design principles for `mem resolve` is: **preserve > deduplicate > sort**. Any behavior that "loses data for the sake of tidiness" is explicitly rejected.

### 6.1 Why preservation comes first

JSONL may contain:

- truncated trailing lines (the writing process crashed);
- new fields/new schemas written by future versions;
- miscellaneous non-record lines;
- content introduced by manual edits.

These lines cannot be fully understood by the current version, but they may be **the only copy**. Therefore:

| Case | Handling | Rationale |
| --- | --- | --- |
| Cannot be parsed as the current record type | Preserved verbatim (including whitespace), appended to the end of the file | Better to leave one unintelligible line in the file than to delete user data |
| Invalid UTF-8 bytes | Preserved byte-for-byte | "Never silently drop data" has to hold for arbitrary bytes, not only for valid text |
| Unknown fields | Preserved as-is (re-emit the original line text rather than re-serialize it) | Forward compatibility: fields written by a new version are not lost after an old version merges them |
| Unparseable lines with identical content | Collapsed into one | Avoids duplicate growth of identical truncated lines from both sides; lines with different content are not collapsed |
| Multiple lines with the same `event_id` / `file_id` | Later write wins (keep the last one) | Events/records are atomic; the same ID means the same record, so take the later side |
| Conflict marker lines | Discarded | They are Git metadata, not data |

Marker recognition is deliberately narrow: only the exact shapes Git writes are removed (`=======` as the whole line; `<<<<<<<` / `|||||||` / `>>>>>>>` followed by end-of-line or a space). A truncated JSON line such as `======= broken line` is *data* and must be preserved; matching a broad prefix would silently delete it.

The boundary between "preserve" and "later write wins" is clear: **different IDs → preserve all; same ID → keep only one**.

### 6.2 Why unparseable lines go at the end of the file

Inserting them back into their original positions could break the continuity of the "record section" and make subsequent reads harder to interpret. The cost of appending them to the end is that their position is unrelated to the real chronological order — but since they cannot be parsed, order does not matter; parseable records still keep the correct ordering.

### 6.3 Why ordering is normalized

After conflict merging, line order may come from interleaved appends on both sides. To make the result deterministic and repeatable (same input → same output), the resolver:

- `Event` / `Generic`: sorts by parsed time + key;
- `FileRecord`: **keeps the original causal order**, retaining only the last occurrence of each `file_id` at its original position;
- `Unknown`: not sorted (only the original relative order is kept).

Re-sorting `FileRecord`s by `file_id` (the pre-0.1.1 behavior) scrambled history: because `get_latest_files` / `compact` apply last-write-wins in file order, a lexicographically larger but older record could be picked as the "latest" state. Keeping causal order is what makes the projection semantics sound.

Normal syncs produce few conflicts anyway; normalized ordering for events makes "the same input always yield the same output", which simplifies review and testing. The same asymmetry appears in `compact`: incremental projection writes and `resolve` keep physical append order, while the rebuild replays events ordered by `ts` then `event_id` (see §3.4). That is intentional — causality for incremental writes, a deterministic total order for reconstruction.

### 6.4 Deciding whether to continue the rebase

The old implementation detected "is a rebase in progress" by matching the English error text of `git rebase --continue`, which turned a normal path into a hard failure under non-English locales. Now it resolves Git's own state paths with `git rev-parse --git-path` and checks whether `rebase-merge` / `rebase-apply` exist — a language-independent source of truth. Using `--git-path` (instead of assuming `<root>/.git` is a directory) is also what makes **linked worktrees and submodules** work: in those layouts `.git` is a file and the rebase state lives in a per-worktree git directory. A failed `git add` is a hard error, because a warning would only hide the real reason why the following `rebase --continue` can never succeed.

A rebase that replays several conflicting commits needs one resolve → `git add` → `git rebase --continue` round per commit. `mem resolve` loops those rounds itself (bounded at 100), using Git's unmerged-file list to decide whether it can still make progress. One special case matters: after de-duplication a replayed commit can become an **empty patch** — every record it added already exists upstream — and Git refuses to commit it ("No changes"). The resolver detects this (rebase still active, index clean — `git diff --cached --quiet`, nothing unresolved) **and only when the current round actually resolved JSONL conflicts** (`resolved_this_round > 0`), then advances with `git rebase --skip`; stopping there would leave the user stuck on a commit that should simply be dropped. The guard matters: a rebase paused for an unrelated reason (`--exec` failure, `edit`, a failed commit signature, ...) also has a clean index and no unmerged files, but skipping it would silently discard the user's commit, so that case returns Git's original error instead. It stops with a precise error only when a **non-JSONL** file is conflicted, because such a file genuinely needs a human.

### 6.5 Costs

- Not a field-level merge: when the same event differs on both sides, the whole "theirs" entry wins and the "ours" content is discarded (only one is kept);
- Output order may differ from the original append order (except for `FileRecord`s, whose causal order is preserved);
- Unparseable lines are concentrated at the end, which human reviewers need to know;
- Unparseable content can be preserved but not interpreted — resolution cannot merge it, only keep it;
- Resolving conflicts requires an explicit `mem resolve`; it is not fully automatic (to avoid silently changing data while unattended).

## 7. Security and privacy boundaries

### 7.1 What is actually stored in the store

| Data | Stored? | Notes |
| --- | --- | --- |
| File path (normalized) | Yes | May expose project structure, module layout, and naming habits |
| File size, SHA256 | Yes | Does not directly expose contents, but can be used to confirm whether "a known file" is in the project |
| Change summary `summary` | Yes | Written by a human or agent; may contain code details, intent, or even sensitive information that slipped in |
| Device name, agent name, project name | Yes | The device hostname may expose the machine name |
| File contents | **No** | Only metadata, no blobs |
| Credentials/keys | Should not appear | But ACDM does not actively scan for or block them |

### 7.2 Precautions before syncing publicly

- Before syncing to a public repository, review the contents of `events/` and `projects/`: paths and summaries may reveal private project structure;
- `mem sync` runs `git add -A` against the store root, **so any file in the store directory will be committed**. Make sure the store directory holds only ACDM data; do not put project source code, `.env`, keys, etc. under the store root;
- Project names supplied by an agent are validated as one safe path component (no separators, no `.`/`..`, no control characters, ≤200 bytes), and — since 0.1.2 — Windows-reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, case-insensitive, including `con.txt`), names ending in `.`, and the Windows-invalid characters `<>:"|?*` are rejected everywhere. This is a security boundary, not cosmetics: without the component rule a `project` argument like `../x` would write `meta.json` / `README.md` outside `projects/` (fixed in 0.1.1); without the Windows rules, a store created on Unix could contain a project directory that Windows cannot check out;
- Git history is immutable: even if a record is later deleted/overwritten, it may remain in historical commits. If you want to publish, prefer creating a fresh clean store and recording again rather than cleaning up the old repository;
- SHA256 does not "leak contents", but it can be used to fingerprint known content; for strongly confidential scenarios, treat even hashes and sizes as sensitive information;
- ACDM itself has no authentication/encryption: `mem-mcp` runs as the local user over stdio; the store is stored in plaintext on disk; transport security depends on the Git remote protocol (SSH/HTTPS).

### 7.3 Recommendations

- Use a **private** remote by default;
- Publish only a sanitized store, or publish only the code while keeping the store in a private repository;
- Avoid writing keys, tokens, internal addresses, or customer information into summaries;
- When stronger protection is needed, layer encryption on top of Git (e.g. git-crypt); ACDM does not prevent this.

## 8. Trade-offs and known limitations

| Limitation | Current state | Impact | Mitigation/future |
| --- | --- | --- | --- |
| Index rebuilt O(N) every time | Every `query` / `mem_query` scans all events | Queries become slower as the event count grows | Incremental/persistent index in the future |
| No fsync on append | `append_event` only `flush`es (the projection rebuild does `sync_all` its temp file) | Power loss may lose the last append | Accepted; Git commits provide another layer of durability |
| Single-writer scope | A store-wide advisory lock serializes writers on **one machine**; no cross-device lock | Concurrent writes to the same store from multiple devices must converge via sync | Convention: "write locally, sync periodically" |
| No multi-writer transactions | `events/` and `files.jsonl` are two separate appends | A crash may leave the projection briefly stale | `events/` is the source of truth; `mem compact` rebuilds the projection (self-healing) |
| `compact` and concurrent appends | Holds the store-wide write lock for read → replay → rename; replay orders events by `ts` then `event_id` | Concurrent appends on the same machine wait instead of being lost; a projection rebuilt after a crash may reorder same-file records by timestamp | Cross-device concurrency still needs sync; incremental append/resolve keep causal order |
| Large files | `mem add` streams SHA-256 in 64 KiB chunks; the read path still loads a whole shard with `fs::read` | Write memory is bounded; query memory peaks around the largest shard | Month sharding bounds shard size; MCP callers supply size/hash themselves |
| Non-UTF-8 content | `resolve` is byte-level and preserves invalid UTF-8 and CRLF verbatim | Such lines are kept but cannot be interpreted as records | Keep the store UTF-8 when possible; non-UTF-8 data is no longer a failure |
| Non-UTF-8 paths | Paths go through `to_string_lossy` | Identity may be lossy | Avoid non-UTF-8 file names in the store |
| No content storage | Only metadata is stored | File contents cannot be replayed from the store | File contents remain managed by Git/the project repository |
| Events are append-only, never deleted | No archival mechanism | The Git repository grows over time | Archival/snapshots in the future |
| Time order ≈ real order | Depends on device clocks | Clock skew is reflected in ordering | NTP assumption; `event_id` tie-break |
| Regex search | `--fuzzy` is Rust regex with a 10 MiB compiled-program cap | Not tokenized/semantic search; a bad or oversized pattern reports an error | Extend as needed |
| No `git pull --autostash` | Sync commits first and holds the store write lock, so the tree is clean before the pull | A dirty tree is never silently stashed | This is deliberate: a stash-pop conflict in the middle of a rebase is very hard to recover from |
| Project names are restricted | One safe path component (no `/`, `\`, `.`/`..`, control characters; ≤200 bytes); Windows device names and trailing `.` rejected | A project cannot be named like a path, and a Unix-created store always checks out on Windows | Security/cross-platform boundary; rename legacy projects before syncing to Windows |
| No authentication/encryption | Plaintext store + stdio MCP | The security boundary lies in the OS and Git layers | Private remote + external schemes such as git-crypt |
| Symlinks are canonicalized only for existing paths; final file symlinks keep their own name | Existing prefixes are resolved, missing tails are lexical; a final symlink **to a file** keeps its name so in-repo links pointing outside stay repo-relative (0.1.4), while symlinks to directories are fully resolved. A dangling file symlink is recorded with Git link semantics (target path bytes + SHA-256, 0.1.5) instead of `Deleted` | A symlink created later can change the identity of a not-yet-existing path; renaming a file symlink changes its identity | Create/record files before linking them; avoid reorganizing store paths with symlinks |

## 9. Future work

Candidate directions ordered by "benefit / cost":

1. **Incremental index**: persist `EventPointer`, with per-shard `(size, mtime)` caching; reduce queries from O(everything) to O(delta).
2. **Event archival/snapshots**: compress old months of events into project-level snapshots to bound the Git repository size.
3. **Finer merge semantics**: currently the whole record with the same ID is last-write-wins; field-level merging can be introduced when clearly needed.
4. **Content addressing (optional)**: store blobs by SHA256, supporting "replaying file contents from memory"; the boundary with the "no content storage" default must be drawn clearly.
5. **More MCP tools**: cross-project search, batch appends, time-range filtering for `mem_query`, diff views.
6. **Conflict preflight**: `mem sync --dry-run` / `mem resolve --check`, to surface the conflict scope before an actual rebase.
7. **Encryption**: built-in store-level encryption or a standardized git-crypt usage documented in the docs.
8. **Cross-platform validation**: path case sensitivity and non-UTF-8 behavior on Windows/macOS need CI coverage.
9. **Import/export**: batch-import events from other memory formats (Markdown/notes).

> None of these directions changes the current core constraints: Git remains the only transport layer; the local store remains the append-only JSONL source of truth.

## 10. Further reading

- [ARCHITECTURE.md](ARCHITECTURE.md) — components, read/write paths, storage format, concurrency and sync details;
- [CLI.md](CLI.md) — `mem` command reference;
- [MCP.md](MCP.md) — MCP tools and client configuration;
- [MIGRATION.md](MIGRATION.md) — migrating data from the old format;
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — troubleshooting common issues;
- [../CHANGELOG.md](../CHANGELOG.md) — version change history.
