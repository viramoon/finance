# 进程入口与后续规划

| 入口 | 职责与依赖 | 生命周期 |
| --- | --- | --- |
| trader | Nautilus Live 节点、策略、风险和执行；当前 IBKR paper；依赖 Gateway、数据和可恢复状态 | 独立常驻，web / api 故障不停止核心 |
| collector | 行情采集、补数和 Parquet catalog 写入 | 独立常驻，可与交易部分分开迁移 |
| backtest | 读取历史 catalog，在 Backtest 模式批量回测 | 按任务运行，不作为常驻服务 |
| api | 面向 web 的 HTTP 服务，由 Rust handler 用 utoipa 导出 OpenAPI | 独立常驻；不承担交易主循环 |

不再创建自研 core / connector / data / execution / backtest / py-bindings crate。当前已实现 finance-api（默认 127.0.0.1:8081）与示例回测批任务，命令和 JSON 文件边界见 [API 文档](../docs/api.md)。trader / collector 的启动命令、通信边界、端口、状态存储和资源预算仍为 TBD，由其实现 issue 确定；API 与实时 Nautilus 节点如何对接仍待验证。三类常驻进程的设计字段见 [service.yaml](../service.yaml)，backtest 的结果与输入必须可追溯。
