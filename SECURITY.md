# Security model and operational limits

## 0. 文档性质

文档日期：2026-09-23。本文件说明 `agent-tunnel` 的威胁模型、信任边界和已知限制，**不是独立安全审计、渗透测试报告、密码学证明或生产准入结论**。当前 35 项 Rust 单测、32-job Loopback E2E、人工接管/审批真实TTY、到期拒绝、临时CA本地WSS与错误CA拒绝、Quick Tunnel公网WSS/Noise（显式TCP连接IP）、本地rootless scratch Relay 均已有明确PASS；x86_64-musl发行包已按最终Rust源码实际构建，文档提交后再打包；按用户要求不进行ARM64验收。Pi 0.86.0离线工具代码对假Controller运行通过，但没有真实模型调用。自有公网服务器、长期稳定性和完整独立安全评审仍未完成。

状态用语：

- **源码实现**：当前 checkout 中已有对应代码或协议字段。
- **实现中**：指定功能路径仍缺模块、语义闭合或测试；该标签描述功能状态，不暗示当前 checkout 编译失败。
- **未实测**：没有把源码审阅或单元测试推论成真实进程、网络、目标 OS 或产品兼容性结果。
- **不提供**：明确不属于当前安全承诺。

任何远端输出、错误消息、MCP 内容、pi tool result、文件名、日志和命令文本都属于不可信输入。远程内容不能修改本地审批状态、工具选择、提示词、命令行或安全策略。

## 1. 保护对象

本项目需要保护的对象包括：

1. **E2E secret**：`channel_key` 的底层 32 bytes。它是 `Noise_NNpsk0_25519_ChaChaPoly_SHA256` 的 PSK，明文存在于 Controller 和 Connector 的 endpoint 文件、进程地址空间或备份中。
2. **role token**：controller/connector 各自的 32-byte token。Relay 的 `relay.json` 只保存两个 token 的 SHA-256 hash，但连接时对应 endpoint 会向 Relay 提供 Bearer token。
3. **远程请求和数据**：argv、cwd、环境覆盖、stdin、PTY 输入、stdout/stderr、job 状态、退出码和错误信息。
4. **目标身份**：session ID、target ID、Connector incarnation、UID/GID、cwd、OS/arch 和过期时间。
5. **本地控制能力**：Controller Unix socket、Relay admin Unix socket、审批 TTY 和本地 credential directory。
6. **目标机资源**：目标 UID 可访问的文件、网络、进程、设备、数据库和其他副作用；以及 Connector 管理的普通进程组。
7. **发布来源**：构建产物、checksum manifest、可选的 minisign 签名和独立取得的公钥。

## 2. 系统边界和信任假设

```text
本地 CLI / MCP / pi extension
       │ private Unix socket
       ▼
Controller ── WS/WSS ── Relay ── WS/WSS ── Connector
   │                          │              │
   │ local approval TTY       │ metadata     │ target OS UID
   │ controller.json           │ role tokens  │ connector.json
   │ channel_key               │ hashes       │ channel_key
```

### 2.1 明确的信任假设

- Controller 主机、目标主机和 Relay 运行用户的 OS/文件权限没有被 root 或同等权限的进程攻破。
- `controller.json`、`connector.json` 通过已有可信运维渠道交付，且只有预期角色能读取。
- Relay 的公网入口使用正确配置的 TLS/WSS；证书、CA、域名和反向代理配置由部署者验证。
- 运行 `local` 的操作者能够识别目标身份，并在使用默认 Gate 时能操作独立 `/dev/tty`。
- 发布验证者通过独立可信渠道取得 minisign 公钥；不能把下载目录中的公钥和签名一起当作信任根。

这些是假设，不是项目可以自动保证的事实。目标 OS、TLS 终止端、文件备份、容器运行时和运维账号都属于系统边界。

### 2.2 不在信任假设内的对象

