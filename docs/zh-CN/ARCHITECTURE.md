[English](../ARCHITECTURE.md) · **简体中文**

# ACDM 架构

> 读者：想理解 ACDM 如何工作、准备贡献代码或排查存储/同步问题的开发者。
> 本文描述当前源码（v0.1.2）的实际行为；为什么这样设计见 [DESIGN.md](DESIGN.md)，命令用法见 [CLI.md](CLI.md) 与 [MCP.md](MCP.md)。

## 1. 设计目标与约束

ACDM 要在多台机器、多个 AI Agent 之间共享「项目记忆」。它不追求强一致或实时，而是把正确性押在**可读的 append-only 日志 + Git 传输**上。

目标：

- **跨设备**：同一项目在不同机器上看到同一份事件历史；
- **离线可用**：没有网络也能读写本地 store；
- **无服务端**：不需要数据库或后端进程，Git 仓库就是传输层；
- **可审查**：所有数据都是文本 JSONL，可 diff、可人工阅读；
- **多入口**：CLI（人/脚本）与 MCP（Agent）共享同一套核心逻辑；
- **不丢数据**：损坏行、未知字段、冲突行都不能被静默丢弃。

约束（有意为之）：

- 只支持「各设备本地写 + 周期性同步」的弱一致模型，不提供跨设备事务；
- Git 是唯一传输层，没有内置网络协议；
- 文件内容不入库，只记录元数据（路径 / 大小 / SHA256）；
- 单机写操作由 store 跨进程写锁串行，读路径无锁；
- 入口在接触文件系统前做输入校验：项目名必须是单一安全路径组件（0.1.2 起还拒绝 Windows 保留名与尾点，见 5.3）；
- 单机规模假设：适合仓库级 store（数千到数万条事件）；没有持久化索引，每次查询重建；
- 时间排序依赖设备时钟（见 [DESIGN.md](DESIGN.md) 的一致性模型一节）。

## 2. 组件与分层

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

三个 crate 的职责与依赖方向：

| Crate | 类型 | 职责 | 依赖 |
| --- | --- | --- | --- |
| `mem-core` | 库 | 数据结构、ID、存储、索引、Git、冲突解决、渲染、压缩、通用工具 | 不依赖二进制；`git` 通过子进程调用 |
| `mem-cli` | 可执行文件 `mem` | 8 个子命令：`init` / `add` / `query` / `list` / `compact` / `sync` / `resolve` / `render` | `mem-core`、`clap` |
| `mem-mcp` | 可执行文件 `mem-mcp` | stdio MCP server（JSON-RPC 2.0），暴露 8 个工具 | `mem-core`、`serde`、`serde_json` |

入口只有一个方向：`mem-cli` / `mem-mcp` → `mem-core`；`mem-core` 不认识任何入口。两个入口都通过 [`resolve_store_root`](../../mem-core/src/util.rs) 定位 store，并共享同一套时间戳、ID 与路径规范化函数，避免「CLI 和 MCP 行为不一致」。

`mem-mcp` 是 stdio 上的换行分隔 JSON-RPC 2.0 server：它只接受 `jsonrpc: "2.0"`，并把两类失败分开——协议错误返回 JSON-RPC error（`-32700` 非法 JSON、`-32600` 非法请求、`-32601` 未知工具/方法、`-32602` 参数不合法），工具级业务失败则返回**成功响应 + `result.isError = true`**，便于 Agent 读取错误并重试。单行请求上限 16 MiB：超限行会被流式丢弃并返回 `-32600`，不会拖垮 server，也不会让输入流失步。

## 3. 写路径

### 3.1 `mem add`（CLI）

入口在 [`mem-cli/src/main.rs`](../../mem-cli/src/main.rs) 的 `Commands::Add` 分支：

1. clap 解析全局 `--path` 与 `add` 参数；[`validate_project_name`](../../mem-core/src/storage.rs) 在任何 store 操作前拒绝不安全的项目名；[`resolve_store_root`](../../mem-core/src/util.rs) 得到 store 根目录。
2. 构造 `MemStore`，用 `get_latest_files(project)` 取该项目当前的文件最新状态（用于判断 `Created` / `Modified`）。
3. 对每个待记录文件：

   | 步骤 | 行为 |
   | --- | --- |
   | 路径规范化 | [`normalize_file_name(file_path, &store.root)`](../../mem-core/src/util.rs)；若结果仍是绝对路径（既不在 store、也不在任何 git 仓库、也不在 CWD 之下），向 stderr 打一条警告 |
   | 存在性 | 用**用户传入的原始路径**判断，不用规范化名 |
   | 不存在 | `action = Deleted`，`size_bytes = 0`，`hash = ""` |
   | 存在 | [`hash_file(path)`](../../mem-core/src/models.rs) 以 64 KiB 分块流式计算 → `size_bytes = 字节数`，`hash` = SHA256 十六进制；文件不会整体读入内存 |
   | action | 规范化名在「最新文件记录」里已存在 → `Modified`，否则 `Created` |
   | file_id | [`generate_id()`](../../mem-core/src/id_gen.rs) 生成 |

