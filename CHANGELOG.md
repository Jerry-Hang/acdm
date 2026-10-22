# Changelog

本文件记录 ACDM 的显著变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [0.1.0] - 2026-10-22

首个公开发布版本。

### 新增

- **`mem-core` 库**（9 个模块）
  - `models`：`Event` / `FileRecord` / `ProjectMeta` 数据模型与 SHA-256 工具
  - `util`：共享的时间戳、排序、store 定位、设备名与文件身份规范化逻辑
  - `storage`：append-only JSONL 存储（`fs2` 排他锁、`O_APPEND`）、`mmap` 批量读、
    `.mem` 标记文件、按月分片的最近事件查询
  - `index`：内存索引（项目 / file_id / 文件名三个维度 + 字节偏移指针）
  - `git`：Git 命令封装（init / add / commit / pull --rebase / push / 冲突检测 / rebase continue）
  - `resolve`：JSONL 冲突自动解决
  - `render`：项目 `README.md` 生成
  - `compact`：文件记录压缩（tmp + 原子 rename）
  - `id_gen`：跨设备唯一 ID 生成
- **`mem` CLI**：`init` / `add` / `query` / `list` / `compact` / `sync` / `resolve` / `render` 八个子命令，
  全局 `--path`，store 自动发现。
- **`mem-mcp` MCP server**：stdio + 换行分隔 JSON-RPC 2.0，8 个工具
  （`mem_get_context`、`mem_add_event`、`mem_query`、`mem_list_projects`、`mem_compact`、
  `mem_sync`、`mem_resolve`、`mem_render`），支持 `--path` 启动参数。
- **跨设备语义**：
  - 文件身份规范化为 store / git 仓库 / CWD 相对路径，同一文件在各设备得到同一标识
  - 时间戳统一为定宽 UTC 纳秒格式 `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`
  - store 定位规则在 CLI 与 MCP 之间统一：`--path` > `$ACDM_STORE` > 向上找 `.mem`/`events/` > CWD
- **文档与工程**：中文 README、`docs/` 五份专题文档、GitHub Actions CI
  （fmt / clippy `-D warnings` / test / release build）、MIT License、`rustfmt.toml`。
- **测试**：55 个测试（30 core 单元 + 9 CLI 集成 + 6 MCP 单元 + 10 MCP 集成），
  含真实 rebase 冲突、locale、跨月排序、字节级冲突保留等回归用例。

### 变更（相对内部预发布快照）

- ID 格式改为 `YYYYMMDDHHMMSS_nanos(9)_pid(5)_counter(4)`，时间戳部分使用 **UTC**；
  旧格式 `YYYYMMDDHHMMSS_pid(5)_counter(2)` 在 ID 生成密集或跨设备同秒同 pid 时可能碰撞。
- `mem resolve` 的报告新增 `Lines preserved`（无法解析而被逐字保留的行数）。
- `mem list` 列宽按新的 35 字符 ID 与 30 字符时间戳调整。
- MCP `tools/list` 中 `mem_add_event` 的 `files[].action` 由 required 改为可选（缺省 `Modified`），
  与 handler 实际行为一致。
- MCP `mem_resolve` 的返回文本追加 preserved 行数。
- `mem add` 的 `Created` / `Modified` 判定改用规范化后的文件标识与已有记录比对。

### 移除

- `MemStore::scan_files`（无调用方）。
- `MemStore::read_latest_events`（只读最新月份文件，跨月会漏；由
  `MemStore::read_latest_events_for_project` 取代）。
- store 根下未实现的 `nodes/` 目录（设备信息已由每条事件的 `device` 字段承载）。

### 修复

- **冲突解决不再静默丢数据**：无法解析的行（如写入中断产生的截断行）逐字保留；
  未知字段原样回写；同 `event_id` / `file_id` 后写覆盖；按时间排序后写回。
- **`mem resolve` 不再依赖 locale**：只依据 Git 自身的 rebase 状态目录决定是否
  `git rebase --continue`，不再匹配英文错误文本（此前 zh_CN 等 locale 下会误报失败）。
- **文件身份跨设备一致**：`mem add` / `mem_add_event` 写入规范化路径；
  此前传入绝对路径会原样入库，导致相对名查询与跨设备匹配失效。
- **时间戳单一格式**：CLI 与 MCP 统一写入定宽 UTC 纳秒格式；读取端按解析后的时间排序，
  历史数据中的两种旧格式仍能正确比较。
- **store 发现一致**：CLI 与 MCP 使用同一套定位规则并支持 `--path` / `$ACDM_STORE`；
  `init` 写入 `.mem` 标记，使子目录内可自动发现 store。
- **最近事件跨月正确**：`read_latest_events_for_project` 按月份倒序读取，月份文件内
  按解析后的时间排序；非 `YYYY-MM.jsonl` 的兜底文件不会被误认为最新月份。
- **读取容忍坏行**：事件/文件记录读取遇到无法解析的行时跳过，单行损坏不会让整个 store 不可读。
- **`render` 与 `add` 协作**：`mem render` 先创建项目目录时，`mem add` / `mem_add_event`
  仍会补写 `meta.json`；`render` 在缺少元数据时的兜底时间也改用规范时间戳格式。
- **`compact` 临时文件**：固定命名为 `files.jsonl.tmp`（被 store 的 `.gitignore` 覆盖），
  rename 失败时清理临时文件。
- **非 UTF-8 路径**：`resolve` 不再因无法转 UTF-8 的路径 panic。
- **`mem resolve` 的失败信息可操作**：非 JSONL 文件（如 `projects/<p>/meta.json`）仍有冲突时，
  错误会列出未解决的路径并提示 `git add` 后重试，而不是直接抛出裸的 Git 报错。
- **`-n/--filename` 帮助文本**改为明确说明「精确匹配整条相对路径」，与实际行为一致。
- **代码质量**：`cargo clippy -D warnings` 与 `cargo doc -D warnings` 全绿。

### 兼容性说明

- 读取端向后兼容历史数据：旧时间戳格式可解析并按真实时间排序；损坏/未知行被跳过或保留。
- 旧数据中的绝对路径 `file_name` 不会被自动改写；详见 [docs/MIGRATION.md](docs/MIGRATION.md)。
- 已移除的 API 为公开库接口的破坏性变更（`mem-core` 尚未发布过稳定版本，按 0.x 处理）。

[0.1.0]: https://github.com/Jerry-Hang/acdm/releases/tag/v0.1.0
