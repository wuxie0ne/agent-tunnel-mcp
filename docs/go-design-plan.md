# agent-tunnel：Go 版整体规划设计

日期：2026-09-30。状态：**已实施；部分真实客户端/公网场景已验证，完整验收未通过**。逐项证据见 `docs/implementation-status.md`。

需求依据：`docs/go-requirements.md`。需求文档决定做什么，本文决定怎样以较小工程量实现；冲突时以需求为准，不能借设计静默删减 P0。后续用户已授权无人值守开发、验证与阶段性提交；设计中的范围变更仍须讨论。

## 1. 设计结论与原则

采用 **Go 单仓库、单可执行文件、两种运行角色**：

- `agent-tunnel target`：目标侧访问端，负责 MCP、凭据/模式、审核、执行、命令历史和接入管理。
- `agent-tunnel relay`：自部署中转，负责目标注册、地址路由和 HTTP 转发，不执行目标命令，不建设另一个 Agent。
- 一个执行核心、一套 MCP 工具、一种本地 JSONL 命令历史。Quick Tunnel 和自部署中转只更换接入方式。
- 默认值直接给出，用户只需选择模式/入口并拿到 URL；不引入数据库、持久任务调度、插件框架、Web 管理界面或多层配置体系。
- 对调用者保持同步命令语义；内部使用 goroutine 不等于交付后台任务。断线后运行中的命令仍收尾，但不提供结果补取。
- 使用目标现有权限；不把目录、模式、日志或 token 当作沙箱。注册准入与命令授权分开，不声称明文中转提供端到端安全。

**开发前优先解除的两个风险**：真实客户端能否通过两种入口完成不依赖 SSE 的审核；Quick Tunnel 能否承载默认及自定义长命令。其余按本文实施，不为普通字段名和默认参数反复决策。

## 2. 已核对的外部约束与验证门槛

以下官方资料于 2026-09-30 查阅。协议/库版本仅是设计候选，不代表已安装或已验证；来源定位见第 15 节。

| 约束 | 设计处理 | 尚需证据 |
| --- | --- | --- |
| 当前正式 MCP 规范为 `2026-07-28`；后续 RC/draft 不作为首版默认基线 [R1] | 优先以此版本与支持该版本的官方 Go SDK 为基线；官方兼容表列出 Go SDK v1.7.0+ 支持该版本，实现时锁定具体正式版本 | 实际用户客户端和锁定 SDK 的协商结果 |
| 此规范允许单个 JSON 或请求级 SSE 响应，审核输入交互改为 MRTR；SDK 提供相应支持 [R2, R3, R5] | JSON-only HTTP，审核走规范的输入请求/续接机制；不手写旧版 SSE 协议栈 | 输入请求、续接、拒绝、过期、断线的完整流程 |
| Quick Tunnel 官方列明不支持 SSE，且有 200 个在途请求上限 [R6] | 不依靠 SSE 绕过限制；项目自身采用更小的资源上限 | 不依赖 SSE 的真实 Quick Tunnel MCP 审核 |
| Cloudflare 代理的默认 Proxy Read Timeout 文档值为 125 秒；不能据此断言 Quick Tunnel 的具体表现 [R7] | 默认命令 120 秒加清理/回传已接近该值，必须实测；不假定自定义长命令可用 | 无输出命令 120 秒、300 秒，及返回较大结果 |

**G0：审核兼容性门槛**。至少用一个用户实际可用的真实客户端，通过两种入口完成“请求 → 本地用户确认 → 目标执行 → 结果”；同时证明拒绝、不支持确认和过期时没有执行副作用。不能用模拟客户端的通过代替这一项。

**G1：同步时限门槛**。分别测试默认 2 分钟、自定义 5 分钟无输出命令、超时终止，以及 256 KiB 内容返回。应用程序设置较长超时，不会自动消除客户端/代理限制。

若现有客户端仅支持旧版 HTTP SSE 审核，或代理时限冲突，记录证据后回到用户讨论。不得悄悄增加本地网关、改用正式 Tunnel、缩短既定超时、改异步任务或把审核降级为放行。优先选择可用客户端/SDK；不为了泛兼容维护两套审核实现。

## 3. 总体架构

### 3.1 Quick Tunnel

