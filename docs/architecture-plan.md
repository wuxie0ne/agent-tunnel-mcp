# Agent Tunnel：临时远程终端接入规划

- 日期：2026-09-22
- 状态：设计提案，尚未实现、构建或实网验证
- 目标：本地 Codex CLI / pi 借助临时执行器，诊断、交互操作远程服务器和容器；远端不安装完整 AI agent。
- 本轮边界：仅编写规划，不安装依赖、不修改 agent 配置、不启动公网监听。
- 工作区观察：目前没有可供审计的应用代码；`git status` 返回非 Git 仓库，不能据此声称 Git 工作区干净。

## 1. 推荐结论

采用“本地控制器 + 本地 MCP/CLI 适配器 + 公网中继 + 远端临时执行器”。

1. 远端执行器主动发起 WSS 出站连接，不监听入站端口。
2. 公网中继做配对、认证、连接路由、限流，不执行 shell。
3. 本地控制器管理身份、目标绑定、持续连接、控制权租约、人工审批及审计。
4. Codex 使用本地 stdio MCP；pi 首先使用 CLI + Skill，随后提供原生扩展。
5. 同一 Go 工程构建单一 `agent-tunnel` 可执行文件，以不同子命令承担不同角色。
6. Cloudflare Quick Tunnel 只暴露开发机器上的中继，而不是部署到每一台目标机器。
7. 先在无敏感数据的测试环境验证；开放真实敏感环境前完成安全阶段和验收。
8. 后续只替换公网入口和中继部署位置，不替换执行协议、MCP 工具或 Skill。

这里的“远端提供 MCP”体现为：远端提供执行能力，本地适配器将它映射为 MCP 工具。不是要求远端本身启动公网 MCP HTTP 服务，也不是第一版实现任意 MCP server 的通用隧道。

### 默认假设

- 个人/小团队自用，第一版单一操作者、多个相互隔离的临时会话。
- 远端 Linux amd64/arm64；本地先 Linux，macOS 作为后续构建和测试目标。
- 用户已有合法的初始登录/运维入口，能将一个二进制放入远端并执行。
- 远端允许执行该二进制，且允许访问指定域名的 TCP 443/WSS。
- “能访问公网”不等于能访问任意域名、WebSocket 或二进制下载源；必须做预检。
- 不提供首次突破访问权限、不绕过组织网络策略，也不自动提权。

## 2. 已核查的外部约束

以下是 2026-09-22 查阅官方文档所得；设计选择与外部产品能力分开记录。

| 事实 | 对本设计的影响 | 来源 |
| --- | --- | --- |
| Quick Tunnel 使用随机域名，面向开发测试，无 SLA；不支持 SSE，并限制 200 个并发在途请求 | 不将依赖 SSE 的远程 MCP 作为首发路径；域名按临时会话处理 | S1 |
| Cloudflare 支持 WebSocket；连接可能因平台更新或空闲而中断 | WSS 值得验证，但必须设计心跳、重连和断线处理；不据此宣称实际链路已经可用 | S2 |
| Codex 支持 stdio 和 Streamable HTTP MCP | 优先 stdio，远程 HTTP 作为可选适配器 | S3 |
| pi 官方 README 明确不内置 MCP，支持通过 CLI/Skill 或扩展添加能力 | 不把安装某个第三方 MCP 插件作为用户必需前提 | S4、S5、S6 |
| Streamable HTTP 可以返回单个 JSON 响应，不必每个请求都返回 SSE | Quick Tunnel 并非完全不能承载 MCP；JSON 短请求是可选路线 | S7 |
| MCP 2026-07-28 版调整了 HTTP 传输，移除了旧版协议级会话和 GET 流端点 | 业务会话独立设计；版本协商交给 SDK，不混用不同版本规则 | S7 |
| MCP 有官方 Go SDK，公开了协议兼容矩阵 | 实施时锁定经兼容测试的 SDK 和 Go 版本，不手写完整 MCP | S8 |

Quick Tunnel 的 WebSocket 穿透、具体网络的代理兼容性、目标机证书环境都必须在 P0 验证；Cloudflare 的通用 WebSocket 文档不能替代此次实网验收。

## 3. 范围与非目标

### 首个可用版本必须具备

- 临时目标注册、人工确认目标、短期凭证和会话撤销。
- 执行 argv 或明确指定的 shell 命令。
- 显式 cwd、受控环境变量、stdout/stderr、退出码、超时。
- 长任务启动、分段读取、输入、终止，断线后查询既有任务。
- Codex MCP 与 pi CLI + Skill 共用同一执行核心。
- 持续本地控制器，避免 pi 每次 CLI 退出导致控制租约消失。
- 目标侧停止开关、会话 TTL、输出配额、认证与最小审计。

### 交互增强版本具备

- PTY：持续 shell、REPL、输入、窗口大小、终端中断。
- 人工接管同一个终端，接管时冻结 agent 写入。
- pi 原生扩展：工具、状态展示、取消和进度反馈。

### 第一版不做

- Web UI、桌面远控、文件浏览器、任意 TCP/SOCKS 代理。
- 任意第三方 MCP 服务的透明代理。
- 自动发现并接入整台机器上的所有容器。
- Docker socket 挂载、默认特权容器、自动 sudo。
- 多租户 SaaS、计费、团队 RBAC、集群高可用。
- 持久化守护进程安装、开机启动、自更新、静默后台驻留。
- 专用大文件传输；小文件可以显式命令处理，文件传输 API 后置。
- 将任意 shell 伪装成“只读安全工具”。

