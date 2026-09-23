# agent-tunnel

`agent-tunnel` 是一个 Unix-only、临时、出站的远程命令执行通道，供本地终端 Agent 在**可丢弃的测试环境**中使用。Connector 运行在目标机上，Controller 和可选的 MCP stdio adapter 运行在本地，Relay 只负责认证后的连接转发。

> **Test-only / not production ready.** Connector 代表启动它的操作系统用户执行 `argv`，不是沙箱。`--allow-exec` 和 `--accept-session-risk` 是整段 session 的显式风险确认；默认 Controller 还会尝试通过独立 `/dev/tty` 对 `exec`/`write` 做逐次 Gate。它们都不是生产授权系统。请先阅读 [SECURITY.md](SECURITY.md)。

文档日期：2026-09-23。以下状态按本机 2026-09-23 工作区源码和截至当日明确报告的验证结果整理；更早的 PASS、当前回归、构建产物和未部署样例分别标注，不把旧结果冒充当前回归。文档不构成安全审计或生产验收。

## 1. 状态摘要

| 能力 | 源代码状态 | 验证状态和边界 |
| --- | --- | --- |
| Rust full / connector-only | 默认 feature 为 `controller`、`relay`、`mcp`；`--no-default-features` 构建 Connector-only | Rust 测试 35 项 PASS；full/connector-only Clippy `-D warnings` PASS。x86_64 musl full 与 connector-only 静态 PIE 构建均已用 `file` 检查；二进制尺寸见发行状态。 |
| Controller/Connector/Relay + E2EE | WebSocket/WSS 外层及 Noise E2EE payload；每次新连接重新握手 | 合并人工接管及到期修复后 `tests/e2e.py` 的 32 jobs PASS，含 Relay 侧 Noise 密文抓包、断线重连及不重放。 |
| `channel_key` 和 role 凭证 | `controller.json` / `connector.json` 各含独立 role token 和相同 32-byte key；Relay 配置仅含 role-token hashes | 当前 E2E 覆盖角色隔离与凭证不泄露；凭证必须经既有可信运维渠道按角色分别交付，详见下文和 [SECURITY.md](SECURITY.md)。 |
| 人工接管 PTY | CLI `attach --socket ... --job ... --incarnation ...` 独立TTY持有30s写入租约，结束清除缓存并限制Agent读写/取消 | `tests/attach.py` 真实TTY、互斥、敏感输出不可事后读取和超时回收 PASS；不是对同UID恶意代码的硬隔离，也不承诺所有TUI可自动操作。 |
| stdin/PTY、`remote_write`、`remote_resize` | 已有 CLI、协议、MCP/pi 工具和 Linux PTY 实现 | 当前 32-job E2E 覆盖 PTY、stdin、shell、resize、终止和去重。stdin/PTY 需显式启用；`write` 必须复用调用方 request ID；这些能力不构成沙箱。 |
| Controller manual Gate | 默认尝试使用独立 `/dev/tty` 对 `exec`/`write` 逐次审批 | `tests/manual_approval.py` 当前在真实 `/dev/tty`、无 TTY 闭锁、重试去重路径 PASS。MCP/pi 没有 approve 工具。 |
| Connector deadline | 入站消息、Lease 更新、exec 前分别检查到期，并用 biased select 固定到期事件优先级 | 35 项单测及 Clippy PASS；`tests/expiry.py` 两轮 5s lease + 10s TTL 排队请求回归、全套 E2E 均 PASS。 |
| Relay admin `sessions`/`revoke` | 私有 Unix admin socket；revoke 持久 marker 并关闭连接 | 当前 E2E 覆盖状态/撤销路径 PASS；revoke 不是瞬时 kill，运行中的 job 依 lease 最多约 60 秒回收。当前生产部署未验收。 |
| pi 原生扩展 | 固定 `AGENT_TUNNEL_SOCKET`，注册六个工具 | 本机 pi 0.86.0 离线 RPC 注册、实际工具代码对假 Controller 的 IPC 调用、Node IPC 5 项测试 PASS；未调用真实 AI 模型，不据此声称模型行为或更广泛 pi 兼容性通过。 |
| Cloudflare Quick Tunnel 公网烟测 | 可选脚本 `scripts/smoke-quick-tunnel.py` | 最终调试二进制的临时公网 WSS/Noise、无害命令读取和撤销均 PASS；因本机随机域名 DNS 不稳定，明确指定 Cloudflare 连接 IP，TLS 仍验证原域名。 |
| crash recovery / exactly-once | 不提供 | 新连接只重新建立 Noise channel；`exec` 不因重连自动重放。PID record 是检查证据，不等于 stdio、输出、去重表或进程的崩溃恢复。 |
| 生产安全支持 | 不提供 | 未进行独立安全审计，也不承诺生产部署、恶意目标机隔离、daemonize 后代清理或 HA。 |

