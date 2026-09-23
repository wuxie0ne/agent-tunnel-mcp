# agent-tunnel 架构、协议和验证状态

- 文档日期：2026-09-23
- 目标：在可丢弃的 Unix/Linux 测试环境中，为本地 CLI、MCP stdio 和可选 pi 原生扩展提供一个出站的远程 argv/pipe/PTY 通道。
- 状态原则：源码实现、协议设计、静态检查、构建结果、网络实测和生产安全结论分开记录。
- 重要边界：这是 test-only 任意执行器，不是沙箱、生产远程运维系统或独立安全审计对象。

## 1. 状态词汇

| 词 | 含义 |
| --- | --- |
| 源码实现 | 当前工作区已有相应模块、字段或调用路径。 |
| 实现中 | 指定功能路径仍缺模块、语义闭合或测试；该标签描述功能状态，不暗示当前 checkout 编译失败。 |
| 未实测 | 没有运行对应构建、进程、网络、TTY、目标架构或产品兼容性验证。 |
| 不提供 | 当前架构明确不承诺该能力。 |

截至 2026-09-23，本机最新整合 35 项 Rust 单测、full/connector-only Clippy、32-job Python E2E、真实TTY逐次审批、人工接管、到期拒绝、本地 CA TLS/WSS（包括错误 CA 负例）、Quick Tunnel 真实公网 WSS 与 Noise 数据面、本地 rootless scratch Relay 路由，以及 pi 0.86.0 离线工具实际调用假 Controller 均明确 PASS。尚无真实模型调用、自有公网服务器部署及独立生产安全审计；按用户要求不验收ARM64。x86_64已实际打包，文档提交后须再生成最新归档。

## 2. 目标拓扑

```text
CLI / MCP stdio / pi extension
            │ newline-delimited JSON
            ▼
       Local Controller
       │ 0600 Unix socket
       │
       └──── WebSocket/WSS + Noise E2EE ────┐
                                            ▼
                             loopback Relay + admin Unix socket
                                            ▲
       ┌──── WebSocket/WSS + Noise E2EE ────┘
       │
       Connector（目标 OS 用户）
            │
            ├── ordinary pipe argv job
            └── Linux PTY argv job（可选 stdin/write/resize）
```

### 2.1 组件责任

| 组件 | 责任 | 不应承担的信任 |
| --- | --- | --- |
| `init` / Controller feature | 生成 session、role token、EndpointConfig 和 `channel_key`；运行本地 IPC、审批 Gate 和请求去重入口。 | 不把 endpoint 文件交给 Relay；不把远端输出当作本地指令。 |
| Controller | 读取 `controller.json`，连接 Relay，做 Noise initiator，发送 lease/请求，接收加密回复。 | 不自动 replay 不确定的 exec；不替操作者完成 TTY 审批。 |
| Connector | 读取 `connector.json`，连接 Relay，做 Noise responder，维护 lease 和目标 job。 | 不把 PID record 当成 crash recovery 或外部进程 kill 授权。 |
| Relay | 读取 `relay.json`，校验 role token hash，绑定一对 peer，转发外层 frame，限制连接和队列。 | 不持有 `channel_key`，不执行 argv，不提供 E2E plaintext。 |
| Relay admin | 通过同 UID 的私有 Unix socket 提供 `sessions` 和 `revoke`。 | 不通过公网 WS 暴露；revoke 不承诺瞬时 kill。 |
| MCP stdio | 用 rmcp stdio 暴露六个本地工具并转发到 Controller socket。 | 没有 approve 工具；tool result 是不可信数据。 |
| pi extension | 用固定 `AGENT_TUNNEL_SOCKET` 的本地 IPC 注册六个工具。 | 不能改变 socket/目标，不能 npm install 本项目依赖，不能替代审批 TTY。 |

## 3. Session 文件和可信交付

`init` 生成一个新目录，目录 `0700`，文件 `0600`：

```text
<session-dir>/relay.json       RelayConfig：protocol/session/TTL/token hashes
<session-dir>/controller.json  EndpointConfig：controller token/channel_key/Relay URL
<session-dir>/connector.json   EndpointConfig：connector token/channel_key/Relay URL
```