## 4. 总体架构

### 4.1 开发阶段

```text
本地工作站
┌────────────────────────────────────────────────────┐
│ Codex ──stdio MCP──┐                               │
│                    ├─ adapters ─IPC─ Local Controller│
│ pi ─CLI + Skill────┘                  │             │
│                                  authenticated WSS │
│                                       │             │
│ Relay（127.0.0.1:8787）◀───────────────┘             │
│      ▲                                             │
│ cloudflared（仅暴露 Relay 的数据面）                 │
└──────┬──────────────────────────────────────────────┘
       │
       ▼
Cloudflare Quick Tunnel 公网入口
       ▲
       │ WSS 主动出站（Connector 发起）
       │
远端服务器 / 容器
┌────────────────────────────────────────────────────┐
│ Connector ── Job Manager ── process / shell / PTY    │
│ 当前 OS 用户权限；无监听端口；无 LLM/API key          │
└────────────────────────────────────────────────────┘
```

Controller 可以走本机认证连接访问 Relay；也提供“强制走公网入口”的验收模式，避免只验证本地路径。图中的双向数据沿已建立连接传递，并不要求远端允许入站连接。

### 4.2 自有公网服务阶段

```text
本地 Controller ──WSS:443──▶ TLS 入口 ──▶ Relay
                                             ▲
                                             │
远端 Connector ───WSS:443──────────────────────┘
```

- 中继管理面仅绑定 loopback/Unix socket，创建会话通过管理员入口；不随数据面公开。
- 初期单 Relay 实例；连接、活动路由内存保存，重启按明确的失效流程处理。
- 后续需要跨实例时再做路由协调；添加数据库并不自动解决活动连接迁移。

### 4.3 角色与信任边界

| 角色 | 承担职责 | 明确不承担 |
| --- | --- | --- |
| Relay | 短期注册、连接认证、路由、配额、撤销 | 运行远程命令、保存模型 API key、解析加密后的执行内容 |
| Connector | 授权校验、进程/PTY 生命周期、输出缓存、TTL、终止 | 运行 LLM、自动获取更高权限、为任意网络服务做代理 |
| Local Controller | 身份、持续连接、目标绑定、审批、端到端通道、审计 | 在本地执行用户要求的远端命令 |
| MCP adapter | 固定工具 schema、参数转换、返回格式、取消映射 | 维护第二份会话状态机 |
| CLI / pi adapter | 将同一操作提供给终端或 pi | 另造认证和执行实现 |
| Skill | 解释工具选择、目标确认、诊断顺序、风险提示 | 充当安全边界、存储私钥或授权执行危险操作 |

安全假设：操作者本地 OS、Connector 所在 OS 以及发布的二进制可信。目标上与 Connector 同权限的恶意进程不在此工具可隔离的范围内。

## 5. 为什么选择这条路线

| 路线 | 优点 | 缺点 | 结论 |
| --- | --- | --- | --- |
| 每台目标直接运行 MCP + cloudflared | 极小演示简单 | 目标需额外组件；每目标公网入口；SSE/鉴权耦合；迁移不自然 | 不作为主架构 |
| Relay 直接暴露远程 MCP | Codex 可直接连接 URL | pi 仍需适配；MCP 生命周期与隧道耦合；Relay 成为执行内容信任点 | 后续可选网关 |
| 本地 MCP/CLI + 应用层反向执行通道 | 目标只需二进制；跨 agent；易替换入口；支持端到端保护 | 需实现少量执行协议与本地控制器 | 推荐 |
| OpenAI Secure MCP Tunnel | 官方提供私网 MCP 的出站连接接入 | 使用 OpenAI 托管入口；不是本项目所需的自有 Relay 和跨 agent 默认方案 | 作为现成替代方案评估，非主线 [S12] |
| 反向 SSH，再套本地 MCP/CLI | 复用成熟终端/传输栈 | 需另行安排 SSH 公网入口和目标工具，未必符合 HTTPS-only 环境 | 已有 SSH 条件时应优先评估，不重复造通用 SSH |

本项目的价值是 agent 友好的临时接入、任务生命周期、目标确认和统一适配，不是重新实现完整远程管理平台。

另已核查 OpenAI Secure MCP Tunnel：它通过运行在私网内的 tunnel-client 轮询 OpenAI 托管入口，将请求转发给私有 MCP。若以后只面向受支持的 OpenAI 产品，可以单独评估以减少自建工作；本规划不假定它能直接满足 pi、自托管入口或本项目的终端任务语义。[S12]

## 6. 使用体验草案

以下 `agent-tunnel` 命令与参数都是拟议接口，目前不可执行。`cloudflared` 和 Codex 的基本调用形式来自官方文档。凭证不出现在命令参数、URL query 或 Skill 中。

### 6.1 开发机启动中继和入口

```bash
# 终端 A：管理 socket 不属于公网数据面
agent-tunnel relay --listen 127.0.0.1:8787

# 终端 B：使用该进程打印的随机公网域名
cloudflared tunnel --url http://127.0.0.1:8787
```

不要为了启用 Quick Tunnel 自动覆盖用户现有 cloudflared 配置；检测到冲突时给出说明。[S1]

### 6.2 本地创建一次临时会话

```bash
agent-tunnel session create \
  --relay https://<random>.trycloudflare.com \
  --label incident-api-01 \
  --ttl 1h
```

此命令还需通过本地管理 socket（自托管阶段为受保护管理通道）取得创建权限。仅有公网 URL 不能任意创建资源。