4. 生成事件字段：`ts = now_rfc3339()`、`event_id = generate_id()`、`device` 取 `-d/--device` 或 [`default_device_name()`](../../mem-core/src/util.rs)、`agent` 由 clap 缺省为 `human`。
5. 若 `read_project_meta(project)` 失败（`meta.json` 缺失或不可读），写一份最小 `meta.json`（`description = "Project <name>"`，`created_at = ts`）。这里用「meta 是否可读」而不是「目录是否存在」判断，避免 `mem render` 先创建了 `projects/<name>/` 目录、导致 `add` 误以为项目已完整初始化。
6. 调用 [`MemStore::append_event`](../../mem-core/src/storage.rs) 落盘（见 3.4）。

### 3.2 `mem_add_event`（MCP）

入口在 [`mem-mcp/src/main.rs`](../../mem-mcp/src/main.rs) 的 `tools/call` 分发与 `handle_add_event`：

1. 从 JSON-RPC `params.arguments` 反序列化 `AddEventArgs`：`project`、`summary` 必填；`device`、`agent`、`files` 可选。
2. 缺省值：`device = default_device_name()`，`agent = "mcp-agent"`；每个文件 `action = "Modified"`、`size_bytes = 0`、`hash = ""`。
3. 每个 `file_name` 同样经 `normalize_file_name` 规范化（MCP 客户端常传绝对路径），`file_id` 由 `generate_id()` 生成。
4. 项目元数据检查用 `read_project_meta(&project).is_err()` 而不是目录存在性——这样即使 `mem render` 先创建了目录、尚未写 `meta.json`，也会补写元数据。
5. `append_event` 落盘，返回文本：「`Event <event_id> recorded with <n> files`」。

### 3.3 `append_event` 的落盘细节

[`MemStore::append_event`](../../mem-core/src/storage.rs) 是唯一的写事件入口：

1. 校验项目名，然后取 **store 写锁**，并在下面两次追加期间一直持有。
2. 计算分片名：`ts` 长度 ≥ 7 且第 5 个字节是 `-` → 取 `ts[..7]`（即 `YYYY-MM`）；否则退化为 `default`。
3. 确保 `events/` 存在，以 `create + append` 打开 `events/YYYY-MM.jsonl`。
4. `fs2` 排他锁 → `serde_json::to_string(event)` 序列化为**单行** JSON → `writeln!` 追加 → `flush` → 解锁。
5. 若 `event.files` 非空：确保 `projects/<project>/` 存在，以同样方式打开 `projects/<project>/files.jsonl`，排他锁下**每条 `FileRecord` 写一行**，`flush` 后解锁。

两条追加（事件日志 + 文件视图）不在同一个事务里：进程若在两次追加之间崩溃，会出现「事件已写入、`files.jsonl` 未更新」的状态。`events/*.jsonl` 是完整历史（source of truth），`files.jsonl` 是可从事件流重放重建的派生投影；[`mem compact`](../../mem-core/src/compact.rs) 会从事件日志重建它（见 6.4），因此这一状态可以自愈。

### 3.4 部分写入与容错

- 追加写失败/崩溃可能留下被截断的末行；读取端对每行独立解析，解析失败的行走「跳过」而非整体报错（`read_events_for_month` 等）。
- `mem resolve` 会把这类无法解析的行走字保留（见第 7 节），不会因为一次截断写就丢数据。
- 写入只 `flush` 不 `fsync`：断电可能丢最后一条内存中的追加，但不会破坏已有内容。

## 4. 读路径

### 4.1 `MemIndex` 构建

[`MemIndex::build`](../../mem-core/src/index.rs) 是 `query` 与 MCP `mem_query` 的公共基础，每次查询时**重新构建**：

1. 列出 `events/` 下所有 `.jsonl` 文件并排序（不限于月份分片）。
2. 对每个文件用 `fs::read` 整体读取（普通缓冲 I/O，不做内存映射，因此并发截断不会触发 `SIGBUS`），按 `\n` 切行，同时累计字节偏移：`offset += line_len + 1`。
3. 每个能解析为 `Event` 的行构造 `EventPointer { event_id, file_path, byte_offset, ts, project }`。
4. 填充三个索引维度：

   | 维度 | 类型 | 用途 |
   | --- | --- | --- |
   | `by_project` | `HashMap<String, Vec<EventPointer>>` | 按项目列出全部事件 |
   | `by_file_id` | `BTreeMap<String, EventPointer>` | 按 `file_id` 精确查找到所属事件（同 ID 后写覆盖） |
   | `by_filename` | `HashMap<String, Vec<EventPointer>>` | 按 `file_name` 找出所有涉及该文件的事件 |