- Relay 不应被视为能解密 E2E payload 的可信应用层中继；它可以被恶意配置或攻破，仍能实施 DoS 和元数据观察。
- 远程命令本身不可信；目标程序可能是恶意程序，可能伪造日志、输出审批请求或尝试诱导本地 Agent。
- Connector 不是沙箱。它使用目标机启动用户的 OS 权限，`cwd`、PTY、普通进程组和 TTL 都不是文件系统或内核隔离边界。
- MCP、pi extension 和 Skill 不是审批主体，也不是第二个安全协议；它们只能调用本地 Controller IPC。

## 3. E2E 协议和密钥边界

目标协议为：

```text
Noise_NNpsk0_25519_ChaChaPoly_SHA256
```

当前 `src/crypto.rs` 的源码设计如下：

- Snow `Builder` 使用标准 `NNpsk0` pattern，`channel_key` 放在 `psk(0, ...)`；prologue 为 `agent-tunnel/ATP<version>/<session_id>/<target_id>`，把协议版本、session 和 target 绑定到握手上下文。
- Snow resolver 使用 ring 的 AEAD、RNG 和 SHA 能力，并使用 curve25519-dalek 提供 25519 DH。这里是实现所依赖的 primitive 路由说明，不是对 ring、Snow 或 curve25519-dalek 的独立审计。
- `init` 从操作系统随机源生成 32 bytes，EndpointConfig 以 64 个 hex 字符保存。`controller.json` 和 `connector.json` 各有同一个 key；`relay.json` 只有 `connector_hash`、`controller_hash` 和 TTL，没有 `channel_key`。
- 新的 WS 连接必须重新建立 Noise handshake；旧 transport state、nonce 或 channel 不复用。认证失败、错误 prologue、篡改、乱序、跨 channel frame、超限或不完整分片必须失败；不能回退到明文 packet。
- 应用 JSON packet 放在加密 `Data` frame 中，源码按约 60,000 bytes 分片，分片完成标记在加密 payload 内。Relay 可转发 `Init`、`Response`、`Data` 和 `RelayEvent` 外层 frame，但没有 key 时不能读取 `Data` plaintext。

### 3.1 E2EE 能保护什么

在 key 文件和端点未被攻破且实现正确的前提下，E2EE 保护 Controller 到 Connector 之间的命令参数、环境、stdin、PTY 输入和输出正文的机密性/完整性，减少 Relay/TLS 终止端读取应用 payload 的能力。

### 3.2 E2EE 不能保护什么

E2EE 不保护：

- Relay 可见的 session URL、role、instance header、连接/断开时间、frame 方向、长度、速率、错误和在线状态。
- Relay、网络攻击者或 TLS 终止端对可用性的影响：丢包、延迟、拒绝升级、断开连接、阻塞队列和反复触发新握手。
- Controller/Connector 主机上的 key 文件、进程内存、日志、备份或 root 访问。
- target OS 上命令的文件、网络、进程和数据库副作用。
- 目标身份是否符合操作者预期；必须通过 `remote_info`/`info` 和独立运维信息确认。

Noise 的单元测试或静态检查不能证明真实部署中的 TLS、代理、文件交付、系统时钟、权限和二进制来源正确。

## 4. 凭证生成和可信运维交付

`init` 生成新 session、target、两个 role token、一个 `channel_key` 和 TTL，并新建 `0700` session directory：

```text
relay.json       0600  session、TTL、controller/connector token hash
controller.json  0600  controller role token、channel_key、Relay URL
connector.json   0600  connector role token、channel_key、Relay URL
```

必须遵守以下分发规则：

1. Relay 主机只得到 `relay.json`。把完整 session 目录或任一 endpoint 文件复制到 Relay 会破坏 E2E key 不在 Relay 的边界。
2. 本地 Controller 只得到 `controller.json`；目标机 Connector 只得到 `connector.json`。两个 endpoint 文件共享 `channel_key`，但 role token 不同。
3. 文件通过已有可信运维渠道交付；项目不定义新的传钥匙协议，也不把 key 放进 URL、命令行、环境变量、公开工单、MCP result、pi 参数或日志。
4. 目标位置必须由正确 OS 用户拥有，目录 `0700`，文件 `0600`；程序拒绝 symlink credential file、其他用户 owner 或 group/other 权限。
5. endpoint 文件的明文 token 和 key 都是秘密；Relay 只保存 token hash 不意味着 endpoint 文件泄露后仍安全。
6. 当前 TTL 为 10–3600 秒。TTL 是过期边界，不是长期身份、自动轮换或完整 revoke 系统。
7. 建立新 session 才能可靠轮换 token/key。销毁、隔离和备份清理旧 endpoint 文件属于运维职责。