```text
用户的本地 Agent
    │ MCP / HTTP JSON（含 token）
    ▼
Cloudflare Quick Tunnel
    │ cloudflared 回源
    ▼
127.0.0.1:随机端口 /mcp
    │ 同一 MCP handler
    ▼
目标核心：授权 → 审核 → 命令执行 → 本地 JSONL
```

目标进程启动回环 HTTP 服务，再启动 `cloudflared tunnel --url ...`。不监听目标的公网管理端口，不要求目标有入站端口。自带版使用包内 `cloudflared`，精简版使用显式指定路径或 `PATH` 中已有程序；无运行时下载。

### 3.2 自部署中转

```text
本地 Agent ── MCP / HTTP JSON ──→ relay 公网 HTTP 路由
                                     │
                          已由目标发起的 WebSocket
                                     │ 有界 HTTP 请求/响应转发
                                     ▼
                             目标内存 HTTP 入口
                                     │ 同一 MCP handler
                                     ▼
                        同一授权/审核/执行/JSONL 核心
```

- 目标主动连接中转的 `ws://` 或已配置的 `wss://` 地址；中转模式的目标不必开启 TCP 监听端口。
- 中转不是新的 MCP 实现。它把允许的 HTTP 方法、MCP 路径、必要请求头及有界 body 转给目标；目标在进程内构造 HTTP 请求交给同一个 `net/http` handler，收集有界 JSON 响应。
- WebSocket 是项目内部转发通道，不是提供给 Agent 的 MCP transport。不支持通用反向代理、任意路径访问、文件访问或端口转发。
- JSON-only 响应可按完整请求/响应转发，不增加 SSE 分块和事件恢复。MRTR 的每次 HTTP 请求都转到同一目标，目标保存审核所需的小量临时状态。
- 明文 HTTP/WS 模式下中转及链路可能看到或篡改内容，不做端到端加密；已有 TLS 部署可接入，但不为首版开发证书自动化。

选择“转发 HTTP，而非在中转重新实现 MCP 工具”，是为了让审核绑定、token 判定和执行状态只在目标维护一份。

## 4. Go 技术选型与代码组织

### 4.1 依赖原则

| 项目 | 选择 |
| --- | --- |
| 开发工具链 | 当前本地已核对为 Go `1.26.2 linux/amd64`，先使用已有工具链；不要求目标安装 Go |
| MCP | 官方 `github.com/modelcontextprotocol/go-sdk/mcp`；验证后锁定具体版本，不随部署自动更新 |
| 内部 WebSocket | 优先 `github.com/coder/websocket`，只用于目标/中转通道；验证后锁版本 [R8] |
| HTTP、CLI、JSON、随机数、日志文件 | 标准库 `net/http`、`flag`、`encoding/json`、`crypto/rand`、`os` 等 |
| 进程组信号等 Linux 操作 | 按实现需要使用 `golang.org/x/sys/unix`；不引入终端/PTY 库 |
| 存储 | JSONL 文件；没有 SQLite、任务数据库、Redis 或外部消息队列 |

先争取 `CGO_ENABLED=0` 的 Linux amd64 发布构建，并验证实际目标运行；不把构建成功等同于 `cloudflared` 的兼容性。`go.mod/go.sum` 在实施阶段建立，本文不猜测依赖发布版本号。

### 4.2 最小目录

```text
cmd/agent-tunnel/main.go       # target/relay 子命令及组装
internal/config/              # 参数检查，有限的环境变量凭据入口
internal/target/              # 实例生命周期、取消/到期/停止协调
internal/mcpserver/           # 两个工具、协议协商与审核适配
internal/executor/            # 命令校验、进程组、输出与清理
internal/history/             # JSONL 事件、同步写入、轮转和健康状态
internal/tunnel/              # cloudflared 查找、启动、URL、停止
internal/relay/               # HTTP 路由、注册表和在途请求
internal/relayproto/          # 内部转发消息及目标连接
scripts/                      # 打包与可重复的验收入口
docs/go-requirements.md
docs/go-design-plan.md
```

不提前建 repository/service/controller 多层模板。不把接口泛化为插件系统：需要的边界只有执行服务、历史写入器、接入管理及共享 HTTP handler，能具体实现就不用抽象工厂。

## 5. 启动配置与默认资源上限

CLI 名称为设计约定，实施时可小幅调整；保持一套启动参数，不加配置文件/环境变量/远程设置三套覆盖链。

