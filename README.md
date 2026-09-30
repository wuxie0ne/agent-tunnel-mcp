# agent-tunnel（Go 首版开发候选）

在临时 Linux x86_64 服务器/容器启动轻量访问端，让用户自己的本地 Agent 通过远程 MCP 使用该环境的命令。无需目标安装 Agent、Go、Node.js 或 Python。

**不是生产运维平台。不是沙箱。有效 URL/token 可代表目标运行身份的全部命令权限。** 公网 HTTP/WS 会暴露凭据、命令与结果；首版允许这一取舍，不提供端到端加密。

当前是开发候选，不是已完成全部 P0 验收的正式发布。**已实测：Quick Tunnel 的 300 秒同步命令可能提前丢失响应，而目标仍执行并记录完成；长操作目前优先使用自部署中转，不要因丢失响应重试。没有静默缩短执行超时，也没有改成异步补取。**真实 Agent 用户确认、公网 IP+HTTP 及 Quick Tunnel 长命令的验收边界，见 `docs/implementation-status.md`。官方 SDK 测试和模拟确认不等于真实用户审核。

## 启动（以下交互命令为 fish）

Quick Tunnel，自带包无需预装 cloudflared；精简包需已有程序：

```fish
./agent-tunnel target --transport quick --mode review
```

如需显式指定已有程序：

```fish
./agent-tunnel target --transport quick --mode allow --cloudflared /path/to/cloudflared --ttl 4h
```

目标终端输出一条就绪 JSON，其中 `mcp_url` 是交给 Agent 的远程 URL，包含短期凭据；`instance_id`、`expires_at`、`history_path` 等说明本次环境。URL 改变时输出新的就绪信息。**不要把完整输出粘贴到公开日志、issue 或仓库。** 不承诺任意 MCP 客户端兼容。

自部署中转（两个终端/不同机器分别设置同一私有密钥；至少 16 字符）：

```fish
# 公网中转机器：建议使用随机生成的私有密钥，不使用示例值
set -gx AGENT_TUNNEL_REGISTRATION_KEY '<你的私有注册密钥>'
./agent-tunnel relay --listen 0.0.0.0:8080

# 目标机器：不需要目标开放入站端口
set -gx AGENT_TUNNEL_REGISTRATION_KEY '<相同私有注册密钥>'
./agent-tunnel target --transport relay --relay-url http://公网IP:8080 --mode review
```

一个二进制同时包含 `target` 与 `relay`，两种入口共用工具和授权核心。已有 HTTPS/WSS 部署可使用，但首版不自动申请证书。国内网络环境可按自己的设置使用外部代理；工具不隐式下载或配置代理。

## 模式与工具

必须显式选择模式：

- `allow`：持有有效 token 可执行任意通用命令。
- `review`：客户端完成 MCP form 用户确认后执行；不支持、拒绝或过期都不执行，不降级。
- `strict`：所有通用命令拒绝，只能查访问端自身身份/状态，不能系统诊断。

首版协议基线为 **MCP 2026-07-28，JSON-only Streamable HTTP + MRTR 审核**。不为旧 SSE 审核加本地网关。配置远程 MCP 地址的方法和是否呈现确认界面取决于实际客户端，必须分别验收。

工具：

- `target_info`：名称、实例、运行身份、模式、到期/连接状态、工作目录、资源及历史写入状态。
- `exec`：`shell_command` 或 `program` + `args` 二选一；可指定绝对 `cwd` 和 `max_output_bytes`。没有 `approved`、stdin、后台或调用级 timeout 参数。

示例工具输入（不是另一个本地 CLI）：

```json
{"shell_command":"uname -a","cwd":"/tmp","max_output_bytes":8192}
```

```json
{"program":"/bin/echo","args":["one argument; not shell"],"max_output_bytes":16384}
```

默认 `/bin/sh`、启动 cwd、继承环境，每次独立执行；上次的 `cd`/变量不会延续。默认超时 2m、并发 2、token 2h。通过 `target -h` 查看 `--command-timeout`、`--concurrency`、`--shell`、`--log-dir` 等启动参数。token 从进程启动起算，重连不延期；到期拒绝新操作，目标进程不自动退出，已启动命令继续收尾。重新访问须重启生成新实例/地址。