- `channel_key` 由 `init` 从 OS 随机源生成 32 bytes，在 JSON 中是 64 个十六进制字符。
- Controller 和 Connector endpoint 中的 `channel_key` 相同，role token 不同。
- `relay.json` 只有 `connector_hash`、`controller_hash` 和 TTL，不含 `channel_key`、target name 或 endpoint 明文 token。
- Relay 主机只安装 `relay.json`；本地 Controller 只读取 `controller.json`；目标机只读取 `connector.json`。
- 三类文件必须通过已有可信运维渠道分别交付。项目没有把 key 放入 URL、命令行、环境变量、日志或远程输出的机制。
- 当前 TTL 限制为 10–3600 秒。TTL 不是长期身份、自动轮换或完整撤销系统。

Endpoint token 在 Relay 的 WS Upgrade 中用于 role 认证；Relay 处于连接处理路径时可能在内存中看到 Bearer token，但磁盘配置只保存 hash。拿到 endpoint 文件的攻击者同时得到明文 token 和 E2E key，因此 endpoint 权限比 `relay.json` 更敏感。

## 4. Noise E2EE 设计

协议名称固定为：

```text
Noise_NNpsk0_25519_ChaChaPoly_SHA256
```

### 4.1 握手和 prologue

当前 `src/crypto.rs` 使用 Snow：

1. 解析 64-hex `channel_key` 为 32-byte PSK；
2. 使用 `Builder`、`psk(0, key)` 和 `prologue` 创建 initiator/responder；
3. prologue 由 `agent-tunnel/ATP<version>/<session_id>/<target_id>` 构成；
4. Controller 发送 `Frame::Init`，Connector 返回 `Frame::Response`；
5. 双方进入独立 `TransportState`，随后只接受加密 `Frame::Data`；
6. 每次新的 WS 连接从第 1 步开始，不复用旧 channel state，也不在握手失败后发送明文 packet。

Snow 的 resolver 将 ring 用于 AEAD/RNG/SHA，curve25519-dalek 用于 25519 DH。该选择反映源码实现，不等价于第三方密码学审计。

### 4.2 加密 frame

外层 Relay 看到的是带 `type` 的 JSON frame：`init`、`response`、`data`、`relay_event`。应用 JSON packet 在 `data` 中以 base64 ciphertext 传输。

- 单个 application message 受 `MAX_FRAME = 256 KiB` 限制。
- 加密 payload 约按 60,000 bytes 分片，低于 Noise 65535-byte message 限制。
- 分片结束标记位于已加密 payload 内，不作为 Relay 可篡改的外层路由字段。
- TransportState 的 nonce、AEAD tag、顺序和 prologue/key 绑定应使重放、篡改、乱序和跨 channel frame 失败；35 项 Rust 测试及 32-job loopback E2E PASS，Relay 侧抓包仅见 Noise 密文；临时 Quick Tunnel 公网 WSS/Noise 已用显式连接 IP（保留域名 TLS 验证）验证。

E2EE 保护应用 payload 的机密性/完整性，不保护 Relay 可见的 session/role/instance、时间、大小、方向、连接状态、DoS 或目标 OS。Relay 仍可以拒绝、延迟、丢弃、关闭或重复触发握手。

## 5. WebSocket、Relay 和 lease

### 5.1 外层连接

- `transport::connect` 将 endpoint role 映射到 `/v1/connect/<session_id>` 或 `/v1/control/<session_id>`。
- Authorization 使用 role token，instance 使用 `X-Agent-Tunnel-Instance`。
- Relay 只接受 loopback listen；跨主机应由 TLS reverse proxy/tunnel 暴露 WSS。未加密 `ws://` 只允许 loopback。
- Relay 拒绝 Origin、非法/过期 session、错 role/token、重复 instance 和第二个已绑定 peer。
- 源码有单 session/连接/队列/frame 上限；这是 DoS 缓解，不是 HA 或可用性保证。

### 5.2 lease 和连接生命周期

- Controller 在 Noise channel 建立后发送加密 lease，并周期性刷新。
- Connector 只有在当前 channel 收到有效 lease 才处理 request；lease 过期后拒绝请求并在边界内回收 job。
- 连接丢失时 Controller/Connector 建立新 WS 和新 Noise handshake。pending request 可以变为 `EXECUTION_UNKNOWN`；Controller 不自动用新 ID replay。
- Relay 重启会丢失内存 peer binding、路由和待转发消息；旧 endpoint 不因此自动获得恢复保证。
- Connector incarnation 每次进程启动重新生成。incarnation 改变后，旧 job/stdio/request 不应被假定可恢复。