### 4.2 单事件读取

`index.read_event(pointer)`：打开 `pointer.file_path`，`seek` 到 `pointer.byte_offset`，`BufReader::read_line` 读一行再反序列化。索引只存偏移量，不缓存事件正文，因此内存占用与事件数量成正比、与事件正文总大小基本无关。

### 4.3 正则模糊搜索

`MemIndex::fuzzy_search(store, pattern)`：`RegexBuilder` 编译一次，并设置 **10 MiB 的编译程序尺寸上限**（防止 Agent 提供的病态正则耗尽内存）；按文件名排序扫描所有 `events/*.jsonl`，对每个非空行先 UTF-8 解码、再用正则匹配**原始 JSON 行文本**，命中后解析为 `Event`。匹配语义是正则而非分词，行内任何字段（包括 `summary`、`project`、路径）都可能命中。

### 4.4 最近事件：`read_latest_events_for_project`

[`MemStore::read_latest_events_for_project`](../../mem-core/src/storage.rs) 专为「最近 N 条」设计：

1. `month_files()` 列出 `events/` 下的月份分片（`is_month_shard`：文件名主干必须是 `YYYY-MM` 且为 7 个字符、数字+连字符结构），升序排列。
2. **从最新月份向最旧月份倒序遍历**。
3. 每个月内：`read_events_for_month` 读入并过滤出该 `project` 的事件 → `sort_events`（parse-aware 时间排序）→ 从后往前取，追加到结果。
4. 凑满 `n` 条即提前停止；最后整体 `reverse()`，返回**最旧在前、最新在后**的顺序。

这个算法保证：跨月时不会因为「旧月份文件字典序靠前」而漏掉新月份事件；月内即使混有旧时间戳格式也能按真实时间排序。

### 4.5 文件最新状态

`read_project_files(project)` 读取 `projects/<project>/files.jsonl` 的全部记录；`get_latest_files(project)` 用 `BTreeMap<file_name, FileRecord>` 逐条 `insert`（同 `file_name` 后写覆盖），最终按 `file_name` 字典序返回。`list -p`、`render`、`compact` 都基于它。

### 4.6 CLI / MCP 查询映射

| 查询入口 | 实现路径 |
| --- | --- |
| `mem query -p <project>` / MCP `by=project` | `MemIndex.by_project` → 逐个 `read_event` |
| `mem query --file-id <id>` / MCP `by=file_id` | `MemIndex.by_file_id` → 定位事件后找到对应 `FileRecord` |
| `mem query -n <filename>` / MCP `by=filename` | `MemIndex.by_filename`（按键精确匹配，需传规范化后的相对路径） |
| `mem query --fuzzy <regex>` / MCP `by=fuzzy` | `MemIndex::fuzzy_search` |
| `mem list` | `list_projects()`（`ignore::WalkBuilder` 深度 1，尊重 `.gitignore`）+ 每个项目的 `meta.json` 与最新文件数 |
| `mem list -p` | `get_latest_files` |
| MCP `mem_get_context` | `read_project_meta` + 读取 `projects/<name>/README.md` + `read_latest_events_for_project(project, 5)` |

## 5. 存储格式规范

### 5.1 目录树

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

### 5.2 事件分片

- 文件名：`events/YYYY-MM.jsonl`，`YYYY-MM` 取自事件时间戳（UTC）。
- 每行一个 `Event` 的紧凑 JSON（`serde_json::to_string`，无缩进），行尾 `\n`。
- 分片只追加；历史月份不会因为新事件而被重写。
- 边界情况：`ts` 格式异常时写 `events/default.jsonl`；月份分片读取路径只认 `YYYY-MM`，而 `MemIndex` / `read_all_events` 会扫描 `events/` 下所有 `.jsonl`。

### 5.3 数据结构

`Event`（一行 = 一次记录）：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `event_id` | string | 是 | `YYYYMMDDHHMMSS_nanos9_pid5_counter4`，UTC |
| `ts` | string | 是 | 固定宽度 UTC 纳秒时间戳（30 字符） |
| `device` | string | 是 | 设备名；CLI/MCP 缺省取 hostname |
| `agent` | string | 是 | 记录者；CLI 缺省 `human`，MCP 缺省 `mcp-agent` |
| `project` | string | 是 | 项目名（同时用作 `projects/<name>/` 目录名） |
| `summary` | string | 是 | 变更摘要 |
| `files` | `FileRecord[]` | 是（可为空数组） | 本次事件关联的文件记录 |