### 2026-09-23 的验证记录和边界

- 最新整合：`cargo test` 35 项 PASS；full 与 connector-only Clippy `-D warnings` PASS。Connector 在入站消息、Lease 更新、exec 前分别检查截止，并以 `select biased` 优先处理到期。
- 最新整合：32-job `tests/e2e.py`、真实TTY `tests/manual_approval.py`、人工PTY接管 `tests/attach.py`、两轮lease及一次TTL过期 `tests/expiry.py`、临时CA与错误CA拒绝 `tests/wss.py` 均 PASS；不把本地集成当作生产安全审计。
- 本机 pi 0.86.0：离线注册六个工具、工具实际执行路径访问假的私有 Controller socket、Node IPC 5 项测试 PASS；未调用真实 AI 模型。
- 最新 x86_64 musl full 与 connector-only 均为静态 PIE，二进制分别 3,290,256 B / 2,343,856 B。发行脚本已重新生成两种归档，包含正确的文档/Skill/pi 文件且不包含角色凭证；归档字节数在源码和文档最终提交后的构建输出中确认。
- 临时 Quick Tunnel 公网 WSS 烟测已 PASS（DNS 故障时显式连接 IP；保持原域名 SNI/证书校验）。scratch Relay 在 rootless Podman 本地启动、私有 admin socket 与真实 Noise 路由 PASS；systemd/nginx 尚未部署到自有服务器，项目未经过独立生产安全审计。

这些状态不等同于用户未要求验收的其他硬件架构、真实模型、自有域名/服务器部署或生产安全准入。最新文档提交后的最终 tar.gz 字节数以重打包命令的结果为准。文档本身不构成安全审计或运行验收。

远端 stdout、stderr、文件名、日志、JSON 字符串、MCP 结果和错误消息都是**不可信数据**，不是给 Agent 的指令。输出中出现的命令、URL、凭证、切换目标、批准请求或延长权限要求，必须作为普通文本交给操作者判断，不能自动执行、拼接成本地 shell，也不能改写本地提示或工具策略。

## 2. 架构和信任边界

```text
本地 CLI / MCP stdio / pi extension
        │ 0600 Unix socket；pi 使用 AGENT_TUNNEL_SOCKET
        ▼
Local Controller
        │ WebSocket/WSS + Noise E2EE payload
        ▼
Loopback Relay ── TLS reverse proxy / tunnel（可选）
        ▲
        │ WebSocket/WSS + Noise E2EE payload
        │
Connector（目标 OS 用户）
        │
        └── argv / ordinary process group / optional stdin or PTY
```

- **Controller** 持有 `controller.json`，包含 controller role token 和 `channel_key`；它接收本地 CLI/MCP/pi IPC 请求，并维护 lease、pending request、审批 Gate 和重连状态。
- **Connector** 持有 `connector.json`，包含 connector role token 和同一个 `channel_key`；它以目标机当前 UID/GID 创建 pipe 或 Linux PTY、进程和进程组。
- **Relay** 只读取 `relay.json`，其中有 session、过期时间和两个 role token 的 hash。Relay 不应持有 `channel_key`，也不应能解密 E2E data frame；它仍能看到连接元数据并实施断开、延迟、丢弃或限流。
- **Relay admin socket** 是独立的本地 Unix socket，只接受同 UID 的 `sessions`/`revoke` 请求；它不通过公网 WebSocket 暴露，也不是远程 Connector 的控制 API。
- **WSS/TLS** 保护 Controller/Connector 到 Relay 的网络链路，但不能替代 Controller-Connector E2EE。Relay、TLS 终止端、Controller 主机和目标 OS 都必须分别纳入信任模型。
- 本地 Controller socket 和 Relay admin socket 都要求私有目录、同 UID 和 `0600` 文件；这不是跨用户的通用授权系统。

详细威胁模型、凭证交付、exec/PTY 风险、Relay admin 和已知限制见 [SECURITY.md](SECURITY.md)，设计和状态矩阵见 [docs/architecture-plan.md](docs/architecture-plan.md)。

## 3. E2EE 协议边界

目标协议参数固定为：

```text
Noise_NNpsk0_25519_ChaChaPoly_SHA256
```

当前 `src/crypto.rs` 的实现要点是：