返回：

- 非秘密的 session ID、label、过期时间。
- 本地保存的 controller 私钥/凭证，不输出给模型。
- 一次性 join 材料，通过人工已有登录渠道交给目标；默认隐藏输入/受限文件/FD。
- 用于独立验证的 controller 身份指纹；目标身份待随后确认。

### 6.3 目标机启动 Connector

```bash
# 二进制预先下载/复制，并完成校验；前台运行
./agent-tunnel connect \
  --relay https://<random>.trycloudflare.com \
  --name api-01 \
  --ttl 1h \
  --join-token-stdin
```

- 从隐藏输入或 FD 接收 join secret，不要求将 secret 粘贴进 shell 命令行。
- 创建会话后 5 分钟内可登记一次；验证成功后原 join secret 作废。
- 打印 hostname、UID/GID、cwd、OS/arch、执行限制、对端身份/目标指纹、到期时间。
- 身份通过人工已可信的目标终端核对；验证前禁止 exec。
- 默认前台，Ctrl+C 撤销接入并清理本工具所管理的任务。
- 在已有容器内运行时，只授予该进程本来具有的访问权限。

### 6.4 本地启动控制器并绑定目标

```bash
# profile 保存目标的不可变 ID 与 incarnation，不仅保存可重用名称
agent-tunnel local --profile incident-api-01

# 独立本地终端完成身份确认；不允许模型自动接受新指纹
agent-tunnel session verify --profile incident-api-01
```

Controller 保持远程连接、控制租约与任务状态同步。单次 CLI/MCP 调用结束不等于整个 controller 离线。

### 6.5 Codex 接入

```bash
codex mcp add incident-api-01 -- \
  /absolute/path/agent-tunnel mcp --profile incident-api-01
```

也可使用配置草案：

```toml
[mcp_servers.incident_api_01]
command = "/absolute/path/agent-tunnel"
args = ["mcp", "--profile", "incident-api-01"]
startup_timeout_sec = 10
tool_timeout_sec = 30
```

MCP 单次等待上限拟定 5 秒、read 最多等待 10 秒，均明显短于上述 30 秒。命令自身可以运行更久，由任务生命周期管理。stdio stdout 只输出协议内容，诊断写 stderr。[S3]

### 6.6 pi 接入

首选 CLI + Skill：

```bash
agent-tunnel info --profile incident-api-01 --json
agent-tunnel exec --profile incident-api-01 --json -- uname -a
agent-tunnel exec --profile incident-api-01 --json \
  --shell sh --command 'cd /app && tail -n 100 logs/app.log'
agent-tunnel read --profile incident-api-01 --job <job-id> --cursor 0 --json
agent-tunnel cancel --profile incident-api-01 --job <job-id> --json
```

- Skill 指示先检查目标身份，再读状态，任何远程操作均通过 CLI。
- v1 不覆写 pi 自带 bash/read/edit，避免路径和执行地点混淆。
- 后续 pi 扩展注册 `remote_*` 工具，共用本地 Controller；可展示目标标签、运行状态、取消按钮。[S4–S6]
- MCP 适配器、pi 扩展需要按用户实际安装版本做验收；官方文档核对不是已安装版本兼容性证明。

### 6.7 使用完毕

```bash
agent-tunnel session revoke --profile incident-api-01
```

撤销控制权、停止任务、关闭 Connector、清理内存缓冲和短期凭证；审计元数据按策略保留。删除目标二进制/临时目录为显式清理步骤，不删除用户原有文件，也不声称能回滚已执行命令的业务副作用。

## 7. 工具与执行语义

### 7.1 MCP 工具保持少而稳定

| 工具 | 作用 | 阶段 |
| --- | --- | --- |
| `remote_info` | 返回绑定目标、权限、能力、剩余 TTL、当前身份状态 | P1 |
| `remote_exec` | 启动命令，短时间等待，返回结果或 job ID | P1 |
| `remote_read` | 按 cursor 读取增量输出及状态 | P1 |
| `remote_write` | stdin 写入或关闭 stdin，之后支持 PTY 输入 | P1/P3 |
| `remote_cancel` | 请求终止本工具管理的指定 job | P1 |
| `remote_resize` | 调整 PTY 窗口大小 | P3 |

- MCP adapter 默认固定一个 target；多目标用多个 profile/服务名，不用可变的全局 current target。
- 即使固定 profile，也要求 exec 携带 `remote_info` 返回的 target ID/incarnation 确认值；目标更换后旧值拒绝执行。
- 每次结果携带 target ID、incarnation、job ID；相同主机名不代表同一执行环境。
- `exec/write/cancel` 不标记为只读/无副作用；工具 annotations 只是提示，不是权限实现。
- 管理命令 create/verify/revoke 不默认提供给模型作为普通执行工具。

### 7.2 exec 输入草案

```json
{
  "expected_target": "target_opaque_id",
  "expected_incarnation": "connector_boot_id",
  "argv": ["uname", "-a"],
  "cwd": "/app",
  "env": {"LANG": "C.UTF-8"},
  "timeout_ms": 60000,
  "yield_ms": 1000,
  "stdin_mode": "closed",
  "pty": false
}
```