`FileRecord`：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `file_id` | string | 是 | 与 `event_id` 同格式的 ID |
| `file_name` | string | 是 | 规范化后的 store / git 仓库相对路径，`/` 分隔 |
| `action` | string | 是 | `Created` / `Modified` / `Deleted` |
| `size_bytes` | number | 是 | 文件字节数；`Deleted` 为 `0` |
| `hash` | string | 是 | SHA256 十六进制；`Deleted` 为空字符串 |

`ProjectMeta`（`projects/<name>/meta.json`）：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | 项目名 |
| `description` | string | 是 | 项目描述 |
| `github_url` | string \| null | 否（可省略或为 `null`） | 关联仓库地址 |
| `created_at` | string | 是 | 创建时间（固定宽度 UTC 纳秒） |

每个 `project` 值（以及 CLI/MCP 的项目名参数）在用作路径之前都由 [`validate_project_name`](../../mem-core/src/storage.rs) 校验：非空、恰好一个普通路径组件（拒绝 `.`/`..`/绝对路径）、不含 `/`、`\` 或控制字符、最多 200 字节，防止项目名逃出 `projects/`。0.1.2 起还拒绝以 `.` 结尾的名字与 Windows 保留设备名（`CON` / `PRN` / `AUX` / `NUL` / `COM1`–`COM9` / `LPT1`–`LPT9`，大小写不敏感，`con.txt` 这类带扩展名的形式同样被拒绝），以及 Windows 非法字符 `<>:"|?*`（错误信息含 "would be invalid on Windows"）。store 会在不同平台之间同步，Windows 无法创建或 checkout 这些目录/名字。

### 5.4 `.mem` 标记文件

`mem init` / `MemStore::init` 在 store 根写入（已存在则不覆盖）：

```json
{
  "format": 1,
  "created_at": "2026-10-22T13:02:45.123456789Z"
}
```

作用：`is_store_root(dir)` 通过「存在 `.mem` 文件」或「存在 `events/` 目录」判断 store 根，[`find_store_root`](../../mem-core/src/util.rs) 据此从任意子目录向上发现 store。CLI 与 MCP 使用同一规则，所以 MCP 客户端从任意工作目录启动都能找到最近的 store。

0.1.2 起 `.mem` 还兼作 store 跨进程写锁的目标：写操作只读打开该文件并 `flock`，因此加锁不会修改它、不会弄脏 Git 仓库，也不会在共享机器上被其他用户阻塞（见 6.1）。

### 5.5 `.gitignore`

`init` 时若 store 根尚无 `.gitignore`，写入默认内容：

```
target/
*.swp
*.tmp
*.tmp.*
*~
.DS_Store
```

`*.tmp` 覆盖 `compact` 使用的 `files.jsonl.tmp` 临时文件，避免进程被杀后残留文件被提交。

### 5.6 ID 与时间戳

**ID 格式**（[`id_gen.rs`](../../mem-core/src/id_gen.rs)）：

```
YYYYMMDDHHMMSS_<nanos:09>_<pid%100000:05>_<counter%10000:04>
```

| 部分 | 宽度 | 来源 | 作用 |
| --- | --- | --- | --- |
| 日期时间 | 14 | `Utc::now().format("%Y%m%d%H%M%S")` | UTC，跨时区可排序 |
| 纳秒 | 9 | `timestamp_subsec_nanos()` | 同秒内排序 + 跨设备去重 |
| pid | 5 | `process::id() % 100_000` | 区分同机不同进程 |
| 计数器 | 4 | 由（启动时间, pid）派生的进程内计数器种子，逐次自增后 `% 10_000` | 同进程突发去重 |

定宽 + 高位时间 → 字典序即时间序。计数器不再固定从 0 开始，而是以「启动时间 + pid」派生一个随机偏移起步，因此两个 pid 相同的进程（容器里常见 pid 恒为 1）即使在同一纳秒生成 ID，也更难撞上相同计数器；ID 格式不变。`event_id` 内嵌时间与事件 `ts` 是两次独立取时，可能相差数百纳秒；排序以 `ts` 为准。

**时间戳格式**（[`util.rs`](../../mem-core/src/util.rs)）：

```
YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ     # 恰好 30 字符，UTC
```

- 由 `now_rfc3339()`（`to_rfc3339_opts(SecondsFormat::Nanos, true)`）生成；
- 读取端 `parse_ts()` 兼容历史格式：纳秒 `+00:00`、秒级 `Z`；
- 排序用 `ts_cmp()`：先按解析后的时间比较，解析失败再退回字符串比较；无法解析的时间戳排在可解析记录之后；`event_cmp` 用 `event_id` 作为同时间戳的稳定 tie-breaker。

### 5.7 `file_name` 规范化

[`normalize_file_name`](../../mem-core/src/util.rs) 把路径变成跨设备稳定的身份，优先级：