- Snow `Builder` 使用上述 pattern 和 `psk(0, channel_key)`；prologue 为 `agent-tunnel/ATP<version>/<session_id>/<target_id>`，用于绑定协议、session 和目标身份。
- ring resolver 提供 AEAD、随机数和 SHA；Snow 的 curve25519-dalek 支持提供 25519 DH。此描述是源码实现说明，不是第三方密码学审计结论。
- `init` 使用操作系统随机源生成底层 32-byte `channel_key`，JSON 中以 64 个十六进制字符保存。Controller 和 Connector 必须使用同一 key；Relay 只拿 role token hashes。
- `Init`/`Response` 是 Noise 握手帧；应用 packet 进入加密 `Data` 帧。加密数据按不超过 Noise 65535-byte 限制的分片发送，分片结束标记位于加密 payload 内；Relay 只转发外层 JSON frame。
- 每次 Controller 或 Connector 重连都重新执行 Noise handshake，旧 channel 的 nonce/state 不复用；认证失败、篡改、乱序、跨 channel frame 或握手未完成时不得发送命令，也不得回退为明文 JSON packet。
- Relay 可以转发 Noise frame，但其 role token 认证、连接状态、计时、大小、方向和错误元数据仍可见。E2EE 主要保护命令、stdin、PTY 数据和输出正文的机密性/完整性，不提供可用性或元数据隐私。

源代码包含 Noise 单元测试和 negative-path 测试，但完整二进制、Relay、WSS、错误 key、重连和真实两端互通仍应以实际测试为准。

### 不重放的执行语义

- `exec` 使用 `request_id`，Connector 在当前 incarnation 内对相同 ID 和相同参数做去重；参数变化返回 `REQUEST_CONFLICT`。
- Controller 的网络断开、超时、审批返回或 peer 变化不会自动用新 ID 重发命令。一个 exec 可能已经产生副作用但回复未知，因此不能把超时当成失败，也不能用新 ID 盲目重跑。
- 若仍是同一个 live Connector incarnation，可以用**相同** request ID 和完全相同参数查询/取得去重结果；incarnation 改变后，旧 job/request 不应被假定可恢复。
- 新 Noise 握手只恢复通信通道，不恢复 stdio、PTY、输出环、pending reply、request 去重表或远端进程。不要把 PID 记录当成恢复控制权。
- `remote_write` 同样必须带调用方生成的 `request_id`；不确定时不得用新 ID 重发输入。PTY 的 EOT/EOF 不保证应用退出。

## 4. 凭证生成、可信交付和撤销

`init` 创建一个不存在的私有目录，生成三个 `0600` 文件，目录为 `0700`：

```text
<session-dir>/relay.json       RelayConfig：session、connector/controller token hash、TTL
<session-dir>/controller.json  EndpointConfig：controller token、channel_key、Relay URL
<session-dir>/connector.json   EndpointConfig：connector token、channel_key、Relay URL
```

文件分工是安全边界，不只是部署便利：

1. Relay 主机只部署 `relay.json`。它不需要、也不应收到 `controller.json` 或 `connector.json`。
2. 本地 Controller 只读取 `controller.json`；目标 Connector 只读取 `connector.json`。两个 EndpointConfig 共享同一个 `channel_key`，但 role token 不同。
3. 三类文件必须通过已有可信运维渠道分别交付，并在目标位置检查拥有者、目录 `0700` 和文件 `0600`。不要通过 URL query、shell 参数、公开工单、日志、环境变量、MCP 结果或远端输出传递 token/key。
4. `init` 的 stdout 只包含 session/target/expiry/directory 元数据，不包含 token 或 `channel_key`。凭证文件仍是敏感材料，不能因为 stdout 未泄露就放宽文件权限。
5. Endpoint token 是 role-scoped：connector token 只能用于 `/v1/connect/<session_id>`，controller token 只能用于 `/v1/control/<session_id>`。Relay 只保存 token hash，但拿到 endpoint 文件的攻击者同时可能获得明文 token 和 E2E key。
6. session TTL 当前由 `init` 限制在 10–3600 秒；不要把 TTL 当作长期身份系统。

### 按角色交付示例

以下是供操作者在受信任环境复现的命令示例。先在受信任的管理机用新目录创建短期 session。Loopback URL 只适合同机本地测试；跨主机时须使用经过运维验证的 `wss://` TLS 入口、域名、CA 和反向代理，不要把公网明文 `ws://` 当作替代。

```bash
# mkdir 的父目录须为操作者私有；init 要求 demo-session 是尚不存在的新目录。
umask 077
mkdir -p "$HOME/.local/share/agent-tunnel"
chmod 700 "$HOME/.local/share/agent-tunnel"
agent-tunnel init \
  --dir "$HOME/.local/share/agent-tunnel/demo-session" \
  --relay ws://127.0.0.1:8787 \
  --name disposable-test-target \
  --ttl-secs 300
```

