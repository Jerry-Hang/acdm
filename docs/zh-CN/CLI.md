[English](../CLI.md) · **简体中文**

# `mem` CLI 完整参考

> 读者：需要日常使用或脚本化调用 `mem` 的开发者与 AI Agent 操作者。本文覆盖全部 8 个子命令的参数、输出格式、退出码与真实边界行为；Agent 侧的 MCP 入口见 [MCP.md](MCP.md)，架构与设计动机见 [ARCHITECTURE.md](ARCHITECTURE.md) 与 [DESIGN.md](DESIGN.md)。

本文以 `mem-cli/src/main.rs` 的 clap 定义为**唯一权威**（参数名、必填性、默认值均以源码为准）。文中示例输出在 v0.1.0 / v0.1.1 的实际二进制上运行得到（0.1.1 行为变更处以 0.1.1 重新实测）；为可读性，示例中的 store 绝对路径统一替换为 `/path/to/store` 占位符，事件 ID、时间戳、SHA256 与其余文本保持原样。示例中执行 git 提交前均已配置 git 用户身份；涉及 git 错误信息的地方以 `LC_ALL=C` 运行，因此错误文本是英文（git 错误消息随 locale 变化）。

## 目录

- [1. 概览](#1-概览)
  - [1.1 构建与安装](#11-构建与安装)
  - [1.2 全局参数与 store 定位](#12-全局参数与-store-定位)
  - [1.3 store 目录结构](#13-store-目录结构)
- [2. 退出码与错误行为](#2-退出码与错误行为)
- [3. 子命令](#3-子命令)
  - [3.1 init](#31-init)
  - [3.2 add](#32-add)
  - [3.3 query](#33-query)
  - [3.4 list](#34-list)
  - [3.5 compact](#35-compact)
  - [3.6 sync](#36-sync)
  - [3.7 resolve](#37-resolve)
  - [3.8 render](#38-render)
- [4. 环境变量](#4-环境变量)
- [5. 常见工作流](#5-常见工作流)
- [6. 相关文档](#6-相关文档)

## 1. 概览

`mem` 是 ACDM（Agent Cross-Device Memory System）的命令行入口。ACDM 用一个由 Git 同步的 append-only JSONL store 记录「哪个 Agent、在哪台设备上、对哪些文件做了什么改动」，从而让多台机器上的多个 Agent 共享同一份项目记忆。`mem` 负责人工操作与脚本调用；`mem-mcp` 把同一套核心能力暴露成 MCP 工具（见 [MCP.md](MCP.md)）。两者读写的是**同一个 store**，可以混用。

### 1.1 构建与安装

需要 Rust stable（edition 2021）；只有 `sync` 与 `resolve` 的 git 操作需要本机有 `git`。

```bash
# 构建 release 二进制（workspace 会同时构建 mem 与 mem-mcp）
cargo build --release
# 产物：
#   target/release/mem
#   target/release/mem-mcp
```

把 `target/release/mem` 放进 `PATH`（或直接用绝对路径调用）即可。下文示例假设 `mem` 已在 `PATH` 中。

### 1.2 全局参数与 store 定位

所有子命令共享一个全局参数：

| 参数 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- |
| `--path <DIR>` | 否 | 见下方定位规则 | 指定 store 根目录；`mem --path <DIR> <子命令>` 与 `mem <子命令> --path <DIR>` 等价 |

store 定位优先级从高到低（`mem` 与 `mem-mcp` 完全一致，实现在 `mem-core/src/util.rs::resolve_store_root`）：

1. 显式 `--path <DIR>`；
2. 环境变量 `ACDM_STORE`；
3. 从当前工作目录（CWD）向上逐级查找**最近的** store 根：某目录存在 `.mem` 标记文件，或存在 `events/` 目录；
4. 兜底：CWD 本身。

`init` 是唯一例外：它**不做向上搜索**，只使用显式的 `--path`（子命令级 `--path` 优先于全局 `--path`）或 CWD 作为**要初始化**的目标目录。

```bash
# 全局参数写在子命令之后同样有效
mem list --path /path/to/store

# init 上的子命令级 --path 优先于全局 --path：
# 实际初始化的是 ./actual，而不是 ./ignored，也不会创建 ./ignored
mem --path ./ignored init --path ./actual
```

### 1.3 store 目录结构

`mem init` 创建的结构：

```text
<store root>/
├── .mem                 # JSON 标记文件，标识 store 根
├── .gitignore           # 默认忽略规则（仅当不存在时创建）
├── events/
│   └── YYYY-MM.jsonl    # 事件日志：每月一个文件，只追加
└── projects/
    └── <project>/
        ├── files.jsonl  # 该项目的文件记录：只追加，可被 compact 重写
        ├── meta.json    # 项目元数据（首次 add 时自动创建，可手工编辑）
        └── README.md    # mem render 生成的项目说明
```

`.mem` 是一个 JSON 文件，内容形如：

```json
{
  "created_at": "2026-10-22T13:05:52.992739415Z",
  "format": 1
}
```

`init` 写入的默认 `.gitignore` 内容：

```gitignore
target/
*.swp
*.tmp
*.tmp.*
*~
.DS_Store
```

## 2. 退出码与错误行为

| 退出码 | 含义 | 输出位置与形式 |
| --- | --- | --- |
| `0` | 成功 | 正常结果写 stdout |
| `1` | 运行时错误 | stderr 打印 `Error: <原因>`（多行错误会续行）；可能在 `Error:` 之前先有 `Warning:` 警告 |
| `2` | 命令行参数解析错误（clap 约定） | stderr 打印 `error: ...` 与 `Usage:` / `For more information, try '--help'.` |

要点：

- 运行时错误统一走 `run() -> Result`，由 `main` 打印 `Error: ...` 后 `std::process::exit(1)`；成功路径退出码为 0。
- `query` 没有给出任何过滤条件时也是**运行时错误**（退出码 1），错误文本为两行：

```text
$ mem --path ./memory query
Error: No query filter specified.
Please specify at least one filter: --project (-p), --file-id, --filename (-n), or --fuzzy.
[exit code 1]
```

- 极少数命令会**先向 stdout 输出标题，再在 stderr 报错**（例如 `query --fuzzy` 的正则非法），脚本解析时需要注意两个流：

```text
$ mem --path ./memory query --fuzzy '['
--- Regex fuzzy search: '[' ---          # stdout
Error: regex parse error:                 # stderr
    [
    ^
error: unclosed character class
[exit code 1]
```

- 参数解析错误退出码为 2，例如：

```text
$ mem nope
error: unrecognized subcommand 'nope'

Usage: mem [OPTIONS] <COMMAND>

For more information, try '--help'.
[exit code 2]

$ mem --path ./memory add -p demo
error: the following required arguments were not provided:
  --summary <SUMMARY>
  <FILES>...

Usage: mem add --project <PROJECT> --summary <SUMMARY> <FILES>...

For more information, try '--help'.
[exit code 2]
```

- `sync` 检测到 Git 冲突时是**运行时错误**：stdout 打印已完成的步骤，stderr 提示运行 `mem resolve`，退出码 1（0.1.0 曾以退出码 0 结束，0.1.1 起改为非零，脚本可直接按退出码判断；见 [3.6 sync](#36-sync)）。

## 3. 子命令

### 3.1 init

初始化一个新的 ACDM store。

**用法**

```text
mem init [OPTIONS]
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| — | `--remote <URL>` | 否 | 无 | 同时执行 `git init` 并添加 `origin` 远端；URL 不校验连通性、不推送 |
| — | `--path <DIR>` | 否 | CWD | 要初始化的目录；优先于全局 `--path` |
| `-h` | `--help` | 否 | — | 打印帮助 |

**行为**

1. 创建 `<DIR>/events/`、`<DIR>/projects/`；
2. 若 `<DIR>/.mem` 不存在则写入标记文件（`format: 1` + 创建时间）；
3. 若 `<DIR>/.gitignore` 不存在则写入默认忽略规则；
4. 若给了 `--remote`：再执行 `git init` 与 `git remote add origin <URL>`。

重复执行 `init` 是幂等的：已存在的 `.mem` 与 `.gitignore` 不会被覆盖，输出仍然成功。`init` 不会配置 git 用户身份，`sync` 前需自行配置（见 [3.6 sync](#36-sync)）。

**示例**

```bash
# 纯本地 store
mem init --path ./memory

# 同时配置 Git remote：会 git init 并添加 origin
mem init --path ./memory --remote git@github.com:you/acdm-memory.git
```

**真实输出**

```text
$ mem init --path ./memory
Successfully initialized ACDM store at: ./memory
Created directory structure:
  ├── events/       (Append-only event log)
  ├── projects/     (Project metadata and file records)
  └── .mem          (Store marker file; lets the CLI/MCP find the store from any subdirectory)

$ mem init --path ./memory --remote git@github.com:you/acdm-memory.git
Successfully initialized ACDM store with remote 'git@github.com:you/acdm-memory.git' at: ./memory
Created directory structure:
  ├── events/       (Append-only event log)
  ├── projects/     (Project metadata and file records)
  └── .mem          (Store marker file; lets the CLI/MCP find the store from any subdirectory)
```

`--remote` 执行后的 git 状态（示例在独立目录中验证）：

```text
$ git -C ./memory remote -v
origin	git@github.com:you/acdm-memory.git (fetch)
origin	git@github.com:you/acdm-memory.git (push)
```

**注意事项**

- `init` 不向上搜索已有 store；要初始化子目录必须显式给 `--path`。
- `--remote` 只添加远端，不验证 URL、不设置分支跟踪、不推送；首次同步前需要本地已有提交并设置 upstream（见 [3.6 sync](#36-sync)）。
- 若仓库已存在 `origin`，再次 `--remote` 会因 `git remote add` 失败而打印 `Error: ...` 并以退出码 1 结束。

### 3.2 add

把一个事件（摘要 + 一组文件记录）追加到项目记忆。

**用法**

```text
mem add [OPTIONS] --project <PROJECT> --summary <SUMMARY> <FILES>...
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `-p` | `--project <PROJECT>` | **是** | — | 项目名（对应 `projects/<name>/`） |
| `-s` | `--summary <SUMMARY>` | **是** | — | 本次改动摘要 |
| `-d` | `--device <DEVICE>` | 否 | 本机 hostname | 设备名；取不到 hostname 时依次回退 `$HOSTNAME`、`$COMPUTERNAME`、`localhost` |
| `-a` | `--agent <AGENT>` | 否 | `human` | 记录者名称 |
| — | `<FILES>...` | **是**（≥1 个） | — | 文件路径（相对 CWD 或绝对路径） |
| — | `--path <DIR>` | 否 | 自动定位 | 全局参数：store 根目录 |
| `-h` | `--help` | 否 | — | 打印帮助 |

**行为**

1. 读取该项目 `projects/<project>/files.jsonl` 中现有的 `file_name` 集合（每个文件名只取最新记录）。
2. 对每个 `<FILES>`：
   - 规范化文件身份 `file_name`：优先相对 store 根；文件在 store 外时相对最近的 git 仓库根；再退回相对 CWD；都无法相对化时保留**绝对路径**，并向 stderr 打印警告（已存在的前缀会做 `fs::canonicalize` 解析符号链接，不存在的尾部按词法拼接）；
   - 文件存在：以 64 KiB 分块流式计算 SHA256，`size_bytes` = 字节数，`hash` = 十六进制摘要；若 `file_name` 已在该项目的已知文件名中则为 `Modified`，否则为 `Created`；
   - 文件不存在：记为 `Deleted`，`size_bytes = 0`，`hash = ""`；
   - 为每条记录生成独立的 `file_id`。
3. 生成事件：`event_id` 形如 `YYYYMMDDHHMMSS_nanos9_pid5_counter4`（UTC）；`ts` 为固定宽度 UTC 纳秒 `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`。
4. 若 `projects/<project>/meta.json` 读取失败（不存在），自动写入最小元数据：`description` 为 `"Project <name>"`、`github_url` 为 `null`、`created_at` 等于本次事件时间。
5. 校验项目名后，持 store 跨进程写锁把事件行追加到 `events/YYYY-MM.jsonl`，并把本次的每条 `FileRecord` 追加到 `projects/<project>/files.jsonl`（追加时另持排他文件锁）。

**真实输出（Created）**

```text
$ mem --path ./memory add -p demo -s "add hello" -d laptop -a claude project/src/hello.txt
Successfully recorded event: 20261022130617_173540407_23160_0001
  Project:   demo
  Timestamp: 2026-10-22T13:06:17.173539195Z
  Device:    laptop
  Agent:     claude
  Summary:   add hello
  Files (1):
    • [Created] src/hello.txt (12 bytes, sha256: a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447)
```

**真实输出（Modified、Deleted、多文件）**

```text
# 修改 project/src/hello.txt 后再次记录：同一 file_name 已知 → Modified
$ mem --path ./memory add -p demo -s "update hello" -d laptop -a claude project/src/hello.txt
Successfully recorded event: 20261022130617_174846583_23161_0001
  Project:   demo
  Timestamp: 2026-10-22T13:06:17.174845431Z
  Device:    laptop
  Agent:     claude
  Summary:   update hello
  Files (1):
    • [Modified] src/hello.txt (15 bytes, sha256: a56bd9a88d97584151e7626675654fc5678c5c9d75be0091100a829262cead2c)

# 文件不存在 → Deleted（size 0，hash 为空，输出中显示为 "-"）
$ mem --path ./memory add -p demo -s "remove obsolete" -d laptop -a claude project/src/removed.txt
Successfully recorded event: 20261022130617_177477279_23163_0001
  Project:   demo
  Timestamp: 2026-10-22T13:06:17.177476127Z
  Device:    laptop
  Agent:     claude
  Summary:   remove obsolete
  Files (1):
    • [Deleted] src/removed.txt (0 bytes, sha256: -)

# 另一示例目录（文件位于独立 git 仓库）：files 可一次传多个
$ mem --path ./memory add -p demo -s "add two files" -d desktop -a codex repo/src/a.txt repo/src/b.txt
Successfully recorded event: 20261022130629_791248624_23233_0002
  Project:   demo
  Timestamp: 2026-10-22T13:06:29.791247482Z
  Device:    desktop
  Agent:     codex
  Summary:   add two files
  Files (2):
    • [Created] src/a.txt (2 bytes, sha256: 87428fc522803d31065e7bce3cf03fe475096631e5e07bbd7a0fde60c4cf25c7)
    • [Created] src/b.txt (2 bytes, sha256: 0263829989b6fd954f72baaf2fc64bc2e2f01d692d4de72986ea808f6e99813f)
```

`meta.json` 自动创建的实际内容：

```json
{
  "name": "demo",
  "description": "Project demo",
  "github_url": null,
  "created_at": "2026-10-22T13:06:17.173539195Z"
}
```

**路径规范化与 stderr 警告（真实输出）**

文件既不在 store 内、也不在任何 git 仓库内、也无法相对 CWD 时，会记录绝对路径并警告（下面在代码仓库的 `project/` 目录内执行，store 位于 `../memory`）：

```text
$ mem --path ../memory add -p demo -s "track loose file" -d laptop -a claude ../elsewhere/loose.txt
Warning: '../elsewhere/loose.txt' is outside the memory store and outside any git repository; recording it under its absolute path, which will not match on other devices.   # stderr

Successfully recorded event: 20261022130629_780024199_23222_0001                                                                                        # stdout
  Project:   demo
  Timestamp: 2026-10-22T13:06:29.780022316Z
  Device:    laptop
  Agent:     claude
  Summary:   track loose file
  Files (1):
    • [Created] /path/to/loose.txt (14 bytes, sha256: 21f822c8aa1e6d8acf623ebf3f340aea89570a5d4f49a42d77d807d12053364f)
```

**注意事项**

- `-p/--project` 必须是单一安全路径组件：非空、不是 `.`/`..`、不含 `/` 或 `\`、无控制字符、≤200 字节；否则报 `Error: invalid project name ...` 并退出 1（在任何 store 操作之前校验）。
- `<FILES>` 是必填的位置参数，至少传一个；路径可以是相对 CWD，也可以是绝对路径。
- `Created` / `Modified` 只根据 `file_name` 是否已出现在该项目的 `files.jsonl`（含历史记录，去重后）来判断；不比较内容 hash，因此对同一文件即使内容没变，第二次记录也会是 `Modified`。
- 文件不存在也会产生一条 `Deleted` 记录，路径仍然按同样规则规范化。
- 记录的绝对路径不会跨设备匹配，只适合临时本地记录；跨设备场景请让文件位于 store 内或某个 git 仓库内（见 [MIGRATION.md](MIGRATION.md) 的旧绝对路径记录说明）。
- 文件内容本身不入库，只保存路径、大小和 SHA256。
- 事件行先写 `events/`，再写 `projects/<p>/files.jsonl`，两者不是一个原子事务；正常结束时一致。

### 3.3 query

查询已记录的事件与文件记录。

**用法**

```text
mem query [OPTIONS]
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `-p` | `--project <PROJECT>` | 否 | — | 列出该项目全部事件 |
| — | `--file-id <FILE_ID>` | 否 | — | 按 `file_id` 精确查询单条文件记录及其父事件 |
| `-n` | `--filename <FILENAME>` | 否 | — | 按 `file_name` **精确匹配**事件（完整相对路径，不做子串匹配） |
| — | `--fuzzy <REGEX>` | 否 | — | 对事件原始 JSON 行做 Rust 正则模糊搜索 |
| — | `--path <DIR>` | 否 | 自动定位 | 全局参数：store 根目录 |
| `-h` | `--help` | 否 | — | 打印帮助 |

**参数约束**：四个过滤条件全部可选，但**至少要给一个**，否则报错退出 1。可以同时给多个：按 `--project` → `--file-id` → `--filename` → `--fuzzy` 的固定顺序依次输出（每段之间空一行）。

**索引机制（一句话）**：`query` 先用 `MemIndex` 扫描全部 `events/*.jsonl`，为每个事件记录「文件路径 + 行首字节偏移」；查询命中后按偏移回到原文件读取该行，因此没有持久化索引，每次查询都会重建内存索引。

**真实输出（按项目）**

```text
$ mem --path ./memory query -p demo
--- Events for project 'demo' ---
Found 4 event(s):
Event: 20261022130617_173540407_23160_0001 | 2026-10-22T13:06:17.173539195Z
  Project: demo | Device: laptop | Agent: claude
  Summary: add hello
  Files (1):
    • [Created] src/hello.txt (ID: 20261022130617_173530378_23160_0000, 12 bytes, hash: a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447)

Event: 20261022130617_174846583_23161_0001 | 2026-10-22T13:06:17.174845431Z
  Project: demo | Device: laptop | Agent: claude
  Summary: update hello
  Files (1):
    • [Modified] src/hello.txt (ID: 20261022130617_174840512_23161_0000, 15 bytes, hash: a56bd9a88d97584151e7626675654fc5678c5c9d75be0091100a829262cead2c)

Event: 20261022130617_176161345_23162_0001 | 2026-10-22T13:06:17.176160193Z
  Project: demo | Device: laptop | Agent: claude
  Summary: add util
  Files (1):
    • [Created] src/util.txt (ID: 20261022130617_176154452_23162_0000, 12 bytes, hash: f957b19529906961933c5c30f8713c500a9bb5d9d0695c40d48c97a26a3594ec)

Event: 20261022130617_177477279_23163_0001 | 2026-10-22T13:06:17.177476127Z
  Project: demo | Device: laptop | Agent: claude
  Summary: remove obsolete
  Files (1):
    • [Deleted] src/removed.txt (ID: 20261022130617_177470336_23163_0000, 0 bytes, hash: -)
```

**真实输出（按 file_id）**

```text
$ mem --path ./memory query --file-id 20261022130617_174840512_23161_0000
--- File record lookup: '20261022130617_174840512_23161_0000' ---
File Record Details:
  File ID:     20261022130617_174840512_23161_0000
  File Name:   src/hello.txt
  Action:      Modified
  Size:        15 bytes
  SHA256:      a56bd9a88d97584151e7626675654fc5678c5c9d75be0091100a829262cead2c

Parent Event:
  Event ID:    20261022130617_174846583_23161_0001
  Timestamp:   2026-10-22T13:06:17.174845431Z
  Project:     demo
  Device:      laptop
  Agent:       claude
  Summary:     update hello
```

**真实输出（按文件名 / 正则）**

```text
$ mem --path ./memory query -n src/hello.txt
--- Events containing filename 'src/hello.txt' ---
Found 2 event(s):
Event: 20261022130617_173540407_23160_0001 | 2026-10-22T13:06:17.173539195Z
  Project: demo | Device: laptop | Agent: claude
  Summary: add hello
  Files (1):
    • [Created] src/hello.txt (ID: 20261022130617_173530378_23160_0000, 12 bytes, hash: a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447)

Event: 20261022130617_174846583_23161_0001 | 2026-10-22T13:06:17.174845431Z
  Project: demo | Device: laptop | Agent: claude
  Summary: update hello
  Files (1):
    • [Modified] src/hello.txt (ID: 20261022130617_174840512_23161_0000, 15 bytes, hash: a56bd9a88d97584151e7626675654fc5678c5c9d75be0091100a829262cead2c)

$ mem --path ./memory query --fuzzy 'update|remove'
--- Regex fuzzy search: 'update|remove' ---
Found 2 matching event(s):
Event: 20261022130617_174846583_23161_0001 | 2026-10-22T13:06:17.174845431Z
  Project: demo | Device: laptop | Agent: claude
  Summary: update hello
  Files (1):
    • [Modified] src/hello.txt (ID: 20261022130617_174840512_23161_0000, 15 bytes, hash: a56bd9a88d97584151e7626675654fc5678c5c9d75be0091100a829262cead2c)

Event: 20261022130617_177477279_23163_0001 | 2026-10-22T13:06:17.177476127Z
  Project: demo | Device: laptop | Agent: claude
  Summary: remove obsolete
  Files (1):
    • [Deleted] src/removed.txt (ID: 20261022130617_177470336_23163_0000, 0 bytes, hash: -)
```

**未命中时的输出（真实输出）**

```text
$ mem --path ./memory query -p nope
--- Events for project 'nope' ---
No events recorded for project 'nope'.

$ mem --path ./memory query --file-id 00000000000000_000000000_00000_0000
--- File record lookup: '00000000000000_000000000_00000_0000' ---
No file record found with ID '00000000000000_000000000_00000_0000'.

$ mem --path ./memory query -n src/nope.rs
--- Events containing filename 'src/nope.rs' ---
No events found containing filename 'src/nope.rs'.

# 精确匹配：只给文件名的子串不会命中
$ mem --path ./memory query -n hello.txt
--- Events containing filename 'hello.txt' ---
No events found containing filename 'hello.txt'.

$ mem --path ./memory query --fuzzy 'zzz-not-present'
--- Regex fuzzy search: 'zzz-not-present' ---
No events matched pattern 'zzz-not-present'.
```

**注意事项**

- `--file-id` 匹配的是 `FileRecord.file_id`（每条文件记录都有独立 ID），输出会带上父事件的完整信息。
- `-n/--filename` 匹配的是**规范化后的相对 `file_name`**（如 `src/hello.txt`），且是**整串精确匹配**、不是子串搜索（查 `hello.txt` 不会命中 `src/hello.txt`）；不是系统绝对路径；历史数据中的绝对路径记录用相对名查不到（见 [MIGRATION.md](MIGRATION.md)）。
- `--fuzzy` 是对事件原始 JSONL 行做 Rust `regex` 匹配，不是全文分词；正则非法时先打印搜索标题，再在 stderr 报 `Error: regex parse error: ...` 并以退出码 1 结束。正则编译程序有 10 MiB 尺寸上限，超大正则会报 `Error: Compiled regex exceeds size limit of 10485760 bytes.`
- 过滤条件可以组合，输出顺序固定为 project → file_id → filename → fuzzy。
- 每次查询都会重新扫描事件文件建立索引；store 很大时查询会变慢，这属于当前实现的已知特征，不是常驻服务。

### 3.4 list

列出 store 中的项目，或某个项目当前的文件状态（每个 `file_name` 只取最新记录）。

**用法**

```text
mem list [OPTIONS]
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `-p` | `--project <PROJECT>` | 否 | — | 给出时列出该项目的文件表；省略时列出项目表 |
| — | `--path <DIR>` | 否 | 自动定位 | 全局参数：store 根目录 |
| `-h` | `--help` | 否 | — | 打印帮助 |

**两种输出**

- **无 `-p`（项目表）**：列 `PROJECT`（宽 24）、`FILES`（宽 8，去重后的文件数）、`CREATED AT`（宽 32，取 `meta.json` 的 `created_at`）、`DESCRIPTION`（取 `meta.json` 的 `description`）；表头下分隔线固定 96 个 `-`；项目名按字典序升序。缺 `meta.json` 时 `CREATED AT` 与 `DESCRIPTION` 显示 `-`。
- **有 `-p`（文件表）**：列 `FILE NAME`（宽 32）、`ACTION`（宽 10）、`SIZE (BYTES)`（宽 14）、`FILE ID`（宽 37）、`HASH (SHA256)`（hash 前 16 个十六进制字符 + `...`，空 hash 显示 `-`）；分隔线固定 125 个 `-`；行按 `file_name` 字典序升序。

**真实输出（项目表）**

```text
$ mem --path ./memory list
PROJECT                  FILES    CREATED AT                       DESCRIPTION
------------------------------------------------------------------------------------------------
demo                     3        2026-10-22T13:06:17.173539195Z   Project demo
```

**真实输出（文件表）**

```text
$ mem --path ./memory list -p demo
FILE NAME                        ACTION     SIZE (BYTES)   FILE ID                               HASH (SHA256)
-----------------------------------------------------------------------------------------------------------------------------
src/hello.txt                    Modified   15             20261022130617_174840512_23161_0000   a56bd9a88d975841...
src/removed.txt                  Deleted    0              20261022130617_177470336_23163_0000   -
src/util.txt                     Created    12             20261022130617_176154452_23162_0000   f957b19529906961...
```

**空 store / 项目无文件（真实输出）**

```text
$ mem --path ./memory list
No projects found in memory store.

$ mem --path ./memory list -p ghost
No files recorded for project 'ghost'.
```

**注意事项**

- 两处表格的列宽是固定宽度格式化，超长值**不会截断**，会挤占右侧列导致对齐错位；脚本解析时不要把空格位置当作稳定分隔符。
- `FILES` 是「去重后的文件名数量」，不是历史记录条数；即使 `compact` 之前，同一文件的多条历史也只计 1。
- 文件表只展示每个 `file_name` 的最新一条记录（`files.jsonl` 中最后出现者优先）。

### 3.5 compact

压缩项目文件记录：`files.jsonl` 中每个 `file_name` 只保留最新一条记录。

**用法**

```text
mem compact [OPTIONS] --project <PROJECT>
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `-p` | `--project <PROJECT>` | **是** | — | 要压缩的项目名 |
| — | `--path <DIR>` | 否 | 自动定位 | 全局参数：store 根目录 |
| `-h` | `--help` | 否 | — | 打印帮助 |

**行为**

1. 校验项目名，取 store 跨进程写锁（一直持有到 rename）；
2. 统计现有 `projects/<project>/files.jsonl` 的非空行数作为 `records_before`（文件不存在时为 0）；
3. 从事件日志重放该项目全部事件（`sort_events` 按时间排序），以 `file_name` 为键保留**最后写入**的记录；
4. 若既无投影又无事件，返回 0/0/0 且不创建目录；否则把结果写到同目录的临时文件 `files.jsonl.tmp`，再原子 `rename` 覆盖原文件；
5. 输出压缩前后的记录数与剪除数。

**真实输出**

先记录：`src/hello.txt` 两条（Created + Modified）、`src/util.txt` 一条、`src/removed.txt` 一条，共 4 条记录：

```text
$ mem --path ./memory compact -p demo
Compacting file records for project 'demo'...
Compaction completed for project 'demo':
  Records before: 4
  Records after:  3
  Records pruned: 1
```

压缩后 `mem list -p demo` 的三行（与压缩前相同，因为 `list` 本来就只显示最新记录）：

```text
$ mem --path ./memory list -p demo
FILE NAME                        ACTION     SIZE (BYTES)   FILE ID                               HASH (SHA256)
-----------------------------------------------------------------------------------------------------------------------------
src/hello.txt                    Modified   15             20261022130617_174840512_23161_0000   a56bd9a88d975841...
src/removed.txt                  Deleted    0              20261022130617_177470336_23163_0000   -
src/util.txt                     Created    12             20261022130617_176154452_23162_0000   f957b19529906961...
```

**注意事项**

- `compact` 从事件日志重建 `projects/<project>/files.jsonl`，**不会改动 `events/`**；`query` 的历史事件仍然完整可查。
- 重写后文件内记录按 `file_name` 字典序排列（内部使用 `BTreeMap`），不再是原始追加顺序。
- 剪除数 = `records_before - records_after`（由 CLI 计算并打印）。
- 因为投影是从事件日志重放的，`compact` 能**自愈**崩溃/并发导致的投影缺失或截断；整个操作持有 store 写锁，可与追加写并发执行，不会丢记录。
- 写临时文件后原子 rename 并 `sync_all`，进程中断最多留下 `files.jsonl.tmp`（已被 store 的 `.gitignore` `*.tmp` 覆盖）。

### 3.6 sync

把本地 store 的改动提交、变基拉取远端更新、再推送回远端。

**用法**

```text
mem sync [OPTIONS]
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| — | `--path <DIR>` | 否 | 自动定位 | 全局参数：store 根目录 |
| `-h` | `--help` | 否 | — | 打印帮助 |

**行为（严格按执行顺序）**

1. 在 store 根执行 `git status --porcelain`，并全程持有 store 跨进程写锁；不是 git 仓库或 git 不可用 → `Error: ...` 退出 1。
2. 工作区**不干净**：`git add -A`，再 `git commit -m "Auto-sync from <device> at <ts>"`。其中 `<device>` 取本机 hostname（同 `add` 的规则），`<ts>` 是固定宽度 UTC 纳秒时间戳。
3. 检查当前分支是否有 upstream：
   - 没有 upstream → **跳过 pull**，继续下一步（push 会因缺少推送目标而失败，错误由 git 给出）；
   - 有 upstream → `git pull --rebase`（**不再带 `--autostash`**：同步前已提交全部改动并持写锁；带 autostash 时若 stash pop 在 rebase 中冲突，会留下没有 rebase 状态的冲突工作区，极难恢复）。
4. 若 pull 失败：
   - 若检测到冲突（`git ls-files -u` 未合并文件、porcelain 冲突码，或 rebase/merge 状态目录）：stdout 继续打印已完成的步骤，向 **stderr** 打印「⚠️  git conflict detected; run `mem resolve`, then retry `mem sync`」，随后以 `Error: sync stopped on a git conflict: ...` **退出码 1** 结束（不执行 push）；此时工作区保留冲突标记，等待 `mem resolve`；
   - 若不是冲突：直接以 `Error: git pull --rebase failed and the repository is not conflicted; aborting sync ...` **退出码 1** 中止，**不再继续 push**（旧版本会忽略该错误继续 push，掩盖真实原因）。
5. `git push`；失败则 `Error: ...` 退出 1；成功打印 `✅ Sync successful: repository is up-to-date and pushed to remote.`

**真实输出（脏工作区，remote + upstream 已配置）**

```text
$ mem --path ./store sync
Synchronizing memory store at: /path/to/store
  • Committed local changes ("Auto-sync from localhost at 2026-10-22T16:14:50.164699829Z")
  • Pulled remote changes with rebase
  • Pushed to remote successfully
✅ Sync successful: repository is up-to-date and pushed to remote.
```

（示例环境取不到 `hostname`，设备名回退为 `localhost`；正常主机上这里是实际 hostname。）

**真实输出（干净工作区）**

```text
$ mem --path ./store sync
Synchronizing memory store at: /path/to/store
  • Working directory clean
  • Pulled remote changes with rebase
  • Pushed to remote successfully
✅ Sync successful: repository is up-to-date and pushed to remote.
```

**真实输出（拉取时发生冲突：退出码 1，stdout + stderr）**

两台设备各自向同一个 `events/2026-10.jsonl` 追加事件，A 先同步，B 再同步：

```text
$ LC_ALL=C mem --path ./store-b sync
Synchronizing memory store at: /path/to/store-b
  • Committed local changes ("Auto-sync from localhost at 2026-10-22T16:17:56.885385697Z")

# stderr:
⚠️  git conflict detected; run `mem resolve`, then retry `mem sync`
Error: sync stopped on a git conflict: Git conflict detected during pull --rebase: git command 'git pull --rebase' failed: From /path/to/remote
   3283b63..452cb6d  main       -> origin/main
Rebasing (1/1)error: could not apply c4b16c4... Auto-sync from localhost at 2026-10-22T16:17:56.885385697Z
...
[exit code 1]
```

依次执行 `mem resolve` 与再次 `mem sync` 后的真实输出见 [3.7 resolve](#37-resolve) 的「两台设备 rebase 冲突后的真实恢复流程」。

**真实输出（无 remote / 无 upstream：跳过 pull，push 失败）**

```text
# The store is a git repository, but has neither a remote nor an upstream
$ LC_ALL=C mem --path ./store sync
Synchronizing memory store at: /path/to/store
# stderr:
Error: git command 'git push' failed: fatal: No configured push destination.
Either specify the URL from the command-line or configure a remote repository using

    git remote add <name> <url>

and then push using the remote name

    git push <name>
...
[exit code 1]
```

**真实输出（不是 git 仓库）**

```text
$ LC_ALL=C mem --path ./store sync
Synchronizing memory store at: /path/to/store
# stderr：
Error: git command 'git status --porcelain' failed: fatal: not a git repository (or any parent up to mount point /)
Stopping at filesystem boundary (GIT_DISCOVERY_ACROSS_FILESYSTEM not set).
[exit code 1]
```

**注意事项**

- 前置条件：store 根是 git 仓库；已配置可用的 remote 与当前分支的 upstream（`git push -u origin HEAD` 或 `git clone` 可建立）；已配置 git 用户身份（`user.name` / `user.email`）——工作区需要提交而身份未配置时，提交会失败并退出 1。
- 冲突时命令**不会**自动合并，也**不会** abort rebase；它把冲突状态留给 `mem resolve`。冲突场景下 `sync` 的退出码是 **1**（0.1.0 曾为 0），脚本直接按退出码判断即可，不需要再扫描文件内容。
- 提交信息固定为 `Auto-sync from <device> at <ts>`；`sync` 不提供自定义消息参数（MCP 的 `mem_sync` 支持可选 `message`，见 [MCP.md](MCP.md)）。
- git 的错误文本受 locale 影响，示例使用 `LC_ALL=C`；内容本身是 git 原始输出。
- `sync` 不调用任何冲突解决逻辑（不会自动执行 `resolve`），也不创建 remote；远端配置只由 `mem init --remote` 或手工 git 命令完成。

### 3.7 resolve

扫描 store 下所有 `.jsonl` 文件中的 Git 冲突标记，自动去重、排序并继续 rebase。

**用法**

```text
mem resolve [OPTIONS]
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| — | `--path <DIR>` | 否 | 自动定位 | 全局参数：store 根目录 |
| `-h` | `--help` | 否 | — | 打印帮助 |

**行为**

1. 取 store 跨进程写锁并全程持有，然后进入循环：递归扫描 store 下所有 `*.jsonl`（`hidden(false)`：隐藏文件也会扫描；store 是 git 仓库时 `.gitignore` 过滤生效）；只处理包含 `<<<<<<<` 字节的文件。
2. **按字节**读入文件（不要求合法 UTF-8），去掉**精确形态**的冲突标记：`=======` 必须整行恰好是七个 `=`；`<<<<<<<` / `|||||||` / `>>>>>>>` 之后只能是行尾或一个空格（可带 label）。以标记前缀开头的普通数据行不算标记、会被保留。
3. 按文件内容识别记录类型（`Event` / `FileRecord` / 其他 JSON / 无法识别），分别处理：
   - 合法记录按去重键（`event_id` / `file_id`）去重，**后出现者胜出**；
   - 事件记录按时间戳解析后排序（兼容旧格式），时间戳相同再用 `event_id` 排序；其他合法 JSON 记录按 `ts`（若有）再去重键排序；**`FileRecord` 不重排**，只保留每个键最后一次出现的位置（保持因果顺序，避免旧状态覆盖 `files.jsonl` 的 last-write-wins）；
   - **无法解析为对应记录类型的行逐字节保留**（含原始前后空白、CRLF、非 UTF-8 字节），放到文件末尾；完全相同的保留行去重；
   - 合法记录**原样重发原始行文本**，因此未知字段、字段顺序、格式都不会丢失。
4. 写回解析后的文件（额外持排他文件锁）。
5. store 根存在 `.git` 时，对每个被改写的文件执行 `git add -- <相对路径>`。
6. 若 `.git/rebase-merge` 或 `.git/rebase-apply` 存在（确实有 rebase 在进行），执行 `git rebase --continue`（用 `GIT_EDITOR=true` 避免编辑器阻塞），然后回到第 1 步继续下一轮；一次调用最多循环 100 轮，多提交 rebase 也能一次推进完。检测不依赖 git 的本地化错误文本。

**真实输出（非 git 目录中的手工冲突文件：含 1 条无法解析的截断行）**

冲突文件内容（`events/2026-10.jsonl`）：

```json
{"event_id":"20261022010000_000000001_00001_0000","ts":"2026-10-22T01:00:00Z","device":"laptop-a","agent":"claude","project":"demo","summary":"first event","files":[]}
<<<<<<< HEAD
{"event_id":"20261022020000_000000002_00001_0001","ts":"2026-10-22T02:00:00Z","device":"laptop-a","agent":"claude","project":"demo","summary":"from HEAD","files":[]}
=======
{"event_id":"20261022020000_000000002_00001_0001","ts":"2026-10-22T02:00:00Z","device":"laptop-b","agent":"codex","project":"demo","summary":"from branch","files":[]}
>>>>>>> branch-b
  {"event_id":"20261022030000_000000003_00001_0002","ts":"2026-10-22T03:00:00Z","summary":"TRUNCATED
```

运行结果：

```text
$ mem --path . resolve
Resolving conflicts in JSONL files at: /path/to/store
Conflict Resolution Report:
  Resolved files:      1
  Conflicts resolved:  1
  Deduplicated items:  1
  Lines preserved:     1
  Files updated:
    • /path/to/store/events/2026-10.jsonl
```

解析后的文件内容（重复的 `event_id` 保留后出现者 `from branch`；截断行逐字保留在末尾）：

```json
{"event_id":"20261022010000_000000001_00001_0000","ts":"2026-10-22T01:00:00Z","device":"laptop-a","agent":"claude","project":"demo","summary":"first event","files":[]}
{"event_id":"20261022020000_000000002_00001_0001","ts":"2026-10-22T02:00:00Z","device":"laptop-b","agent":"codex","project":"demo","summary":"from branch","files":[]}
  {"event_id":"20261022030000_000000003_00001_0002","ts":"2026-10-22T03:00:00Z","summary":"TRUNCATED
```

再执行一次（幂等，没有冲突文件）：

```text
$ mem --path . resolve
Resolving conflicts in JSONL files at: /path/to/store
Conflict Resolution Report:
  Resolved files:      0
  Conflicts resolved:  0
  Deduplicated items:  0
  Lines preserved:     0
```

**真实输出（两台设备 rebase 冲突后的真实恢复流程）**

B 设备 `mem sync` 出现冲突警告（见 [3.6 sync](#36-sync)）后：

```text
$ mem --path ./store-b resolve
Resolving conflicts in JSONL files at: /path/to/store-b
Conflict Resolution Report:
  Resolved files:      2
  Conflicts resolved:  2
  Deduplicated items:  0
  Lines preserved:     0
  Files updated:
    • /path/to/store-b/events/2026-10.jsonl
    • /path/to/store-b/projects/demo/files.jsonl

$ mem --path ./store-b sync
Synchronizing memory store at: /path/to/store-b
  • Working directory clean
  • Pulled remote changes with rebase
  • Pushed to remote successfully
✅ Sync successful: repository is up-to-date and pushed to remote.
```

之后另一台设备 `git pull --rebase` 即可看到双方事件。

**注意事项**

- `resolve` 只处理 `.jsonl` 文件。**非 JSONL 的冲突不会处理**，例如 `projects/<p>/meta.json`（两台设备同时为同一新项目写入元数据时可能冲突）；这类冲突必须先手工解决并 `git add`。若循环中的 `git rebase --continue` 因这些文件失败，命令会打印一条可操作的错误，列出非 JSONL 文件和所有仍未解决的路径，并提示 `git add <file>` 后重试；故障排查参见 [TROUBLESHOOTING.md](TROUBLESHOOTING.md)。
- 不在 git 仓库中也能运行：只是清理冲突标记，不会 `git add`、不会继续 rebase。
- `Files updated` 中显示的是绝对路径。
- 保留行（含非 UTF-8 字节与 CRLF）永远不会被静默丢弃；历史合法记录也不会被重写（除冲突标记、去重与排序外，行文本保持原样）。
- 去重是「后出现者胜出」：rebase 场景中通常意味着 `theirs` 分支的记录覆盖 `ours` 中同 ID 的记录。

### 3.8 render

生成或更新项目的 `README.md`（`projects/<project>/README.md`）。

**用法**

```text
mem render [OPTIONS] --project <PROJECT>
```

**参数**

| 短选项 | 长选项 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `-p` | `--project <PROJECT>` | **是** | — | 要渲染的项目名 |
| — | `--path <DIR>` | 否 | 自动定位 | 全局参数：store 根目录 |
| `-h` | `--help` | 否 | — | 打印帮助 |

**行为**

1. 读取 `projects/<project>/meta.json`；**读取失败时**使用内存中的兜底元数据（`name` = 项目名、`description` = `"Project <name>"`、`github_url` = null、`created_at` = 当前时间），**不会写回 `meta.json`**；
2. 取该项目每个 `file_name` 的最新记录生成文件表；
3. 取该项目最近 10 条事件生成时间线（最新在前）；
4. 所有插值先转义：`|` → `\|`，CR/LF 折叠为空格（防止记录的数据破坏表格/时间线结构或注入内容）；
5. 用排他锁写 `projects/<project>/README.md`（目录不存在则创建），每次覆盖。

生成的 README 结构：

```markdown
# <project>

<description>

### Metadata
- **Created At:** <created_at>
- **GitHub:** <github_url>        # 仅当 meta.github_url 存在

## Tracked Files

| File Name | Action | Size (bytes) | Hash (SHA256) |
| --- | --- | --- | --- |
| ... | ... | ... | ... |

## Recent Activity Timeline

- **<ts>** [`<agent>` on `<device>`]: <summary>   # 最近 10 条，最新在前
```

**真实输出**

```text
$ mem --path ./memory render -p demo
Rendering README.md for project 'demo'...
Successfully generated README at: /path/to/store/projects/demo/README.md
```

生成的 `projects/demo/README.md`（真实内容）：

```markdown
# demo

Project demo

### Metadata
- **Created At:** 2026-10-22T13:06:17.173539195Z

## Tracked Files

| File Name | Action | Size (bytes) | Hash (SHA256) |
| --- | --- | --- | --- |
| `src/hello.txt` | Modified | 15 | `a56bd9a88d97584151e7626675654fc5678c5c9d75be0091100a829262cead2c` |
| `src/removed.txt` | Deleted | 0 | `` |
| `src/util.txt` | Created | 12 | `f957b19529906961933c5c30f8713c500a9bb5d9d0695c40d48c97a26a3594ec` |

## Recent Activity Timeline

- **2026-10-22T13:06:17.177476127Z** [`claude` on `laptop`]: remove obsolete
- **2026-10-22T13:06:17.176160193Z** [`claude` on `laptop`]: add util
- **2026-10-22T13:06:17.174845431Z** [`claude` on `laptop`]: update hello
- **2026-10-22T13:06:17.173539195Z** [`claude` on `laptop`]: add hello
```

**meta 缺失时的兜底（真实输出）**

```text
$ mem --path ./memory render -p ghost
Rendering README.md for project 'ghost'...
Successfully generated README at: /path/to/store/projects/ghost/README.md
```

生成的 `projects/ghost/README.md`（meta 缺失时的兜底内容）：

```markdown
# ghost

Project ghost

### Metadata
- **Created At:** 2026-10-22T13:07:09.682762031Z

## Tracked Files

_No files tracked yet._

## Recent Activity Timeline

_No events recorded yet._
```

`render` 之后项目目录里只有 `README.md`，没有 `meta.json`；因此 `list` 中的创建时间与描述仍为 `-`：

```text
$ ls projects/ghost/
README.md

$ mem --path ./memory list
PROJECT                  FILES    CREATED AT                       DESCRIPTION
------------------------------------------------------------------------------------------------
ghost                    0        -                                -
```

**注意事项**

- `render` 是纯生成操作：每次覆盖 `projects/<project>/README.md`，不会修改 `files.jsonl` / `meta.json` / `events/`。
- `meta.json` 缺失时只影响本次渲染，不会补写；之后任意一次 `mem add`（或 MCP 的 `mem_add_event`）才会自动补 `meta.json`，`list` 的 `CREATED AT` / `DESCRIPTION` 也才会从 `-` 变成真实值。
- 空 hash 在表格中渲染为两个反引号（`` `` ），这是当前格式的真实输出。
- 文件表与时间线中的所有插值都会转义：文件名/动作/hash/时间戳/agent/device/summary 里的 `|` 写成 `\|`，换行折叠为空格。
- 时间线最多 10 条，按时间升序读取后反转为最新在前。

## 4. 环境变量

| 变量 | 影响 | 说明 |
| --- | --- | --- |
| `ACDM_STORE` | store 定位 | 未给 `--path` 时作为 store 根；优先级低于 `--path`、高于向上搜索。`init` 不使用它（`init` 只看 `--path` 或 CWD） |
| `HOSTNAME` / `COMPUTERNAME` | 默认设备名 | `add` / `sync` 取 device 的兜底来源：先尝试 `hostname` 命令，再依次读这两个环境变量，最后 `localhost` |
| `PATH` | `sync` / `resolve` | 需要能找到 `git` 可执行文件 |
| git 相关变量（`GIT_AUTHOR_NAME`、`GIT_COMMITTER_*`、`GIT_EDITOR` 等） | git 提交行为 | `sync` 的提交、`resolve` 的 `rebase --continue` 都会继承环境；`resolve` 内部固定把 `GIT_EDITOR` 设为 `true` |
| `LC_ALL` / `LANG` | 报错语言 | git 子进程的 stderr 本地化；示例使用 `LC_ALL=C` 以得到英文错误 |
| `XDG_RUNTIME_DIR` | store 写锁位置 | store 跨进程写锁的锁文件目录；未设置或不可用时退回系统临时目录（锁文件不进入 Git 仓库） |

`ACDM_STORE` 示例（真实输出）：

```text
$ ACDM_STORE=/path/to/store mem list
PROJECT                  FILES    CREATED AT                       DESCRIPTION
------------------------------------------------------------------------------------------------
demo                     4        2026-10-22T13:06:17.173539195Z   Project demo
```

## 5. 常见工作流

### 5.1 单机日常

```bash
# 1) 初始化（一次）
mem init --path ./memory

# 2) 记录改动
mem --path ./memory add -p ACDM -s "实现增量索引" -d laptop -a claude src/index.rs src/storage.rs

# 3) 查询/浏览
mem --path ./memory query -p ACDM
mem --path ./memory query -n src/index.rs
mem --path ./memory list
mem --path ./memory list -p ACDM

# 4) 生成项目说明
mem --path ./memory render -p ACDM

# 5) 定期压缩文件记录（可选）
mem --path ./memory compact -p ACDM
```

### 5.2 跨设备同步

```bash
# 设备 A：初始化 store 并配置远端
mem init --path ./memory --remote git@github.com:you/acdm-memory.git
cd ./memory
git add -A && git commit -m "initial store"
git push -u origin HEAD
cd ..

# 设备 B：克隆同一个 store
git clone git@github.com:you/acdm-memory.git ./memory

# 两台设备都通过 add / sync 工作（git 用户身份需先配置）
mem --path ./memory add -p ACDM -s "change on A" -d laptop-a -a claude src/index.rs
mem --path ./memory sync

# 常见冲突处理（见 5.3）
mem --path ./memory sync        # 冲突时退出码为 1
mem --path ./memory resolve
mem --path ./memory sync
```

### 5.3 冲突恢复

```bash
# 1) sync 提示冲突后，先看看到底哪些文件冲突
mem --path ./memory sync
git -C ./memory status --porcelain

# 2) 自动解决 JSONL 冲突（会去重、排序、保留不可解析行）
mem --path ./memory resolve

# 3) 如果还有非 JSONL 文件冲突（例如 meta.json），手工解决后 git add
#    然后继续 rebase；否则 resolve 的 rebase --continue 会失败
git -C ./memory add projects/<project>/meta.json
git -C ./memory rebase --continue

# 4) 再同步
mem --path ./memory sync
```

### 5.4 从零接入已有项目

推荐把 store 作为一个**独立的 Git 仓库**（与代码仓库分开），记录代码仓库里的文件：

```bash
# 1) 在任意位置建立 store（它是独立的同步单元）
mem init --path ~/acdm-memory --remote git@github.com:you/acdm-memory.git

# 2) 在代码仓库里照常开发，用 add 记录改动
#    文件在 store 外时，file_name 会规范化为「相对于最近 git 仓库根」的路径，
#    因此不同设备/不同挂载点下保持同一身份
cd ~/src/my-project
mem --path ~/acdm-memory add -p my-project -s "add parser" -d laptop -a claude src/parser.rs

# 3) 查询与渲染
mem --path ~/acdm-memory query -p my-project
mem --path ~/acdm-memory render -p my-project
```

如果想设置 `meta.json` 里的 `github_url` / `description`，可手工编辑 `projects/<project>/meta.json` 后再 `render`。

### 5.5 脚本调用

- 用退出码判断结果：`0` 成功；`1` 运行时错误；`2` 参数错误。`sync` 冲突时返回 1，可直接据此进入 `mem resolve` 流程。
- 命令输出面向人类阅读，**没有 `--json` 之类的机器可读输出模式**；脚本若需结构化数据，建议直接读 store 内的 `events/*.jsonl` 与 `projects/<p>/files.jsonl`（格式见 [README.md](../../README.zh-CN.md#数据结构)）。
- 错误信息走 stderr，正常结果走 stdout；解析时分开处理（例如 `query --fuzzy` 非法正则会先输出 stdout 标题再在 stderr 报错）。

## 6. 相关文档

- [MCP.md](MCP.md) —— `mem-mcp` 的 JSON-RPC 协议、8 个工具与客户端配置
- [ARCHITECTURE.md](ARCHITECTURE.md) —— 组件分层、读写路径、同步与并发
- [DESIGN.md](DESIGN.md) —— 为什么这样设计（Git + JSONL 取舍、一致性模型、冲突语义）
- [MIGRATION.md](MIGRATION.md) —— 旧时间戳格式、旧绝对路径记录等历史数据问题
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) —— 常见故障与恢复步骤
- [README.md](../../README.zh-CN.md) —— 项目总览、数据格式与快速开始
