[English](../MIGRATION.md) · **简体中文**

# 迁移指南

> 读者：正在把已有 ACDM store 升级到 0.1.2 的用户，或从内部预发布快照迁移的开发者。
> 本文件说明「哪些变化需要动手、哪些读取端已经兼容」，并给出经过实测的迁移脚本。

## 数据格式与兼容性总则

- **store 目录结构未变**：`events/`、`projects/`、`.mem`（0.1.0 新增）、`.gitignore`。
  `.mem` 是标记文件，内容形如 `{"format": 1, "created_at": "..."}`，用于从子目录发现 store。
- **读端向后兼容**：历史数据里的两种旧时间戳格式（纳秒 + `+00:00`、秒级 `Z`）都能解析，
  时间排序按「解析后的时间」比较；无法解析的行会被跳过（读取）或逐字保留（`mem resolve`）。
- **写端格式统一**：新写入的时间戳一律是定宽 UTC 纳秒 `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`，
  文件标识一律是规范化相对路径。
- **事件日志保持 append-only**：除了 `mem resolve` 处理冲突、以及下文的可选迁移脚本，
  正常写入不会重写既有行。
- **0.1.1 无数据格式变更**：从 0.1.0 升级到 0.1.1 不需要迁移数据；升级后建议执行一次
  `mem compact -p <project>` 重建 `files.jsonl` 投影（0.1.1 的 `compact` 会从事件日志重放，自愈缺失/截断的投影）。

## 从 0.1.1 升级到 0.1.2

- **无数据格式变更**：`.mem` 的 `format` 仍为 `1`，事件与文件记录 schema 未变；0.1.1 与 0.1.2 可以互相读写同一份 store。
- **修复自动生效**：写锁覆盖范围、冲突解决增强（空补丁自动 `git rebase --skip`、`git worktree`/submodule 支持、`git add` 失败改为硬错误）、项目名校验扩展（拒绝 Windows 保留名与尾点）都是运行时行为修复，**不需要迁移数据**。
- **可选**：对每个项目再执行一次 `mem compact -p <project>`，按 0.1.2 的「按 `ts` 再 `event_id` 排序后重放」语义重建投影；日常追加与 `resolve` 仍遵循物理追加顺序（见 `DESIGN.md` 3.3）。
- **默认提交信息统一**：CLI `mem sync` 与 MCP `mem_sync` 的缺省提交信息统一为 `Auto-sync from <device> at <ts>`；若脚本依赖旧 MCP 的默认提交信息，请改为显式传 `message` 或适配新文本。
- **历史项目名需要留意**：若 store 里已有项目名命中 Windows 保留设备名（`CON` / `PRN` / `AUX` / `NUL` / `COM1`–`COM9` / `LPT1`–`LPT9`，含 `con.txt` 这类带扩展名形式）或以 `.` 结尾，0.1.2 的 CLI/MCP 会在新建事件前拒绝该名字；请先改名（`git mv projects/<old> projects/<new>`，并确认没有外部脚本引用旧名）再同步到 Windows。

## 0.1.0 相对内部预发布快照的变化

| 变化 | 影响 | 是否需要动作 |
| --- | --- | --- |
| 时间戳格式统一为 30 字符 UTC `Z` | 读取端兼容旧格式，排序正确 | 否 |
| ID 格式改为 `YYYYMMDDHHMMSS_nanos9_pid5_counter4`（UTC） | 新旧 ID 可共存；按 `ts` 排序，不依赖 ID | 仅当下游脚本解析 ID 结构时需要 |
| `file_name` 规范化为相对路径 | 旧数据里的绝对路径不会自动改写 | 可选，见下文「迁移 B」 |
| `mem resolve` 输出新增 `Lines preserved` | 若脚本解析了该命令的 stdout 需适配 | 视情况 |
| MCP `mem_add_event.files[].action` 改为可选（缺省 `Modified`） | 旧客户端传 action 仍正常 | 否 |
| MCP `mem_resolve` 返回文本追加 preserved 行数 | 若按旧文本全量匹配需适配 | 视情况 |
| 移除 `MemStore::scan_files` / `MemStore::read_latest_events` / `nodes/` | 调用这些 API 的代码会编译失败 | 库使用者需要改，见下文 |
| 读取容忍损坏行 | 以前直接报错的 store 现在会跳过坏行继续读 | 建议跑一次健康检查 |

## 升级步骤（单机）

```bash
# 1) 备份：整个 store 目录（含 .git）
cp -a /path/to/store /path/to/store.bak-$(date +%Y%m%d)

# 2) 确认没有进行中的 rebase / 未解决冲突
git -C /path/to/store status --short
ls /path/to/store/.git/rebase-merge /path/to/store/.git/rebase-apply 2>/dev/null

# 3) 升级二进制（cargo build --release 后替换 PATH 中的 mem / mem-mcp）
cargo build --release

# 4) 冒烟检查
/path/to/mem --path /path/to/store list
/path/to/mem --path /path/to/store resolve     # 清理历史遗留的冲突标记（若有）

# 5) 推荐：为每个项目重建一次 files.jsonl 投影（同时修复陈旧投影）
/path/to/mem --path /path/to/store compact -p <project>

# 6) 同步一次，把迁移过程中的本地改动推上去
/path/to/mem --path /path/to/store sync
```