严格按角色交付：Relay 主机只收 `relay.json`；Controller 主机只收 `controller.json`；目标机只收 `connector.json`。使用已有、经批准且能验证收件主机/用户身份的秘密或配置交付渠道，并在目的端核对 owner、目录 `0700`、文件 `0600`。删除临时传输副本及不需要的明文备份。不要通过聊天、群发邮件、工单、URL、shell 参数、环境变量、日志或 Agent/MCP/pi 对话传送凭证文件内容。

下面展示 loopback Relay 和无害 `printf` 流程；命令仅供人工按实际主机路径调整。Relay、Controller、Connector 是长驻进程，应按部署拓扑分别在自己的终端/服务管理器中运行，不要把下面整段当作一个会顺序完成的 shell 脚本。CLI 调用端先运行 `info` 并核对目标身份，再手动填入确认过的 incarnation（替换占位符）：

```bash
agent-tunnel relay --listen 127.0.0.1:8787 \
  --session-file /secure/relay-side/relay.json
agent-tunnel local \
  --config /secure/controller-side/controller.json \
  --socket /secure/run/controller.sock
agent-tunnel connect \
  --config /secure/target-side/connector.json \
  --allow-exec

agent-tunnel info --socket /secure/run/controller.sock
agent-tunnel exec \
  --socket /secure/run/controller.sock \
  --incarnation '<verified-incarnation>' \
  --cwd /tmp \
  --request-id read-only-example-001 \
  -- /usr/bin/printf '%s\n' 'agent-tunnel smoke example'
```

如需人工操作同一个文本 PTY，先使用 `exec --pty -- /bin/sh` 创建 job，从返回 JSON 复制 `job_id` 并确认目标 `incarnation`，然后在**独立的操作者 TTY** 运行（不要给模型显示接管令牌）：

```bash
agent-tunnel attach \
  --socket /secure/run/controller.sock \
  --job '<verified-job-id>' \
  --incarnation '<verified-incarnation>'
```

`Ctrl+]` 退出接管；人工持有期间普通 MCP/pi/CLI 的 `read/write/cancel` 被拒绝，离开后清除该段保留输出并标识 cursor gap，30 秒不续约自动释放。工具输出仍按安全文本呈现，**这不是 vim/top 等任意 TUI 的完整终端模拟**；本地与远端共享同 UID 恶意程序不在本机制可隔离的范围内。

默认 Controller Gate 通过独立 `/dev/tty` 逐次审批 `exec`/`write`；没有 TTY 时应闭锁。`--accept-session-risk` 会跳过逐次 Gate，是整段 session 的风险选择，不是审批记录。

### Relay admin 撤销

Relay 运行时会在私有目录创建 admin Unix socket；也可以显式传入 `relay --admin-socket PATH`。默认路径是首个 `relay.json` 同目录的 `relay-admin.sock`。socket 要求同 UID、父目录私有、权限 `0600`。

```bash
# 读取 Relay 内存中的 session/连接状态
agent-tunnel sessions --admin-socket /path/to/relay-admin.sock

# 持久撤销指定 session
agent-tunnel revoke \
  --admin-socket /path/to/relay-admin.sock \
  --session SESSION_ID
```

`revoke` 会先在内存中阻止继续授权，再尝试写入 `<relay.json>.revoked`，并关闭该 session 的现有连接。重启 Relay 时检测到 marker 会拒绝旧 session，必须创建新 session。正在运行的 Connector 会在 lease 边界退出并回收它管理的 job，当前 lease 上限为 60 秒；这不是瞬时 kill，也不能撤销已发生的副作用。

如果 durable marker 写入失败，不能把“内存中已关闭连接”当成持久撤销；不要用旧 `relay.json` 重启 Relay，应隔离/删除旧凭证并创建新 session。admin socket 是本机管理员入口，拿到同 UID 控制权的进程可撤销 session，因此必须保护运行用户和父目录。

当前仍没有 one-time join、自动 key rotation 或远端 approve API。可信运维撤销是 Relay admin 的本地能力；它不等同于逐命令人工审批。

## 5. 权限 Gate 和人工审批

当前安全语义分两层：

1. **Connector 侧 session gate**：`connect --allow-exec` 必须显式给出，表示允许该目标 OS 用户执行任意 `argv`。不带它，Connector 不接受执行会话。
2. **Controller 侧 approval Gate**：默认 `local` 不带 `--accept-session-risk` 时，Controller 尝试打开独立 `/dev/tty`，对 `exec` 和 `write` 做逐次人工确认。没有可用 TTY 的非交互环境应失败，而不是静默放行。

