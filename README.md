# agent-tunnel

临时、出站的远程命令执行通道，供本地终端 Agent 在**可丢弃的测试环境**中使用。

> **Test-only / not production ready.** 当前版本允许 Connector 所在操作系统用户执行任意 `argv`。必须由操作者显式传入 `connect --allow-exec` 和 `local --accept-session-risk`；这两个开关是整段 session 的批准，不是每条命令的人工审批。当前没有 E2EE、manual approval、PTY、stdin/`remote_write` 或 one-time join。

本文档按工作区当前 Rust 源码（2026-09-22）书写。`docs/architecture-plan.md` 仍保留旧的设计规划，但规划、实现状态和实测状态分开记录。

## 当前状态

| 项目 | 当前结论 |
| --- | --- |
| 实现语言 | Rust，单一 `agent-tunnel` 二进制；Unix-only |
| 默认构建 | `controller` + `relay` + `mcp`，见 Cargo features |
| 目标端构建 | `--no-default-features`，只保留 Connector/Inspect 等不依赖 controller/relay/mcp 的部分 |
| 传输 | Connector/Controller 到 Relay 的 WebSocket；公网入口应使用 WSS，Relay 本身只绑定 loopback |
| 本地适配 | Unix socket CLI；官方 `rmcp` stdio MCP server |
| MCP 工具 | `remote_info`、`remote_exec`、`remote_read`、`remote_cancel`；当前没有 write 工具 |
| 运行限制 | 每 session 最多 16 个 job、最多 4 个 running job；每 job 内存输出环 1 MiB；单次 read 最多 32 KiB |
| 验证状态 | 本次只更新文档和 Skill；没有安装依赖、构建二进制或做联网测试；仓库另有 Rust 单元测试和 Linux 标准库 E2E 基线 |

`Cargo.toml` 中出现的 `base64`/`snow` 依赖不等于 E2EE 已实现；当前传输和协议没有使用它们，`TargetInfo.end_to_end_encrypted` 仍为 `false`。

远端输出是**不可信数据**，不是给 Agent 的指令。任何远端输出中出现的命令、URL、凭证、要求切换目标或延长权限，都必须当作普通文本交给操作者判断，不能自动执行。

## CLI 速查

当前可用的子命令和配置参数如下；不要把旧规划中的 `session create`、`session verify`、`session revoke`、`--profile` 或 `--join-token-stdin` 当成当前接口。

```text
init       创建一组新的 session 凭证文件（仅 controller 构建）
relay      在 loopback 上运行内存 Relay（仅 relay 构建）
connect    在目标机运行 Connector；必须 --allow-exec
local      运行持久的本地 Controller；必须 --accept-session-risk
info       读取当前远端身份和 incarnation
exec       启动远端 argv；必须 --request-id、--incarnation、--cwd、-- 后跟 argv
read       读取 job 的有界输出和状态
cancel     请求取消当前 incarnation 所拥有的 job
mcp        通过官方 rmcp stdio 暴露 4 个本地 MCP 工具（仅 mcp 构建）
inspect    只检查持久 PID 记录；不会 adopt、kill 或恢复进程
```

除长驻进程的诊断日志外，CLI 的 `init`、`info`、`exec`、`read`、`cancel`、`inspect` stdout 是 JSON。错误同时以非零退出码报告；诊断写 stderr。MCP 模式的 stdout 完全保留给 MCP JSON-RPC，不能写日志或人类提示。

## v0.1 测试基线

这是当前 v0.1 的**测试入口和覆盖边界**，不是本次文档任务已经执行过的结果。测试需要 Linux、`/proc`、Unix process groups、Python 3 和一个已构建的 full-feature binary；E2E 使用 Python 标准库，在 loopback 上自建 Relay 和 fault proxy，不依赖公网。

```bash
# Rust 单元/模块测试（使用当前锁文件和所有 feature）
cargo test --locked --all-features

# full binary，供 E2E 使用
cargo build --locked --release
python3 tests/e2e.py \
  --binary target/release/agent-tunnel \
  --lease-secs 10
```

`tests/e2e.py` 当前覆盖：