| 参数/资源 | 默认或上限 |
| --- | --- |
| `target --transport` | 必填：`quick` / `relay` |
| `target --mode` | 必填：`allow` / `review` / `strict` |
| token 有效期 | 默认 2h，启动时起算；可调，首版正值且最多 24h |
| 命令超时 | 默认 2m；可调，最多 30m。上限不是链路兼容性承诺 |
| 执行并发 | 默认 2；可调，1～32 |
| shell | 默认 `/bin/sh`；可启动时指定 |
| 默认工作目录 | 访问端启动工作目录；调用可指定绝对目录 |
| 日志根目录 | 默认 `<启动目录>/agent-tunnel-logs`；可启动时指定 |
| 初次接入等待 | 最多 30s；失败退出，不展示可用 URL |
| 审核等待 / 待确认数 | 固定 60s / 每目标 8 个 |
| 内存防重 | 在途记录 + 最近 256 条结束记录；不存输出 |
| 输出采集 / 默认返回 / 最大返回 | 每路 1 MiB / 合计 8 KiB / 合计 256 KiB |
| 命令/参数内容合计 | 最多 64 KiB；参数数量最多 1024；超限明确拒绝，不截断后执行 |
| HTTP 请求 body / 响应序列化大小 | 128 KiB / 2 MiB；包含协议和转义开销，超限拒绝，不无限缓冲 |
| 每目标 MCP HTTP 在途数 | 最多 16；独立于命令并发与审核名额，超出返回资源错误 |
| relay 目标容量 / 全局 HTTP 在途数 | 16 个已注册路由 / 64 个请求；离线路由也占注册名额直到回收 |
| WebSocket 单消息 | 最多 4 MiB，含 body 编码开销；写入最长 10s |
| 心跳 / 连接判离线 | 每 10s；连续约 30s 无有效活动则断开。不是零延迟故障检测 |
| 重连退避 | 1s 起，倍增至 10s，带小量随机抖动；只重连，不重放请求 |
| 离线路由回收 | token 仍有效时保留以保证同进程重连原地址；到期且无在途调用后回收 |
| 命令清理 / 管道收尾 | SIGTERM 后等待 2s 再 SIGKILL；后代持管道收尾最多再等 1s |
| 命令历史 | 每文件 10 MiB，最近 5 个文件/实例，轮转不压缩 |

这些值是个人使用的保守设计默认值，不是性能指标。调整涉及生命周期语义时更新需求；只调实现资源上限时同步文档和测试。不能为了让 120 秒命令适应代理时限而静默缩短清理等待。

未来用户使用示例（不是当前已有命令），按 fish：

```fish
./agent-tunnel target --transport quick --mode review

set -gx AGENT_TUNNEL_REGISTRATION_KEY '<私有注册密钥>'
./agent-tunnel relay --listen 0.0.0.0:8080
./agent-tunnel target --transport relay --relay-url http://公网IP:8080 --mode allow --ttl 4h
```

两种角色都从同一个专用环境变量读取共享注册密钥。不把密钥放进 URL/命令示例，启动后也不打印；命令实际权限仍能接触运行身份可读取的环境，不能以此声称凭据隔离。

## 6. MCP 工具与结果模型

### 6.1 工具集合

| 工具 | 输入 | 行为 |
| --- | --- | --- |
| `target_info` | 空对象 | 返回名称、实例、模式、到期时间、接入方式/状态、执行身份、默认 cwd/shell、资源限制、历史目录/写入健康状态 |
| `exec` | 以下结构 | 按模式执行非交互命令，同步返回结果 |

```json
{
  "shell_command": "uname -a",
  "cwd": "/tmp",
  "max_output_bytes": 8192
}
```

- `shell_command` 和 `program` 必须二选一；shell 形式不得额外附带独立 `args`。直接形式为 `program` + 字符串数组，不拼接成 shell。
- 不接受 `approved`、调用级 `timeout`、`env`、`stdin` 或后台执行参数；拒绝未知字段，不能用它们绕过权限。
- `cwd` 缺省时取启动 cwd；非空时必须为存在且可进入的绝对路径。禁止 NUL 字节，大小按解析后实际参数统计。
- 请求标准化后生成不可变 `CommandSpec`，包含实例、模式、执行身份、形式、shell/程序、参数、绝对 cwd、超时与返回预算；审核和日志绑定这一份内容，不在批准后重新解析替换。
- 名称、cwd、输出都是不可信目标数据，工具描述不把其中的文本当作授权或新的 Agent 指令。