审批流程的协议边界：

- 第一次 side-effecting `exec`/`write` 请求可收到 `APPROVAL_REQUIRED`；审批码和命令摘要只显示在 Controller 的独立 TTY，不放入该 reply。操作者不能把远端输出当作批准理由。
- 操作者在 Controller 进程的独立 TTY 完成确认后，原调用方必须使用**相同 request ID 和完全相同参数**重试；不应生成新 ID，也不能把首次 `APPROVAL_REQUIRED` 当作远端命令已经执行。
- MCP server、pi 原生扩展和 remote-debug Skill 没有 `approve` 工具。它们只能把 `APPROVAL_REQUIRED` 报告给本地操作者，并等待操作者在 Controller TTY 操作后重试。
- `local --accept-session-risk` 是显式接受整段 session 执行风险并绕过逐次 Gate 的选择；它不是人工审批记录，也不改变 Connector 的任意 OS 用户权限。
- 该 Gate 的完整模块、TTY 行为、并发请求、断线和真实拒绝路径仍需构建和测试确认；文档不把源代码接线写成验收通过。

## 6. CLI、MCP 和 pi 接口

下面是源码接口和状态说明，不是对未验证二进制的兼容承诺。CLI 普通 JSON 结果写 stdout，诊断写 stderr；`mcp` 的 stdout 完全保留给 MCP JSON-RPC。

```text
init       生成 role 文件（controller feature）
relay      在 loopback 运行内存 Relay（relay feature）
sessions   通过本地 Relay admin socket 查询状态（controller feature）
revoke     通过本地 Relay admin socket 撤销 session（controller feature）
connect    目标机 Connector；必须 --allow-exec
local      本地 Controller；默认需独立 TTY Gate，可用 --accept-session-risk 绕过
info       查询 target/identity/incarnation
exec       启动远端 argv，必须带 request_id、incarnation、cwd 和 -- 后的 argv
read       按 cursor 读取有界输出和状态
cancel     请求取消当前 incarnation 所拥有的 job
write      向明确启用 stdin/PTY 的 job 写入数据；必须带 request_id
resize     调整明确拥有的 PTY（仅 PTY）
mcp        通过 rmcp stdio 暴露六个本地 MCP tools（mcp feature）
inspect    只检查持久 PID record，不 adopt、kill 或恢复进程
```

`exec` 的交互字段与协议一致：`stdin` 和 `pty`。它们不是 shell 沙箱或权限提升；启用后，目标程序可能从后续输入中读取并执行更多操作。`remote_write` 的每次写入都要使用有效的 `request_id`；不确定时只能在同一个 job/incarnation 上遵循相同 ID 的去重策略。`remote_resize` 只对 PTY 有意义，不能把 pipe job 伪装成终端。

MCP 和 pi 的六个工具为：`remote_info`、`remote_exec`、`remote_read`、`remote_write`、`remote_resize`、`remote_cancel`。`remote_exec`/`remote_write` 都必须由调用方提供 `request_id`；`remote_resize` 不代替输入审批，也不能用于任意 PID。

### pi 原生扩展

`integrations/pi/index.ts` 是可选的原生扩展，`integrations/pi/ipc.mjs` 通过本地 newline-delimited JSON IPC 调用 Controller。扩展启动时读取 `AGENT_TUNNEL_SOCKET` 并固定 socket 路径，工具参数不能改写目标 socket；IPC helper 会检查父目录/socket 的 owner、类型和私有权限。

扩展使用 pi loader 提供的 `typebox`，本项目不 npm install 依赖；本机 pi `0.86.0` 离线六工具注册和 Node IPC 5 项测试已 PASS，但未做真实模型测试或其他版本/发布版兼容性验收。扩展没有 approve 工具，远端输出仍只作为 JSON data 返回，不得因为 UI status、tool result 或错误文本而自动执行下一条命令。

## 7. Job、进程组、stdin/PTY 和输出