### 4.1 角色交付检查单

从通过 owner/mode 检查的私有 session directory 中，只选取接收方所需的单个文件：Relay 只得 `relay.json`，Controller 只得 `controller.json`，Connector 只得 `connector.json`。不要为传一个文件就复制整个 session directory，也不要把 endpoint 文件放到 Relay、nginx document root、公开对象存储或普通共享目录。使用组织现有、能确认收件主机和运行用户身份的加密配置/秘密交付通道；接收端核对身份、owner、目录 `0700` 和文件 `0600`，并按保留策略清除暂存件和不需要的明文备份。不要把 JSON 内容放入聊天、工单、邮件群组、URL、shell 参数、环境变量、日志、模型上下文或 MCP/pi tool 参数。

下面命令仅为创建配置的示例，不表示执行了安全交付。loopback `ws://` 只适用于同机本地测试；跨主机须用经过独立检查的 WSS 入口：

```bash
agent-tunnel init \
  --dir "$HOME/.local/share/agent-tunnel/new-session" \
  --relay wss://relay.example.invalid \
  --name disposable-target \
  --ttl-secs 300
```

TLS 终止/反向代理仍会看到外层连接元数据，不能替代角色文件隔离和 E2EE key 保管。endpoint 文件若泄露或收件身份无法确认，应创建新 session、通过 Relay 管理接口撤销旧 session，并清理可控副本。

不要把 `init` stdout 不含秘密误解为凭证已安全交付；stdout 之外的私有文件、shell history、备份和 debug dump 仍可能泄露秘密。

## 5. 执行权限和审批模型

### 5.1 Connector 的 OS 权限

Connector 只有在启动时显式提供 `connect --allow-exec` 才接受任意 argv 执行。它以自身 UID/GID 创建目标进程；远程命令可能读取/修改该 UID 可访问的文件、网络、进程、设备和凭证。项目没有 seccomp、容器、cgroup、chroot 或 capability-drop 形式的通用沙箱承诺。

`argv` 不经过 shell 解析；如果调用方显式传 `/bin/sh -c`，shell 行为和相应风险由目标 OS 用户承担。不要把 secrets 放入 argv 或环境覆盖：它们可能被进程列表、审计、错误消息、子进程或目标程序读取。

### 5.2 Controller 的两种授权路径

Controller 的授权是独立于 Connector 的另一层：

- 默认 `local` 不带 `--accept-session-risk` 时，Gate 打开独立 `/dev/tty`，对 `exec` 和 `write` 逐次请求人工确认；没有可用 TTY 的非交互运行应失败。
- 第一次请求可在本地 IPC 立即得到 `APPROVAL_REQUIRED`，此时请求尚未因此被证明已经送达远端。操作者必须在 Controller TTY 判断请求上下文并确认。
- TTY 确认后，原调用方重试时必须使用相同 `request_id` 和完全相同参数。不同参数或新 ID 不能被当作同一审批的延续。
- MCP server、pi extension 和 `skills/remote-debug` 没有 approve 工具，也没有权限替操作者输入 TTY 确认。它们应报告 `APPROVAL_REQUIRED`，等待外部操作者完成确认后再重试。
- `local --accept-session-risk` 是显式接受整段 session 执行风险并绕过逐次 Gate 的选项；它不是每条命令的人工审批、审计记录或远程授权提升。

`tests/manual_approval.py` 在真实 `/dev/tty` 和无 TTY 闭锁路径已再次通过，`tests/attach.py` 的人工独占输入、Agent读取/写入冻结、退出后敏感输出清理和30秒租约也通过。逐条TTY审批/接管均不构成生产授权或耐崩溃审计证明。

