[English](../MCP.md) · **简体中文**

# `mem-mcp` MCP 参考

> 读者：要把 ACDM 接入 Claude Desktop、Cursor 或自研 MCP 客户端的开发者。本文覆盖 `mem-mcp` 的启动方式、stdio 上的 JSON-RPC 2.0 会话、8 个工具的参数/返回/边界行为和错误码；CLI 侧参考见 [CLI.md](CLI.md)，整体设计见 [ARCHITECTURE.md](ARCHITECTURE.md) 与 [DESIGN.md](DESIGN.md)。

本文以 `mem-mcp/src/main.rs` 中 `tools/list` 返回的 `inputSchema` 与各 handler 的实际行为为**唯一权威**。文中报文与返回文本都在 v0.1.0 / v0.1.1 的实际二进制上通过管道喂 stdin 实测得到（0.1.1 行为变更处以 0.1.1 重新实测）；为可读性，示例中的本机绝对路径替换为 `/path/to/...` 占位符，事件 ID、时间戳、hash 保持原样。实际响应是**单行 JSON**；文中保留原始单行报文的示例直接引用实测输出，少数结构示意（如 `tools/list` 的树形展开）会明确标注为示意。

## 目录

- [1. 概览](#1-概览)
- [2. 启动与配置](#2-启动与配置)
  - [2.1 命令行参数](#21-命令行参数)
  - [2.2 store 定位与启动初始化](#22-store-定位与启动初始化)
  - [2.3 客户端配置示例](#23-客户端配置示例)
  - [2.4 启动失败的报错](#24-启动失败的报错)
- [3. 会话流程](#3-会话流程)
  - [3.1 传输格式](#31-传输格式)
  - [3.2 initialize](#32-initialize)
  - [3.3 notifications/initialized](#33-notificationsinitialized)
  - [3.4 ping](#34-ping)
  - [3.5 tools/list](#35-toolslist)
  - [3.6 tools/call](#36-toolscall)
- [4. 工具](#4-工具)
  - [4.1 mem_get_context](#41-mem_get_context)
  - [4.2 mem_add_event](#42-mem_add_event)
  - [4.3 mem_query](#43-mem_query)
  - [4.4 mem_list_projects](#44-mem_list_projects)
  - [4.5 mem_compact](#45-mem_compact)
  - [4.6 mem_sync](#46-mem_sync)
  - [4.7 mem_resolve](#47-mem_resolve)
  - [4.8 mem_render](#48-mem_render)
- [5. 错误码与语义](#5-错误码与语义)
- [6. 与 CLI 共用 store 的推荐工作流](#6-与-cli-共用-store-的推荐工作流)
- [7. 限制](#7-限制)
- [8. 相关文档](#8-相关文档)

## 1. 概览

`mem-mcp` 是 ACDM 的 stdio MCP server（二进制名 `mem-mcp`）。它由 MCP 客户端作为**子进程**拉起，把同一个 ACDM store 的读写能力暴露成 8 个工具，让 AI Agent 能读取项目上下文、记录事件、查询历史并做同步维护。`mem-mcp` 与 `mem` CLI 共享 `mem-core` 的全部实现，读写的是同一个 store，二者可以交替使用。

关键事实：

- **传输层**：stdio 上的**换行分隔 JSON-RPC 2.0**。stdin 每行一个 JSON 对象；stdout 每行一个响应对象。不是 LSP 那种 `Content-Length` 分帧。
- **单行上限**：一行请求最多 16 MiB；超限行被丢弃（保持输入流同步）并返回 `-32600`，server 继续服务。
- **错误分两类**：协议问题返回 JSON-RPC error（`-32700` / `-32600` / `-32601` / `-32602`）；工具级业务失败返回**成功响应 + `result.isError = true`** 与可读的错误文本，Agent 可据此自我纠正并重试（见 [5. 错误码与语义](#5-错误码与语义)）。
- **协议版本**：`initialize` 固定返回 `protocolVersion: "2024-11-05"`，`serverInfo` 为 `{"name":"mem-mcp","version":"0.1.1"}`。
- **能力**：只声明 `capabilities.tools = {}`；没有 resources、prompts、logging、sampling 等能力。
- **单线程串行**：主循环逐行读取 stdin，同步处理完一个请求、写回一个响应并 flush 后才读下一行；没有并发处理。
- **启动即初始化 store**：启动时按与 CLI 相同的规则定位 store，并调用 `MemStore::init()`——目录结构或 `.mem` 缺失时自动创建（等价于 `mem init`）。
- **生命周期**：stdin EOF 时进程正常退出（退出码 0）。客户端负责拉起/关闭进程，不需要手动常驻。
- **无鉴权**：只面向本机 stdio；能力边界见 [7. 限制](#7-限制)。

## 2. 启动与配置

### 2.1 命令行参数

```text
mem-mcp [--path <store-root>]
```

| 参数 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- |
| `--path <DIR>` / `--path=<DIR>` | 否 | 自动定位（见 2.2） | 固定 store 根目录；两种写法都支持，重复给出时**最后一个**生效 |
| `-h` / `--help` | 否 | — | **不支持**：会被当作未知参数拒绝（见 2.4） |

参数解析是手写的（不是 clap）：

- 只认识 `--path` 与 `--path=`；其它任何参数都报错退出 2；
- `--path` 缺少值、`--path=` 值为空 → 报错退出 2；
- 空字符串路径不合法。

### 2.2 store 定位与启动初始化

定位规则与 `mem` CLI 完全一致（`mem-core/src/util.rs::resolve_store_root`），优先级：

1. 命令行 `--path <DIR>`；
2. 环境变量 `ACDM_STORE`；
3. 从**进程当前工作目录**向上逐级查找最近的 store 根（存在 `.mem` 文件或 `events/` 目录）；
4. 兜底：进程当前工作目录。

定位到目标目录后，`mem-mcp` 立即调用 `MemStore::init()`：创建 `events/`、`projects/`、`.mem`、`.gitignore`（已存在的文件不覆盖）。初始化成功时 stdout/stderr 都是空的——客户端不会在启动阶段收到任何 JSON-RPC 报文。若初始化失败（例如目标路径不可写），会向 stderr 打印 `Notice: store initialization check: <原因>`，但 server **继续运行**（后续工具调用可能失败）。

> 注意：MCP 客户端的进程工作目录不一定是你的项目目录。**推荐显式使用 `--path` 或 `ACDM_STORE`**，否则"向上查找"可能落到客户端自己的目录，甚至在 CWD 里自动创建 store。

### 2.3 客户端配置示例

三种常见风格（占位符：`/path/to/mem-mcp` 为二进制绝对路径，`/path/to/store` 为 store 根目录）：

**① Claude Desktop 风格**（`claude_desktop_config.json`）：

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

**② Cursor 风格**（`~/.cursor/mcp.json` 或项目内 `.cursor/mcp.json`）：

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

Cursor 与 Claude Desktop 都使用 `mcpServers` 映射；`command` 需要在客户端的 `PATH` 中，否则请写绝对路径。上面同时给了 `--path` 和 `ACDM_STORE`（二者一致，属于冗余保险）。

**③ 通用 command + args**（任意 MCP 客户端/自研客户端）：

```json
{
  "command": "mem-mcp",
  "args": ["--path", "/path/to/store"],
  "env": {}
}
```

若客户端以"command line"形式配置，等价于：`mem-mcp --path /path/to/store`；也支持 `mem-mcp --path=/path/to/store`。若不想传 `--path`，可以用 `"env": {"ACDM_STORE": "/path/to/store"}` 指定 store；两者都不给时按 2.2 的规则自动定位。

### 2.4 启动失败的报错

参数错误时 stderr 打印 `mem-mcp: ...` 并以退出码 **2** 退出（这也是客户端会看到的启动失败信息）。真实输出：

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

启动时的 store 定位/初始化失败（`resolve_store_root` 报错，例如 CWD 不可读）会打印 `Failed to resolve memory store root: <原因>` 并以退出码 **1** 退出。

## 3. 会话流程

### 3.1 传输格式

- **一行一个 JSON 对象**，UTF-8 编码，`\n` 结尾。空行被忽略。
- 请求：`{"jsonrpc":"2.0","id":<id>,"method":"<method>","params":<object?>}`。`id` 可以是数字、字符串或 `null`。
- 成功响应：`{"jsonrpc":"2.0","id":<id>,"result":<object>}`。
- 错误响应：`{"jsonrpc":"2.0","id":<id>,"error":{"code":<int>,"message":"<text>"}}`。
- 响应中的 `id` 原样回显请求的 `id`（`null` 仅用于无法解析出请求对象的 Parse error）。
- **没有 `id` 的报文按通知处理，不会产生响应，也不会执行该方法**（方法名以 `notifications/` 开头或等于 `initialized` 的报文在更早处就被静默丢弃）。
- **不支持 JSON-RPC batch 数组**：整行必须是一个请求对象，数组会作为解析错误返回 `-32700`。
- **单行请求上限 16 MiB**：超限行会被完整丢弃（含换行，保持后续输入流同步），服务器返回 `-32600` 并继续处理下一行。
- 标准输入输出是协议通道：stdout 只写 JSON-RPC 响应；诊断信息写 stderr。

### 3.2 initialize

客户端握手的第一步。服务端**不校验**请求中的 `protocolVersion` / `capabilities` / `clientInfo`，固定返回同一份结果。

请求（真实报文）：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"docs-client","version":"0.1.1"}}}
```

响应（真实报文）：

```json
{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"mem-mcp","version":"0.1.1"}}}
```

### 3.3 notifications/initialized

握手完成后的通知，**不产生任何响应**。下面这段真实会话中，三行输入只产生两行输出（`initialize` 与 `ping` 的响应），证明通知没有输出：

```text
# stdin（3 行）
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"docs-client","version":"0.1.1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"ping"}

# stdout（2 行）
{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"mem-mcp","version":"0.1.1"}}}
{"jsonrpc":"2.0","id":2,"result":{}}
```

任何 `notifications/` 前缀的方法都按通知静默处理；此外方法名 `initialized` 也被当作通知。

### 3.4 ping

```text
// 请求
{"jsonrpc":"2.0","id":2,"method":"ping"}
// 响应
{"jsonrpc":"2.0","id":2,"result":{}}
```

`ping` 返回空对象，可用于探活；服务端在收到未知方法/未知工具后仍保持运行（见 [5. 错误码](#5-错误码与语义)）。

### 3.5 tools/list

返回 8 个工具的完整定义：每个元素含 `name`、`description`、`inputSchema`（JSON Schema，根类型为 `object`）。真实响应是**单行 JSON**（约 2.6 KB），结构如下（示意，逐工具的准确 `inputSchema` 见 [4. 工具](#4-工具)）：

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

工具顺序固定为：`mem_get_context`、`mem_add_event`、`mem_query`、`mem_list_projects`、`mem_compact`、`mem_sync`、`mem_resolve`、`mem_render`。所有 `inputSchema` 都没有 `additionalProperties: false`，因此多传的字段会被忽略（serde 默认行为）。

### 3.6 tools/call

调用工具的报文形状：

```json
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"<tool>","arguments":{...}}}
```

- `params.name` 必填（字符串），`params.arguments` 可省略（省略时按 `{}` 处理）。
- 成功响应形如 `{"jsonrpc":"2.0","id":20,"result":{"content":[{"text":"...","type":"text"}]}}`：`result.content` 是内容数组，当前每个工具只返回**一个** `text` 项。
- **工具参数不合法**（缺必填字段、类型错、`action` 不是三个枚举值之一等）→ JSON-RPC `error` `-32602`：`{"jsonrpc":"2.0","id":20,"error":{"code":-32602,"message":"Invalid arguments for <tool>: ..."}}`。
- **工具执行期业务失败**（git 冲突、非法查询维度、不安全项目名、正则错误、同步失败等）→ **成功响应 + `result.isError = true`**：`{"jsonrpc":"2.0","id":20,"result":{"content":[{"type":"text","text":"..."}],"isError":true}}`，Agent 可读取错误文本后自我纠正。
- 未知工具返回 `-32601`，且不会终止 server。

真实示例（`mem_render`）：

```text
// 请求
{"jsonrpc":"2.0","id":26,"method":"tools/call","params":{"name":"mem_render","arguments":{"project":"demo"}}}
// 响应
{"jsonrpc":"2.0","id":26,"result":{"content":[{"text":"Successfully rendered README for project 'demo'.","type":"text"}]}}
```

## 4. 工具

| 工具 | 必填参数 | 用途 |
| --- | --- | --- |
| `mem_get_context` | 无 | 读项目上下文（元数据 + README + 最近 5 条事件）；不传 `project` 时列出所有项目 |
| `mem_add_event` | `project`、`summary` | 记录一次事件及其文件变更 |
| `mem_query` | `by`、`value` | 按 project / file_id / filename / fuzzy 查询事件 |
| `mem_list_projects` | 无 | 列出所有项目 |
| `mem_compact` | `project` | 压缩 `files.jsonl`，每个文件名只保留最新记录 |
| `mem_sync` | 无（`message` 可选） | 提交本地改动、变基拉取、推送 |
| `mem_resolve` | 无 | 自动解决 JSONL 冲突标记 |
| `mem_render` | `project` | 生成 `projects/<project>/README.md` |

下文每个工具给出：用途、参数表、`inputSchema` 原文（格式化显示）、返回文本格式、真实返回文本、示例 request/response 与边界行为。

### 4.1 mem_get_context

读取项目上下文；**不传 `project` 时列出 store 中所有项目**，传了则返回该项目的元数据、`README.md` 与最近 5 条事件。

**参数**

| 参数 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `project` | string | 否 | — | 项目名；省略、`null` 或纯空白字符串时列出所有项目 |

**inputSchema（tools/list 原文）**

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

**返回文本格式**

- 不传 `project`（store 为空）：`No projects found in memory store.`
- 不传 `project`（有项目）：
  ```
  Found <N> project(s):
  - <name>: <description> (created: <created_at>)
    GitHub: <github_url>        # 仅当 meta.github_url 存在
  ```
- 传 `project`：
  ```
  === Project: <name> ===
  Description: <description>
  Created At: <created_at>
  GitHub: <url>                 # 仅当存在

  --- README.md ---
  <projects/<name>/README.md 的内容，或 "(No README.md available)">

  --- Recent Events (last 5) ---
  [<ts>] (<event_id>) <summary>
    Device: <device> | Agent: <agent> | Files: <n>
      - <file_name> [<action>] (<size> bytes)
  ```
  最近事件按时间**最新在前**，最多 5 条；`meta.json` 缺失时第二行变为 `Metadata: (not found or error: <原因>)`；没有事件时该段为 `(No events found for this project)`。

**真实返回文本（空 store）**

```text
$ mem-mcp --path /path/to/store   # 发送 mem_get_context
No projects found in memory store.
```

**真实返回文本（列出所有项目）**

```json
{"jsonrpc":"2.0","id":22,"result":{"content":[{"text":"Found 1 project(s):\n- demo: Project demo (created: 2026-10-22T13:08:57.814181116Z)","type":"text"}]}}
```

**真实返回文本（指定 project，节选）**

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

**边界行为**

- `project` 传空白字符串等同于省略（会列出所有项目）。
- 项目不存在时**不报错**：返回 `=== Project: nope ===` + `Metadata: (not found or error: ...)` + `(No README.md available)` + `(No events found for this project)`。
- `README.md` 是否存在取决于是否跑过 `mem_render` / `mem render`；事件最多 5 条，但会跨月份文件正确取最新（不是只看最新月份文件）。

### 4.2 mem_add_event

记录一次事件及其关联的文件变更。这是 Agent 写入记忆的主要入口。

**参数**

| 参数 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `project` | string | **是** | — | 项目名 |
| `summary` | string | **是** | — | 事件摘要 |
| `device` | string | 否 | 本机 hostname | 设备名；`null`/空白字符串视为未给，回退 hostname（规则同 CLI） |
| `agent` | string | 否 | `mcp-agent` | 记录者名称；空白字符串回退默认值 |
| `files` | array | 否 | `[]` | 受影响的文件列表；省略表示本次事件没有文件变更 |
| `files[].file_name` | string | **是** | — | 文件路径；**绝对路径会被规范化**为 store / git 仓库 / CWD 相对路径 |
| `files[].action` | string | 否 | `Modified` | 只接受 `Created` / `Modified` / `Deleted`（大小写敏感）；其它值返回 `-32602` |
| `files[].size_bytes` | integer | 否 | `0` | 文件字节数 |
| `files[].hash` | string | 否 | `""` | 调用方提供的 SHA256 十六进制摘要；服务端**不计算也不校验** |

**inputSchema（tools/list 原文）**

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

**返回文本格式**：`Event <event_id> recorded with <N> files`（`N` = 本次 files 数量，无文件时为 0）。

**真实返回文本**

```json
{"jsonrpc":"2.0","id":20,"result":{"content":[{"text":"Event 20261022130857_814125160_26572_0000 recorded with 1 files","type":"text"}]}}
```

对应的（真实）请求——注意 `file_name` 是绝对路径，服务端会规范化存储：

```json
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"mem_add_event","arguments":{"project":"demo","summary":"add main.rs","device":"laptop","agent":"claude","files":[{"file_name":"/path/to/project/src/main.rs","action":"Created","size_bytes":33,"hash":"0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1"}]}}}
```

写入 `projects/demo/files.jsonl` 后（真实内容）`file_name` 是 `src/main.rs`，因此 `mem_query by=filename` 用相对路径可以命中：

```json
{"file_id":"20261022130857_814188379_26572_0001","file_name":"src/main.rs","action":"Created","size_bytes":33,"hash":"0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1"}
```

省略可选字段的真实请求与响应（`action` 缺省 `Modified`、`size_bytes` 缺省 0、`hash` 缺省空串、`device` 回退 hostname、`agent` 回退 `mcp-agent`）：

```text
// 请求
{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"mem_add_event","arguments":{"project":"demo","summary":"touch lib.rs","files":[{"file_name":"/path/to/project/src/lib.rs"}]}}}
// 响应
{"jsonrpc":"2.0","id":21,"result":{"content":[{"text":"Event 20261022130857_814598761_26572_0002 recorded with 1 files","type":"text"}]}}
```

**边界行为**

- `project` 第一次被记录时，若 `projects/<project>/meta.json` 不存在，会**自动写入**最小元数据（`description` = `"Project <name>"`、`github_url` = null、`created_at` = 事件时间），与 CLI `add` 一致。
- `files` 省略或为空数组时事件仍会写入（`recorded with 0 files`），且不会追加 `files.jsonl` 记录。
- `file_name` 规范化规则与 CLI 相同：相对 store 根 → 相对 git 仓库根 → 相对 CWD → 绝对路径（MCP 侧**不会**像 CLI 那样打印绝对路径警告）。
- `action` 只接受 `Created` / `Modified` / `Deleted`（缺省 `Modified`）；传 `"Renamed"` 等其它值会返回 `-32602`（`Invalid arguments for mem_add_event: unknown variant ...`）；`size_bytes` / `hash` 仍然透传，不校验文件是否存在。
- 事件 ID 由服务端生成（`YYYYMMDDHHMMSS_nanos9_pid5_counter4`，UTC）；`ts` 为固定宽度 UTC 纳秒。
- `project` 必须是单一安全路径组件（同 CLI 校验）；非法值（如 `../evil`）在写入前被拒绝，返回 `isError` 结果：`Failed to append event: invalid project name "../evil": ...`。
- 参数错误示例：缺 `project` / `summary` 返回 `-32602`（`Invalid arguments for mem_add_event: missing field \`project\``）；`action` 非法同样返回 `-32602`。

### 4.3 mem_query

按指定维度查询事件。

**参数**

| 参数 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `by` | string | **是** | — | 查询维度枚举：`project` / `file_id` / `filename` / `fuzzy` |
| `value` | string | **是** | — | 查询词 / ID / 文件名 / 正则 |

**inputSchema（tools/list 原文）**

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

`by` 大小写不敏感（内部 `to_lowercase()` 后匹配）。

**返回文本格式**

- `by=project`：`Found <N> event(s) for project '<value>':` + 每个事件块：
  ```
  [<ts>] (<event_id>) Project: <project>
  Summary: <summary>
  Device: <device> | Agent: <agent>
  Files:
    - <file_name> [<action>] size: <n> bytes, hash: <hash>
  ```
- `by=file_id`：`Found event for file_id '<value>':` + 一个事件块。
- `by=filename`：`Found <N> event(s) affecting file '<value>':` + 事件块。
- `by=fuzzy`：`Found <N> matching event(s) for pattern '<value>':` + 事件块。
- 无命中：
  - project → `No events found for project '<value>'.`
  - file_id → `No event found for file_id '<value>'.`
  - filename → `No events found for filename '<value>'.`
  - fuzzy → `No events matched pattern '<value>'.`
- `by` 非法 → **业务失败**（`isError`），文本：`Invalid query dimension 'by': '<值>'. Allowed values: 'project', 'file_id', 'filename', 'fuzzy'.`
- `fuzzy` 正则非法 → **业务失败**（`isError`），文本：`Fuzzy search failed: regex parse error: ...`；正则编译程序上限 10 MiB，超限时文本为 `Fuzzy search failed: Compiled regex exceeds size limit of 10485760 bytes.`

**真实返回文本（by=project）**

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

**真实返回文本（by=file_id）**

请求（真实报文）：

```json
{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"mem_query","arguments":{"by":"file_id","value":"20261022130857_814188379_26572_0001"}}}
```

返回的文本：

```text
Found event for file_id '20261022130857_814188379_26572_0001':

[2026-10-22T13:08:57.814181116Z] (20261022130857_814125160_26572_0000) Project: demo
Summary: add main.rs
Device: laptop | Agent: claude
Files:
  - src/main.rs [Created] size: 33 bytes, hash: 0955a8767e80cb2d29f2ce12ed50ac1ebf37612fc8d93ffd734029da7ba23be1
```

**真实返回文本（by=filename）**

```text
Found 1 event(s) affecting file 'src/lib.rs':

[2026-10-22T13:08:57.814602658Z] (20261022130857_814598761_26572_0002) Project: demo
Summary: touch lib.rs
Device: localhost | Agent: mcp-agent
Files:
  - src/lib.rs [Modified] size: 0 bytes, hash:
```

**真实报文（`by=filename` 的原始单行响应，展示 `id` 与 `result.content[0].text` 形状）**

```json
{"jsonrpc":"2.0","id":24,"result":{"content":[{"text":"Found 1 event(s) affecting file 'src/lib.rs':\n\n[2026-10-22T13:08:57.814602658Z] (20261022130857_814598761_26572_0002) Project: demo\nSummary: touch lib.rs\nDevice: localhost | Agent: mcp-agent\nFiles:\n  - src/lib.rs [Modified] size: 0 bytes, hash:","type":"text"}]}}
```

**真实返回文本（by=fuzzy）**

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

**真实错误返回（by 非法 / fuzzy 正则非法）**

```json
{"jsonrpc":"2.0","id":10,"result":{"content":[{"text":"Invalid query dimension 'by': 'commit'. Allowed values: 'project', 'file_id', 'filename', 'fuzzy'.","type":"text"}],"isError":true}}
{"jsonrpc":"2.0","id":5,"result":{"content":[{"text":"Fuzzy search failed: regex parse error:\n    [\n    ^\nerror: unclosed character class","type":"text"}],"isError":true}}
```

**边界行为**

- `by=filename` 是 `file_name` 的**整串精确匹配**，不是子串/模糊搜索：`value` 必须与 `files.jsonl` 中规范化后的 `file_name` 完全一致（例如 `src/hello.txt`），传 `hello.txt` 不会命中。
- `by=filename` 只匹配**规范化后的相对 `file_name`**；绝对路径记录查不到。
- `by=fuzzy` 是对事件原始 JSONL 行做 Rust `regex` 匹配（不是全文分词），命中的行必须能反序列化为 `Event`。
- 与 CLI `query` 一样，每次调用都重建内存索引；没有分页参数，返回全部命中。
- 多余的参数字段会被忽略；`by` / `value` 缺失走 `-32602`（`Invalid arguments for mem_query: missing field ...`），`by` 值不在枚举内则是 `isError` 业务失败。

### 4.4 mem_list_projects

列出 store 中所有项目。

**参数**：无。

**inputSchema（tools/list 原文）**

```json
{"type": "object", "properties": {}}
```

**返回文本格式**

```text
Projects in memory store (<N>):
- <name>: <description>      # 有 meta.json
- <name>                     # 无 meta.json
```

空 store：`No projects found in memory store.`

**真实返回文本**

```json
{"jsonrpc":"2.0","id":29,"result":{"content":[{"text":"Projects in memory store (1):\n- demo: Project demo","type":"text"}]}}
```

**边界行为**：项目按名字典序排序；只读 `projects/` 下的一级子目录（经 ignore crate 遍历：隐藏目录默认跳过，store 是 git 仓库时应用 `.gitignore`）。

### 4.5 mem_compact

从事件日志重建项目 `files.jsonl` 投影：每个 `file_name` 只保留最新一条记录，并修复缺失/截断的投影。

**参数**

| 参数 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `project` | string | **是** | — | 要压缩的项目 |

**inputSchema（tools/list 原文）**

```json
{
  "type": "object",
  "properties": {
    "project": {"type": "string", "description": "Project name to compact"}
  },
  "required": ["project"]
}
```

**返回文本格式**：`Compacted project '<project>': before: <A> records, after: <B> records (reduced by <C> duplicate/old records).`（`C = A - B`）

**真实返回文本**

```text
// 有重复记录时
{"jsonrpc":"2.0","id":32,"result":{"content":[{"text":"Compacted project 'demo': before: 3 records, after: 2 records (reduced by 1 duplicate/old records).","type":"text"}]}}
// 没有重复时
{"jsonrpc":"2.0","id":28,"result":{"content":[{"text":"Compacted project 'demo': before: 2 records, after: 2 records (reduced by 0 duplicate/old records).","type":"text"}]}}
```

**边界行为**：与 CLI `compact` 同一实现（持 store 写锁从事件日志重放，临时文件 + 原子 rename）；项目既无投影又无事件时返回 0/0/0；失败（如项目名非法）返回 `isError`，文本形如 `Compaction failed for project '<p>': ...`。

### 4.6 mem_sync

把 store 同步到远端 git 仓库（与 CLI `mem sync` 共用 `sync_store`）：全程持 store 写锁 → 工作区有改动则提交 → 有 upstream 时 `git pull --rebase`（无 autostash）→ `git push`。无 upstream 时跳过 pull；非冲突的 pull 失败会中止同步；冲突以 `isError` 返回。

**参数**

| 参数 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `message` | string | 否 | `"Auto-sync memory store"` | 本地有改动时的 commit message；空白字符串视为未给 |

**inputSchema（tools/list 原文）**

```json
{
  "type": "object",
  "properties": {
    "message": {"type": "string", "description": "Optional commit message"}
  }
}
```

**返回文本格式**：`Sync completed successfully: <step1>; <step2>; ...`，步骤可能包括：

- `Working directory clean` 或 `Committed local changes ("<message>")`
- `Pulled remote changes with rebase` 或 `No upstream configured; skipped pull`
- `Pushed to remote successfully`

**真实返回文本**

```text
// 有改动 + 自定义 message
{"jsonrpc":"2.0","id":51,"result":{"content":[{"text":"Sync completed successfully: Committed local changes (\"sync from mcp-agent\"); Pulled remote changes with rebase; Pushed to remote successfully","type":"text"}]}}
// 工作区干净
{"jsonrpc":"2.0","id":52,"result":{"content":[{"text":"Sync completed successfully: Working directory clean; Pulled remote changes with rebase; Pushed to remote successfully","type":"text"}]}}
```

**边界行为**

- store 不是 git 仓库或 `git` 不可用 → `isError`，文本：`Sync failed: git command 'git status --porcelain' failed: ... Check the git remote/upstream configuration, then run mem_sync again.`（真实例，`LC_ALL=C`）：
  ```json
  {"jsonrpc":"2.0","id":1,"result":{"content":[{"text":"Sync failed: git command 'git status --porcelain' failed: fatal: not a git repository (or any parent up to mount point /)\nStopping at filesystem boundary (GIT_DISCOVERY_ACROSS_FILESYSTEM not set).. Check the git remote/upstream configuration, then run mem_sync again.","type":"text"}],"isError":true}}
  ```
- 无 remote / 无 upstream：跳过 pull，push 失败 → `isError`，文本以 `Sync failed: git command 'git push' failed: fatal: No configured push destination.` 开头。
- 拉取时发现冲突：返回 `isError`（对应 CLI 的非零退出码），文本为 `Sync stopped on a git conflict: ... Resolve the JSONL conflicts with the \`mem_resolve\` tool, then retry (run mem_resolve then retry mem_sync).`
- 非冲突的 pull 失败现在**中止同步并返回 `isError`**（不再忽略错误继续 push）。
- `git add -A` / `git commit` / `git push` 失败同样以 `Sync failed: ...` 的 `isError` 返回。
- 前置条件同 CLI：需要 git 仓库、配置好 remote 与 upstream、配置 git 用户身份。
- `message` 空白字符串会被过滤，回退默认消息。

### 4.7 mem_resolve

自动解决 store 下所有 `.jsonl` 文件中的 Git 冲突标记。

**参数**：无。

**inputSchema（tools/list 原文）**

```json
{"type": "object", "properties": {}}
```

**返回文本格式**：`Conflict resolution completed: <A> files resolved, <B> conflict markers resolved, <C> duplicate items removed, <D> non-record lines preserved.`

**真实返回文本**

```text
// 有冲突（1 条记录去重 + 1 条不可解析行保留）
{"jsonrpc":"2.0","id":40,"result":{"content":[{"text":"Conflict resolution completed: 1 files resolved, 1 conflict markers resolved, 1 duplicate items removed, 1 non-record lines preserved.","type":"text"}]}}
// 没有冲突
{"jsonrpc":"2.0","id":34,"result":{"content":[{"text":"Conflict resolution completed: 0 files resolved, 0 conflict markers resolved, 0 duplicate items removed, 0 non-record lines preserved.","type":"text"}]}}
```

**边界行为**

- 与 CLI `resolve` 同一实现：只删除精确形态的冲突标记、按字节处理（非 UTF-8/CRLF 逐字保留）、去重（后出现者胜出，`FileRecord` 保持因果顺序）、不可解析行逐字保留、原始行文本原样重发（未知字段不丢）；存在 `.git` 时 `git add` 被改写文件；只在 rebase 确实在进行时才 `git rebase --continue`，并在一次调用内循环推进多提交 rebase（上限 100 轮）；全程持 store 写锁。
- 没有冲突不是错误：返回全 0 的成功文本。
- 只处理 `.jsonl`；`meta.json` 等非 JSONL 冲突不会被处理，可能使 `git rebase --continue` 失败 → 返回 `isError`，文本以 `Conflict resolution failed:` 开头。此时需要人工解决该文件后再继续 rebase（见 [CLI.md](CLI.md#37-resolve) 与 [TROUBLESHOOTING.md](TROUBLESHOOTING.md)）。

### 4.8 mem_render

生成或更新 `projects/<project>/README.md`。

**参数**

| 参数 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `project` | string | **是** | — | 要渲染的项目名 |

**inputSchema（tools/list 原文）**

```json
{
  "type": "object",
  "properties": {
    "project": {"type": "string", "description": "Project name to render README for"}
  },
  "required": ["project"]
}
```

**返回文本格式**：`Successfully rendered README for project '<project>'.`

**真实返回文本**

```json
{"jsonrpc":"2.0","id":26,"result":{"content":[{"text":"Successfully rendered README for project 'demo'.","type":"text"}]}}
```

**边界行为**

- 与 CLI `render` 同一实现：README 含元数据、最新文件表、最近 10 条事件时间线（最新在前），每次覆盖。
- `meta.json` 缺失时使用内存兜底元数据（`description` = `Project <name>`、`created_at` = 当前时间），**不会写回** `meta.json`。
- 项目没有任何文件/事件时也能成功，README 里显示 `_No files tracked yet._` / `_No events recorded yet._`。
- 失败（如目录不可写、项目名非法）返回 `isError`：`Failed to render README for project '<p>': ...`。

## 5. 错误码与语义

| 层 | 返回形式 | 触发场景 | 示例 message |
| --- | --- | --- | --- |
| 协议错误 | JSON-RPC `error` | `-32700`：整行无法解析为合法请求对象（JSON 语法错误、缺 `jsonrpc`/`method` 字段、batch 数组） | `Parse error: EOF while parsing a value at line 1 column 34` |
| 协议错误 | JSON-RPC `error` | `-32600`：`jsonrpc` 不是 `"2.0"`；或单行请求超过 16 MiB | `Invalid Request: expected jsonrpc "2.0", got "1.0"`；`Invalid Request: request line exceeds 16777216 bytes` |
| 协议错误 | JSON-RPC `error` | `-32601`：未知方法；或 `tools/call` 的 `params.name` 不是 8 个工具之一 | `Method not found: no/such/method`；`Unknown tool: mem_does_not_exist` |
| 协议错误 | JSON-RPC `error` | `-32602`：`tools/call` 外层参数或工具参数无法反序列化（缺字段、类型错、`action` 非法） | `Missing parameters for tools/call`；`Invalid arguments for mem_render: missing field \`project\`` |
| 业务失败 | 成功响应 + `result.isError = true` | 工具执行期错误：git 冲突、同步失败、非法查询维度、不安全项目名、正则错误等 | `Invalid query dimension 'by': ...`；`Failed to append event: invalid project name ...` |
| （保留） | — | `-32603` 保留给真正的服务端故障 | 当前版本不会返回 |

**真实错误报文**

```text
// -32700: unparseable input (id is null)
{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error: key must be a string at line 1 column 2"}}

// -32600: wrong jsonrpc version
{"jsonrpc":"2.0","id":3,"error":{"code":-32600,"message":"Invalid Request: expected jsonrpc \"2.0\", got \"1.0\""}}

// -32601: unknown tool
{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Unknown tool: nope"}}

// -32602: tool arguments missing a required field
{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"Invalid arguments for mem_compact: missing field `project`"}}

// -32602: invalid action enum value
{"jsonrpc":"2.0","id":5,"error":{"code":-32602,"message":"Invalid arguments for mem_add_event: unknown variant `Renamed`, expected one of `Created`, `Modified`, `Deleted`"}}

// business failure: successful JSON-RPC response with result.isError = true
{"jsonrpc":"2.0","id":6,"result":{"content":[{"type":"text","text":"Invalid query dimension 'by': 'bogus'. Allowed values: 'project', 'file_id', 'filename', 'fuzzy'."}],"isError":true}}
```

**语义要点**

- **错误不会终止 server**：未知方法、未知工具、坏 JSON、超长行之后，server 都继续处理后续请求。真实验证：发送未知工具得到 `-32601` 后紧接 `ping`，仍正常返回：
  ```json
  {"jsonrpc":"2.0","id":12,"result":{}}
  {"jsonrpc":"2.0","id":13,"result":{}}
  ```
  （分别是坏 JSON 之后的 ping、未知工具之后的 ping。）
- 成功响应与错误响应互斥：成功不含 `error`，错误不含 `result`；**但成功响应可能带 `result.isError = true`**（业务失败），客户端应优先检查该字段，再读 `result.content[].text`。
- 无法解析/超长/超限的行只影响该行：`-32700` / `-32600` 后循环继续（`id` 为 `null`）。
- `tools/call` 的 `params` 形状错误与工具参数 schema 不匹配是 `-32602`；工具执行期的业务失败一律是 `result.isError = true`，不再使用 `-32603`。
- 单行请求上限 16 MiB（`MAX_REQUEST_LINE_BYTES`），超限返回 `-32600` 并丢弃整行。

## 6. 与 CLI 共用 store 的推荐工作流

Agent 侧推荐节奏（`mem-mcp` 启动时不需要做任何额外初始化）：

1. **开工**：`mem_get_context`（不带 `project` 先看有哪些项目；带 `project` 读元数据 + README + 最近 5 条事件）了解上下文。
2. **改动后**：`mem_add_event` 记录摘要与文件变更（`file_name` 用相对路径最稳妥；绝对路径会被规范化）。
3. **需要历史**：`mem_query`（`by=project` 看全量、`by=filename` 看某文件、`by=fuzzy` 找关键词）。
4. **周期性维护**：`mem_render` 刷新项目 README；`mem_compact` 压缩 `files.jsonl`；`mem_sync` 提交并同步到远端。
5. **冲突时**：`mem_sync` 返回 `isError`（文本提示冲突）后，按提示调用 `mem_resolve`，再调用一次 `mem_sync`。

示例会话序列（每一行都是一个请求；实际输出见对应工具小节）：

```text
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mem_get_context","arguments":{"project":"demo"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mem_add_event","arguments":{"project":"demo","summary":"implement parser","files":[{"file_name":"src/parser.rs","action":"Created","size_bytes":1200}]}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"mem_query","arguments":{"by":"filename","value":"src/parser.rs"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"mem_render","arguments":{"project":"demo"}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"mem_sync","arguments":{"message":"agent: add parser"}}}
```

与 CLI 共用的注意事项：

- **同一个 store**：CLI 的 `mem list` / `mem query` 能立即看到 MCP 写入的事件，反之亦然；两边是不同进程，靠 `fs2` 文件锁串行化追加写。
- **并发写入**：多个 MCP 进程 / CLI 同时写是安全的（store 跨进程写锁串行化追加、`compact`、`resolve`、`sync`）；跨设备一致性仍靠显式 `mem_sync`。
- **同步不自动**：写事件只落本地文件；跨设备可见需要显式调用 `mem_sync`（或 CLI `mem sync`）。
- **命名一致**：`project` 直接映射为 `projects/<project>/` 目录名，必须是单一安全路径组件（非空、不含 `/`、`\`、控制字符、≤200 字节），与 CLI 侧保持一致；两边不要给同一项目起不同名字。
- **文件身份一致**：两边都按同一套规则规范化 `file_name`，跨设备/跨入口才能互相匹配。

## 7. 限制

- **只有 tools**：不提供 resources、prompts、logging、sampling 等 MCP 能力；`capabilities` 中只有 `tools`。
- **无鉴权**：stdio 本地进程，权限继承自启动它的客户端；**不要**把 `mem-mcp` 通过网络对外暴露。
- **单进程串行**：一个请求处理完才读下一行；`mem_sync` / `mem_resolve` 这类可能耗时的 git 操作会阻塞后续请求。
- **单行上限 16 MiB**：超长请求行被丢弃并返回 `-32600`；fuzzy 正则编译程序上限 10 MiB。
- **不做文件校验**：`mem_add_event` 不读取文件、不计算 SHA256，`size_bytes` / `hash` 完全由调用方提供，写入时不做校验；但 `action` 只接受 `Created` / `Modified` / `Deleted`。
- **无分页**：`mem_query` / `mem_get_context` 一次性返回结果；大 store 下查询会较慢（每次重建内存索引）。
- **上下文截断**：`mem_get_context` 只取最近 5 条事件，`mem_render` 时间线只取最近 10 条。
- **同步前置条件**：`mem_sync` 要求 store 是 git 仓库、有可用 remote/upstream 与 git 用户身份；否则返回 `isError`。
- **冲突解决只覆盖 JSONL**：`mem_resolve` 不处理 `meta.json` 等非 JSONL 文件的冲突（可能需要手工解决后 `git rebase --continue`）。
- **协议版本固定**：固定返回 `2024-11-05`，不校验客户端版本、不支持批处理数组、不支持服务端主动通知。
- **项目名已校验**：`project` 必须是单一安全路径组件（非空、非 `.`/`..`、不含 `/`、`\`、无控制字符、≤200 字节）；CLI 与 MCP 都会拒绝非法值。

## 8. 相关文档

- [CLI.md](CLI.md) —— `mem` 全部子命令与参数参考
- [ARCHITECTURE.md](ARCHITECTURE.md) —— 组件分层、读写路径、同步与并发
- [DESIGN.md](DESIGN.md) —— 设计取舍、一致性模型、ID 与文件身份
- [MIGRATION.md](MIGRATION.md) —— 旧时间戳格式、旧绝对路径记录等历史数据问题
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) —— 常见故障与恢复步骤
- [README.md](../../README.zh-CN.md) —— 项目总览与快速开始
