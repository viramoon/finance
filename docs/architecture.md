# finance 架构与边界

finance 是 fertile 的主项目与 submodule，交易核心作为 NautilusTrader 依赖使用，不 fork。Rust 内核与 PyO3 Python 接口提供统一回测、模拟和实盘基础；我们只管理策略、做市商数据 adapter、配置、进程入口、运维和研究。

```text
IB Gateway / 数据源 → Nautilus IB adapter / mm_api → collector → Parquet catalog
                                             → trader → 风控 / 执行 → IB Gateway
研究 / backtest ← 历史 catalog
浏览器 → web → HTTP → finance api → 节点状态与查询（内部对接 TBD）
```

trader、collector、api 是独立常驻进程，backtest 是批任务。交易节点与行情采集分开，采集停止需触发行情陈旧保护；web 和 api 不可用不影响交易主循环。API 不是下单幂等或风控的唯一保护层。

当前云电脑只用 paper。迁移时 IB Gateway、实时数据、交易和订单状态可搬到稳定服务器，研究回测留云电脑，边界遵守 [fertile ADR-001](https://github.com/viramoon/fertile/blob/main/docs/adr/ADR-001-cloud-box-network-constraints.md)。部署环境与运行模式独立，见 [配置](../config/README.md)。

行情持久化使用 Parquet；业务与博客 PostgreSQL 归 web。Nautilus 节点缓存、订单标识与恢复状态的持久化方案 TBD，不能把 web 数据库或 Tunnel 变为交易前置依赖。连接恢复、补数、对账与熔断归属见 [connectivity.md](connectivity.md)。

不再规划 v1 的自研核心 crate。新增能力先查 [Nautilus 组件](nautilus.md)，替换自研必须符合 [ADR-0002](adr/ADR-0002-nautilus-dependency.md)。所有 API 数值保持精度，契约由代码导出，见 [api.md](api.md)。
