# Changelog

本文件记录 ACDM 的显著变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [0.1.2] - 2026-10-23

第二轮独立外部审查（3 项 PARTIAL 残留 + 7 项新 P2/P3 发现）的修复版本。
**无数据格式变更**：既有 store 不需要迁移，升级后所有修复自动生效。

### 数据安全与并发

- **`mem render` / `MemStore::init` 现在持全库写锁**：README 再生成不会在 `mem sync` 的
  commit → `pull --rebase` 之间弄脏工作区（此前与 ARCHITECTURE / DESIGN 的声明不一致）。
- **写锁锁目标改进**：`.mem` 标记存在时直接以只读方式 `flock` 它（不写临时文件、不污染 Git、
  天然按 store 文件权限隔离）；无标记时回退到运行时/临时目录下**按用户隔离、0600 权限**的
  锁文件，修复多用户共享机上的 `EACCES`。
- **新增原子核心 API**：`append_event_with_meta` 与 `record_file_event`——`mem add` 的
  「读最新文件 → 判 Created/Modified → 初始化 meta → 追加事件」现在在**同一把锁内**完成，
  消除并发 `Created` 误判与 meta 覆写；meta.json 缺失或损坏都会在持锁路径内自愈。
- **MCP `mem_add_event` 先校验项目名再动手**，并且不再吞掉 meta 写入错误（`let _ =` 已移除），
  非法项目立即以 `isError` 返回且不在磁盘留下任何目录。

### 冲突解决

- **多步 rebase 空补丁自动跳过**：某个被重放提交在去重后变成空补丁时，resolver 自动执行
  `git rebase --skip` 继续推进，而不是以 "No changes" 中断（混合空补丁/正常冲突的多提交
  rebase 一次 `mem resolve` 完成）。
- **支持 `git worktree` / submodule**（`.git` 为文件）：rebase 状态与暂存判断改用
  `git rev-parse --git-path` / `--git-dir`，不再假设 `.git` 是目录；`git add` 失败从警告
  改为硬错误（避免掩盖 `rebase --continue` 必败的真因）。
- **compact 排序语义写入文档**：增量投影与 resolve 遵循物理追加顺序；`compact` 重放按事件
  `ts`（再 `event_id`）排序并 last-write-wins，这是有意的设计权衡。

### 跨平台与协议

- **项目名跨平台加固**：拒绝 Windows 保留设备名（`CON`、`PRN`、`AUX`、`NUL`、`COM1`–`COM9`、
  `LPT1`–`LPT9`，大小写不敏感，含 `con.txt` 形式）与以 `.` 结尾的名字，避免 Unix 创建的项目
  在 Windows 上 checkout 失败。
- **项目名拒绝 Windows 非法字符**：除保留设备名与尾点外，`<>:"|?*` 也会在任意平台上被拒绝，避免 Unix 上建出的项目名在 Windows 检出失败。
- **`mem add` 不再持锁做慢 IO，并拒绝非普通文件**：文件哈希移到获取 store 写锁之前完成；FIFO、设备节点、目录等非普通文件直接报错（`not a regular file`），因此 `mem add /dev/zero` 之类输入不会长期占用全库写锁、阻塞其它写操作（0.1.2 复验发现的安全侧加固）。
- **MCP JSON-RPC 通知语义修正**：无 `id` 的消息仍是 Notification（不响应）；**带 `id` 的
  `notifications/*` 是普通 Request**，现在返回 `-32601 Method not found` 而不是被静默丢弃
  （此前客户端会永久等待该 `id` 的响应）。
- **文档统一**：CLI 与 MCP 的同步提交信息统一为 `Auto-sync from <device> at <ts>`；
  写锁覆盖范围、排序语义、项目名校验规则与 worktree 支持同步更新到中英文文档。

### 测试

- **84 → 100 个测试**（59 core + 16 CLI 集成 + 6 MCP 单元 + 19 MCP 集成），新增覆盖：
  纯重复/空补丁多提交 rebase（pin apply backend，附 GIT_TRACE2 证据）、`git worktree` rebase
  冲突解决、Windows 保留名与尾点项目名（core + CLI）、并发 meta 初始化、损坏 meta 自愈、
  `record_file_event` 三种动作、`render` 与全库锁互斥、MCP 带 id 通知与非法项目即时 `isError`；
  以及非普通文件拒绝（FIFO/设备/目录）与 Windows 非法字符项目名（`<>:"|?*`）的拒绝。

## [0.1.1] - 2026-10-23

针对独立外部代码审查（15 项发现，含 2 个 P0）的安全与健壮性加固版本。
**无数据格式变更**：既有 store 不需要迁移；升级后建议执行一次 `mem compact -p <project>` 重建投影。

### 安全