| 优先级 | 规则 | 适用场景 |
| --- | --- | --- |
| 1 | 相对 **store 根**（如 `src/index.rs`） | 文件就在 store 目录树内 |
| 2 | 相对**最近的 git 仓库根** | 文件在项目仓库里，store 在别处（最常见） |
| 3 | 相对**当前工作目录** | 既不在 store 也不在 git 仓库，但 CWD 可用 |
| 4 | **绝对路径**（CLI 会警告） | 以上都不满足，跨设备无法匹配 |

实现细节：

- 先做词法绝对化与 `.` / `..` 解析，再对**已存在的最长前缀**做 `fs::canonicalize`（解析符号链接），不存在的尾部按词法拼接（被删除的文件也保持确定的身份）；
- 每次 `strip_prefix` 比较的两侧都做同样的规范化，因此经由软链访问的 store 与真实路径得到相同的相对身份；
- 组件统一用 `/`（Windows 的 `\` 会被替换）；
- 非 UTF-8 路径经 `to_string_lossy` 处理，可能有损。

### 5.8 项目 README 生成规则

[`render_project_readme`](../../mem-core/src/render.rs) 输出固定结构：

```markdown
# <project>

<description>

### Metadata
- **Created At:** <meta.created_at>
- **GitHub:** <github_url>           # when github_url is present

## Tracked Files

| File Name | Action | Size (bytes) | Hash (SHA256) |
| --- | --- | --- | --- |
| `<file_name>` | <action> | <size_bytes> | `<hash>` |

## Recent Activity Timeline

