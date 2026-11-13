# ADR-0001：交易与数据技术栈

状态：已采纳（设计，尚未实现）

依据：issue #1 方案 v2

适用范围：finance

## 背景与决策

交易、回测、模拟需要复用成熟执行与数据能力，云电脑资源与网络受限。采用 **NautilusTrader 作为依赖**，使用 Rust 内核加 Python（PyO3）接口，不自研交易核心，不选择 C，也不 fork。finance 聚焦策略、做市商数据 adapter、配置、入口、运维和研究。

行情使用 Nautilus Parquet data catalog，不另起时序数据库；web 通过 HTTP 调用 finance，Rust API 用 utoipa 从代码导出 OpenAPI，快照由 fertile contracts/ 管理。业务与博客数据库 PostgreSQL 归 web。

## 理由与后果

依赖成熟连接、对账、风险与回测基础，减少维护面；代价是需要锁定上游版本、验证兼容和接受扩展边界。collector 与 trader 分进程，保留迁移边界，API 不成为交易主循环依赖。

当前 IBKR 只用 paper，所有路径配置化，部署环境与运行模式独立。环境证据与迁移规则引用 [fertile ADR-001](https://github.com/viramoon/fertile/blob/main/docs/adr/ADR-001-cloud-box-network-constraints.md)。依赖许可和替换触发条件见 [ADR-0002](ADR-0002-nautilus-dependency.md)。

## 后续

TBD：锁定版本、API 内部对接、配置真实字段及性能 / 故障恢复验证；通过后续独立 issue 完成，本次不创建构建文件。
