# Go 首版实施记录

日期：2026-09-30。本文件记录实际证据，不将设计或模拟测试当作完整验收。

## 阶段记录

- 文档基线：`ab21808`，需求与整体设计已提交。
- 执行/历史核心：Go 1.26.2；`go test -race ./internal/config ./internal/history ./internal/executor` 通过。覆盖显式模式、两种执行形式、stdin 关闭、启动失败、并发日志/权限/轮转/粘性故障、输出限额、超时与后代管道收尾。
- 依赖获取：默认 goproxy.cn 连接被重置；通过配置的 mgraftcp 及官方 Go proxy 获取成功，区别于编译/功能故障。

- MCP/目标核心：锁定官方 Go SDK v1.8.0。`go test -race ./internal/target ./internal/mcpserver` 通过，包括 JSON-only MRTR 审核同意/拒绝、全部放行/严格、确认不可用、命令绑定、防重、取消/繁忙、连接变代及日志前后故障。确认回复由测试程序生成，不是实际 Agent UI 或真人确认，不计作 G0。

- 多目标中转：`go test -race ./internal/relay ./internal/relayproto` 通过；覆盖两目标/同名隔离、审核同意/拒绝、错误注册密钥、离线不排队、断线后继续一次执行、同实例原地址重连、中转重启新地址/原到期时间。转发丢失的已知未知结果转换为 MCP 工具错误（started=null），避免 SDK 丢弃 HTTP 502 body 后只显示 Bad Gateway；目标仍唯一授权/执行。

## 尚未验收

真实 Agent 用户确认（G0）、两种公网入口长命令（G1）、发布物及完整故障矩阵仍未通过，不宣称首版已经完成。