- **<ts>** [`<agent>` on `<device>`]: <summary>
```

- 元数据缺失时用 `Project <name>` 兜底，`created_at` 取当前时间；
- 文件表来自 `get_latest_files`（每个 `file_name` 最新一条）；
- 时间线取最近 10 条事件、最新在前；
- 所有插值（项目名、描述、`created_at`、`github_url` 标签、文件表单元格、时间线字段）写入前都会转义：`|` → `\|`，CR/LF 折叠为空格，记录的数据无法破坏 Markdown 表格/时间线结构或注入内容；
- `write_project_readme` 以 `create + truncate + 排他锁` 整体重写，不是只追加；整个「渲染 + 写入」过程持有 store 跨进程写锁，避免在 `pull --rebase` 期间写脏工作区。

## 6. 并发与一致性

### 6.1 同机并发

| 操作 | 锁 | 说明 |
| --- | --- | --- |
| 所有 store 写操作：`MemStore::init`、`append_event`、`compact`、`resolve`、`sync`、`write_project_meta`、`write_project_readme` | store 跨进程写锁（`fs2`） | 有 `.mem` 标记时以标记文件为锁目标：**只读打开 + `flock`**，不写临时文件、不弄脏 Git 仓库，锁权限自然继承 store 文件权限；仅在还没有标记的首次 `init` 期间回退到按用户隔离的 `0600` 锁文件（放在 `$XDG_RUNTIME_DIR` 或系统临时目录，修复多用户共享机器上的 `EACCES`） |
| `append_event`（事件行） | 额外 `fs2` 排他锁 | 打开 `events/<ym>.jsonl` 后加锁，写一行、`flush`、解锁 |
| `append_event`（文件行） | 额外 `fs2` 排他锁 | 对 `files.jsonl` 同样处理 |
| `resolve` 写回 | 额外 `fs2` 排他锁 | 重新整文件覆盖后 `git add` |

store 跨进程写锁让**同一台机器上的多文件操作**在进程之间串行：追加不会与投影重建、冲突解决或 rebase 交错。`fs2` 锁是**建议锁（advisory）**，不跨设备生效；跨设备顺序由 Git 同步与 `mem resolve` 收敛。

### 6.2 读侧

- 读路径**不加锁**；每次读取都重新打开文件并用普通 `fs::read` 读取，不存在长期持有的映射或句柄。
- 读路径不使用 `mmap`：并发的 `resolve` / `pull` 可能随时截断或替换文件，读被截断文件的映射是未定义行为（可能触发 `SIGBUS`），普通读取始终安全。
- 读端逐行容错：某行非法 JSON 就跳过该行；单行损坏不会让 store 不可读。
- 空文件被显式处理（直接返回空结果）。

### 6.3 跨设备一致性

- 各设备独立写本地 store，**同步前互不可见**；一致性只在 `mem sync` 成功后达成（弱一致）。
- 没有跨设备锁：两台设备同时修改同一 store 由 Git 的 rebase 与 `mem resolve` 合并（见第 7 节）。
- 语义上是「事件日志 + 最近状态」：事件按时间近似排序，冲突记录同一 ID 时后写覆盖（last-write-wins），不是字段级合并。

### 6.4 `compact` 的投影重建

[`compact_project_files`](../../mem-core/src/compact.rs) 从事件日志重建 `files.jsonl`，同时完成压缩与自愈：

1. 校验项目名，然后取 store 写锁，并一直持有到下面的 rename。
2. 统计现有 `files.jsonl` 的非空行数作为 `records_before`（文件不存在时为 0）。
3. 读取权威事件日志（`read_all_events`），筛出本项目的全部事件，用 `sort_events` 按因果/时间顺序排序。
4. 按事件顺序把每条 `FileRecord` 写入 `BTreeMap<file_name, FileRecord>`——同一 `file_name` 后写覆盖先写。
5. 若既无投影又无任何事件，返回 0/0 报告，不创建目录。
6. 把投影写入 `files.jsonl.tmp`，`flush` + `sync_all` 后 `fs::rename` 覆盖 `files.jsonl`——同一文件系统内原子替换，读端要么看到旧完整文件、要么看到新完整文件。

整个「读取 → 重放 → 替换」序列持有 store 写锁，因此并发追加不会再在读与 rename 之间被覆盖丢失；重建来源是事件日志，所以崩溃留下的投影缺失或截断也能被修复。`Records before / after / pruned` 输出保持不变。

## 7. 同步与冲突解决

### 7.1 同步流程

**CLI `mem sync`**（[`mem-cli/src/main.rs`](../../mem-cli/src/main.rs)）**与 MCP `mem_sync`**（[`mem-mcp/src/main.rs`](../../mem-mcp/src/main.rs)）**共用 [`sync_store`](../../mem-core/src/git.rs)**，流程与保证一致：

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

差异只在入口层：

- 提交信息：CLI 与 MCP 共用默认消息 `Auto-sync from <device> at <ts>`（`<device>` / `<ts>` 在运行时生成）；MCP 可用 `message` 参数覆盖；
- 冲突/失败：CLI 打印步骤 + stderr 警告，`main` 以非零退出码结束；MCP 把同样的错误文本包装成 `result.isError = true` 的成功响应；
- 打印/返回的步骤文本相同（clean/committed、pulled、pushed 等）。

[`git_has_conflicts`](../../mem-core/src/git.rs) 只依据 Git 自身的记录：`git ls-files -u` 的未合并文件、`git status --porcelain` 的冲突状态码（`UU` / `AA` / `UD` / `DU` / `DD` / `AU` / `UA`）、以及 rebase/merge 状态目录（`rebase-merge` / `rebase-apply` / `MERGE_HEAD`）。它**不再扫描文件内容**找 `<<<<<<<`：事件摘要或文档里恰好引用该字符串不会造成永久误判，也省掉 O(仓库大小) 的扫描。

### 7.2 JSONL 冲突的形态

Git 对文本行做三方合并，两个分片都追加了不同事件时，冲突块形如：

```
{"event_id":"...","ts":"...","summary":"base","files":[]}
<<<<<<< HEAD
{"event_id":"...","ts":"...","summary":"ours","files":[]}
=======
{"event_id":"...","ts":"...","summary":"theirs","files":[]}
{"event_id":"...","ts":"...","summary":"theirs-2","files":[]}
>>>>>>> origin/main
```

因为是 append-only 文件，冲突通常是「同 ID 的重复行」或「两侧新增行交错」，适合用「去重 + 排序 + 保留」策略自动解决，而不需要语义级合并。

### 7.3 `mem resolve` 算法

[`resolve_conflicts`](../../mem-core/src/resolve.rs) 的步骤：

1. 取 store 写锁并全程持有；随后进入循环：递归扫描 store 下所有 `.jsonl`（`WalkBuilder::hidden(false)`，包含隐藏文件），按路径排序。
2. 只处理包含 `<<<<<<<` 字节的文件；以**字节**方式读入全文，不再要求合法 UTF-8。
3. 逐行去除**精确形态**的冲突标记：`=======` 必须整行恰好是七个 `=`；`<<<<<<<` / `|||||||` / `>>>>>>>` 之后只能是行尾或一个空格（可带 label）。以标记前缀开头的普通数据行不算标记、会被保留；同时统计冲突块数，其余非空行保留**原始字节**（含首尾空白与 CRLF）。
4. 按「第一条可解析行」判定文件类型：`Event` → `FileRecord` → 通用 JSON（`Generic`）→ `Unknown`。
5. 对每条数据行分类：

   | 类型 | 解析成功 | 解析失败 |
   | --- | --- | --- |
   | `Event` | 以 `event_id` 为去重键、`ts` 为排序时间 | 逐字节保留 |
   | `FileRecord` | 以 `file_id` 为去重键，保持因果顺序 | 逐字节保留 |
   | `Generic` | 以 `event_id` / `file_id` / `id` 之一为键（缺省用整行） | 逐字节保留 |
   | `Unknown` | — | 全部逐字节保留 |

6. 去重：同 key 后出现者覆盖先出现者（last-write-wins）；统计被去重条数。`FileRecord` 不做重排：只保留每个 key 最后一次出现的位置，避免打乱历史顺序、让 `files.jsonl` 的 last-write-wins 被旧状态覆盖。
7. 排序：`Event` / `Generic` 按 `ts_cmp` + 键排序；`Unknown` 不排序。
8. 无法解析的行（含相同内容的重复行折叠一次）**追加在文件末尾**。
9. 以排他锁 `truncate` 覆盖写回。
10. 若 store 根存在 `.git`：`git add -- <相对路径>` 把解决后的文件暂存。**`git add` 失败是硬错误**：rebase 中继续执行会让后续 `git rebase --continue` 失败并留下半完成状态，所以直接返回错误、不继续推进。
11. rebase 状态通过 Git 自身解析（`git rev-parse --git-path`）：普通仓库是 `.git/rebase-merge` / `.git/rebase-apply`；`git worktree` / submodule 中 `.git` 是**文件**，状态在 per-worktree 的 git 目录里。存在 rebase 时执行 `git rebase --continue`（`GIT_EDITOR=true` 防止阻塞），然后回到第 1 步继续下一轮；一次调用最多循环 100 轮，直到 rebase 结束。若剩余冲突包含非 JSONL 文件，则停止并打印可操作的错误（列出非 JSONL 文件与所有未解决路径）。
12. 若 `git rebase --continue` 发现当前提交是**空补丁**（要应用的改动已经在目标分支中，常见于多轮冲突解决），自动执行 `git rebase --skip` 丢弃该提交并继续，而不是把空补丁误报为失败。

`ResolveReport` 汇报：`resolved_files`、`total_conflicts_resolved`、`deduplicated_items`、`lines_preserved`。

### 7.4 为什么用 Git 状态目录判断 rebase

旧实现靠匹配 `git rebase --continue` 的报错文本（英文 `no rebase in progress`）来决定是否跳过，这在非英文 locale 下会失效，把「没有 rebase」误判为硬错误。现在改为检查 Git 自己的状态目录（通过 `git rev-parse --git-path` 解析，普通仓库与 `git worktree` / submodule 的 gitfile 都适用）：

- `.git/rebase-merge/`（交互式/普通 rebase 进行中）
- `.git/rebase-apply/`（`am` / 非交互式 rebase 进行中）

这是 Git 的内部状态，与语言环境无关，判断严格且稳定。

### 7.5 冲突解决之后的流程

1. `mem resolve` 写回 JSONL 并 `git add`；若 rebase 仍在进行，会在同一次调用里循环 `git rebase --continue` 推进剩余提交（上限 100 轮）。只有非 JSONL 文件仍冲突时才需要手工处理并在 `git add` 后继续 rebase。
2. 用户再执行一次 `mem sync`：若还有未提交改动会提交，随后 pull/push；若工作区干净则直接 pull/push。冲突会以非零退出码结束，不再被当作成功。
3. 冲突期间**不要**手工编辑 JSONL 或运行 `git checkout`，让 `mem resolve` 统一处理（否则可能把冲突标记重新引入或丢行）。

## 8. 性能特征与限制

### 8.1 复杂度

| 操作 | 时间 | 内存 | 说明 |
| --- | --- | --- | --- |
| `append_event` | O(1) | O(事件大小) | 加锁 + 单行追加；不 fsync |
| `MemIndex::build` | O(事件总字节数) | O(索引条目数) | 每次 `query` / MCP `mem_query` 都重建 |
| `read_event` | O(该行长度) | O(事件大小) | `seek` 到字节偏移后读一行 |
| `fuzzy_search` | O(事件总字节数) | O(命中事件数) | 正则扫所有行 |
| `read_latest_events_for_project` | 最坏 O(事件总字节数) | O(读入月份的记录) | 从最新月份倒查，凑满即停 |
| `get_latest_files` | O(`files.jsonl` 行数) | O(不同文件数) | `BTreeMap` 去重 |
| `compact` | O(事件总数) | O(不同文件数) | 从事件日志重放 + 临时文件 + rename；持 store 写锁 |
| `list_projects` | O(项目数) | O(项目数) | `ignore` 深度 1 遍历 |

### 8.2 规模假设

- 目标场景是**项目级记忆**：数千到数万条事件、几百到几千个文件记录。
- `MemIndex` 每次查询全量扫描：事件量到十万级时，单次查询会明显变慢（具体耗时视事件量、磁盘与 CPU 而定）。
- Git 同步成本随仓库体积增长：事件只追加、不归档，长期使用需要未来提供归档能力。
- 单文件大小：`events/` 按月分片，避免单文件无限增长；`files.jsonl` 由 `compact` 控制。

### 8.3 其它限制

- **无持久化索引/缓存**：每次查询重建，进程退出即丢；
- **`add` 流式计算哈希**：CLI 以 64 KiB 分块读取文件计算 SHA256，内存占用 O(1)，但超大文件仍会消耗等量 I/O；MCP 由调用方提供 `size_bytes` / `hash`，不读内容；
- **`resolve` 按字节处理**：不再要求合法 UTF-8，非 UTF-8 字节逐字保留；但 `git add` 的路径仍以 lossy 形式传给 Git；
- **没有 mmap**：读取用普通 `fs::read`，避免并发截断导致的未定义行为；代价是每个分片每次读取都要完整读入内存（分片按月切分，单文件大小可控）；
- **资源上限**：MCP 单行请求 16 MiB、fuzzy 正则编译程序 10 MiB，防止超大输入耗尽资源；
- **没有 fsync**：依赖 OS 页缓存与 Git 的提交语义，适合「记忆」而非金融账本。

## 9. 扩展点

### 9.1 新增事件字段

- 读取端用 serde 反序列化结构体，未知字段默认被**忽略**，所以旧代码读新数据不会报错；
- `resolve` 重新输出的是原始行文本，未知字段、字段顺序、格式在冲突解决后**原样保留**；
- 向结构体新增字段时建议加 `#[serde(default)]`，保证旧数据仍可读；
- 若要严格拒绝未知字段需要显式 `deny_unknown_fields`（当前未启用，属于有意的向前兼容）。