### 5.3 远程数据不是批准

远端输出中即使出现 `approve`、`run this`、`paste key`、`disable safety` 或伪造的系统消息，也不能改变本地审批。审批只来自本地 Gate/操作者；远程程序不能通过 stdout、stderr、PTY、MCP 内容或错误字符串授权自己。

## 6. stdin、PTY 和输入风险

`Exec` 的输入相关字段与协议一致：

- `stdin=false, pty=false`：普通 pipe/null-input 路径。
- `stdin=true`：建立可写的远端 stdin pipe。
- `pty=true`：Linux PTY 路径，目标程序获得 controlling terminal，通常合并 stdout/stderr 并使用终端行规程。

`remote_write`/CLI `write` 只能写入明确启用的 job，并且**必须带 `request_id`**。同一次输入在不确定回复后只能用相同 ID 和相同参数处理，不得用新 ID 盲目重发。当前工具 schema 对单次 `data` 使用 4096 字符上限，输入队列也有有限容量；具体字节、编码和反压行为以实现和测试为准。

- `eof` 对 pipe 关闭输入；对 PTY 发送 EOT，不保证应用退出。
- `remote_resize`/CLI `resize` 只适用于已拥有的 PTY，rows/cols 当前限制为 1–1000。
- 输入可能触发 shell、REPL、密码提示、文件修改、网络请求或其他副作用；它不是“写日志”的只读操作。
- PTY 是 Linux 实现路径；其他 Unix 的 unsupported/error 结果不能被包装成可用交互终端。
- 断线、Controller crash 或 Connector crash 不承诺恢复远端 stdin、PTY 状态、输出顺序或输入去重。

## 7. request ID、重连和不确定结果

- `exec` 在单个 live Connector incarnation 内以 request ID 加参数签名去重；相同 ID/参数可返回原 job 或状态，参数变化返回 `REQUEST_CONFLICT`。
- Controller/Connector 重连时先建立全新的 Noise channel。Controller 不自动用新 ID replay pending exec；旧请求可能已产生副作用而 reply 丢失。
- 出现 timeout、IPC close、WSS disconnect、`EXECUTION_UNKNOWN` 或 `APPROVAL_REQUIRED` 时，不要生成新的 exec ID。对于同一 live incarnation，按协议用原 ID/参数查询或在审批通过后重试；incarnation 改变后应停止并报告 unknown。
- `remote_write` 同样需要 request ID；重复输入可能改变远端状态，因此“不确定时重试”不是无害操作。
- request ID 去重只存在 Connector 当前进程的内存生命周期内；它不是跨 crash、Relay restart 或新 incarnation 的持久 exactly-once 机制。

## 8. 普通进程组、恶意 daemon 和 PID record

Connector 会为普通 exec 建立并记录进程组，cancel/超时/Connector shutdown 会尝试向该组发送终止信号，并在输出 drain/reap 后更新状态。

这不等于内核级隔离：

- `daemonize`、`setsid`、双 fork、重新加入其他 cgroup 或打开独立文件描述符的后代可能脱离普通进程组。
- 恶意或失控 daemon 可能继续访问目标 UID 可访问的资源、占用端口、保持 pipe/PTY 或持续产生副作用。
- PID/PGID 可能被复用；`boot_id`、`process_start_ticks`、`/proc` 权限和 container PID namespace 可能导致 `matches`、`mismatch`、`not_running` 或 `unknown`。
- PID record 只供 `inspect` 和人工核对。程序不根据磁盘 record 自动 adopt、kill 或恢复外部进程；PID record 也不恢复 stdio、输入、输出、job manager 或审批状态。
- cancel 是请求，不是已终止证明；必须 `read` 查看终态，且无法回滚已发生的文件、数据库、网络或其他副作用。

需要强清理或恶意代码隔离时，应使用经过独立验证的容器/cgroup/VM/专用目标机，不要把本项目普通进程组当作沙箱。