stdout/stderr 各采集最多 1 MiB，默认合计返回 8 KiB，调用可提高到 256 KiB；结果标明采集/返回截断和编码替换，无补取工具。超时/正常停止尽力终止进程组，不保证所有脱离控制的后代，也不回滚副作用。

## 断线和结果未知

客户端断线/等待取消不杀已启动命令。已知离线的新调用直接拒绝，不排队；连接恢复不重放。同一中转进程正常重连保留原 URL，中转重启则重新注册并显示新 URL；旧 URL 不重定向到新目标。

**断线、超时、HTTP 错误或没收到响应不能证明命令未执行，不要自动重试有副作用的操作。** 目标只对已识别的内部请求和审核续接做小量内存防重，不能阻止 Agent 另发相同命令。不提供任务查询/恢复/取消接口。

## 本地命令历史

默认写 `<启动目录>/agent-tunnel-logs/<实例 ID>/commands.jsonl`。每条 JSONL 记录执行意图、结果或拒绝；包含完整命令、cwd、模式/审核、超时及退出/清理/截断元数据，默认不存 stdout/stderr 正文或环境全集。

- 目录 `0700`、文件 `0600`；日志根目录不能是 symlink 或组/其他用户可写目录。
- 10 MiB 轮转，每实例保留最近 5 个文件，正常停止不删除；旧实例目录由用户清理。
- 意图必须先写入并同步后执行；失败不启动。执行后写入失败不代表未执行，目标停止接受新命令，修复后重启。
- 有意图但无结果表示不确定，不会自动恢复/重放。命令本身可能含密码/SQL，日志不自动脱敏、不抗同身份命令或 root 篡改。
- 临时容器销毁也可能丢日志，需要保留时用 `--log-dir` 指向持久挂载目录。

不要通过日志缺失或截断推断“没有副作用”。本地日志不是客户端已收到结果的证明。

## 开发、测试与打包

开发需要 Go 1.26.2 或兼容工具链；目标只需要发布二进制，Quick 模式另需 cloudflared。开发脚本使用 Python 3，但运行目标/中转不依赖 Python。

```fish
go test -race ./...
go vet ./...
go build -o dist/agent-tunnel ./cmd/agent-tunnel
go build -o /tmp/agent-tunnel-smoke-client ./scripts/smoke-client
```

自动化本地/公网测试（确认回复由 SDK 测试程序模拟，**不是 G0 真人审核验收**）：

```fish
python3 -B scripts/smoke-local.py --binary dist/agent-tunnel --client /tmp/agent-tunnel-smoke-client
python3 -B scripts/smoke-quick.py --binary dist/agent-tunnel --client /tmp/agent-tunnel-smoke-client
# 使用已有 mgraftcp 配置时可显式加：
# --proxy-command mgraftcp --client-proxy-command mgraftcp
```

打包精简版：

```fish
python3 -B scripts/package.py
```

打包两种包需要提供原始官方 cloudflared、从官方发布 metadata 核对的 SHA-256、对应版本/LICENSE，以及本机 strip/UPX：

```fish
python3 -B scripts/package.py --cloudflared /path/to/official/cloudflared --official-sha256 '<官方原始SHA-256>' --cloudflared-version 2026.3.0 --cloudflared-license /path/to/upstream/LICENSE
```

脚本验证原始校验值，再 `strip --strip-debug` 与 `upx --best --lzma`；不会处理系统已安装的原文件。输出 Linux amd64 自带/精简归档及 SHA256SUMS、BUILD-INFO、第三方许可。压缩后须真实 Quick Tunnel/MCP 功能测试，不能只凭 `--version` 或 UPX 完整性检查发布。已有同名归档须显式 `--force`。

可选真实客户端界面验收：使用已安装的官方 Inspector 2.8.0、Playwright 和独立 headless Chrome，不自动安装、不改用户浏览器/全局客户端配置。

```fish
# 用安装了 Playwright 的开发 Python；按钮由自动化操作，不宣称真人点击
python3 -B scripts/smoke-inspector.py --binary dist/agent-tunnel --inspector /path/to/inspector/clients/launcher/build/index.js
```

源码自身许可尚未指定，不由打包器擅自选择开源许可；保留依赖与 cloudflared 许可，不宣称第三方合规审计完成。

## 文档

- `docs/go-requirements.md`：产品范围与已选取舍。
- `docs/go-design-plan.md`：Go 整体架构、状态和资源边界。
- `docs/implementation-status.md`：提交、实际测试和未完成验收。