### 9.2 新增 MCP 工具

1. 在 [`get_tools_list`](../../mem-mcp/src/main.rs) 中加 `name` / `description` / `inputSchema`；
2. 在 `handle_request` 的 `tools/call` 分发里加一个分支，把 `arguments` 反序列化成参数结构体，调用 `mem-core` 的公开函数；
3. 返回 `mcp_text_content(...)` 包装的文本结果；参数反序列化失败返回 `-32602`，handler 业务失败经 `Err(String)` 转成**成功响应 + `result.isError = true`**（`-32603` 保留给服务端故障，不再用于业务错误）。

### 9.3 新增 CLI 子命令

在 [`Commands`](../../mem-cli/src/main.rs) 枚举中加变体与参数，在 `run()` 的 `match` 中实现；store 定位、时间戳、设备名、哈希等一律复用 `mem-core`，不要在 CLI 内重新实现。

### 9.4 增量索引

当前 `MemIndex` 是「每次全量重建」。可扩展方向：

- 持久化 `EventPointer` 表（如 `events/.index.json`），`append_event` 后增量更新；
- 按文件 `(size, mtime)` 缓存每个分片的索引片段；
- 查询前只重扫变化的文件。

### 9.5 其它方向

- 事件归档 / 快照（把老事件压缩成项目级 summary 后移出活跃分片）；
- 内容寻址（把文件内容以 SHA256 blob 形式存入，用于跨设备回放）；
- 可选的字段级合并策略（目前同 ID 整体后写覆盖）；
- 更多传输方式（当前有意只支持 Git）。

