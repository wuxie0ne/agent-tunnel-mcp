# 实测记录及未完成验证

验证主机日期：2026-09-23（Asia/Shanghai）。以下结果来自本机实际命令；源码、入口、CA 或依赖发生变化后应重新运行。标记为“之前构建”的文件尺寸不能当作最终发行产物。

| 范围 | 实际命令 / 方法 | 已观察到的结果与边界 |
| --- | --- | --- |
| Rust 核心 | `cargo test --offline --all-features`、`cargo clippy --offline --all-targets --all-features -- -D warnings`、`cargo check --offline --no-default-features` | 集成人工接管及到期修复后，35 项单测全通过，Clippy 两种 feature 构建无警告。 |
| 出站执行与 MCP | `python3 tests/e2e.py --binary target/debug/agent-tunnel --lease-secs 10` | 32 个 job，覆盖角色隔离、Noise密文抓包、错误key、独立stdout/stderr、stdin/PTY、cursor/配额、断线后同ID不重放、revoke、MCP六工具；最新整合源码通过。 |
| 审批与人工接管 | `python3 tests/manual_approval.py --binary target/debug/agent-tunnel`、`python3 -B tests/attach.py --binary target/debug/agent-tunnel` | 真实 `/dev/tty` 下的批准/拒绝及无TTY闭锁通过；PTY人工专有写入、Agent读写/取消冻结、退出后缓冲清理与30秒owner lease回收通过。同UID恶意代码不在隔离范围。 |
| 过期拒绝 | `python3 tests/expiry.py --binary target/debug/agent-tunnel` | 两轮已认证5秒lease到期及10秒TTL到期后排队命令均被拒绝；不能代替全部调度顺序的形式证明。 |
| TLS/SNI与CA | `python3 tests/wss.py --binary target/debug/agent-tunnel` | 临时可信CA + WSS + Noise执行通过；不可解析域名采用**显式TCP IP**时，仍按原域名验证SNI/证书；错误CA拒绝连接。测试使用本机临时证书，非用户公网证书部署。 |
| Cloudflare Quick Tunnel | `python3 scripts/smoke-quick-tunnel.py --binary target/debug/agent-tunnel --proxy-command mgraftcp --connect-ip <经核对的Cloudflare IP>` | 最终调试二进制上两端公网WSS、Noise、无害exec/read、durable revoke通过；之前无IP的直连及mgraftcp测试受到本机临时域名DNS解析失败/超时影响。固定TCP IP**没有**禁用域名/SNI/证书校验，仅用于经允许的DNS故障排查。随机Quick域名不是生产入口。 |
| Pi 0.86.0适配 | `node --test integrations/pi/ipc.test.mjs`、`python3 scripts/check-pi-extension.py`、`python3 scripts/check-pi-execution.py` | 5项IPC测试、实际Pi离线扩展注册六工具、扩展工具代码对**假**Controller执行并保留request ID均通过。没有调用模型，也没有任何真实远端命令由模型触发。 |
| 本地容器 | `podman build --pull=never --network=none -f deploy/Containerfile ...`、`python3 scripts/smoke-container.py --binary ...` | 静态scratch Relay rootless构建、仅loopback监听、只读role挂载、私有可写admin socket和真实Noise exec/read通过；使用的是之前的x86_64静态二进制，需要最终包重复验证。未在用户自有公网服务器部署。 |
| 静态构建（之前） | `file target/package-{full,connector}/x86_64-unknown-linux-musl/release/agent-tunnel` | 最新full 3,290,256 B、connector-only 2,343,856 B，均为x86_64静态PIE；两种归档已重新生成且哈希通过，文档提交后应再打包一次。 |
| 签名检验 | 临时目录生成一次性Minisign测试密钥、签manifest、执行 `scripts/verify-release.sh` | 正确manifest/归档验签通过，归档或manifest篡改均拒绝；测试密钥已销毁。用户实际发行公钥需通过独立可信渠道分发；仅有SHA-256不认证发布者。 |
| 包默认安全及重建 | 对现有 `dist/` 运行 `scripts/package.sh x86_64-unknown-linux-musl`，然后用 `scripts/package.sh --force x86_64-unknown-linux-musl` 重建我先前生成的包 | 默认拒绝覆盖已有包且原SHA-256不变；明确--force后两种归档、manifest哈希和角色文件排除检查PASS，x86_64最新二进制3,290,256/2,343,856 B。文档最后一次提交后须再打包以包含最终说明。 |

## 未在本轮验收的范围与运营前提

1. 按当前已提交Rust源码重新生成x86_64 full/connector-only 静态包，核查 `file`、CLI、包中文档/Skill/pi文件、SHA-256与构建前后Git状态；如原有同名 `dist/` 文件已存在，必须显式同意 `--force`。
2. 按照用户本轮明确要求，只验收 x86_64；ARM64 及 QEMU 不在本轮交付验证范围内，不将之前的实验性构建当作发行件。
3. 一次性join、长期目标fingerprint、在线密钥轮换、持久可审计授权记录、cgroup隔离、Relay HA及跨崩溃任务恢复均未实现或不属于当前安全承诺；见架构规划的 P0–P4 保留缺项。
4. 自有公网服务器的真实证书、管理面权限与systemd/nginx部署，以及真正Codex/pi模型参与的诊断循环，需用户提供受信任环境和数据外发许可后才能验收；本文件不等同于独立安全审计或生产准入。

所有执行测试均指向临时本机或可丢弃的公网烟测目标，未接入真实线上主机。