2026-09-23 报告的 Connector deadline race 修复在入站消息处理、Lease 更新和 exec 前分别检查 deadline，并以 `select biased` 让到期分支优先，避免并发到期事件被普通输入或 lease 更新续活。相关 35 项 Rust 测试、Clippy、两轮5秒租约/10秒TTL到期回归、32-job E2E、真实TTY审批均已 PASS。不得把这个修复写成 crash recovery 或持久 lease。

## 6. 请求、审批和不重放

### 6.1 `exec`

`Exec` 结构包括：

```text
expected_incarnation
argv
cwd
env
 timeout_ms
stdin
pty
```

`argv` 不经过 shell 解析；shell 只有在调用方显式把 `/bin/sh -c` 等作为 argv 时才出现。`cwd` 不是访问控制边界。

Connector 在当前 incarnation 的内存 request map 中按 request ID 和参数签名去重：

- 相同 ID + 相同参数：返回原 job/状态或原 `EXECUTION_UNKNOWN`；
- 相同 ID + 不同参数：`REQUEST_CONFLICT`；
- 网络断开、Controller 超时、peer 变化或审批返回，不会自动以新 ID replay；
- 同 incarnation 可用原 ID/参数查询不确定结果；incarnation 改变后停止并报告 unknown；
- 该去重表不跨 Connector crash、Relay restart 或新 incarnation 持久化，不提供 exactly-once。

### 6.2 manual Gate

Controller 的目标授权路径是：

```text
本地 IPC request
    │
    ├─ info/read/cancel 等只读或管理路径按各自规则处理
    │
    └─ exec/write ── Gate
                      ├─ --accept-session-risk：整 session 显式绕过逐次 Gate
                      └─ 默认：独立 /dev/tty；无 TTY 失败；首次返回 APPROVAL_REQUIRED
                                      │
                                      └─ 操作者通过后，原调用方用同一 request_id + 同一参数重试
```

MCP、pi 和 Skill 没有 approve operation。它们不能从远端输出中推断批准，也不能伪造 TTY 确认。真实 `/dev/tty` 与无 TTY 闭锁的 `tests/manual_approval.py`、人工PTy接管测试当前均 PASS，不等同于生产授权或持久审计。

## 7. stdin、PTY、write 和 resize

### 7.1 pipe 路径

`stdin=true` 创建有界可写 stdin pipe；`remote_write`/CLI `write` 将字符串数据排入 Connector 的输入队列。单次工具 schema 的 `data` 上限为 4096 字符，队列容量有限；当 pipe 关闭、EOF 或 job 终止时应返回明确状态。

### 7.2 Linux PTY 路径

`pty=true` 由 `src/terminal.rs` 打开 Linux PTY：

- child 使用 slave，执行 `setsid` 并设置 controlling terminal；
- Connector 保留非阻塞 master，通过 Tokio `AsyncFd` 读取/写入；
- PTY 输出通常是合并终端流，不等同于分离的 stdout/stderr pipe；
- 默认窗口为 24×80，`remote_resize`/CLI `resize` 当前允许 rows/cols 1–1000；
- `eof` 在 PTY 上发送 EOT，不保证应用退出；
- 非 Linux 的 PTY/resize 以 unsupported/error 为准。

`remote_write` 必须有 request ID，因为输入本身可能执行命令或提交数据；不确定回复时不得生成新 ID。PTY、stdin、EOF、resize、断线和取消需要真实 E2E 验证。

## 8. 进程、PID 和清理

`executor` 为 job 记录 target/incarnation/job/request、PID/PGID、Linux boot ID/start ticks、状态、退出码和终止原因。

- 普通 pipe/PTY child 使用当前 Connector 创建的普通进程组；cancel/timeout/shutdown 向该组发信号并尝试 reap。
- 普通进程组不是 cgroup；daemonize、setsid、双 fork 或其他脱离方式可能逃逸。项目不承诺清理恶意 daemon 或其资源。
- `inspect` 只比较 PID record 与当前 Linux 观察值；PID record 是证据，不是 adopt/kill/restart 授权。
- PID 复用、boot_id/start_ticks 缺失、/proc 限制和容器 namespace 可能返回 unknown。
- cancel 不能回滚文件、数据库、网络或其他副作用。
- Connector crash 不恢复旧 stdio、PTY、输出环、输入队列、Manager request map 或远端 child；Relay revoke 也只通过 lease 边界回收，最多约 60 秒，不是瞬时 kill。