- `argv` 与 `shell + command` 二选一；argv 不经 shell 拼接。
- shell 模式显式指定，默认只依赖目标已有 `/bin/sh`，不假定 bash 存在。
- 默认 stdin 已关闭，避免程序无故等输入；明确选 pipe/pty 后才允许 write。
- `cwd` 每次显式确定；非 PTY exec 不继承上一次 exec 的 cd/export。
- 不自动读取交互 shell profile，也不把本地环境变量整体复制到远端。
- 环境变量由 Connector 基线 + 明确授权覆盖构成；不默认继承注册 token 等内部秘密。
- 当前目录不是访问控制边界；目录限制只有配套 OS 隔离才有强制性。
- Go 的 os/exec 本身不会自动展开 shell 语法；采用 argv 有助于减少无意拼接，但不是任意代码执行的安全沙箱。[S9]

### 7.3 返回草案

```json
{
  "target_id": "target_opaque_id",
  "incarnation": "connector_boot_id",
  "job_id": "job_opaque_id",
  "state": "running",
  "events": [
    {"seq": 1, "stream": "stdout", "text": "Linux\n"}
  ],
  "next_cursor": 2,
  "exit_code": null,
  "termination_reason": null,
  "output_truncated": false,
  "dropped_before_cursor": null
}
```

- `exit_code: null` 意味着未知/仍在运行，不可转换为 0。
- 工具/网络错误与远端命令非零退出分开；同时给模型明确成功/失败说明。
- stdout、stderr 保留流标识；统一序号只代表采集顺序，不承诺跨管道精确时序。
- 管道模式保持字节准确性，协议二进制输出按需编码；模型视图提供 UTF-8 安全摘要并注明截断。
- PTY 模式通常输出合流，不能伪造独立 stderr。
- ANSI/OSC 控制序列在模型和日志视图清洗；人工 TTY 视图单独处理。

### 7.4 长任务必须独立于单次请求

例：启动 `tail -f` 后很快返回 job ID，agent 再 read，必要时 write/cancel。

两个超时分开：

- `yield_ms`：本次调用多久返回，建议最大 5 秒。
- `timeout_ms`：远端进程允许存活多久，建议默认 60 秒，普通 job 上限 10 分钟，可由启动策略显式调整且不超过 session TTL。

不能通过无限提高 MCP HTTP 超时解决任务生命周期。不得把“响应丢失”解释为“命令没执行”。

### 7.5 PTY 定位

- P3 支持持久 shell / Python REPL / 数据库客户端等文本交互。
- `remote_write` 发送输入，`remote_read` 获取输出；正常 shell 的 cd/export 才能在该 PTY 内持续。
- PTY 的中断由终端语义处理；管道任务用进程组信号，不能把字符 Ctrl+C 当作通用 kill。
- 一次只允许一个写入拥有者；人工 attach 接管后 agent write 被拒绝或排队失败，不静默竞争。
- 对密码输入优先人工接管，不让模型获取敏感交互内容。
- 不承诺模型可靠操作 vim/top 等完整 TUI；优先使用非交互命令和有限输出。

## 8. 应用层协议与状态机

### 8.1 不把它称为“WebSocket MCP 标准传输”

MCP 仅发生在本地 adapter 与 agent 之间。WSS 中承载的是项目自有、版本化的远程执行协议，暂称 ATP/1。业务 session、job ID、控制租约与 MCP 协议会话无关。[S7]

### 8.2 控制面与数据面

- 管理面：创建、撤销、配额；只允许管理员，默认本机 socket。
- 公网连接面：有限注册 API、`/v1/connect`（Connector）、`/v1/control`（Controller）、不泄漏身份的 `/healthz`。
- 公网只允许经授权会话连接；拒绝匿名 target 列举。
- WSS 升级前认证，限制头/帧大小、注册速率、认证失败次数。
- 原生客户端可无 Origin；如果提供 Origin，只允许配置值，不能放行任意浏览器站点。
- 协议协商 major/minor 和 capabilities；未知 major 直接拒绝，minor 功能按能力启用。
- TLS 验证默认强制，跨域重定向不转发认证头；无静默 `--insecure` 降级。

### 8.3 帧与消息

外层保留路由、连接 epoch、帧长度等必要字段。P2 后内部执行消息加密，Relay 不应看见命令/输出正文。

内部消息至少包括：

```text
hello / capabilities
exec.start / exec.accepted
job.read / job.output / job.exited
job.stdin / job.stdin.closed
job.cancel / job.cancelled
pty.resize
controller.lease.renew
resume / resume.result
error
```

消息关联字段：`session_id`、`target_id`、`incarnation`、`request_id`、`job_id`、`seq`、deadline，以及可选能力字段。与权限相关字段必须处于认证保护之下，不只信任外层路由标签。

### 8.4 会话与任务状态

```text
session:
created -> connector_joined -> identity_pending -> ready
ready <-> disconnected
任意活动状态 -> revoked / expired / closed

job:
accepted -> running -> exited
                   -> cancelled / timed_out / failed
重连无法确认执行结果 -> unknown（禁止自动当作失败重跑）
```

- Connector 重启产生新 incarnation，不自动恢复之前的控制授权或任务状态。
- Relay 重启第一版让原路由和短期凭证失效，要求重新配对；Connector 本地失联租约负责回收任务。
- Quick Tunnel 域名改变要求重新配置/配对，不跟随未知新域名自动带凭证连接。

### 8.5 重连、幂等和不确定性

