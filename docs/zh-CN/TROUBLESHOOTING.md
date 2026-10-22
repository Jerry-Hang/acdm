[English](../TROUBLESHOOTING.md) · **简体中文**

# 故障排查

> 读者：正在使用 `mem` CLI 或 `mem-mcp` 时遇到问题的用户。本文按「症状」组织；
> 设计层面的取舍与边界见 [DESIGN.md](DESIGN.md)，升级/兼容问题见 [MIGRATION.md](MIGRATION.md)。

## 快速索引

| 症状 | 最可能原因 | 处理 |
| --- | --- | --- |
| 记录写到了别的目录 / 找不到 store | store 定位与预期不一致 | [§1](#1-store-定位与预期不一致) |
| `query -n` 查不到刚记录的文件 | 旧绝对路径记录，或项目名/路径不一致 | [§2](#2-查询不到记录) |
| 时间线顺序看起来不对 | 手工编辑过 JSONL，或 ts 非法 | [§3](#3-时间线顺序看起来不对) |
| `mem sync` 报错 | 没有 remote / 没有 upstream / 认证 / 冲突 | [§4](#4-mem-sync-失败) |
| sync 提示 conflicts | JSONL 合并冲突 | [§5](#5-冲突处理) |
| `compact` 后记录数变少 | 设计如此（去重） | [§6](#6-compact-后记录变少) |
| `README.md` 被覆盖 | 它是生成文件 | [§7](#7-项目-readmemd-被覆盖) |
| MCP 客户端连不上 / 工具报错 | 配置路径、协议、参数 | [§8](#8-mcp-客户端问题) |
| 查询变慢 | 索引每次重建 O(N) | [§9](#9-性能问题) |
| 怀疑数据损坏 | 断电/并发写导致截断行 | [§10](#10-数据健康检查) |

## 1. store 定位与预期不一致

CLI 与 MCP 使用**同一套**定位规则（`mem-core/src/util.rs`）：

1. 显式 `--path <dir>`（MCP 也支持 `--path <dir>` / `--path=<dir>`）
2. 环境变量 `ACDM_STORE`
3. 从当前工作目录向上查找含 `.mem` 文件或 `events/` 目录的祖先
4. 以上都没有：当前工作目录

`mem init` 是例外：它只接受显式 `--path`，否则就在当前目录初始化，不会向上搜索。

排查：

```bash
# 当前目录会解析到哪个 store？（list 会打印内容；没有 store 时会显示空/报错线索）
mem list
# 显式指定，确认是不是你想要的那个
mem --path /path/to/store list
# 看标记文件与事件目录是否存在
ls -la /path/to/store/.mem /path/to/store/events
```

常见坑：在 store 的子目录里运行但想写到「项目仓库」而不是 store——因为向上搜索会命中 store，
此时应显式 `--path`，或用 `ACDM_STORE` 固定。MCP 客户端建议始终显式配置 `--path`。

## 2. 查询不到记录

`mem query -n <file>` 按 `file_name` **精确匹配**（不是子串匹配，大小写敏感，分隔符是 `/`）。
先看记录里实际存了什么：

```bash
mem --path /path/to/store list -p <project>
```

- 如果显示的是绝对路径（例如 `/home/alice/repo/src/main.rs`），说明是旧版本写入的数据：
  读取端不会自动改写，按相对名查不到。按 [MIGRATION.md](MIGRATION.md) 的「迁移 B」处理。
- 如果显示 `src/main.rs`，确认查询时的目录分隔符与大小写完全一致。
- 确认 `-p` 项目名一致（项目名是写入时的逻辑名，不是目录名）。
- 刚写入就查不到：确认写到了同一个 store（§1），并检查 `events/` 下当月的文件是否新增了行。

## 3. 时间线顺序看起来不对

- ACDM 对时间排序是**解析后比较**，不是字符串比较，因此旧数据两种格式混存也能排对。
- 如果 `ts` 完全无法解析：旧版本会把它写进 `events/default.jsonl`。该文件不是合法月份分片，
  「最近事件」查询会忽略它；`mem render` 的时间线也只包含月份分片中的事件。
- 手工编辑过 JSONL、把 `ts` 改坏，也会导致该条被读取端跳过（读取容忍坏行）。
- 同一秒内的事件顺序由事件时间戳（纳秒）决定；若时间戳相同，再按 `event_id` 排序。

## 4. `mem sync` 失败

`mem sync` 与 MCP `mem_sync` 共用同一实现（`mem_core::git::sync_store`），全程持有 store 写锁：
脏工作区自动 commit（提交信息 `Auto-sync from <device> at <timestamp>`）→
`git pull --rebase`（不再带 `--autostash`）→ `git push`。无 upstream 时跳过 pull；
pull 失败若**不是冲突**会直接中止（不再继续 push 掩盖原因）；检测到冲突时 CLI 打印提示并以
**退出码 1** 结束，MCP 返回 `result.isError = true`。

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `git pull ... failed: ... no tracking information` / `no upstream` | 本地分支没设置上游（sync 会跳过 pull，但 push 仍会失败） | `git -C <store> push -u origin <branch>`（先手动推一次） |
| `git push ... failed: ... No configured push destination` | 没配置 remote | `git -C <store> remote add origin <url>`，或 `mem init --remote <url>` 重新初始化 |
| `rejected` / `non-fast-forward` | 远端有新提交且本地未成功 rebase | 先 `git -C <store> pull --rebase`，冲突按 §5 |
| `Permission denied (publickey)` / `Authentication failed` | Git 凭据问题 | 修 SSH key 或 HTTPS 凭据；ACDM 不管理凭据 |
| `could not apply ...` / 提示 conflicts | JSONL 合并冲突（sync 退出码 1） | 见 §5 |
| `git pull --rebase failed and the repository is not conflicted; aborting sync...` | 非冲突的 pull 失败（网络、认证、脏工作区等） | 按提示先修好 git 状态再重试；不会再继续 push |

注意：如果 store 根本不是 Git 仓库（只用了 `mem init`，没有 `git init`），`sync` 在第一步
`git status` 就会失败；如果只是没有 remote/upstream，sync 会跳过 pull，但 push 仍会失败。
此时改用本地用法即可，或补建仓库/remote。

## 5. 冲突处理

跨设备同步时，两个设备同时追加同一个 `events/YYYY-MM.jsonl` 是很常见的，Git 会把它合并成
冲突标记。处理：

```bash
mem --path /path/to/store sync     # 冲突时退出码 1，提示运行 mem resolve
mem --path /path/to/store resolve  # 自动解决 JSONL 冲突（含多提交 rebase）
mem --path /path/to/store sync     # 再次同步
```

`mem resolve` 的行为（详见 [ARCHITECTURE.md](ARCHITECTURE.md#7-同步与冲突解决)）：

- 只删除 Git **精确形态**的标记：`=======` 必须整行恰好是七个 `=`，`<<<<<<<` / `>>>>>>>` / `|||||||`
  之后只能是行尾或空格；以标记前缀开头的普通数据行会被保留；
- 按**字节**处理：非 UTF-8 字节与 CRLF 换行都逐字保留，不再因为文件不是合法 UTF-8 而失败；
- 同 `event_id` / `file_id` 的记录**后写覆盖**（歧义时保留文件里靠后的版本）；`FileRecord` 保持因果顺序，
  不再按 `file_id` 重排；
- 能解析的 `Event` / `Generic` 记录按时间排序后写回；**无法解析的行逐字保留**（放在文件末尾）；
- 写回后 `git add`；只有确实存在 rebase 状态（`.git/rebase-merge` / `rebase-apply`）时才
  `git rebase --continue`，并循环推进多个冲突提交（上限 100 轮），与系统语言无关；
- 整个操作持有 store 写锁，与追加/压缩/同步串行。

排查：

- 输出里 `Lines preserved` 非零：说明有无法解析的行（通常是写入中断产生的截断行）。这是
  **有意的数据保护**，不是错误。确认内容后用编辑器删掉或无碍保留。
- 报 `git add ... failed: warning`：文件可能被 `.gitignore` 忽略或路径特殊。
  检查 `git -C <store> check-ignore -v events/2026-10.jsonl`，必要时 `git add -f`。
- resolve 后仍然冲突：看 `git -C <store> status`；确认冲突文件在仓库内、`.jsonl` 后缀、
  且当前处于 rebase/merge 状态。剩余冲突通常意味着存在**非 JSONL 文件冲突**，
  最常见的是 `projects/<project>/meta.json`：两台设备同时首次为同一项目写入元数据时，
  两边都创建了 `meta.json`，JSONL 可以自动合并，而 `meta.json` 不行。
  `mem resolve` 会先解决 JSONL 并循环推进 rebase，直到只剩非 JSONL 文件时停下，
  然后打印一条**可操作的错误**，列出非 JSONL 文件与所有仍未解决的路径。恢复方式：

  ```bash
  git -C /path/to/store status --short          # 找到 UU 的 meta.json
  # 内容基本一致（name/description 相同，只有 created_at 不同），任选一侧即可：
  git -C /path/to/store checkout --theirs projects/<project>/meta.json
  git -C /path/to/store add projects/<project>/meta.json
  git -C /path/to/store rebase --continue
  ```

  或直接编辑该文件删除 `<<<<<<<` / `=======` / `>>>>>>>` 三行后 `git add`。
  只有在 `git rebase --continue` 成功、rebase 状态目录消失后，`mem sync` 才能继续。

## 6. `compact` 后记录变少

`mem compact -p <project>` 从**事件日志**重建 `files.jsonl` 投影：按时间顺序重放所有事件，每个
`file_name` 只保留最后写入者，所以记录数减少是预期行为（输出会给出 before / after / pruned）。
它保留 `Deleted` 记录，并会自愈崩溃/并发导致的投影缺失或截断；全程持有 store 写锁，可与追加并发执行。
执行后 `sync` 一次。

## 7. 项目 README.md 被覆盖

`projects/<name>/README.md` 是 `mem render` **生成**的文件，每次渲染都会重写。不要手工编辑它；
需要额外说明请写在别的文件里，或修改渲染逻辑。

## 8. MCP 客户端问题

- **服务器不响应**：确认客户端配置里 `command` 指向真实存在的 `mem-mcp`（建议绝对路径），
  `args` 形如 `["--path", "/path/to/store"]`。MCP 使用 stdio + **逐行** JSON-RPC 2.0；
  不需要 `Content-Length` 头。
- **启动即退出**：未知启动参数会打印 usage 并 `exit 2`（防止拼写错误静默用错 store）；
  确认只传 `--path`。
- **工具调用返回 error**：先区分协议错误与业务错误：
  - `-32700` 请求不是合法 JSON；
  - `-32600` `jsonrpc` 不是 `"2.0"`，或单行请求超过 16 MiB（超限后 server 会丢弃该行并继续服务）；
  - `-32601` 未知方法或未知工具；
  - `-32602` 工具参数不匹配 schema（缺字段、类型错、`action` 不是 `Created`/`Modified`/`Deleted` 等）；
  - **业务失败**（git 冲突、非法查询维度、不安全项目名、正则错误等）不再返回 JSON-RPC error，
    而是成功响应 + `result.isError = true`，错误文本在 `result.content[].text`；
    `-32603` 保留给真正的服务端故障，当前版本不会出现。
- **工具执行失败但 server 还活着**：这是预期行为，读 `isError` 文本、修正参数后重试即可。
- **找不到 store / 事件写到奇怪位置**：MCP 启动时会 `init`（缺 store 时在解析出的根目录自建）。
  建议显式 `--path` 或设置 `ACDM_STORE`，避免依赖客户端进程的 CWD。
- **多客户端并发**：每个客户端各自启动一个 `mem-mcp` 进程；写操作由**跨进程 store 写锁**（`fs2`，
  锁文件在运行时/临时目录、不进仓库）串行化，多进程同时写是安全的；查询无锁（普通文件读取，不再用 `mmap`）。

## 9. 性能问题

- 每次 `mem query` 都会**从头构建内存索引**（扫描全部 `events/*.jsonl`），复杂度 O(事件数)。
  事件量到几十万条时单次查询会明显变慢；这是当前设计的已知限制（见 [DESIGN.md](DESIGN.md)）。
- `mem query --fuzzy` 是正则逐行扫描，比其它查询更慢；尽量收窄正则。正则编译程序上限 10 MiB，
  超大正则会报 `Compiled regex exceeds size limit of 10485760 bytes.`；`mem add` 以 64 KiB 分块
  流式计算 SHA256，超大文件也不会整文件读入内存。
- `mem render` 只取最近 10 条事件，不受历史总量影响。
- 建议定期 `mem compact`；旧月份的 `events/YYYY-MM.jsonl` 可以归档到仓库外
  （注意：归档后 `mem render` 时间线与最近事件将不含这些月份，属于预期）。

## 10. 数据健康检查

检查 JSONL 中无法解析的行、事件总数与文件列表：

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

- `unparseable > 0`：通常是写入中断产生的截断行，读取端会跳过；可用 `mem resolve`（会逐字保留）
  或手工删行处理。
- 读取端对坏行的容忍意味着「能 `list` 出来」不等于「数据完整」，建议保留备份。

## 11. 提交问题前的诊断信息

请附上：

```bash
mem --version
mem --path /path/to/store list
git -C /path/to/store status --short
ls -la /path/to/store /path/to/store/events
```

以及出现问题的命令、完整 stderr、MCP 客户端配置（隐去敏感路径）与 `initialize` 响应。

## 12. 已知限制

- 无并发多写者事务：写操作由 store 跨进程写锁串行，单机并发安全，但跨设备一致性仍是「最终一致 + 去重」，没有分布式事务。
- 事件记录里的 SHA-256 由写入方提供（CLI 会计算；MCP 调用方可以传空）。
- 非 UTF-8 路径：可以写入与读取；`resolve` 已按字节处理，但 `git add` 的 stage 路径会以 lossy 形式传给 Git。
- 没有内置的鉴权与加密：store 的安全性依赖 Git 远端权限与磁盘权限；项目名经过安全校验，文件路径与摘要仍需自行审查。