### 6.2 结果

`structuredContent` 作为机器可读主体，`content` 只给简短状态摘要，不再复制全部输出以免预算翻倍。示意字段：

```json
{
  "request_id": "目标生成的内部标识",
  "status": "exited",
  "started": true,
  "exit_code": 0,
  "signal": null,
  "timed_out": false,
  "stdout": "...",
  "stderr": "",
  "stdout_capture_truncated": false,
  "stderr_capture_truncated": false,
  "stdout_return_truncated": false,
  "stderr_return_truncated": false,
  "cleanup_status": "not_needed",
  "history_status": "ok"
}
```

- `started` 可为 true/false/未知；未知时不能造一个退出码 0。清理与历史状态独立于命令退出状态。
- 退出码非零表示程序实际退出，不是启动失败；保留已采集输出。拒绝、繁忙、启动失败、超时、重复与未知分别表达。
- 输出按返回文本的 UTF-8 字节预算分配；非法字节使用替换表示并标记编码处理，不声称无损传输二进制。截断不切断有效 UTF-8 字符。
- 服务端已知未知时，返回 `EXECUTION_UNKNOWN` 及说明；链路彻底断开时只能依靠工具说明提醒客户端不能据此安全重试。
- 协议错误使用 MCP/HTTP 约定；业务错误采用稳定代码：`UNAUTHORIZED`、`TOKEN_EXPIRED`、`MODE_DENIED`、`APPROVAL_UNAVAILABLE/REJECTED/EXPIRED`、`INVALID_ARGUMENT`、`TARGET_OFFLINE`、`BUSY`、`HISTORY_UNAVAILABLE`、`START_FAILED`、`COMMAND_TIMEOUT`、`REQUEST_DUPLICATE/CONFLICT`、`EXECUTION_UNKNOWN`。具体映射由 SDK 验证确定，不把所有业务错误都做成 HTTP 500。

## 7. 授权、审核与执行状态

### 7.1 唯一判定点

目标核心是执行的最终判定点。两种入口均先验证 token（支持 URL query，必要时支持 Bearer 头；两者同时存在且不同则拒绝）。token 和实例标识用加密安全随机数生成；实例 ID 只标识目标，不充当密码。过期/停止/历史故障状态不接受新执行。

MCP handler 使用该 SDK 的 `Stateless=true`、`JSONResponse=true`，首版把支持版本限制为候选 `2026-07-28`，由 SDK 提供必要的发现/版本处理；不宣告订阅、进度流或动态列表通知能力，避免意外打开 SSE。选项与能力屏蔽需在 M0 核对锁定 SDK [R5]。

HTTP 层限制路径、方法、body 和Origin 检查（有 Origin 时仅允许已交付入口对应的受信任 origin）；不配置通配 CORS，不公开通用 `/debug`。回环服务也验证凭据。协议协商按 SDK 完成，不假定 URL token 等于一套完整 OAuth 实现。

### 7.2 审核与 MRTR

- 只有 `review` 使用规范的 form elicitation，展示目标/实例、完整命令、cwd 和超时，让客户端请求用户明确确认。确认 schema 只请求一个默认不勾选的布尔值，必须同时满足 `action=accept` 和确认值为 true；decline/cancel/缺少字段/false 都不执行，不让用户改写执行内容；`allow` 不额外询问，`strict` 直接拒绝。
- 先检查客户端能力。SDK 返回“不支持确认”、拒绝或超时即拒绝，不允许调用方的普通参数充当批准。
- `2026-07-28` 的 MRTR 可能将一次逻辑操作分成多次 HTTP：服务端返回输入请求及续接状态，客户端获取用户输入后重新提交。**这不是已启动命令的重试，也不是异步执行任务**；批准前没有命令副作用 [R3, R4]。
- 通过 SDK 的 MRTR 接口传递状态，目标本地仅保存最多 8 条待确认记录。`requestState` 优先用高熵随机句柄，查表得到不可变 `CommandSpec`、实例、token 范围、接入代际和 60s 期限；句柄一次消费，回传参数重新核对指纹。不把完整命令/权限直接编码成可修改的客户端 JSON，也不依赖客户端自报的 clientInfo 作认证身份。用不可猜句柄、服务端查表/内容绑定和一次消费保护状态，不自建加密状态容器；若 SDK 无法传递这样的 opaque state，作为 M0 的接口冲突处理。不得用未验证的 state 决定是否执行 [R3]。续接内容冲突、标识无效或已使用则拒绝。
- 每目标只有 8 个待确认条目。当前 HTTP 正常结束并返回输入请求，不等于取消逻辑审核；显式取消、过期或连接代际失效才取消对应等待。实现必须按 SDK 真实生命周期处理，不盲目用每次 HTTP 的 context 销毁审核状态。
- 中转断线/重新注册取消该接入代际未完成审核；Quick Tunnel 接入重建同理，不恢复旧批准。批准后再次检查 token、实例/代际、停止/历史健康状态、取消及执行额度。