- 心跳初值 15 秒，离线判定约 45 秒；带抖动的指数退避并设置上限。
- 执行器在同一 incarnation 内缓存已接收 request ID 与参数摘要；重复相同请求返回同一个 job，参数不同则报冲突。
- write、cancel 等变更请求也有 ID/序号和去重窗口，防止重复 stdin 写入。
- 单会话只允许一个有效 controller 写入 lease；连接替换原子递增 epoch，Connector 拒绝旧 epoch，避免重连产生双写。
- 重连优先查询 job/读取缺失输出，不能盲目重发新 exec。
- 去重记录未过期时提供限定范围的 at-most-once 接受语义，不声称跨崩溃 exactly-once。
- 接收方已执行而确认丢失时可能只能返回 unknown；副作用由操作者核对。
- 不在 Relay 给离线目标积压命令，避免目标上线后执行过时操作。
- 取消操作必须有 Connector 确认；没收到确认只能报告“已请求/待确认”，不能说进程已结束。
- MCP 取消适配成业务取消，不简单把所有连接断开等同于所有 job 终止；目标 lease/TTL 最终兜底。

### 8.6 背压与容量初值

以下都是待实测的设计初值，不是已达到的性能指标：

| 项目 | 初值 |
| --- | --- |
| 每目标运行任务 | 4 |
| 每目标 PTY | 1（P3） |
| 单次 read 最大输出 | 64 KiB |
| 单 job 内存输出环 | 4 MiB |
| 单目标所有 job 输出总额 | 16 MiB |
| 输出数据帧 | 32 KiB |
| 已完成 job 元数据保留 | 10 分钟或会话结束，先到为准 |
| Relay 开发容量测试 | 10 个 Connector，每个最多 4 个任务 |

输出超限继续排空 OS pipe，丢弃最旧内容并明确报告丢失范围；不能阻塞子进程到无限挂死，也不能无限耗内存。控制消息有优先级，输出洪泛时 cancel/心跳仍需可用。除非显式开启有额度/TTL 的 spool，输出不写磁盘。

### 8.7 仅 HTTPS 可用时

- P0 验证实际 WSS；失败时先诊断 TLS、域名、代理和 Upgrade，而非绕过出口管控。
- 若用户场景确实禁止 WebSocket，但允许批准的 HTTPS 请求，再实现短轮询传输适配器。
- 该适配器复用相同身份、租约、任务 ID、序号与加密消息语义；不是另一套执行系统。
- 不在首发同时维护多套未验证传输；SSE 不是 Quick Tunnel 备用方案。

## 9. 安全设计与上线门槛

### 9.1 最重要的权限事实

授予任意 exec，就是授予 Connector OS 身份可实现的操作能力，包括读敏感文件、修改文件和访问其可达网络。Skill 提示词、命令黑名单和 MCP 工具标签不能将它变成真正只读环境。

- 默认非 root；显式展示 UID/GID、执行位置和能力。
- 用户需要额外权限时，在原有运维渠道决定如何提供，不由模型自行升级。
- 只读模式若需要强保证：另做固定诊断工具 + OS 隔离，不开放任意 shell。
- 仅靠进程组不能保证清理恶意 daemonize/setsid 后的全部后代；严格清理需可用的 cgroup/容器级隔离。
- 第一版承诺管理普通前台任务及进程组，不承诺在无权限环境可靠约束恶意任意代码。

### 9.2 凭证分离

- 管理员：创建/撤销会话，凭证不交给 agent。
- controller：只能控制已配对的特定会话/目标。
- join：短时一次性注册，不能调用 exec，也不能获取 controller 凭证。
- connector：登记成功后换取与自身身份绑定的重连凭证，不能控制其他目标。
- session ID 和随机公网域名都不是秘密，不能替代认证。
- 初值采用至少 256-bit 随机能力 token；服务端不记录原文，凭证过期/角色/受众严格验证。
- 不在 URL、进程参数、日志、工具响应中写秘密；本地凭证目录 0700，文件 0600。
- MCP HTTP 网关若后续实现，应单独设计标准授权，不能将内部 join token 当作任意 OAuth token 透传。[S10]

### 9.3 配对与端到端保护

分两个明确等级：

**P0/P1：受控测试。** 外层 TLS/WSS + 分角色短期凭证，只使用无敏感数据的测试目标。Relay 和 TLS 终止方仍处于内容信任链中，不可宣传为端到端加密。

**P2：真实敏感环境门槛。** Controller 与 Connector 之间建立独立认证的端到端加密通道，Relay 只转发密文。建议优先验证“在 Relay 有序双向字节流上运行标准 TLS 1.3 双向认证”，复用标准 TLS 实现；不自创握手/密码算法。

实施前以独立 ADR 固化以下要求并做攻击测试：

1. 身份密钥由端点生成，不由 Relay 代持。
2. controller 身份通过已有可信运维入口交给 Connector；目标身份通过同一可信终端返回核对并 pin。
3. 若信任材料全部从同一不可信 Relay 取得，不能抵抗 Relay 中间人；必须拒绝这种“自动配对即可信”的设计。
4. 一次性 join 只授权路由登记，不等同于对端身份验证。
5. 标准证书校验/私有信任根与身份 pin 策略明确，不能用“关闭校验”代替认证。
6. session、target、incarnation、控制 epoch 绑定到认证会话；拒绝重放、错路由、降级。
7. 断线重连重新建立加密通道，应用层恢复依靠既有 job ID/cursor；不重放旧加密流。
8. 端到端保护启用后失败即停止，不能自动退回明文应用载荷。

E2EE 不隐藏连接 IP、时间、流量和部分路由元数据，不防端点本身被入侵。远程输出最终仍会被本地 agent 发给所用模型服务；敏感生产数据是否允许进入模型上下文，需另行授权和脱敏。

