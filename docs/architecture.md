# finance 架构与边界

finance 是 fertile 的主项目与 submodule，交易核心以 vendor/nautilus_trader 源码 submodule 引入并从源码构建，补丁维护遵守 ADR-0003。Rust 内核与 PyO3 Python 接口提供统一回测、模拟和实盘基础；我们只管理策略、做市商数据 adapter、配置、进程入口、运维和研究。

```text
CSV Replay / IB 数据源 → finance-collector → Nautilus Parquet catalog + 状态文件
                                             → trader → 风控 / 执行 → IB Gateway
Yahoo / Ken French / FRED 存档 → 历史适配器 → catalog/historical → Python 研究
经审核的 NYSE CSV → 独立研究 catalog → SmaCrossover 回测作业
浏览器 → web → HTTP → finance api → 示例回测 latest.json
上游 CSV 示例 → 回测批任务 → 原子 latest.json
实时节点状态与查询：后续设计（内部对接 TBD）
```

trader、collector、api 是独立常驻进程，backtest 是批任务。交易节点与行情采集分开，采集停止需触发行情陈旧保护；web 和 api 不可用不影响交易主循环。API 不是下单幂等或风控的唯一保护层。

当前云电脑只用 paper。迁移时 IB Gateway、实时数据、交易和订单状态可搬到稳定服务器，研究回测留云电脑，边界遵守 [fertile ADR-001](https://github.com/viramoon/fertile/blob/main/docs/adr/ADR-001-cloud-box-network-constraints.md)。部署环境与运行模式独立，见 [配置](../config/README.md)。

行情持久化使用 Parquet；业务与博客 PostgreSQL 归 web。Nautilus 节点缓存、订单标识与恢复状态的持久化方案 TBD，不能把 web 数据库或 Tunnel 变为交易前置依赖。连接恢复、补数、对账与熔断归属见 [connectivity.md](connectivity.md)。

不再规划 v1 的自研核心 crate。新增能力先查 [Nautilus 组件](nautilus.md)，替换自研必须符合 [ADR-0002](adr/ADR-0002-nautilus-dependency.md)。所有 API 数值保持精度，契约由代码导出，见 [api.md](api.md)。

finance-api 通过数据根目录中的 collector 心跳与水位提供市场状态。冷启动缺文件与过期状态的规则、回放及原子发布协议见 [行情目录](../catalog/README.md)。

公开历史行情和因子由离线 Python 适配器读取，按序列写入数据根 `catalog/historical` 下的 Nautilus Parquet 版本。研究读取 API 返回源价格、复权因子、公司行为与宏观数据。指数以参考序列保存。`SmaCrossover` 仍从独立的审核快照加载 NYSE 股票或 ETF，且继续执行原有准入、覆盖和价格口径检查。分区与行情语义见 [历史数据目录](../catalog/README.md#公开历史数据)。