## 10. 源码索引

| 文件 | 职责 |
| --- | --- |
| [`mem-core/src/lib.rs`](../../mem-core/src/lib.rs) | crate 导出面 |
| [`mem-core/src/models.rs`](../../mem-core/src/models.rs) | `Event` / `FileRecord` / `ProjectMeta`、SHA256 |
| [`mem-core/src/id_gen.rs`](../../mem-core/src/id_gen.rs) | 跨设备唯一 ID 生成 |
| [`mem-core/src/util.rs`](../../mem-core/src/util.rs) | 时间戳、store 定位、文件身份规范化 |
| [`mem-core/src/storage.rs`](../../mem-core/src/storage.rs) | `MemStore`：init / append / 读取 / 月份分片 |
| [`mem-core/src/index.rs`](../../mem-core/src/index.rs) | `MemIndex`、`EventPointer`、正则模糊搜索 |
| [`mem-core/src/git.rs`](../../mem-core/src/git.rs) | git 子进程封装 |
| [`mem-core/src/resolve.rs`](../../mem-core/src/resolve.rs) | JSONL 冲突解决 |
| [`mem-core/src/render.rs`](../../mem-core/src/render.rs) | 项目 README 渲染 |
| [`mem-core/src/compact.rs`](../../mem-core/src/compact.rs) | `files.jsonl` 压缩 |
| [`mem-cli/src/main.rs`](../../mem-cli/src/main.rs) | CLI 8 个子命令 |
| [`mem-mcp/src/main.rs`](../../mem-mcp/src/main.rs) | MCP server 与 8 个工具 |
