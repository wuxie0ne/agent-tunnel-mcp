# Go 首版实施与验收记录

日期：2026-09-30。结论：**代码、基础测试、两种开发候选包已交付；完整 P0 发布验收尚未完成，不能标记为正式发布。**

所有实际测试均为本项目拥有的临时进程、目录和只读容器，不执行真实数据库修复，不调用真实 Agent 的付费模型，不更改用户浏览器/全局 MCP 配置。

## 1. 阶段提交

| 提交 | 内容 |
| --- | --- |
| `ab21808` | 定稿需求与 Go 整体设计 |
| `a86c52a` | Linux 执行、资源控制和同步 JSONL 命令历史 |
| `1025f27` | 共用 MCP 工具、三种模式、绑定且一次消费的审核 |
| `b4fef4e` | 多目标中转、归属/重连、离线拒绝及未知结果转换 |
| `82e8daf` | CLI、Quick 生命周期、打包/许可、集成收尾与可复现脚本 |

继续开发于 `docs/go-requirements` 分支；旧 Rust `main` 不改动、不重置。最终候选制品代码源为 `82e8daf1ccd51a3f5e3e9955f0f20e63aec166e0`，构建时 source_dirty=false。后续验收文档和可选界面脚本不冒充已通过新一轮全部发布验证。

## 2. 已通过的验证

### 2.1 Go 本地测试

工具链 Go 1.26.2，官方 Go SDK v1.8.0、coder/websocket v1.8.14、x/sys v0.41.0。24 项顶层测试（含子用例）通过：

```text
go test -race -timeout 60s ./...
go vet ./...
```

覆盖：显式模式/配置、shell 与直接执行、stdin 关闭、启动失败、输出采集/返回截断、UTF-8 处理、超时及后代持管道、JSONL 并发/权限/轮转/粘性故障、执行前后记录失败、JSON-only MRTR、同意/拒绝/确认不可用、审核内容冲突/重放、取消/繁忙、连接换代失效、停止等待已接收 HTTP、多目标/同名隔离、错误注册密钥、离线不排队、丢响应未知、同实例重连原 URL、中转重启新 URL及原到期时间、HTTP 头规范化、Quick URL 变化与子进程停止。

SDK 测试的确认回复由程序生成，不等于真实客户端界面/真人确认。故障注入测试不等于任意文件系统/断电保证，也不等于生产安全审计。

### 2.2 真实跨进程与容器

- 两个真实 CLI 进程（relay/target）经 SDK 模拟确认后执行 `sleep 300`，300.010s 正常返回结果。该次二进制 SHA-256 为 `1e06a92ca26b66bc77e729658a667c701c50e8478f7b11be1c860e60a7bb9a1e`，属于最终打包前的代码快照，不假称最终制品哈希。
- 最终打包二进制在缓存的 Alpine 3.22 x86_64 **只读容器**内，以 UID 0 运行目标端，通过本地 relay 执行并返回 `id -u`、`uname -m` 与固定输出。运行目标/中转不需要 Go、Node.js 或 Python；此项验证的是容器 root，不是主机提权。
- 自带/精简归档解包后逐文件 `SHA256SUMS` 全部通过，两个包的 Go 二进制相同。

### 2.3 真实官方客户端界面（本地中转）

官方 **MCP Inspector 2.8.0** 的真实 Web UI，Node 24.15.0，独立 headless Chrome/Playwright，明确选 `protocolEra=modern` / MCP 2026-07-28：

1. 真实界面显示目标实例、完整命令、cwd、运行身份和超时。
2. 用户确认前临时文件不存在。
3. 自动化勾选 confirm 并 Submit，命令仅追加一次 `x`。
4. 第二个命令点击 Decline，目标没有副作用。

这不是自定义 Go 模拟客户端；但**按钮由自动化操作，不宣称本轮是真人点击**，也不证明任意客户端诚实或不支持的 Agent 能使用审核。最终打包二进制已在该流程通过。

可复现入口 `scripts/smoke-inspector.py`。Inspector/Playwright 仅作为可选开发验收依赖，不进入 Go 发布物。临时 HOME、存储和配置隔离；不使用用户原浏览器 profile/OS keyring，不保存凭据进仓库。

### 2.4 真实 Quick Tunnel / 最终自带制品

使用处理后的 cloudflared 2026.3.0，真实公网 Quick Tunnel，SDK 模拟确认；cloudflared/客户端经已有 mgraftcp，客户端测试工具可指定 DNS-over-TCP `1.1.1.1:53`，**不改全局 DNS**。

- 短命令返回成功，实际总耗时 15.945s，历史记录正常。
- 命令 `sleep 130`、目标执行超时 2m：实际命令 122.001s（120s + 清理）返回 `COMMAND_TIMEOUT`，历史正常；总客户端耗时 135.451s 含发现/审核/网络。这是**预期工具超时结果成功回传**，不是链路失败；测试驱动 exit=1 因其按工具错误退出。
- 上述均使用最终打包 Go SHA `a6edc9fd99cd382ca4ed70968fd6ac08fe4477f33db702d8d83a514a1f5fc394` 和处理后 cloudflared SHA `9c8b68e77ae71e6aee980ed15484ad4164285d9e44493c095d7870a2c548830a`，不是只跑 `--version`。