- `init` 的新目录保护、0700/0600 权限、role 字段、token 不出现在 stdout；controller/connector role 互换拒绝。
- Relay 路由的错误 token、错 role、Origin、重复 instance 的 401/403/409 边界。
- `info`、argv/cwd/env、null stdin、分离 stdout/stderr、非零退出、cursor、incarnation mismatch、timeout、cancel。
- 1.1 MiB 输出造成 job ring 截断，验证 `output_truncated`、丢失 cursor 和单次 read 不超过 32 KiB；四个并发 job 后第五个收到 `RESOURCE_LIMIT`。
- fault proxy 断开后的连接重建：同一 request ID 返回同一 job、计数文件仍为 1、不同参数返回 `REQUEST_CONFLICT`，不自动重跑。
- controller 退出后的 lease 回收、Connector 正常退出时的 job 清理、PID record 字段和 0700/0600 状态权限。这里是受控退出路径，不是 crash recovery 或 stdio 恢复承诺。
- rmcp stdio 的 JSON-RPC initialize/tools/list/tools/call；探测 `2025-06-18` 后再尝试 `2025-11-25`，一次运行记录实际协商到的版本，不宣称两个版本都已同时验收。

Rust `state` 单元测试还覆盖 Linux `boot_id`/`start_ticks`、带 `)` 的 `/proc/<pid>/stat` comm、独占 state lock，以及 `matches`/`not_running`/`unknown` 检查结果。测试通过也不代表生产安全、E2EE、PTY、公网 WSS 或目标架构兼容。

`scripts/package.sh`（由操作者明确运行时）构建 full 和 connector-only 两个分发包，并打印实际 binary/archive 字节数；本仓库不在文档中编造体积数字。`tests/e2e.py` 和打包脚本本身不在本次文档写入范围内。

## 可执行快速开始：同一台 Linux 测试机

下面是 5 个终端的最小闭环。它只使用 loopback Relay 和无敏感数据的测试命令；多主机场景见下一节。先构建二进制，且不要把 `connector.json` 或 `controller.json` 内容粘到命令行、聊天、日志或 issue 中。

### 0. 构建并初始化 session

```bash
cargo build --locked --release

BIN="$PWD/target/release/agent-tunnel"
mkdir -p -m 700 "$PWD/.agent-tunnel"
SESSION="$PWD/.agent-tunnel/demo-20260922" # 该目录必须尚不存在
"$BIN" init \
  --dir "$SESSION" \
  --relay http://127.0.0.1:8787 \
  --name demo-target \
  --ttl-secs 3600
```

`init` 创建一个新的 0700 目录和三个 0600 文件：

```text
$SESSION/relay.json       # Relay 侧：session 元数据和 connector/controller token hash
$SESSION/controller.json  # 本地 Controller：controller role 的明文 token
$SESSION/connector.json   # 目标 Connector：connector role 的明文 token
```

目录必须是新目录，因为 `init` 不递归创建 session 目录，也不会覆盖已有文件。stdout 只包含 `session_id`、`target_id`、过期时间和目录，不包含 token。两个明文 token 只存在于各自 role 文件；只把 `connector.json` 经已有可信运维渠道交给目标，不要把完整 session 目录复制到目标。

### 1. 终端 A：启动 Relay

```bash
"$BIN" relay \
  --listen 127.0.0.1:8787 \
  --session-file "$SESSION/relay.json"
```

Relay 只绑定 loopback。要从另一台机器使用，需由操作者另行提供获准的 TLS/WSS 入口或反向隧道；不要让本项目自动修改 cloudflared、系统代理或全局网络配置。

### 2. 终端 B：启动 Connector

同机测试：

```bash
"$BIN" connect \
  --config "$SESSION/connector.json" \
  --state-dir "$SESSION/connector-state" \
  --lease-secs 60 \
  --allow-exec
```

`--allow-exec` 是明确的 session-wide 任意执行许可，缺少它 Connector 会拒绝启动。`connector-state` 会创建为 0700，状态和锁文件为 0600。目标机只需要 `connector.json` 和二进制；Controller 文件、Relay 文件和本地 socket 不应复制过去。