- **项目名校验（P0）**：`project` 之前被直接拼进 `projects/<project>`，CLI 与 MCP 可传入 `../..`、
  绝对路径等导致目录穿越（写 `meta.json` / `README.md`、读任意目录）。现在项目名必须是单一
  路径组件：非空、非 `.`/`..`、不含 `/`、`\`、无控制字符、≤200 字节；所有读写入口统一拒绝。
- **MCP `files[].action` 强校验**：只接受 `Created` / `Modified` / `Deleted`（缺省 `Modified`），
  其他值返回参数错误。
- **资源上限**：MCP 单行请求上限 16 MiB（超限返回 `-32600` 并继续服务；此前可被超长行 OOM）；
  fuzzy 正则编译限制 10 MiB 程序尺寸；`mem add` 改用 64 KiB 分块流式 SHA-256（不再整文件读入内存）。
- **渲染转义**：`mem render` 对表格单元格与时间线字段中的 `|`、换行转义，数据不再能破坏
  Markdown 结构或注入内容。

### 冲突解决

- **精确冲突标记判定（P0）**：只有 Git 精确形态的标记才会被删除（`=======` 必须整行恰好；
  `<<<<<<<` / `>>>>>>>` / `|||||||` 之后必须是行尾或空格）。以标记前缀开头的普通数据行不再被误删。
- **字节级保留**：`mem resolve` 改为字节级处理，非 UTF-8 字节同样逐字保留（"never silently
  dropping data" 现在对任意字节成立）；CRLF 换行正确处理。
- **多提交 rebase 一次完成（P1）**：`mem resolve` 现在循环执行「解决 JSONL → `git add` →
  `git rebase --continue`」（上限 100 轮），一次调用即可推进多个冲突提交；只有非 JSONL 文件仍冲突时
  才报错并列出需要手工处理的文件。
- **FileRecord 因果顺序（P2）**：去重不再按 `file_id` 重排历史，`files.jsonl` 投影的
  last-write-wins 语义不再可能被旧状态覆盖。
- 冲突解决全程持有 store 写锁，与追加/压缩/同步串行。

### 同步

- `sync` 收敛到 `mem_core::git::sync_store`，CLI 与 MCP 行为一致；全程持 store 写锁。
- **不再静默吞掉 pull 失败（P1）**：非冲突的 `git pull --rebase` 错误会中止同步并报错，
  不会再继续 `git push` 掩盖真实原因。
- **冲突时 CLI 非零退出**：`mem sync` 检测到冲突会提示运行 `mem resolve` 并以退出码 1 结束
  （此前退出 0，自动化脚本会误判成功）；MCP `mem_sync` 以 `isError` 结果返回同一提示。
- 去掉 `git pull --rebase --autostash`：同步前已提交全部改动并持写锁，autostash 只会在
  中途制造难以恢复的 stash 冲突；无 upstream 时跳过 pull。

### 存储

- **新增跨进程 store 写锁**：追加事件、`compact`、`resolve`、`sync` 全部串行；锁文件位于
  `$XDG_RUNTIME_DIR`（或系统临时目录），按 store 规范路径哈希命名，不进入 Git 仓库。
- **`mem compact` 重写为从事件日志重建投影（P1）**：持写锁读取全部事件 → 按时间排序 →
  每个 `file_name` 保留最后写入者 → 临时文件 + 原子 rename。因此可修复崩溃/并发导致的投影
  缺失或截断（自愈），并发追加的记录也不会再被覆盖丢失；`Records before/after/pruned` 输出不变。
- **移除 mmap（P1）**：所有读取改为普通 `fs::read`。此前并发 `resolve` 截断文件时，读取进程
  可能触发 `SIGBUS`/未定义行为；顺带消除全部 `unsafe` 块与 `memmap2` 依赖。

### 其他修复

- **Git 冲突检测（P2）**：只依据 `git ls-files -u`、porcelain 冲突码与 rebase/merge 状态文件，
  不再扫描全部跟踪文件查找 `<<<<<<<` —— 事件摘要或文档引用该字符串不会再导致永久误判冲突。
- **路径身份（P2）**：对存在的路径前缀做 `fs::canonicalize`（解析符号链接），不存在的尾部词法拼接；
  store 根与文件一侧使用软链路径时也能得到一致的相对身份（macOS `/var`→`/private/var` 等）。
- **ID 碰撞熵（P2）**：计数器以「启动时间 + pid」派生的随机偏移起步（**格式不变**），
  显著降低容器环境（pid 恒为 1、同纳秒启动）下的跨设备碰撞概率。
- **MCP 协议一致性（P3）**：校验 `jsonrpc == "2.0"`（否则 `-32600`）；工具参数反序列化失败返回
  `-32602`；工具执行期业务错误改为成功响应 + `result.isError = true`（便于 Agent 读取错误并自我纠正）；
  未知工具/方法 `-32601`、坏 JSON `-32700` 不变。

### 质量

- 测试从 55 个增加到 **84 个**（48 core + 13 CLI 集成 + 6 MCP 单元 + 17 MCP 集成），
  新增覆盖：项目名穿越、多提交 rebase、非 UTF-8 冲突、marker 前缀数据行、并发追加（8 线程 × 50）、
  compact 与 append 并发、投影自愈、render 转义、sync 冲突/无 upstream 退出码、MCP isError/错误码/
  超长行恢复、软链路径身份、冲突检测无误报。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo doc -D warnings` 全部通过。
- Release 二进制：`mem` 3.11 MB、`mem-mcp` 2.85 MB。

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
- **文档**：中英双语。英文为默认展示（`README.md` 与 `docs/` 下六份专题文档），中文在
  `README.zh-CN.md` 与 `docs/zh-CN/`；每篇顶部都有语言切换行，两版本章节、代码块与链接保持
  一一对应（含可实测的迁移脚本）。
- **工程**：GitHub Actions CI（fmt / clippy `-D warnings` / test / release build）、
  MIT License、`rustfmt.toml`。
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