## 9. Relay、DoS 和可见元数据

Relay 的职责是 loopback 监听、role token 认证、每 session 两个 peer 的绑定和有限队列转发。它不执行 argv，也不应得到 E2E key。

即使 E2EE 正常，Relay 仍可看到或推断：

- session ID、controller/connector role、instance header、连接/断开、在线/离线、TTL 和 admin revoke 状态；
- frame 方向、数量、长度、时间、心跳、错误、队列拥塞和重连频率；
- 它自己保存的 role token hash，以及连接处理期间用于 Bearer 校验的 token 值（可能短暂存在进程内存）。

Relay 可被攻击者用来拒绝 WebSocket 升级、耗尽连接/队列、延迟/丢弃/重排/关闭 frame、诱发新握手或使请求结果变为 unknown。源代码有 session、worker、frame 和 queue 上限，但这些是 DoS 缓解，不是可用性保证；没有 HA、持久路由或离线命令队列。

外部 Nginx/TLS 入口必须：

- 只把 `/v1/` 和健康检查转到 loopback Relay；
- 不记录 `Authorization`、request body、Noise payload 或包含秘密的错误；
- 保持 WebSocket Upgrade、超时、大小和连接限流与目标实现一致；
- 使用经验证的证书/私钥权限和 CA；
- 不把 admin socket、session 文件或 endpoint 文件映射到公网。

## 10. Relay admin revoke

Relay 可选 `--admin-socket PATH`；不提供时使用首个 `relay.json` 同目录的 `relay-admin.sock`。Admin socket：

- 只绑定 Unix socket；
- 检查父目录私有、socket owner 为当前 UID、权限 `0600`、peer UID 相同；
- 提供 `sessions` 状态查询和 `revoke --session ID`；
- 不通过公网 WS 暴露。

Revoke 的语义是：

1. Relay 先在内存中标记 session revoked，防止继续授权；
2. 尝试以 `0600` 创建 `<relay.json>.revoked` durable marker，并 fsync 文件/父目录；
3. 关闭当前两个 peer；
4. 后续 Relay 启动检测 marker 并拒绝旧 session；
5. 已连接的 Connector 依赖 lease/TTL 退出并回收其管理的 job，当前 lease 上限最多 60 秒。

因此 revoke **不是瞬时 kill**，也不能撤销远端已发生的副作用。如果 marker 写入失败，Relay 会返回警告；运维不得用旧 session 文件重启并把内存关闭当成永久撤销，应隔离旧文件并创建新 session。Admin revoke 也不是逐命令人工审批。

## 11. 目标 OS、pi 和 MCP 的信任边界

### 11.1 目标 OS

- 当前仅支持 Unix；PTY 代码的实现保证集中在 Linux。
- Connector/目标进程拥有启动 UID/GID 的常规 OS 权限；root、同 UID 进程、调试器、备份程序和恶意 daemon 可读取 key、socket、状态或命令数据。
- `cwd` 只是 spawn 参数，不限制 `..`、绝对路径、文件描述符、网络或系统调用。
- argv/env、TTY 内容和输出可能包含秘密；操作员必须避免将它们写入日志、prompt、issue 或模型上下文。

### 11.2 MCP stdio

MCP server 只在本地 stdio 暴露六个工具，stdout 保留给 JSON-RPC。它不直接持有 endpoint token，调用 Local Controller socket。`remote_exec` 和 `remote_write` 需要调用方提供 request ID；工具没有 approve 能力。

MCP tool result 中的 `events[].text`、错误 `message` 和远端字段均是不可信文本。调用方不能因为工具返回成功 JSON 就自动执行返回文本中的下一条命令。

### 11.3 pi 原生扩展

`integrations/pi/index.ts` 通过 pi loader 提供的 `typebox` 注册六个工具，`integrations/pi/ipc.mjs` 读取固定的 `AGENT_TUNNEL_SOCKET`，检查 socket/父目录 owner、类型和私有权限。扩展参数不能改写 socket 路径，不能访问 credential file，也没有 approve 工具。