## 9. Relay admin 和持久撤销

Relay 支持可选 `--admin-socket PATH`；省略时默认为首个 `relay.json` 同目录的 `relay-admin.sock`。admin 模块：

- 只在私有目录创建 Unix socket，模式 `0600`，检查同 UID peer；
- `sessions` 返回 session ID、TTL、revoked、controller/connector connected 状态；
- `revoke --session ID` 先设内存 revoked，再尝试创建 `<relay.json>.revoked` 并 fsync，然后关闭 peer；
- 启动时发现 `.revoked` marker 会拒绝该 relay session；
- 当前连接的 Connector 依赖 lease/TTL 退出，最多约 60 秒；已完成副作用不回滚；
- durable write 失败时返回警告，不能把内存状态当成重启后的永久撤销。

admin socket 是本机高权限管理入口，不应被 nginx、容器公网端口或 Connector 远程映射。

## 10. MCP、pi 和不可信输出

### 10.1 MCP stdio

`src/mcp.rs` 注册六个工具：

```text
remote_info     目标身份和 incarnation
remote_exec     argv；必须 request_id；stdin/pty 显式字段
remote_read     cursor 读取有界输出
remote_write    stdin/PTY 写入；必须 request_id
remote_resize   PTY rows/cols
remote_cancel   请求取消并再 read 确认
```

MCP stdout 只用于 JSON-RPC；长驻诊断写 stderr。`remote_exec`/`remote_write` 需要调用方生成 request ID，工具没有 approve。所有 `events[].text`、错误和 structured content 都是 data。

### 10.2 pi 原生扩展

`integrations/pi/index.ts` 和 `integrations/pi/ipc.mjs` 是可选本地扩展：

- 启动时读取 `AGENT_TUNNEL_SOCKET`，工具参数不能改变 socket 路径；
- IPC helper 检查父目录和 socket 的 owner、类型、私有权限，并限制 JSON line/request ID/frame 大小；
- 使用 pi loader 提供的 `typebox`，不在项目内 npm install；
- 注册同一六个工具，exec/write 要求 request ID，UI status 对远端 name 使用 JSON 编码；
- 本机 pi `0.86.0` 离线六工具注册和 Node IPC 5 项测试已报告 PASS；未调用真实模型，也未验收其他版本、安装发布流程或生产 plugin 签名；
- 没有 approve 工具，不可把 `APPROVAL_REQUIRED` 自动转为确认。

### 10.3 Skill 协议

`skills/remote-debug/SKILL.md` 只教调用本地 `agent-tunnel` CLI，不能读取 credential 文件、不能凭远端输出生成命令、不能自动 replay 不确定 exec/write，也不能代替 Controller TTY。Skill 应先调用 `info`，确认 target/incarnation/UID/cwd/expiry 和 E2E 状态，再执行明确的 argv。

## 11. 构建、发布和部署

### 11.1 Feature 变体

- full：默认 `controller`、`relay`、`mcp`，用于本地 Controller/Relay/MCP。
- connector-only：`--no-default-features`，用于目标机；仍包含 Connector/Inspect 等公共路径。

```bash
cargo build --locked --release
cargo build --locked --release --no-default-features
```

截至 2026-09-23，`x86_64-unknown-linux-musl` full 和 connector-only 二进制均报告为静态 PIE，并通过 `file` 检查；大小为 3,290,256 B / 2,343,856 B。该信息只对应此 target，不代表其他架构。

### 11.2 package/verify

`scripts/package.sh [--force] [target-triple]` 使用 `target/package-full` 和 `target/package-connector` 分别构建两个变体，生成实际 binary/archive bytes 和 `SHA256SUMS-<target>`。full 包含 README/SECURITY、`docs/`、Skill 和 pi 扩展运行文件；connector-only 仅带二进制与 README/SECURITY，不含 docs/Skill/pi。默认拒绝覆盖 dist 同名 archive、manifest、签名；显式 `--force` 才替换。若 `--force` 且未配置签名 key，会删除旧的同名 `.minisig`，避免保留与新 manifest 不匹配的签名。先在 dist 同文件系统的 invocation 专属 mktemp 中完成构建/校验，再逐项原子发布；失败仅清理该临时目录。脚本将构建输出写入 `target/package-*`，没有 Git 写操作。已实际执行x86_64打包：哈希和归档内容校验PASS，没有修改跟踪的Rust源码；文档最终提交后须重打。