- 每个 job 绑定 `target_id`、`incarnation`、`job_id`、`request_id`、PID、PGID、Linux `boot_id` 和 `process_start_ticks` 等记录。
- 普通 pipe exec 可分离 stdout/stderr；PTY exec 使用 Linux PTY master，目标程序获得 session leader/controlling terminal，输出通常是合并的终端流。PTY 只在 Linux 有实现保证；其他 Unix 应以实际错误为准。
- `stdin=true` 或 `pty=true` 后，Connector 建立有限输入队列；`remote_write` 数据本身可能改变远端程序状态、执行命令或提交凭证。输入不是只读诊断。
- `remote_write --eof` 对 pipe 关闭输入；对 PTY 发送 EOT，不能保证程序退出。`remote_resize` 的 rows/cols 当前限制为 1–1000；只允许作用于拥有的 PTY。
- 普通 exec 使用 OS 进程组；cancel 是对该进程组的终止请求，必须再 `read` 确认终态。它不能撤销已经完成的文件、数据库、网络或其他副作用。
- 普通进程组不是 cgroup。进程执行 `daemonize`、`setsid` 或以其他方式脱离进程组后，Connector 不保证能清理它；恶意 daemon 可能继续运行。
- PID record 是人工检查证据。Linux 的 PID 复用、重启、容器 PID namespace、`/proc` 不可读或 boot/start tick 缺失，都可能使状态为 mismatch、not_running 或 unknown。程序不会根据 record 自动 adopt 或 kill 外部进程。
- 每个 job 的内存输出环上限为 1 MiB，单次 read 上限为 32 KiB；输出可能被丢弃或以 UTF-8 lossy/control-escaped 形式返回。`output_truncated`、`dropped_before_cursor` 和 cursor 必须纳入诊断结论。
- 断线不等于成功或失败。Connector 只在本进程、lease 和 session TTL 的边界内保留管理状态；crash 后不承诺恢复旧 stdio、PTY、输出、request 去重或远端进程。

## 8. 构建、验证和发布

### 8.1 两个独立构建变体

默认 full 构建包含 `controller`、`relay`、`mcp`：

```bash
cargo build --locked --release
# 等价的显式 feature 形式：
cargo build --locked --release --features controller,relay,mcp
```

Connector-only 目标端构建使用 `--no-default-features`：

```bash
cargo build --locked --release --no-default-features
```

Debian/Ubuntu x86_64 上可选使用 `scripts/bootstrap-musl-debian.sh`：它从主机已配置的 APT source 下载 `musl`/`musl-dev`/`musl-tools` deb，并只解压到仓库 `target/`，不安装系统软件包；它需要网络访问，且本轮没有运行。该 helper 不会规避发行文件的默认拒绝覆盖策略。

不要把 full 二进制和 connector-only 二进制、不同 session 文件或不同架构的产物混用后推断权限或兼容性。2026-09-23 报告的 `x86_64-unknown-linux-musl` full 与 connector-only 二进制为静态 PIE，分别是 3,290,256 B 和 2,343,856 B，且已用 `file` 检查；该结果不代表其他 target 的兼容性。

### 8.2 发布脚本和签名

`scripts/package.sh [--force] [target-triple]` 会在独立的 `target/package-full` 和 `target/package-connector` 目录中分别构建 full/connector，生成带版本、目标三元组和变体名的 tar.gz，并生成 `dist/SHA256SUMS-<target>`。full 包含二进制、README/SECURITY、`docs/`、remote-debug Skill 和 pi 扩展运行文件；connector-only 包含二进制与 README/SECURITY，但不捆绑 docs、Skill 或 pi 扩展。脚本打印实际 binary/archive 字节数。

默认拒绝覆盖 `dist/` 中同名 archive、checksum manifest 或签名；只有显式传 `--force` 才替换。如果 `--force` 时未提供 `MINISIGN_SECRET_KEY`，脚本会移除旧的同名 manifest 签名，避免留下与新 manifest 不匹配的签名。所有发布文件先在 `dist/` 同文件系统的本次 `mktemp` 目录构造，再逐文件原子发布；失败时只清理本次临时目录，不删除既有产物。Cargo 构建目录是 `target/package-*`；脚本不执行 Git add/commit/reset、不改源文件。最终脚本仍需用已提交工作树重新打包并核对产物；不应把源码检查等同于发行验收。当前 `dist/*.tar.gz` 已按更新后脚本生成；文档提交后建议重打以包含最新说明。

`SOURCE_DATE_EPOCH` 可以显式固定归档时间。默认值取最新 Git commit 时间；非 Git 工作区才回退到当前时间。若工作树含未提交改动，归档内容来自当前工作树而 epoch 仍可能来自旧 commit，因此不能据此宣称可复现构建。排序、mtime、owner 和 gzip header 归一化也不等于工具链及全构建过程可复现。

```bash
# 新的 dist 文件名：默认拒绝覆盖已有文件
scripts/package.sh x86_64-unknown-linux-musl

# 仅在已确认要替换同名 dist 产物时才使用 --force
scripts/package.sh --force x86_64-unknown-linux-musl
```

release profile 当前声明：