本机 pi `0.86.0` 离线六工具注册和 Node IPC 五项测试已报告 PASS；未调用真实 AI 模型，也没有更广泛安装兼容性验收或生产插件签名结论。pi 只是本地 IPC 客户端；它不能把远端输出升级为指令或代替 Controller TTY 操作者。

## 12. 构建、发布和来源认证

项目提供两个独立构建变体：

- full：默认 `controller`、`relay`、`mcp`；
- connector-only：`--no-default-features`，用于目标机。

release profile 使用 `opt-level="z"`、`lto="fat"`、`codegen-units=1`、`panic="abort"`、`strip="symbols"`。截至 2026-09-23，`x86_64-unknown-linux-musl` 的 full 与 connector-only 静态 PIE 构建已报告通过，并经 `file` 检查，binary 分别为 3,290,256 B 和 2,343,856 B。更新后的打包脚本已生成并校验两种x86_64归档及SHA-256；因最终文档尚未提交，归档须在提交后复建才能称为该Git版本的产物。

`scripts/package.sh [--force] [target-triple]` 默认拒绝覆盖同名 archive、manifest 或签名；明确使用 `--force` 才会替换。打包先构造到 `dist/` 内的新临时目录，再逐个原子发布；失败清理仅针对当前 invocation 的 `mktemp` 目录。构建输出位于 `target/package-*`，脚本不执行 Git 写命令；x86_64上已实际运行打包并确认未修改跟踪的Rust源码，SHA-256 manifest与包内凭证排除检查PASS；文档提交后的归档须重建一次。

未显式设置 `SOURCE_DATE_EPOCH` 时，归档时间默认取最新 Git commit 时间；若当前工作树含未提交内容，包体反映工作树而时间戳可能仍取 HEAD commit，不能声称构建可复现。即使显式固定 epoch，归一化 tar/gzip metadata 也不能单独证明工具链、依赖和二进制均可复现。

`MINISIGN_SECRET_KEY` 只是在发布者选择时签名 `SHA256SUMS-<target>` 的可选输入。验证者必须从独立可信渠道取得 minisign 公钥，再用 `scripts/verify-release.sh` 验证 manifest 签名和指定 archive 的 SHA-256。

**SHA-256 checksum 本身不认证来源。** 未签名 manifest、未知来源公钥、只比较文件大小或只比较 checksum，都不能证明 archive 来自预期发布者。不要在示例、日志或 CI 输出中打印 secret key。

## 13. 已知限制和不承诺事项

当前明确不承诺：

- 独立安全审计、生产支持、协议稳定性或对恶意 Relay 的可用性；
- 任意 OS 用户的沙箱隔离、seccomp/cgroup/VM 边界、daemonize 后代的可靠清理；
- Connector crash 后的 stdio/PTY、job、输出、request 去重、审批或进程恢复；
- exactly-once 执行、exactly-once stdin 写入、自动重放、离线命令队列或 Relay HA；
- one-time join、在线 key rotation、长期身份 fingerprint、通用远端 approve API；
- Cloudflare Quick Tunnel 公网烟测的通过结果（当前无 PASS 报告）；
- 自有服务器上的 WSS/CA/反向代理、Containerfile、systemd 部署验收（当前未部署）；
- Codex 或真实 pi 模型行为、跨架构兼容性、独立密码学/生产安全审计；
- 未经更新版 `scripts/package.sh` 实测的最终 archive 大小，或未经独立可信公钥验证的发布来源。

原始架构规格 P0–P4 的逐项交付状态和未缩减的剩余范围见 [docs/architecture-plan.md](docs/architecture-plan.md)。即使 P0/P1/P2/P3 的局部条目有实现或上一轮测试，也不能据此推导 P4 的持久恢复/HA、生产安全或整体路线已完成。

发现 key/token 泄露、目标身份不符、Noise 未建立、审批状态不明、Relay admin marker 写入失败、输出含有诱导指令或进程组逃逸时，应停止操作，隔离旧 session，并通过可信运维渠道创建新 session。