### 9.4 审批与控制权

建议策略：

- `manual`：每个新 exec，以及交互输入授权，在本地独立终端人工确认；无人可确认时拒绝。
- `session-approved`：人工为一个绑定目标、权限和 TTL 的会话授予执行权限，适合无人值守诊断循环；这意味着主动接受任意 exec 风险。
- PTY 可按“整段交互授权”处理，不能假装审批一个 shell 启动等于审查了后续所有输入。

审批不能长时间占用 MCP 调用：未获批准时快速返回 `APPROVAL_REQUIRED` 和非秘密 approval ID；人工在本地终端确认后，调用方使用同一 approval ID、request ID 和完全相同的参数恢复提交。审批 ID 本身不能授予权限，拒绝/过期的审批不能自动重建并执行。CLI/MCP schema 需提供恢复字段，Controller 负责去重与状态查询。

第一版默认 manual。可信操作人员可以显式切换策略；模型不拥有更改策略和自己批准自己的接口。approval 绑定实际参数摘要、target、incarnation、有效期，不能挪用给另一条命令。

本地 agent 的安全提示和沙箱不能取代 Connector 的权限/TTL 校验。Controller/Connector 的所有入口共用授权核心，避免 pi CLI 绕过 MCP 上的限制。进程内审批不是对同 UID 恶意本地程序的强隔离；强保证需独立身份或 OS 边界。

### 9.5 断线与撤销

- Controller 的控制权心跳与 Connector↔Relay 网络心跳分离；Relay 连着不能代表操作者仍在线。
- Connector 验证端到端控制 lease，失联后立即拒绝新任务；现有任务拟定 60 秒宽限后取消。
- 会话 TTL 初值 1 小时，Connector 自己根据本地计时执行，即使 Relay 不可达也到期关闭。
- 在线撤销立即推送；网络分区时承诺在 lease 上限内收敛，不虚称瞬时全局撤销。
- kill 后不恢复业务副作用。应用子进程异常逃离管理边界时必须报告，不假称完全清理。
- Connector 正常退出清理进程组和本工具创建的临时目录；异常崩溃的清理能力必须按 OS 测试报告说明。

### 9.6 审计与不可信输出

- Connector/Controller 记录会话 ID、请求 ID、目标身份、时间、授权决定、退出原因、是否确认取消。
- Relay 只记录认证/路由/连接/配额元数据；E2EE 下没有命令正文。
- 默认不集中保存完整 stdout/stderr 或环境变量；完整审计为显式选项，权限、额度和 TTL 可配。
- 命令摘要/哈希也可能泄漏可猜测信息，按敏感元数据处理。
- 远端日志和文件内容视为不可信数据，不能指挥模型换目标、上传密钥、延长 TTL 或修改授权。
- 不从远端动态下载 Skill/工具 schema；适配器和 Skill 随本地可信发行版发布。
- ANSI/OSC、超大 JSON、异常 Unicode、输出洪泛都纳入恶意输入测试。

## 10. 服务器与容器适配

### 普通 Linux

- 单一可执行文件，用户指定可写且可执行目录，不要求安装系统服务。
- 验证 amd64/arm64、glibc/musl 常见环境；构建目标是尽量减少运行时依赖，不能只凭交叉编译成功宣称兼容。
- 检查 DNS、CA、系统时间、HTTPS/WSS、代理、目录执行权限和 shell 可用性。
- 支持显式受信 CA 配置；缺证书时不得静默关闭 TLS 校验。
- 远端不要求 Go/Node/Python/npm，但要有运行二进制所需的系统条件。

### 已有 Docker / Podman 容器

- 优先将 Connector 复制进目标容器，通过现有 exec 入口以前台方式运行。
- 不挂载 Docker/Podman socket，不默认 privileged。
- 镜像只读、无可写可执行目录、noexec 或 seccomp 限制时，清楚报告限制，由运维方选允许的方法。
- 不自动退回到宿主机执行同一条命令。

### Kubernetes / distroless

- 有 shell/exec 条件时进入目标容器运行；没有 shell 不代表绝对无法运行二进制 argv，但 shell 命令无法提供。
- 可评估经授权的 ephemeral debug container；它不是无需条件就“等同进入原容器”的能力。
- Kubernetes 的 ephemeral container 支持与隔离条件需以集群 RBAC、安全策略、runtime 和命名空间可见性实测；官方将其定位于临时故障诊断。[S11]
- Skill 必须显示当前是哪个 Pod/容器/调试容器，不能误述文件系统和 PID 可见范围。

### 发布与获取

- 第一阶段手工复制固定版本二进制；后续提供平台识别下载器。
- 固定版本、校验和、签名/可信公钥；同一被劫持下载源提供的 hash 不能单独证明发布者可信。
- 不将 `curl | sh` 作为唯一或默认安装方式。
- 下载入口、Relay 入口、Cloudflare connector 出口分别记录网络依赖。
- 目标 Connector 只需访问 Relay 的获准 HTTPS/WSS 入口；无需安装 cloudflared，也无需承担 cloudflared 自身的出口网络要求。

## 11. 工程实现建议

### 技术栈