### 3. 终端 C：启动 Local Controller

```bash
"$BIN" local \
  --config "$SESSION/controller.json" \
  --socket "$SESSION/controller.sock" \
  --accept-session-risk
```

`--accept-session-risk` 是明确接受当前 test-only 风险的 session-wide 开关；它不提供 per-command manual approval。Unix socket 会被设为 0600，父目录必须是当前用户拥有且无 group/other 权限的 0700 目录。

### 4. 终端 D：确认身份，然后执行远端 argv

先查看完整 JSON，人工复制当前 `incarnation`：

```bash
"$BIN" info --socket "$SESSION/controller.sock"
INCARNATION='<把 info JSON 中当前 Connector 的 incarnation 原样填入>'
```

执行必须明确给出 request ID、incarnation、绝对远端 cwd，并在 `--` 后给 argv：

```bash
"$BIN" exec \
  --socket "$SESSION/controller.sock" \
  --request-id demo-exec-001 \
  --incarnation "$INCARNATION" \
  --cwd /tmp \
  -- /bin/echo agent-tunnel-ok
```

`exec` 的 JSON 会返回 `job_id`、PID/PGID、incarnation 和初始输出。复制其中的 `job_id` 后读取：

```bash
JOB_ID='<把 exec JSON 中的 job_id 原样填入>'
"$BIN" read --socket "$SESSION/controller.sock" --job "$JOB_ID" --cursor 0
```

长任务也只能通过显式 argv 启动，例如：

```bash
"$BIN" exec \
  --socket "$SESSION/controller.sock" \
  --request-id demo-sleep-001 \
  --incarnation "$INCARNATION" \
  --cwd /tmp \
  --timeout-ms 60000 \
  -- /bin/sh -c 'sleep 30'
```

这里 `/bin/sh -c` 是远端 argv 的显式第一项；本工具不会替你拼 shell。需要取消时使用 `cancel`，再用 `read` 确认：

```bash
"$BIN" cancel --socket "$SESSION/controller.sock" --job "$JOB_ID"
"$BIN" read --socket "$SESSION/controller.sock" --job "$JOB_ID" --cursor 0
```

### 5. 不确定响应时的唯一安全规则

如果 `exec` 的响应因断网、超时或 Controller/stdio 退出而不确定，不要换一个 request ID 重跑。使用**同一个 request ID 和完全相同的参数**重试查询路径；当前 Connector incarnation 内会按 request ID 和参数去重：相同参数返回原 job 视图，参数不同返回 `REQUEST_CONFLICT`。网络层不会自动 replay，也不会自动重跑命令。若 Connector 已崩溃或 incarnation 已变，不能假设旧的内存去重表还在，应创建新 session 并由操作者核对副作用。

## 多终端、多主机启动顺序

推荐把长驻角色分在独立终端，并保留 stderr 日志：

```text
终端 1（本地开发机）：agent-tunnel relay --listen 127.0.0.1:8787 --session-file relay.json
终端 2（目标机）：    agent-tunnel connect --config connector.json --allow-exec
终端 3（本地开发机）：agent-tunnel local --config controller.json --socket controller.sock --accept-session-risk
终端 4（本地/Agent）：agent-tunnel info/exec/read/cancel --socket controller.sock ...
终端 5（Codex）：      agent-tunnel mcp --socket controller.sock
```

顺序是：先在本地 `init`，再让 Relay 读取 `relay.json`，然后将仅限目标 role 的 `connector.json` 交给目标并启动 Connector，最后在本地使用 `controller.json` 启动 Controller。Controller 和 Connector 都主动连 Relay；目标不监听入站端口。

跨主机时，`init --relay` 应指向获准的 `https://`/`wss://` 入口。Relay 进程仍只绑定 loopback，TLS 终止/隧道由外部组件负责。不要把 token 放到 URL query、Bearer 参数、shell history 或 Skill。若未来得到明确的网络测试授权且 DNS/路由失败，可以尝试在已有环境中用 `mgraftcp <原命令>` 运行诊断；本项目不安装它，也不修改系统代理。