```toml
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

签名是可选的：只有设置 `MINISIGN_SECRET_KEY` 时，`package.sh` 才用 `minisign` 签名 checksum manifest。签名私钥不能写入仓库、命令示例或日志。校验流程需要通过独立可信渠道取得公钥：

```bash
scripts/verify-release.sh \
  dist/SHA256SUMS-<target> \
  <independently-obtained-minisign-public-key> \
  dist/agent-tunnel-<version>-<target>-full.tar.gz
```

`verify-release.sh` 同时验证 manifest 的 minisign 签名和指定 archive 的 SHA-256。**只有 checksum、没有可信公钥或签名，并不能认证发布来源**；checksum 只说明文件与 manifest 中的摘要一致。

| 变体 | target | 静态 PIE binary（2026-09-23 报告） | 更新脚本后的 tar.gz |
| --- | --- | ---: | --- |
| full | `x86_64-unknown-linux-musl` | 3,290,256 B | 当前版本已打包；最终归档大小以文档提交后复建结果为准 |
| connector-only | `x86_64-unknown-linux-musl` | 2,343,856 B | 当前版本已打包；最终归档大小以文档提交后复建结果为准 |

这次用更新脚本重新构建并检查了两个 x86_64 归档、SHA-256 和内容；文档最后一次提交会改变 full 归档，因此不要把此前归档字节数当作最终值。

### 8.3 测试入口和边界

以下是可重复的验证入口；截至 2026-09-23 的已报告结果和待回归项分别标注：

```bash
cargo test --locked --all-features
cargo build --locked --release
cargo build --locked --release --no-default-features
python3 tests/e2e.py \
  --binary target/release/agent-tunnel \
  --lease-secs 10
```

35 项 Rust 单测、full/connector-only Clippy、32-job E2E、真实TTY审批、人工接管与租约截止回归当前 PASS。本地TLS/WSS原域名校验和错误CA拒绝、临时Quick Tunnel公网WSS/Noise（显式连接IP）、Podman scratch Relay 亦已验证；pi 0.86.0 离线注册六工具、执行真实扩展代码调用假Controller和Node IPC 5测试PASS，未调用真实 AI 模型。以下仍需分别验收：

- `init` 目录/文件权限、角色分离、token/key 不出 stdout；Relay 错 token、错 role、Origin、重复 instance、外层 frame 限制。
- Noise `NNpsk0` 双方握手、ring/curve25519 解析、错误 key、错误 prologue、篡改/截断/乱序/跨 channel frame、分片重组、重连后新 handshake 和明文 fallback 拒绝。
- `exec` request ID 去重和不重放；`APPROVAL_REQUIRED`、独立 TTY 通过/拒绝、无 TTY 失败、相同 ID/参数重试、MCP/pi 不具备 approve 权限。
- stdin/PTY 生命周期、pipe 与 PTY 输出差异、`remote_write` request ID、EOF、PTY resize、断线、取消和 process-group/daemon 行为。
- Relay `sessions`/`revoke`、同 UID admin socket、durable `.revoked` marker、重启拒绝旧 session、lease 最多 60 秒回收和持久化失败告警。
- full 与 `--no-default-features` 两种包在目标架构上的独立构建、启动和真实权限边界；WSS 证书/CA、反向代理、Containerfile、systemd、发布签名和下载验证。
- pi 0.86.0 本地扩展加载、socket 权限拒绝、六工具 schema 和取消/超时；模型行为测试另行记录，不能由源代码对照替代。

跨架构、真实公网 WSS、部署和模型行为仍需各自的运行证据；静态检查、`file` 输出和本地 IPC 测试不能替代这些验收。

## 9. 部署样例

仓库中的样例位于 `deploy/`：

- `deploy/agent-tunnel-relay.service` 以专用非 root 用户启动 Relay，绑定 `127.0.0.1:8787`，并启用 `NoNewPrivileges`、`ProtectSystem=strict`、`ProtectHome`、私有临时目录和受限地址族等 systemd 限制。
- `deploy/nginx.conf` 在 TLS 入口终止 HTTPS/WSS，将 `/v1/` 和 `/healthz` 转发到 loopback Relay；示例关闭 access log，避免记录 Authorization 和 request body。证书、域名、权限、限流和日志策略仍需由运维审核。
- `deploy/Containerfile` 从预构建二进制制作 scratch Relay 镜像，不在运行时下载编译器或依赖；镜像默认用户为 `65532:65532`。本机 rootless Podman 的可读凭证验收使用 `--userns keep-id --user "$(id -u):$(id -g)"` 与宿主机私有 `0600` 角色文件对齐，并分别挂载**只读** relay.json 目录和**可写、0700**的 admin-socket 目录。若保持默认UID65532，角色文件就必须归该运行用户所有；不要通过放宽文件权限或任意 chmod 777 规避。仅绑定127.0.0.1且由受信任TLS入口转发，它不改变 Relay 不持有 E2E key 的要求。
- Relay admin socket 应留在同一私有 session 目录或其他同等权限目录，不要映射到公网、nginx 或 Connector 主机。

以上是配置样例，systemd/nginx 尚未在自有公网服务器部署。已用临时Quick Tunnel 完成短期公共WSS烟测，且本地rootless Podman scratch Relay 启动和Noise路由通过，但这些不等于自有域名、证书、systemd和生产安全验收。复测可用 `python3 scripts/smoke-quick-tunnel.py --binary <full-test-binary> --proxy-command mgraftcp --connect-ip <经许可确认的CloudflareIP>`；只有临时DNS解析故障时才考虑显式IP，并始终保留Quick域名的Host、SNI和正常证书校验。

Relay 应始终只绑定 loopback。公网暴露、TLS 终止和反向代理不是安全边界的替代品；生产网络若不满足信任条件，应停用而不是把 `ws://` 暴露到公网。