续接字段名、SDK API 和客户端能力的具体对应关系以锁定版本原型为准；本节不是已验证的安全实现，也不独立证明真人点击。

### 7.3 状态机与取消

```text
received → validated → [awaiting_approval] → admitted
                                          │
                                   intent_synced
                                          │
                                      starting
                                      /      \
                              start_failed   running
                                                │
                                      exited / timed_out
                                                │
                                           finalized
```

- 拒绝/取消发生在启动前：不执行，必要时记录拒绝事件。没有额度立即繁忙，不进入任务队列。
- 最后校验、占名额、写入意图和启动之间需有清晰的启动判定点；token 在该点已到期则不启动。历史同步写入后还需检查取消/到期，未启动也写相应收尾事件。
- 原始 HTTP context 只用于尚未启动的请求和返回通道；已经启动后使用实例所属的执行 context，不能让 `exec.CommandContext` 直接绑定客户端请求导致断线杀进程。
- 客户端取消若已被目标处理且尚未跨越启动判定点则阻止启动；跨越后按运行中命令处理。SDK/HTTP 自动取消不改变产品语义。
- 主动正常停止：拒绝新操作、取消等待确认、终止受控命令并写历史，最后关闭接入/文件；token 到期只是拒绝新操作，不取消运行中命令。

## 8. 命令进程、输出与轻量防重

### 8.1 执行和收尾

- 使用 `os/exec`，shell 为配置的 shell + `-c`；直接执行严格传递 argv。使用明确 cwd 和继承环境，关闭 stdin，不使用 PTY。
- 为每个命令建立独立 Linux 进程组。stdout/stderr 独立读取，各最多保留 1 MiB，继续 drain 丢弃后续输出；输出量不会反向控制命令是否结束。
- 执行时间到限或主动停止时，对进程组发送 SIGTERM，等待 2s 后发送 SIGKILL；读取/等待独立有界，必要时关闭管道，避免后代持有 fd 无限阻塞。
- 对退出确认不足、信号发送失败或后代脱离进程组如实报告。进程组 PID 生命周期、回收和信号竞态需要专项测试，不误杀复用 PID，不声称清理所有后代。
- 命令名额在实际执行与必要收尾完成后释放；身份查询不占该名额。

### 8.2 防重范围

- 目标生成内部请求标识：relay 用连接代际 + 转发标识定位同一次投递；审核续接用目标生成的随机审核句柄关联原调用。Quick 普通新 HTTP 请求各自生成标识，不把相同命令文本或自报 clientInfo 当作同一请求。不能只拿外部 JSON-RPC 数字 ID 当跨客户端唯一标识；MRTR 各轮 ID 可以不同 [R3]。
- 使用一把短临界区 mutex 管理在途与最近 256 条结束记录，保存内容指纹/状态，不保存 stdout/stderr。相同标识不同内容返回冲突；已启动/结束的相同请求不再次启动，只说明未再次执行、原结果不补取。
- 尚未启动的审核续接是同一个状态转换，不能被防重误拒绝，也不能每轮创建新的执行标识。
- 这只能防止已识别的内部重复投递和已消费审核状态再次启动；客户端重新发起独立 HTTP 调用，即使参数或 JSON-RPC ID 相同，也不承诺识别为原操作。新请求、记录淘汰或目标重启后的相同命令不保证语义防重。中转重新连接不恢复请求，历史文件不参与重放/去重。

## 9. 目标侧命令历史

### 9.1 文件与事件

```text
<日志根目录>/<实例 ID>/
    commands.jsonl              # 当前
    commands.1.jsonl            # 最近轮转
    ...                        # 共最多 5 个文件
```

创建实例目录 `0700`、文件 `0600`，检查已有路径类型/权限，避免误写到不安全路径；不跟随非预期符号链接。权限不隔离任意同身份命令或 root。