release profile 当前为：

```toml
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

未设置 `SOURCE_DATE_EPOCH` 时，脚本默认使用最新 Git commit 时间；工作树有未提交内容时，输入内容与 epoch 来源不同，因此不能声称可复现构建。设置 `MINISIGN_SECRET_KEY` 才会签名 checksum manifest。`scripts/verify-release.sh` 需要独立取得的公钥、manifest 和 archive；它验证签名和所选 archive 的 SHA-256。checksum 本身不认证来源。

2026-09-23 上述二进制尺寸对应当前 x86_64 源码；更新后脚本已生成并校验 x86_64 full/connector 的 tar.gz。文档最终提交后还需重打，最终归档字节数以当次命令输出和新 SHA-256 manifest 为准，不沿用旧包数字。

### 11.3 deploy 样例

- `deploy/agent-tunnel-relay.service`：专用非 root 用户、loopback 8787、systemd hardening。
- `deploy/nginx.conf`：TLS/WSS 入口、Upgrade、256 KiB body/frame 相关上限、连接限流、关闭 access log，Relay 仍 loopback-only。
- `deploy/Containerfile`：从预构建二进制制作 scratch 镜像，运行时不下载编译器/依赖，用户 `65532:65532`。

这些是样例，不是已部署或已验收生产配置；自有公网服务器未实际部署。Quick Tunnel 公网 WSS/Noise、无害命令读取和持久撤销已在短期临时测试中通过（显式连接 IP 保留原域名证书校验）；本地 rootless scratch Relay 与临时 CA 的WSS正负路径也已通过。证书、CA、私钥、域名、日志和 admin socket 权限仍须部署者单独审核。

## 12. 验证矩阵

| 验证项 | 源码/测试线索 | 当前结论 |
| --- | --- | --- |
| Noise primitive、握手、分片、错误 key/prologue、replay/tamper | `src/crypto.rs` tests | 35 项 Rust 测试当前 PASS；不等于独立密码学审计。 |
| Relay 只能拿 hashes，endpoint 才有 key | `src/config.rs`、`tests/e2e.py` key checks | 当前 E2E PASS；角色文件仍必须隔离交付。 |
| Relay-side outer WS 抓包不泄露 Noise payload | `tests/e2e.py` capture | 最新 32-job E2E PASS，观察到 Relay 外层帧的 payload 为密文。 |
| reconnect 新握手、不自动 replay | controller/connector/`tests/e2e.py` | 最新 E2E 与过期截止回归 PASS；断线后的不确定 exec 不自动重放。 |
| pipe stdin、PTY shell、write、resize、quota、revoke | `src/terminal.rs`、executor、`tests/e2e.py` | 最新 32-job E2E 覆盖 stdin、PTY、持续 shell、resize、16-job quota、revoke；独立人工接管测试确认写入互斥、旧输出清理和所有权超时。 |
| manual Gate / real TTY / no TTY fail-closed | `src/approval.rs`, `tests/manual_approval.py` | 当前真实 `/dev/tty`、无 TTY 闭锁以及操作员接管均 PASS。 |
| Relay admin status/revoke/marker/lease | `src/admin.rs`、relay、CLI | 最新 E2E PASS；不是瞬时 kill 或生产撤销服务。 |
| full/connector-only musl binaries | `Cargo.toml`、`scripts/package.sh` | 两变体静态 PIE binary 构建和 `file` 检查 PASS；尺寸分别 3,290,256 / 2,343,856 B。 |
| 更新脚本后的 tar.gz 与无 Git/worktree 副作用 | `scripts/package.sh` | 已实际运行更新后脚本，x86_64两种归档、SHA-256及包内角色文件排除校验PASS；未改变跟踪Rust源码，文档提交后须重打以包含本记录。 |
| pi 0.86.0 extension | `integrations/pi/*` | 离线六工具注册 + Node IPC 5 项 PASS；未调用真实模型。 |
| Cloudflare Quick Tunnel 公网 WSS smoke | `scripts/smoke-quick-tunnel.py` | 最终调试二进制的 Quick Tunnel 公网 WSS/Noise/无害 exec/read/durable revoke 已 PASS；默认 DNS 在本机曾解析失败，显式连接IP仍强制原域名TLS验签。 |
| 自有服务器 WSS/CA/proxy/systemd/container | `deploy/*` | 配置样例；没有在自有服务器部署或验收。 |
| independent production security audit | threat model and source review | 未进行；不得声称生产安全审计或生产准入。 |

建议验证入口：

```bash
cargo test --locked --all-features
cargo build --locked --release
cargo build --locked --release --no-default-features
python3 tests/e2e.py --binary target/release/agent-tunnel --lease-secs 10
scripts/package.sh <target-triple>
```

每次回归输出应带目标、工具链、TTY/Relay入口和脱敏日志。以上当前 PASS 仅对应实际测试过的临时环境；源码和依赖更新后应重新运行。

## 13. 原架构规格 P0–P4 状态对照

下表保留原始路线完整范围。一项中的局部能力已实现或上一轮测试通过，不代表该 P 项整体完成；后续设计和验收不得据此缩小原 scope。

| 原规格 | 当前覆盖 | 剩余缺口 / 验收边界 |
| --- | --- | --- |
| **P0**：隔离目标 full/connector-only 构建、loopback/WSS 链路、Codex/pi 实际版本验收 | 最新 32-job E2E、本地TLS证书正负路径与Quick Tunnel公网WSS/Noise均PASS；pi 0.86.0六工具注册/假Controller tool执行PASS；x86_64 musl双变体静态PIE编译/初次打包已完成。 | 最新文档归档需再打包；Codex与pi真实模型调用未验收，ARM64不在本次用户要求的验收范围。 |
| **P1**：session revoke、one-time join、长期目标身份确认、人工逐命令审批及可审计绑定 | Relay durable revoke、PTY操作者接管和真实TTY逐次审批通过近期回归。 | one-time join、独立可信目标fingerprint及持久审批审计尚缺；当前目标incarnation只是短期绑定。 |
| **P2**：Controller–Connector E2EE、重放防护、key rotation和不可信Relay | Noise AEAD/握手/重放/篡改已有单元测试，Relay侧实测只见密文，本地TLS与临时Quick公网测试PASS。 | 在线key rotation、完整不可信Relay审查及独立生产安全审计尚缺。 |
| **P3**：cgroup/容器级清理、stdin/PTY、resize及写入lease | 真实PTY shell、stdin/write/resize、独立人工接管30s写入lease及本地rootless scratch Relay路由实测PASS。 | 容器Relay并不等同于容器级Connector进程隔离；对恶意daemonize后代的清理或cgroup级保证尚缺。 |
| **P4**：持久 job metadata/output spool、受限 resume、Relay HA/固定域名和运维撤销 | Relay 本地管理 socket、durable revoke marker 是当前可用的运维撤销路径。 | job metadata/output spool、受限 resume、Relay HA 和固定域名仍未实现/验收；PID record 不等于恢复。 |

P0–P4 以外，Streamable HTTP MCP 和公网 MCP gateway 仍是独立设计项。crash recovery、exactly-once、通用远端 approve、恶意目标隔离、独立安全审计及生产支持仍不承诺。

### 剩余非目标和明确缺项

- 本项目不是沙箱；不提供通用 seccomp/cgroup/VM 边界、对恶意目标的隔离或对 daemonize/脱离进程组后代的可靠清理。
- Connector crash、Relay restart 或新 incarnation 后不恢复 stdio/PTY、输入、job manager、输出 spool 或 request-dedup map；不承诺 exactly-once、自动重放或离线队列。
- 不提供 one-time join、在线 key rotation、可验证的长期目标 fingerprint、跨崩溃持久的审批审计记录或通用远端 approve API。
- 不提供 Relay HA、离线路由、固定生产域名或公网 MCP gateway；外层公网烟测不等于自有服务器运行验收。
- Quick Tunnel、临时CA本地WSS和本地scratch容器已有测试；自有公网域名/nginx/systemd的实际部署和真实模型行为仍待验收；按用户要求不验收ARM64，最新文档版的归档尺寸以提交后复建为准。
- 未进行独立安全审计、密码学证明、生产准入评估或稳定协议兼容性承诺。

发现凭证泄露、目标身份不符、Noise 未建立、审批状态不明、revoke marker 写入失败、远端输出诱导执行或 daemon 逃逸时，应停止操作、隔离旧 session，并通过可信运维渠道创建新 session。
