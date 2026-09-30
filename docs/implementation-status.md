# Go 首版实施记录

日期：2026-09-30。本文件记录实际证据，不将设计或模拟测试当作完整验收。

## 阶段记录

- 文档基线：`ab21808`，需求与整体设计已提交。
- 执行/历史核心：Go 1.26.2；`go test -race ./internal/config ./internal/history ./internal/executor` 通过。覆盖显式模式、两种执行形式、stdin 关闭、启动失败、并发日志/权限/轮转/粘性故障、输出限额、超时与后代管道收尾。
- 依赖获取：默认 goproxy.cn 连接被重置；通过配置的 mgraftcp 及官方 Go proxy 获取成功，区别于编译/功能故障。

- MCP/目标核心：锁定官方 Go SDK v1.8.0。`go test -race ./internal/target ./internal/mcpserver` 通过，包括 JSON-only MRTR 审核同意/拒绝、全部放行/严格、确认不可用、命令绑定、防重、取消/繁忙、连接变代及日志前后故障。确认回复由测试程序生成，不是实际 Agent UI 或真人确认，不计作 G0。

- 多目标中转：`go test -race ./internal/relay ./internal/relayproto` 通过；覆盖两目标/同名隔离、审核同意/拒绝、错误注册密钥、离线不排队、断线后继续一次执行、同实例原地址重连、中转重启新地址/原到期时间。转发丢失的已知未知结果转换为 MCP 工具错误（started=null），避免 SDK 丢弃 HTTP 502 body 后只显示 Bad Gateway；目标仍唯一授权/执行。

- CLI/Quick 管理：使用真实 cloudflared 2026.3.0 完成公网短命令（SDK 模拟确认），一次记录耗时 5.889s；自带/精简最终制品还需重新检查。其余尝试出现入口未就绪及客户端域名 NXDOMAIN，尚不能以这些网络失败判断长命令功能。
- 本地跨进程 relay：SDK 模拟确认后无输出运行 300s，正常返回结果，耗时 300.010s；二进制 SHA-256 `1e06a92ca26b66bc77e729658a667c701c50e8478f7b11be1c860e60a7bb9a1e`。后续小修改需另列最终制品检查。
- cloudflared 来源：本机原始文件与官方 GitHub 2026.3.0 release asset digest 完全匹配；记录在 `validation-evidence/cloudflared-source.json`。官方下载尝试出现 403、SSL 超时/慢速中断，未把残缺下载用于发布。

- 最终基础检查：当前 24 项顶层 Go 测试（含子用例）通过 `go test -race -timeout 60s ./...`，`go vet ./...` 通过。增加显式 null/0/未知字段校验、停止等待已接收 HTTP、保留 leader PID 至管道清理结束防复用，以及匿名 WebSocket 连接上限。

## 尚未验收

真实 Agent 用户确认（G0）、两种公网入口长命令（G1）、发布物及完整故障矩阵仍未通过，不宣称首版已经完成。