## 构建变体与体积优化

### full

默认 feature 是 `controller`、`relay`、`mcp`；完整本地控制面、Relay 和 MCP 入口：

```bash
cargo build --locked --release
# 等价地显式指定默认角色 feature：
cargo build --locked --release --features controller,relay,mcp
```

### connector-only

目标机不需要 controller、relay、rmcp 时：

```bash
cargo build --locked --release --no-default-features
```

该变体保留 `connect` 和 `inspect` 等公共/目标侧代码，去掉 feature-gated 的 Local Controller、Relay 和 MCP server。目标二进制应从该构建产物复制；不要把 full 二进制和另一套凭证混用后再猜测权限边界。

### 体积说明

`Cargo.toml` 的 release profile 已启用 `opt-level = "z"`、`lto = "fat"`、`codegen-units = 1`、`panic = "abort"` 和 `strip = "symbols"`。connector-only 通过 feature gating 减少代码路径和依赖，full 则包含 Relay/Controller/rmcp；最终字节数仍取决于目标架构、Rust/Cargo 版本和链接器。**本项目没有在本次文档更新中测量体积，不给出未经测量的 MB 数字。**在目标环境实际构建后再记录：

```bash
stat -c '%n %s bytes' target/release/agent-tunnel  # GNU/Linux
```

## 凭证、权限与 session 生命周期

- `init` 每次生成一个新的 session ID、target ID、controller token 和 connector token，并写出 `relay.json`、`controller.json`、`connector.json` 三个 0600 文件。
- token 是 role-scoped：Connector token 只能尝试 `/v1/connect/<session_id>`，Controller token 只能尝试 `/v1/control/<session_id>`；Relay 保存 hash 并按 role 校验。
- 当前没有 one-time join。endpoint 文件中的 token 在 TTL 内可被对应 role 使用；没有自动 verify/revoke 子命令。手工删除/隔离凭证和创建新 session 是当前撤销/轮换边界。
- token 不能出现在命令行、环境变量、URL、stdout、MCP 工具结果、Skill 或远端输出中。示例只使用文件路径和占位符。
- `incarnation` 在每次 Connector 进程启动时随机生成。`target_id`/session ID 相同不代表仍是同一个 Connector 进程。
- Connector 或 Controller 进程重启会产生新的实例身份；不要把它当作可恢复 session，按“新 session”处理。Relay 仍在运行时，旧 role binding 会拒绝新的 instance。
- Relay 自身重启时，其内存中的 peer binding、路由和待转发消息会丢失；`relay.json` 中的 hash 没有因此自动轮换。存活的 endpoint 可能用原 instance 重新绑定，但这不是持久恢复承诺，未确认的 request 仍可能是 unknown。运维上应把 Relay 重启当作需要重新创建/配对新 session 的事件，而不能依赖旧 job 恢复。

## Job、PID 与断网语义

每个 job 的持久记录包含：`session_id`、`target_id`、`incarnation`、`job_id`、`request_id`、PID、PGID、Linux `boot_id`、`process_start_ticks`、状态、退出码和终止原因。`JobView` 也带回 target/incarnation/job/PID/PGID/start ticks，便于操作者判断“同名主机”是否已换代。

- `boot_id` 来自 `/proc/sys/kernel/random/boot_id`，`start_ticks` 来自 `/proc/<pid>/stat` 的 process starttime；二者只用于识别 PID 是否可能对应原进程。
- `inspect --state-dir` 会将记录与当前 Linux 观察结果比较，并明确输出 stale/unknown 风险。记录是证据，不是 kill/adopt 权限。
- Connector 只会对自己当前 `Command` 创建的进程组发信号；代码不会根据重启后恢复的 PID 记录直接杀进程，也不会自动 adopt 外部进程。进程组不是 cgroup，不能保证清理 daemonize/setsid 后代。
- 传输短暂断开时，当前 Connector 内存中的 job 和输出环保留，Controller/Connector 在 lease 内尝试重连。Controller 每秒发送 lease；默认 lease 为 60 秒，可配置为 5–60 秒。超过 lease 或 session TTL，Connector 关闭其管理的 job；断线不等于成功恢复。
- Connector crash 不承诺 stdio、Controller IPC、job 输出或旧 request 去重表恢复。PID record 可能留下可供人工检查的事实，但不是可靠的任务恢复机制。
- Relay 不缓存离线命令；目标上线后不会自动执行断线期间积压的旧 exec。