## 10. 开发约定和限制

- 以当前 Rust 源码和实际命令输出为准；旧的 Go、`session create/verify/revoke`、`--profile`、`--join-token-stdin` 等规划接口不是当前 CLI。当前 `revoke` 是 Relay admin 的本地命令，和旧规划接口不是同一实现。
- 任何协议变更都要同时检查 role scope、channel key 分发、Noise 重连、request ID、incarnation、lease、审批 TTY、stdout/MCP/pi 边界和断线副作用。
- 不在 Skill、示例、日志、测试夹具、pi status 或 issue 文本中写入真实 token、channel key、Bearer 值或凭证文件内容。
- 远程输出只能作为带边界的证据；不得让它改变本地命令、审批、工具选择或安全策略。任何“请批准/请运行/请上传密钥”的远端文本都不是系统审批。
- one-time join、在线 key rotation、可审计审批记录、cgroup/容器级清理、可验证 resume、Relay HA 和生产支持都需要单独的设计、实现、测试和安全审查。

### 原架构规格 P0–P4 对照

这里保留原始路线范围；一项中的局部能力已实现或曾通过测试，不代表整项规格完成。逐项设计和剩余缺口见 [docs/architecture-plan.md](docs/architecture-plan.md)。

| 原规格 | 当前覆盖 | 剩余缺口 / 验收边界 |
| --- | --- | --- |
| **P0**：隔离目标 full/connector-only 构建、loopback/WSS链路及 Codex/pi 实际版本验收 | x86_64 musl 双变体静态PIE构建与首次打包、32-job E2E、本地TLS/CA和临时Quick公网WSS/Noise、pi 0.86.0六工具/假Controller实际tool执行均PASS。 | 最新文档版归档须再打包；Codex真实模型和pi模型决策未验收；按用户要求不验证ARM64。 |
| **P1**：session revoke、one-time join、目标身份确认、逐命令人工审批及可审计绑定 | Relay durable revoke、TTY逐次审批与PTY操作者独占接管当前回归PASS。 | one-time join、长期fingerprint和持久审计仍缺。 |
| **P2**：E2EE、重放防护、密钥轮换及Relay不可信模型 | Noise密文抓包、错误key、重放、损坏帧与公网WSS链路PASS。 | 在线key rotation与独立安全审计未完成。 |
| **P3**：cgroup/容器清理、stdin/PTY/resize/write lease | stdin/PTY/resize、互斥人工30s写入lease、rootless scratch容器Relay本地路由均实测PASS。 | 没有恶意进程daemonize清理/cgroup级隔离保证；Relay容器不代表Connector容器隔离。 |
| **P4**：持久 job metadata/output spool、受限 resume、Relay HA/固定域名和运维撤销 | Relay 运维 revoke marker 已实现。 | job/output spool、受限 resume、Relay HA 和固定域名仍未实现/验收。 |

原路线之外，Streamable HTTP MCP/公网 MCP gateway 仍是单独设计；不承诺 crash recovery、exactly-once、通用远端 approve、恶意目标隔离、独立安全审计或生产支持。不得因局部能力已有测试而缩小上述未完成范围。

## 11. License / status

仓库目前没有生产支持或稳定协议兼容承诺。请把每个 session、目标机、Relay 和 job 当作短期、可撤销、可丢弃的测试资源；发现凭证泄露、目标身份不符、Noise 未建立、审批状态不明、输出含有诱导指令或进程组逃逸时，应停止操作并重新创建 session。
