# 进程入口与后续规划

| 入口 | 职责与依赖 | 生命周期 |
| --- | --- | --- |
| trader | Nautilus Live 节点、策略、风险和执行；当前 IBKR paper；依赖 Gateway、数据和可恢复状态 | 独立常驻，web / api 故障不停止核心 |
| collector | 行情采集、补数和 Parquet catalog 写入 | 独立常驻，可与交易部分分开迁移 |
| finance-decision | 只读消费闭合行情回放，写入本地策略目标与水位 | 可执行一次回放或独立常驻追加消费 |
| backtest | 读取历史 catalog，在 Backtest 模式批量回测 | 按任务运行，不作为常驻服务 |
| backtest-worker | 从 SQLite 领取并续租回测作业 | 独立常驻，同一宿主可启动多个实例 |
| api | 面向 web 的 HTTP 服务，由 Rust handler 用 utoipa 导出 OpenAPI | 独立常驻；不承担交易主循环 |

当前入口及运维参数见 [service.yaml](../service.yaml) 和 [运行手册](../RUNBOOK.md)。trader 的启动命令、状态存储和资源预算仍为 TBD，由未来实现 issue 确定。回测结果与输入保留可追溯性。
