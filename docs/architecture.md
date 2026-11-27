# finance 架构与边界

finance 是 fertile 的主项目与 submodule，交易核心以 vendor/nautilus_trader 源码 submodule 引入并从源码构建，补丁维护遵守 ADR-0003。Rust 内核与 PyO3 Python 接口提供统一回测、模拟和实盘基础；我们只管理策略、做市商数据 adapter、配置、进程入口、运维和研究。

```text
CSV Replay / IB 数据源 → finance-collector → Nautilus Parquet catalog + 状态文件
F1 事件生产者 → MarketIngress
  ├─ 仅展示事件 → 短时内存
  └─ 允许落盘事件 → 本地 WAL → finance-projector → 事件 Parquet + SQLite 索引与游标
Yahoo / Ken French / FRED 存档 → 历史适配器 → catalog/historical → Python 研究
经审核的 NYSE CSV → 独立研究 catalog → 回测 worker → 共享策略求值 → 历史模拟
F1 闭合 bar 回放 / 配置策略会话快照 → finance-decision → 共享策略求值 → 本地决策记录
浏览器 → web → HTTP → finance-api → collector 状态 / 回测作业与示例结果
上游 CSV 示例 → 回测批任务 → 原子 latest.json
```

collector、finance-projector、回测 worker、finance-decision 和 finance-api 可独立运行，示例回测是批任务。F1 数据源适配器由后续 issue 接入。交易节点及其实时状态由后续 issue 实现。交易设计要求查询故障时主循环独立运行，采集停止时触发行情陈旧保护，并在交易侧落实下单幂等和风控。

回测 worker 通过本地作业队列租约横向扩展。实时决策从持久断点重启，与 `SmaCrossover` 和 `StrategyBundle` 回测共用对应的闭合行情求值器。配置策略的会话输入包含点时成分及只读组合快照。模拟执行留在回测进程。输入与数据目录见 [实时决策接口](decision-process.md)。

当前云电脑只用 paper。迁移时 IB Gateway、实时数据、交易和订单状态可搬到稳定服务器，研究回测留云电脑，边界遵守 [fertile ADR-001](https://github.com/viramoon/fertile/blob/main/docs/adr/ADR-001-cloud-box-network-constraints.md)。部署环境与运行模式独立，见 [配置](../config/README.md)。

旧 collector 将闭合 bar 直接写入 Nautilus Parquet。允许落盘的 F1 事件先提交到本地 WAL，再投影为事件 Parquet 与 SQLite 索引。封段备份和恢复、四条水位及许可规则见 [本地行情 WAL](market-wal.md)。

业务与博客 PostgreSQL 归 web。Nautilus 节点缓存、订单标识与恢复状态的持久化方案 TBD，不能把 web 数据库或 Tunnel 变为交易前置依赖。连接恢复、补数、对账与熔断归属见 [connectivity.md](connectivity.md)。

不再规划 v1 的自研核心 crate。新增能力先查 [Nautilus 组件](nautilus.md)，替换自研必须符合 [ADR-0002](adr/ADR-0002-nautilus-dependency.md)。所有 API 数值保持精度，契约由代码导出，见 [api.md](api.md)。

finance-api 通过数据根目录中的 collector 心跳与水位提供市场状态。冷启动缺文件与过期状态的规则、回放及原子发布协议见 [行情目录](../catalog/README.md)。

公开历史行情和因子由离线 Python 适配器读取，按序列写入数据根 `catalog/historical` 下的 Nautilus Parquet 版本。研究读取 API 返回源价格、复权因子、公司行为与宏观数据。指数以参考序列保存。`SmaCrossover` 仍从独立的审核快照加载 NYSE 股票或 ETF，且继续执行原有准入、覆盖和价格口径检查。分区与行情语义见 [历史数据目录](../catalog/README.md#公开历史数据)。