- 核心：Go，统一 CLI/Relay/Connector/Controller。
- MCP：官方 Go SDK，实施时固定通过兼容矩阵和测试的版本。[S8]
- 执行：os/exec + 平台进程管理；PTY 库单独选型、锁版本并检查许可/维护情况。
- WSS：选择维护活跃的 Go 库；由 P0 以心跳、取消、帧限额和代理测试选定，不在规划阶段凭印象锁定。
- pi 扩展：薄 TypeScript 适配，仅负责 schema/UI/调用 Controller，不再实现隧道栈。[S6]
- 状态：第一版内存 + 本地受保护 profile/身份文件；不引入数据库到执行热路径。
- 发布：Linux amd64/arm64 首发；可复现构建、校验和、签名、依赖清单和许可检查。

### 拟议目录

```text
cmd/agent-tunnel/
internal/
  protocol/          # 版本化消息、错误、能力和帧限额
  transport/         # WSS，后续可选 HTTPS 轮询
  relay/             # 配对注册、路由、配额；不包含 exec 实现
  connector/         # 授权、目标状态、任务调度
  controller/        # 本地 IPC、租约、目标绑定、人工审批
  executor/          # argv/shell、pipe、进程组
  terminal/          # PTY、resize、人工接管
  identity/          # token、端点身份、端到端认证
  mcpserver/         # 官方 SDK 适配
  cli/               # 命令、JSON 输出
  audit/
integrations/pi/     # 后续扩展
skills/remote-debug/ # 后续可信 Skill
examples/
tests/integration/
tests/e2e/
docs/
```

这里只定义目录规划，不在本轮生成占位代码或安装依赖。

### 错误模型

至少区分：`UNAUTHORIZED`、`ROLE_MISMATCH`、`SESSION_EXPIRED`、`TARGET_UNVERIFIED`、`TARGET_MISMATCH`、`TARGET_OFFLINE`、`LEASE_EXPIRED`、`APPROVAL_REQUIRED`、`RESOURCE_LIMIT`、`REQUEST_CONFLICT`、`JOB_NOT_FOUND`、`OUTPUT_GAP`、`UNSUPPORTED_CAPABILITY`、`PROTOCOL_MISMATCH`、`EXECUTION_UNKNOWN`。

输出包含 retryable 与结构化上下文，但涉及非幂等 exec 时，即使网络错误可重试，也只允许用同一个请求 ID 查询/恢复，不能重新生成命令请求。

## 12. 分阶段路线与完成标准

工期是单人集中开发的粗估，非承诺；安全评审、目标环境差异和网络准入可能显著影响时间。按验收门槛推进，不按日期强推上线。

### P0：链路与兼容性探针（1–2 工作日）

交付：最小 echo Relay/Connector、Quick Tunnel 操作记录、协议能力探针。

必须验证：

- 目标主动出站，经 Quick Tunnel 双向 WSS 收发；同时验证 Controller 公网路径。
- 空闲至少 30 分钟；持续连接至少 2 小时；主动断网/重启后能报告失联并重连。
- HTTP 代理和目标实际出口策略；失败能定位到 DNS/TLS/Upgrade/认证层。
- Linux 两架构至少构建与独立运行；无样机的架构标注“仅构建未运行”。
- Go SDK stdio 工具被实际 Codex 识别；pi CLI JSON 输出能被实际版本调用。
- 只运行测试 payload，不接真实生产系统。

退出门槛：证实推荐传输可用；若 WSS 不可用，先决定获准入口或轮询方案，再继续。

### P1：最小功能闭环（5–8 工作日）

交付：角色拆分、配对、TTL/撤销、Local Controller、exec/read/write/cancel、stdio MCP、pi CLI + Skill 草案。

必须验证：

- 两个测试目标互相隔离，普通 controller 无法访问未授权目标。
- 短命令、非零退出、超时、长任务、stdin EOF、大输出截断。
- 同一 exec ID 重发不再次启动；丢失响应后不会自动重跑。
- session/profile 固定目标；目标重启后旧 incarnation 拒绝执行。
- manual 审批、明确的 session-approved 切换；token 不出现在工具输出。
- 正常撤销和 lease 超时清理普通进程组。

退出门槛：可在可丢弃测试环境由 Codex/pi 完成一次完整诊断循环；不作为生产就绪声明。

### P2：敏感环境准入（4–7 工作日 + 独立安全复核）

交付：端到端身份与加密 ADR/实现、审批绑定、凭证生命周期、可验证发布和审计。

必须验证：

- Relay 篡改、路由交换、重放、假目标、过期凭证均拒绝。
- join 凭证无法控制 target，controller 凭证无法扩展 scope。
- Relay 抓取应用载荷看不到命令/输出；错误日志不泄漏密钥。
- 错误证书/指纹、降级、陌生新域名不会自动接受。
- 断线 60 秒 lease 门槛与 session TTL 由 Connector 本地执行。
- 清理边界、残留进程检测、模型数据外发风险得到明确说明。

退出门槛：完成安全复核，用户明确授权后才进行低风险、短时、受监督的真实环境试用。Quick Tunnel 仍按开发测试入口定位。

### P3：交互体验（3–5 工作日）

交付：PTY、resize、持续 shell、人工接管、pi 原生扩展。

必须验证：

- shell 中 cd/export 持续，普通 exec 不意外共享状态。
- REPL 输入/输出、SIGINT、EOF、终端 resize、断线清理。
- agent 与人工写入互斥；敏感输入不进入默认模型/审计正文。
- 与实际 Codex/pi 版本验证取消、进度、输出截断和多轮读取。

退出门槛：代表性故障排查任务成功，不以“支持 PTY”推断“所有 TUI 可自动操作”。

### P4：自有公网部署（2–4 工作日）

交付：固定域名 TLS 入口、单实例 Relay 的容器/服务部署、受保护管理面、运行手册。

