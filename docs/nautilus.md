# NautilusTrader 依赖使用

规划依赖 Nautilus 的 Strategy / Actor、TradingNode、Backtest 引擎、风险与执行引擎、IB 适配器、数据模型、Parquet data catalog 和 Python API。按 issue v2 的选型使用上游已有连接、重订阅、对账与 Docker IB Gateway 生命周期能力；具体启用参数及行为需对选定版本验证，不视为本仓库已运行。

版本锁定方式 TBD：后续实现 issue 选择经验证的上游发布版本，记录 Rust / Python / PyO3 兼容性与依赖锁定文件。本次没有构建文件或虚构的依赖版本。

升级流程：读上游变更与许可证 → 在隔离环境更新锁定 → 重跑回测 / Sandbox 回归 → 验证断网、重订阅、补数、订单恢复、精度和 catalog 兼容 → 经 PR 更新文档 → 按 fertile 集成发版。需数据迁移时先备份并验证恢复，不直接用最新分支替换。

只有已证实的能力缺口、性能瓶颈或维护风险才触发自研替换；先尝试配置、扩展点与上游 PR，记录测量证据、兼容与退出方案，用 ADR 单独决策。禁止 fork 或修改 Nautilus 源码，见 [ADR-0002](adr/ADR-0002-nautilus-dependency.md)。

参考：[NautilusTrader](https://github.com/nautechsystems/nautilus_trader)、[架构](https://github.com/nautechsystems/nautilus_trader/blob/develop/docs/concepts/architecture.md)、[IB 集成](https://github.com/nautechsystems/nautilus_trader/blob/develop/docs/integrations/interactive_brokers.md)。