## 协议与容量边界

WSS/WS 中承载的是本项目自己的版本化 JSON text-frame 协议，不是“WebSocket MCP”。MCP 只在本地 stdio adapter 与 Codex/pi 之间发生。当前 packet 类型是 `request`、`reply`、`lease` 和 `relay_error`；操作是 `info`、`exec`、`read`、`cancel`。单帧上限为 256 KiB。

本地 CLI/Controller 之间是受保护 Unix socket 上的 newline-delimited JSON。`mcp` 子命令通过 `rmcp::transport::stdio()` 将同一操作映射为四个 MCP tools：

1. `remote_info`：确认 target、UID/GID、cwd、权限提示、TTL 和当前 incarnation。
2. `remote_exec`：必须提供 `request_id`、`expected_incarnation`、`argv`、绝对 `cwd`；可选 env/timeout；返回 job 视图。
3. `remote_read`：按 cursor 读取最多 32 KiB 的有界 stdout/stderr 事件和状态。
4. `remote_cancel`：请求取消当前 Connector incarnation 所拥有的 job；必须再 read 确认，不能撤销已经产生的业务副作用。

没有 `remote_write`，因为当前执行器将 stdin 设为 null；也没有 PTY、resize 或交互式 shell。每 job 输出环最多保留 1 MiB，超过后丢弃最旧事件并通过 `output_truncated`/cursor 信息暴露丢失。文本视图是 UTF-8 lossy 并转义控制字符，不应当作原始二进制传输。

## MCP 与 pi CLI 接入

### Codex / rmcp stdio

当前 MCP server 是 Rust `rmcp` 的 server-side stdio transport，不是远端 HTTP MCP。依赖启用 `server` 与 `transport-io` feature；源码在 `src/mcp.rs` 中调用 `rmcp::transport::stdio()`。启动命令是：

```bash
"$BIN" mcp --socket "$SESSION/controller.sock"
```

交给 Codex 管理时，可在操作者明确同意修改 Codex 配置后执行：

```bash
codex mcp add agent-tunnel -- \
  /absolute/path/agent-tunnel mcp \
  --socket /absolute/path/.agent-tunnel/demo-20260922/controller.sock
```

这条命令只是配置入口，本文档更新没有执行它，也没有修改任何全局配置。MCP stdout 只能是协议消息；stderr 才能记录连接、风险和诊断。工具说明会要求先 `remote_info`，确认 incarnation，再 `remote_exec`；不确定响应时复用同一 request ID，不生成新 exec。