| 事件 | 内容 |
| --- | --- |
| `execution_intent` | 版本、时间、实例/请求、完整标准化命令、cwd、模式、超时、返回预算和审核状态 |
| `execution_result` | 同一实例/请求、是否启动、启动失败/未启动原因、起止与耗时、退出/信号/超时/清理、采集及返回截断元数据 |
| `execution_rejected` | 认证通过且参数有效，但因模式/审核/繁忙等未启动；拒绝原因及已知命令信息 |

不记录输出正文、环境全集或额外凭据。命令本身含密码时不自动解析脱敏。记录时间用带时区格式，耗时使用单调时间计算；字段中不可信内容通过 JSON 编码，不能直接拼接造成伪造多行。

### 9.2 写入与故障

- 使用一个实例内 writer 和 mutex 串行完成写入/轮转；并发命令不同事件可交错，但一条 JSON 不交错。不建设日志后台服务。
- 意图必须写入并 `Sync` 成功后才进入启动；结果收尾也同步写入，之后才标记 `history_status=ok`。写入和进程启动不是原子事务。
- 执行前写入失败不启动，历史健康变为故障；执行后失败不抹掉实际结果，终端告警，原调用可达时附带记录失败，并停止新的命令。身份查询保留写入状态；已启动命令照常收尾，用户修复后重启。
- 10 MiB 阈值前轮转，保留 5 个文件；轮转/创建/同步失败一并作为历史故障处理，不静默丢弃记录。单条最多超出阈值一个已限制大小的事件。
- 新实例不修改旧实例目录，不从旧日志重建运行任务。提供人工读取说明：不完整尾行或只有意图的请求表示不确定，不能补造成功记录或自动执行。
- 正常停止关闭文件但不删除；旧实例总容量由用户清理。容器持久化通过可指定的日志根目录实现，不自动上传备份。

## 10. 中转注册与内部协议

### 10.1 路由与归属

中转启动生成随机 `boot_id`，每次新注册生成随机 `route_id` 和实例专属 `resume_secret`。公网入口示意：

```text
http://公网IP:端口/t/<boot_id>/<route_id>/mcp?token=<短期MCP凭据>
```

注册在目标发起的 WebSocket 建立后完成，消息包含内部协议版本、共享注册密钥、实例信息、token 摘要/到期信息及必要恢复材料。共享密钥比较失败即拒绝，不先占用正式路由。注册原始密钥不写日志。

- relay 保存路由、实例、MCP token 摘要、到期信息、resume 凭据摘要和当前连接代际；目标保留原 token/重连材料。
- 常规断线后在同一 relay 进程重连，需同实例和有效 resume 证明才继续原路由。活跃连接重复注册不抢占；换连接时原代际请求不能转到新连接。
- relay 重启导致 resume 失效，目标明确重新注册得到新 URL，保留实例 ID 和原到期时间，不延长 token。旧 boot/route 组合永不指向新目标。
- relay 先验证路由归属/凭据/到期，再返回离线，避免匿名枚举目标；目标仍最终验证 token。relay 的时钟/有效期判定是提前拒绝和回收的辅助，不能替代目标对启动时限的最终检查。两端时钟偏差纳入测试，不从中转给出的时间扩大目标权限。

### 10.2 转发协议 v1

使用带版本字段的 JSON envelope；HTTP body 用 base64，以避免任意 JSON/body 再解释。不是任意命令 RPC，不引入代理可直接发送的“已批准 exec”入口。

| 类型 | 用途 |
| --- | --- |
| `register` / `registered` | 首次准入及路由分配 |
| `resume` / `resumed` / `resume_rejected` | 同 relay 进程重连，或明确要求重新注册 |
| `http_request` | 转发 ID、连接代际、允许的方法/路径/头和有界 body |
| `http_response` | 同 ID/代际的状态码、允许的响应头和完整 JSON body |
| `request_abandoned` | 原 HTTP 客户端停止等待；目标按启动前后语义处理，不等于杀运行进程 |

连接心跳用 WebSocket ping/pong；不另建心跳协议。每连接一个读循环，写入串行且有期限；处理的 goroutine、在途 map 和 body 都受第 5 节上限约束。