## 3. 已发现失败/需求冲突

### Quick Tunnel 300 秒同步结果回传：未通过

最终自带制品，目标超时 6m，命令 `sleep 300; printf long-completed`：

- 客户端约 **138.343s**（含发现/审核/网络）收到调用失败，没有完成结果。
- 客户端不重试；目标继续运行，JSONL 记录命令在 **300.002s** 正常退出。
- 一次意图/一次结果，无重放；本地结果不等于客户端收到了结果。

证据表明该测试路径不能可靠完成已确认的长命令同步回传。没有精确归因到某个 Cloudflare 内部时限，也不从总耗时推定某个“138 秒”固定上限。

**需要用户后续确认的产品取舍**：保持同步且长命令优先走自部署中转、允许 Quick 模式明确限定长命令适用范围，或重新讨论异步/续接/补取和接入方式。后两类会改变首版范围或增加工程量。当前代码保留已定超时可调范围，不静默缩短、不开发额外查询、不自动重放；不能把此 P0 冲突仅标成限制就默认正式发布。

## 4. 网络失败与未验证边界

- Quick 申请过程中曾出现 `api.trycloudflare.com/tunnel` 等待 headers 超时、EOF，以及临时域名在 WSL 系统 DNS 返回 NXDOMAIN。它们发生于接入/发现阶段，不证明命令引擎或 MRTR 不支持。
- 官方 Inspector 经 Quick 的真实界面测试本次因接入申请超时/EOF而未就绪，**未完成 G0 的两入口真实客户端界面验收**。已通过的是本地 relay 界面及真实 Quick 的 SDK 模拟确认，不能合并成“两入口真实客户端均通过”。
- 自部署**真实公网 IP + HTTP** 未验证；没有给定可部署公网服务器。本地 TCP/WS跨进程通过，不等于公网/防火墙/实际 Agent 兼容性通过。
- 用户最终选用的 Codex/Claude/其他实际 Agent 未做模型驱动集成，不为验证擅自调用付费模型。可依据支持 MCP 2026-07-28 与 form elicitation 的真实客户端继续验收。
- 没有跨崩溃任务恢复、抗篡改审计、远程结果补取、生产权限/隔离或 ARM 验收；没有真实数据库修复和容量压测。

依赖获取时 goproxy.cn 曾连接重置，通过 mgraftcp/官方 Go proxy 获取成功。官方 cloudflared 大文件下载发生过 SSL 超时/慢速中断；未使用残缺下载，本机原文件与官方 GitHub release asset digest 逐字节校验匹配，详见来源证据。

## 5. 开发候选制品

| 制品 | 字节数 | SHA-256 |
| --- | ---: | --- |
| `dist/agent-tunnel-0.1.0-dev-linux-amd64-slim.tar.gz` | 3,736,096 | `1f48755b3d81d3f6b7262b0f1b6d5698c8690104f1a95c49c9e3f067db6bb873` |
| `dist/agent-tunnel-0.1.0-dev-linux-amd64-full.tar.gz` | 11,195,665 | `0b50ba95c1e160b19656c062d890fe343c1fa33a5073aa7ec5185d84f3136abf` |

构建 CGO_ENABLED=0、GOAMD64=v1、trimpath/strip Go 调试符号。官方 cloudflared 原始 39,667,364 bytes，经 strip --strip-debug/UPX 4.2.2 后 7,454,320 bytes；保留原始/处理后哈希、版本、处理方式、上游 LICENSE和依赖许可。

`dist/` 及运行日志/临时凭据不提交。已按上述范围验证的归档为开发候选，不打正式 release tag。候选包里的实施文档是 `82e8daf` 时的快照；本文件为更新后的验收结论，不能把它和归档内旧快照混为同一版本。

## 6. 证据与复现

- `validation-evidence/cloudflared-source.json`：官方发布 metadata 与原始文件匹配。
- `validation-evidence/runtime-summary.json`：原始结果摘要及明确评估，含各次实际二进制哈希。
- `scripts/smoke-local.py`：真实 CLI 跨进程中转，确认由 SDK 模拟。
- `scripts/smoke-quick.py`：真实 Quick，支持单独的 proxy/DNS 测试路径，断线后可等待本地结果；不会重试命令。SDK 只可在发出命令之前重试 discovery。
- `scripts/smoke-inspector.py`：第三方真实客户端界面，自动化按钮。
- `scripts/package.py`：校验原始官方输入、处理副本、记录许可/校验值，不改系统安装的 cloudflared。

临时证据路径写在 JSON 摘要中，可能被系统清理；已提交摘要不等于完整原始执行环境可永久重建。命令包含的秘密不自动脱敏，使用实际目标时用户须保护历史目录。