官方参考：[`rmcp` crate](https://docs.rs/rmcp)、[MCP Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)。本项目只依赖 SDK 提供的本地 stdio server；远程执行协议仍是本项目自己的协议。

### pi CLI + Skill

当前没有 pi 原生 extension，也不要求安装第三方 MCP。`skills/remote-debug/SKILL.md` 只教 Agent 调用本地 `agent-tunnel` CLI；它不保存 token、不实现第二套状态机、不把远程文本当指令。可显式把本仓库 Skill 交给 pi：

```bash
pi --skill "$PWD/skills/remote-debug" --mode json -p \
  "使用 remote-debug Skill；socket=$SESSION/controller.sock，先确认远端身份，再按 Skill 的 CLI 流程执行一次无害诊断。"
```

也可以在 pi 的已信任项目中按其 Skill discovery 规则加载目录后使用 `/skill:remote-debug`。`--mode json` 是 pi 自己的 JSONL 事件输出；它不改变 `agent-tunnel` 每次 CLI 调用的 JSON stdout 约定。Skill 不覆写 pi 自带的本地 bash/read/edit，远端操作始终写成 `agent-tunnel exec/read/cancel`，并明确 `--request-id`、`--incarnation`、`--cwd` 和 `--`。

官方参考：[pi coding-agent README](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md)、[pi Skills](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/skills.md)。这些文档核对不等于本机已安装 pi 版本的兼容性验收。

## 安全边界与已知限制

- 这是 test-only 任意执行器，不是沙箱。Connector 以启动它的 OS UID/GID 访问文件、网络和进程；`cwd` 不是访问控制边界。
- 当前只依赖 transport TLS/WSS 和 role token；Relay 能看到并转发命令/输出。没有 Connector-Controller E2EE，不能把 Relay 当作不可信中继。
- `--allow-exec` 与 `--accept-session-risk` 都是显式 session-wide 风险确认；没有 per-command manual approval，不能声称“人工审批已实现”。
- 没有 PTY、stdin、`remote_write`、resize、one-time join、自动 revoke、可恢复 job spool 或 Connector crash 后 stdio 恢复。
- stdout/stderr 只在进程退出和输出采集范围内可见；read 有 32 KiB 上限，job ring 有 1 MiB 上限；输出可能截断、丢失或在断线时变为 unknown。
- Relay 是内存状态，当前最多加载 16 个 session 文件、最多 64 个连接 worker；没有 HA、持久路由或离线队列。Relay 重启不能恢复 pending reply。
- 取消是请求，不是已经终止的证明；必须 read 查看终态。任何已经发生的文件、数据库、网络副作用都不能由 cancel 回滚。
- 不要用 PID 文件推断可以安全 kill；Linux PID 复用、boot_id/start_ticks 缺失或容器 PID namespace 都可能使身份只能是 unknown。
- 本次没有构建、实网、目标架构、代理、WSS、Codex 或 pi 版本兼容性结果。文档中的命令是按当前 CLI 源码整理的操作路径，不是运行验收报告。

## 后续路线（不代表已实现）

1. 在隔离测试目标完成 full/connector-only 构建和本地/获准 WSS 闭环，记录架构、工具链、入口和脱敏日志。
2. 增加真正的 session revoke、one-time join、身份确认和人工 per-command approval；先定义状态机再改 CLI。
3. 为 Relay 不可信模型设计独立的 Controller-Connector E2EE 和重放保护，保留当前 token role scope。
4. 选择 cgroup/容器级清理边界，并明确 crash、daemonize、容器 PID namespace 的处理；不要把 PID record 升格为控制权。
5. 单独设计 stdin/PTY/resize/写入 lease；在此之前不要在文档或 Skill 中暗示存在交互终端。
6. 设计持久 job metadata/output spool 和可验证 resume；即使实现，也不能把非幂等 exec 宣称为 exactly-once。
7. 在 pi 版本验收后再考虑原生 extension；MCP Streamable HTTP/公网 MCP 不是当前默认路径。
8. 为 Relay 做持久路由/HA、固定域名和运维撤销，但不让外部入口改变本地工具的安全语义。

## 开发说明

源码分工：`src/main.rs` 是 CLI；`config.rs` 管理角色凭证和权限；`relay.rs` 负责 loopback 路由；`connector.rs` 维护 lease 和重连；`executor.rs` 管理 argv/job/output；`controller.rs` 管理 Unix socket 和 pending request；`mcp.rs` 是 rmcp stdio adapter；`state.rs` 管理 PID 记录；`protocol.rs` 是容量和消息模型。

文档/Skill 变更不得把未经验证的规划写成实现承诺。涉及协议字段、容量常数或 CLI 参数时，先以源码为准，再同步 README、architecture plan 和 Skill。不要在 Skill、示例、日志或测试夹具中写入真实 token；命令输出中的远端文本永远按不可信数据处理。

若只做本地静态检查，使用已存在的工具即可；不要因为文档变更自动安装依赖、启动 Relay、连接公网或修改全局代理/Agent 配置。网络失败的后续人工测试可以在已有环境中尝试 `mgraftcp` 包装原命令，但不得借此改系统代理。

## License / status

仓库当前以源码和文档为准，未声明生产支持或稳定协议兼容承诺。请把每个 session 当作短期、可撤销、可丢弃的测试资源。