多设备升级顺序：**先升级一台 → 完成一次 `sync` → 再升级其他设备**。旧版本仍能读取新格式
（它把时间戳当字符串存），但旧版本会继续写出旧时间戳格式与未规范化的路径，尽量缩短混用窗口。
0.1.0 与 0.1.1 之间没有数据格式差异，可以混用，但 0.1.1 的 `sync` 在冲突时会以非零退出码结束、
非冲突的 pull 失败会中止同步，脚本升级后请按新语义处理退出码。

## 迁移 A：时间戳格式（通常不需要）

不需要做任何事。读取端已按解析后的时间排序，旧格式不会导致排序错误。

只有在「必须让整份 JSONL 的字符串序等于时间序」的特殊场景（例如外部工具直接按字符串比较
`ts`）才考虑重写；重写会破坏 append-only 语义与哈希类审计，**不建议**。

## 迁移 B：把绝对路径 `file_name` 改为相对路径（可选）

**何时需要**：旧版本按「用户传入的路径原样」记录 `file_name`。如果你曾经传入绝对路径
（例如 `/home/alice/repo/src/main.rs`），那么：

- `mem query -n src/main.rs` 查不到这些旧记录；
- 同一文件在不同设备上会被当作不同文件，`mem list -p` 会多出条目、`mem render` 的文件表出现重复。

**先备份，再 dry-run，最后执行**。脚本只改 `file_name` 字段，保留未知字段与无法解析的行，
未改动的行保持字节不变，采用「临时文件 + 原子 rename」写回。

```bash
# 预览（不写盘），退出码 0 = 全部可迁移；2 = 有绝对路径没匹配到任何前缀
python3 migrate_absolute_paths.py --store /path/to/store \
    --strip-prefix /home/alice/repo --dry-run

# 执行
python3 migrate_absolute_paths.py --store /path/to/store \
    --strip-prefix /home/alice/repo

# 迁移后验证
/path/to/mem --path /path/to/store query -n src/main.rs
/path/to/mem --path /path/to/store list -p <project>
/path/to/mem --path /path/to/store compact -p <project>   # 会保留每个 file_name 的最新记录
/path/to/mem --path /path/to/store sync
```

需要多个前缀（例如不同项目目录）时重复 `--strip-prefix`：

```bash
python3 migrate_absolute_paths.py --store /path/to/store \
    --strip-prefix /home/alice/repo --strip-prefix /srv/work/repo
```

注意：

- 同一文件在旧数据里若同时存在「绝对路径记录」和「相对路径记录」，迁移后它们会归并为同一个
  `file_name`，历史条目仍在，`mem compact` 会收敛为最新一条。
- 脚本会同时改写 `events/*.jsonl` 里的 `event.files[].file_name` 与
  `projects/*/files.jsonl` 里的记录（查询索引读的是事件，文件表读的是后者，两者都要改）。
- `mem resolve` 依赖 `file_id` / `event_id` 去重，迁移不改这两个字段，冲突解决不受影响。

完整脚本（复制为 `migrate_absolute_paths.py`）：

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

## 代码迁移（直接使用 `mem-core` 库的调用方）

| 旧用法 | 新用法 |
| --- | --- |
| `MemStore::read_latest_events(n)` | `MemStore::read_latest_events_for_project(project, n)`（跨月正确；全局最新事件可用 `read_all_events` + 自行筛选） |
| `MemStore::scan_files(dir)` | 已删除；用 `ignore::WalkBuilder` 自行遍历 |
| `MemStore::nodes_dir()` | 已删除；设备信息在每条事件的 `device` 字段中 |
| `render_project_readme` 需要先有 `meta.json` | 缺少元数据时会自动兜底生成 |
| `ResolveReport { resolved_files, total_conflicts_resolved, deduplicated_items }` | 结构体新增 `lines_preserved` 字段；用 `..Default::default()` 或逐字段构造时补上 |
| 自行拼接 `Event.ts` | 改用 `mem_core::now_rfc3339()`；比较时间用 `mem_core::ts_cmp()` / `sort_events()` |
| 自行拼接设备名 / 哈希 | `mem_core::default_device_name()` / `mem_core::compute_sha256()` |
| 自行实现 store 向上查找 | `mem_core::resolve_store_root(None)`；写 `file_name` 用 `mem_core::normalize_file_name(path, &store.root)` |

## 回滚

- **代码**：`git checkout <之前的提交>` 并重新构建即可。
- **数据**：优先恢复升级前的备份（`store.bak-*`）。旧版本能读取新格式数据，但按字符串比较时间戳
  排序可能不准确，所以回滚后如果还要长期使用旧版本，建议同时恢复数据备份。
- 迁移脚本没有自动回滚开关；执行前必须有备份。

## 数据格式版本策略

- `.mem` 的 `format` 字段当前为 `1`。未来若发生不兼容的数据格式变更，会提升该值并在此文档给出
  迁移步骤；读端会先检查该标记再决定是否升级数据。