- 必要头白名单包括内容/Accept、Authorization、Origin、`MCP-Protocol-Version`、`Mcp-Method`、`Mcp-Name`；项目不在工具 schema 增加可携带完整命令的自定义参数头，SDK 验证头/body 一致性 [R2, R5]。不转发 hop-by-hop 头或不受控 Host/路径。`?token=` 仅交给目标验证，URL 不写普通日志。
- 路由在调用接受时绑定连接代际。已知离线立即拒绝；转发前确定没发出去则可报告未执行，写入/发送是否完成无法确定或转发后丢失响应则未知。不在新连接重新发送。
- 请求超过在途上限立即繁忙，连接写入故障关闭连接并结算在途状态。协议帧的串行发送不构成“等目标重连再执行”的命令队列，不能额外加入离线 mailbox。
- 不设置比命令更短的统一 relay 响应等待时间；上限按目标命令超时 + 清理/传输余量计算。授权过的在途调用在 token 到期后可以收尾，不能被路由定时回收直接切断。
- 对可识别的 `exec` JSON-RPC 请求，中转仅把转发失败转换为标准 MCP 工具错误，未知时 `started=null`；否则某些客户端会丢弃 HTTP 错误正文。此转换不执行/审核工具，不改变目标的唯一判定点。
- HTTP 客户端断线时释放中转响应等待资源，发送尽力而为的 abandon；目标执行/历史独立继续。未返回的结果不在 relay 或重连缓存中留作补取。

## 11. Quick Tunnel 与生命周期

### 11.1 启动顺序

1. 检查参数/权限和有限配置，生成实例/token/到期时间，建立历史目录及 writer。
2. 构造共用目标核心与 MCP handler。Quick 模式监听 `127.0.0.1:0`；relay 模式准备进程内 HTTP 入口。
3. Quick 查找/检查 `cloudflared`，启动子进程并持续读取 stdout/stderr（不可因日志缓冲满阻塞子进程）；relay 建立出站通道并完成注册。
4. 成功获得 URL 才输出就绪摘要；超过初次等待时限或关键组件失败则清理退出，不遗留无人管理的 child。

Quick 地址从经过验证的 `cloudflared` 输出/能力中取得，不开放通用日志字符串作为 URL。进程退出可在 token 有效时按有界退避重启；地址改变必须打印新 URL。网络中断但进程仍在时不谎报旧 URL 已恢复可用。

### 11.2 到期与停止

- token 到期：禁止新操作，取消等待审核，不停止目标进程；已有命令及仍可达的结果回传继续，收尾后可释放接入，不进行新的注册重连。
- SIGINT/SIGTERM：停止总等待上限 10s；进入 stopping，不再准入，取消审核，清理受控命令，写结果并关闭接入、`cloudflared` 和历史文件；整个停止也有界。清理故障在终端明确展示；超过总等待期限仍无法收尾时退出并明确记录可能不完整，不伪造完成事件。
- 强制杀死、崩溃、断电：不承诺清理后代、完整历史或执行恢复。下次启动新实例/地址，不接续旧任务。

## 12. 发布与交付

只交付 Linux x86_64：

1. **精简版**：`agent-tunnel`，同时包含 target/relay 子命令；Quick 模式需已有 `cloudflared`。
2. **自带版**：归档包含同一个 `agent-tunnel`、处理过的官方 `cloudflared`、许可/第三方声明及校验清单；不做自解压。
3. **中转运行说明**：使用精简版的 `relay` 子命令即可，不另维护一个中转构建项目。

发布记录项目/Go/依赖版本、目标平台、构建选项和制品 SHA-256；记录原始及处理后 `cloudflared` 的版本/校验值/处理方法。构建和验收产物放 `dist/`，不把它们、日志、token 或环境文件提交进源码。

不要为体积目标先指定压缩工具或承诺压缩率。处理后必须跑真实 Quick Tunnel 和 MCP，不只检查 `--version`；失败则回到需求讨论。安装不依赖在线下载，最低系统/动态库/出站需求以发布验收结果说明。

## 13. 实施里程碑与验收映射

不估未经验证的工期；建议每个可验证里程碑一个提交，提交前检查 diff/敏感文件并取得用户授权。