必须验证：

- 不修改 ATP/MCP 工具即可从 Quick Tunnel 切到固定入口。
- 代理支持 WebSocket、合理空闲超时，凭证和敏感头不记访问日志。
- 管理面不公开；限流、帧大小、连接上限生效。
- Relay 重启明确失效旧会话，Connector 依靠 lease 回收，不复活旧任务。
- 灰度升级、协议版本不兼容拒绝、撤销、紧急停止和数据清理演练。
- 至少 24 小时 soak test、10 个 Connector/40 个有限任务的资源与背压测试。

退出门槛：自用稳定版。多租户、HA、HTTP MCP 网关另立项目范围。

建议总排期：约 3–6 周开发及验证，再预留安全复核/网络准入时间；不要将此粗估解释为固定交付日期。

## 13. 总体验收矩阵

| 类别 | 必测情形 | 通过条件 |
| --- | --- | --- |
| 权限 | 匿名、错角色、错 target、过期/复用 join、撤销后调用 | 在 Connector 执行前拒绝，Relay 无越权路由 |
| 身份 | 目标同名替换、incarnation 改变、假 Relay/假 Connector | 不自动信任；要求重新确认 |
| 命令 | argv 引号/空格、shell 管道、cwd、env、stdin、退出码 | 语义一致，不意外在本地执行 |
| 生命周期 | TTL、命令 timeout、Ctrl+C、controller/connector 崩溃 | 结果可解释；不虚报清理成功 |
| 幂等 | exec/write ack 丢失、重发、去重缓存失效 | 不静默重复副作用；不确定状态明确 |
| 输出 | 大 stdout/stderr、二进制、ANSI/OSC、Unicode | 内存受限、可读结果安全、丢失可见 |
| 网络 | 断网、WSS 断开、域名变化、TLS/CA 错误、代理 | 安全失败；不退回 insecure；不盲目重跑 |
| 容器 | 非 root、只读文件系统、noexec、无 shell、不同 libc | 明确支持/不支持；不隐式升级权限 |
| agent | 实际 Codex stdio、pi Skill、后续 pi 扩展 | 一次完整诊断与取消闭环 |
| 迁移 | Quick Tunnel -> 自有域名 | 换入口与配对材料，不改工具 schema/业务代码 |
| 安全 | 路由篡改、帧重放、输出 prompt injection | 身份认证阻止伪造；不把远程文本当授权 |

每次验证保存：版本、架构、入口类型、网络环境、命令、脱敏日志、观测结果、未验证项。模拟/单元测试、容器集成测试、实网测试分别记账。

## 14. 后续可选演进

1. JSON 响应模式的 Streamable HTTP MCP 网关；单独做认证与版本协商。它不是第一版依赖，且若由 Relay 解密映射工具，会改变 E2EE 信任边界，需要明确选择可信网关或在操作者端终止。
2. 批量只读诊断工具；不要以通用 exec 黑名单冒充只读模式。
3. 小文件 get/put：配额、路径策略、内容校验、审批、下载工件 TTL。
4. WebSocket 不可用场景的 HTTPS 轮询传输。
5. 操作者共享、只读旁观者、团队 RBAC 与外部身份系统。
6. Relay HA/跨实例路由、持久审计、受控 job spool。
7. 有现成 SSH/堡垒机时的 transport/backend 复用。

## 15. 开工决策记录

默认采用下面的决定即可进入实现，不需要先建设完整平台：

- [建议采用] Go 单二进制，Linux amd64/arm64 目标。
- [建议采用] 本地 stdio MCP + pi CLI/Skill，pi 扩展后置。
- [建议采用] Relay 放开发机，Quick Tunnel 只暴露 Relay 数据面。
- [建议采用] 单操作者、固定目标 profile、短期会话、默认人工审批。
- [建议采用] P0/P1 只接测试目标，P2 为敏感环境准入门槛。
- [需 P0 实测] 当前网络能否稳定 WSS、目标架构/CA/可执行目录。
- [需实施前固化] 端到端 TLS 字节流封装、双向身份配对 ADR 与库版本。
- [真实使用前确认] 生产输出是否获准发送至所用模型服务；哪些权限和变更允许授权给 agent。

## 16. 官方资料记录

以下地址是本次核对来源，不代表本项目已经完成对应兼容性测试。

- S1 Cloudflare Quick Tunnels：`https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/do-more-with-tunnels/trycloudflare/`
- S2 Cloudflare WebSockets：`https://developers.cloudflare.com/network/websockets/`
- S3 Codex MCP：`https://developers.openai.com/codex/mcp/`（本次跳转至 `https://learn.chatgpt.com/docs/extend/mcp?surface=cli`）
- S4 pi 官方 README：`https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md`（原 badlogic/pi-mono 地址本次跳转到此仓库）
- S5 pi Skills：`https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/docs/skills.md`
- S6 pi Extensions：`https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/docs/extensions.md`
- S7 MCP 2026-07-28 Streamable HTTP：`https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http`
- S8 MCP 官方 Go SDK：`https://github.com/modelcontextprotocol/go-sdk`
- S9 Go os/exec：`https://pkg.go.dev/os/exec`
- S10 MCP Security Best Practices：`https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices`
- S11 Kubernetes Ephemeral Containers：`https://kubernetes.io/docs/concepts/workloads/pods/ephemeral-containers/`
- S12 OpenAI Secure MCP Tunnel：`https://developers.openai.com/api/docs/guides/secure-mcp-tunnels`