| 阶段 | 交付 | 完成条件 |
| --- | --- | --- |
| M0 最小原型 | 锁定 SDK/协议，最少两个工具与确认流程，临时公网验证脚本 | G0/G1 得出可复现结果；冲突先讨论，原型不直接算成品 |
| M1 执行与历史 | config、实例、executor、history | 非交互两形式、关闭 stdin、cwd、root 身份、有界输出、超时/清理、记录写入/轮转故障测试 |
| M2 共用 MCP 核心 | 工具 schema、三模式、审核/取消/到期、防重 | 两种入口可复用；批准绑定、拒绝无副作用、log 故障阻止新执行等本地测试 |
| M3 多目标中转 | 注册、归属、HTTP 转发、重连与代际 | 匿名注册拒绝、目标隔离、原地址重连、离线不排队、中转重启新地址、丢响应未知 |
| M4 Quick 与打包 | cloudflared 管理、两种包、relay 运行说明 | 真 Quick URL、进程/地址变化、缺程序报错、处理后实际功能、Linux amd64 运行 |
| M5 完整验收 | 真实客户端两入口，故障/竞态矩阵，发布记录 | 需求第 6 节全部映射；P0 失败处理完，列明通过/失败/未验证与已接受风险 |

测试层次：

- 单元：参数/schema、额度/审核/取消状态、输出分配/编码、防重、日志事件/轮转、路由归属。
- 本地集成：实际 Linux 子进程、两个同名目标、假中转断线、token 到期、同步写故障注入、后代持管道、SDK 客户端。Go 测试使用 race 检查；这些不证明真实 Agent 确认可用。
- 真实链路：一个明确版本的用户客户端，经两入口审核/拒绝；120s/300s 无输出、最大响应、连接中断/重连、真实 cloudflared 自带/精简包。
- 竞态：批准与到期/取消/繁忙；意图写入后启动前退出；命令启动后客户端断开；执行完成后响应丢失；relay 换代；写历史失败与主动停止。

网络不可达、镜像/下载失败与代码失败分别记录；不借网络失败声称功能不支持。不会为了验收引入 ARM/QEMU、真实数据库修复或额外平台。

## 14. 明确不扩展的设计范围

- 不开发本地 Agent/网关/确认程序，不替第三方客户端自动改写网络错误。
- 不提供 PTY、stdin、后台 job、用户杀任务、增量输出、结果补取或文件/端口工具。
- 不保存任务状态数据库、跨 relay 重启路由、可远程查询的历史、输出正文归档或永久防重表。
- 不自实现 E2EE/OAuth 账号体系、动态白名单、生产沙箱、抗篡改日志或容量压测平台。
- 不以设计完成、单元通过或模拟确认替代真实客户端/公网验收；不私自改成正式 Tunnel 或取消审核来让测试通过。

## 15. 官方参考与证据边界

下列仅用于约束设计，不代表库 API、目标平台和真实链路已验证。实现时记录锁定版本并重新核对。

| 编号 | 官方资料及本次用途 | 定位 |
| --- | --- | --- |
| R1 | MCP Versioning：正式规范与 RC/draft 区分 | `https://modelcontextprotocol.io/specification/versioning` |
| R2 | MCP 2026-07-28 Streamable HTTP：JSON、MRTR 和取消/续接的传输边界 | `https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http.md` |
| R3 | MCP 2026-07-28 MRTR：输入请求、requestState 与多轮续接 | `https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr` |
| R4 | MCP 2026-07-28 Elicitation：form、能力协商、输入结果 | `https://modelcontextprotocol.io/specification/2026-07-28/client/elicitation` |
| R5 | 官方 Go SDK README/server 文档与 HTTP 源码：支持版本、HTTP 选项和 MRTR 适配方向 | `https://github.com/modelcontextprotocol/go-sdk`；`https://raw.githubusercontent.com/modelcontextprotocol/go-sdk/main/docs/server.md`；`https://raw.githubusercontent.com/modelcontextprotocol/go-sdk/main/mcp/streamable_server.go` |
| R6 | Cloudflare Quick Tunnels：SSE/在途请求等明确限制 | `https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/do-more-with-tunnels/trycloudflare/` |
| R7 | Cloudflare Connection limits：代理时限及不可从其推定的 Quick 行为 | `https://developers.cloudflare.com/fundamentals/reference/connection-limits/` |
| R8 | coder/websocket：候选内部通道库的官方仓库 | `https://github.com/coder/websocket` |

制定设计时尚未运行验证。当前已锁定 Go SDK v1.8.0 并完成代码、测试、真实 Quick Tunnel 和官方 Inspector 界面部分验证及候选打包；具体 Source commit、哈希、失败与缺口见实施记录。不以旧协议注释、SDK 模拟测试或局部通过声称全量验收完成。
