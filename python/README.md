# Python 研究与分析

用于研究 notebook、数据分析和调用 Nautilus Python API，不负责重新实现 PyO3 绑定或交易核心。未来 notebook 需说明环境版本、输入 catalog 区间、参数、随机种子、成本和结果复现方式；可复用研究脚本需独立 issue 和必要验证。

数据通过配置读取 Parquet catalog；原始数据、大结果、凭据和 notebook 的敏感输出不进 git。TBD：研究环境锁定、notebook 命名、样例数据与结果保留期，由首个研究 issue 确定。当前 fertile_finance/backtest_sample.py 提供上游示例的批任务摘要包装，不实现策略或研究 notebook；入口与来源规则见 [API](../docs/api.md)。

finance-backtest-worker 使用已锁定源码环境，通过独立 Python 子进程调度上游示例和具名历史策略。接口请求、进程资源限制和恢复流程见 [回测作业](../docs/backtest-jobs.md)。

## 研究作业

源码构建的 Python 环境包含研究日线导入器和均线策略。配置、运行和费用见 [历史回测](../docs/historical-backtest.md)。

### Unix epoch 边界

本系统日线时间戳下界是 `1970-01-01T00:00:00Z`，该时刻及之后的有效时间戳均可表示。预 1970 历史不可用。最长样本从第一条可用观测起算，研究作业从 warmup 完成后的第一条净值起算。

- `fertile_finance.epoch.filter_pre_epoch(rows, timestamp=..., mode="reject")` 接收返回 Python 整数的时间戳提取函数。传入值是转换为无符号类型之前的 UTC 纳秒。`mode="drop"` 返回保留行与 `DroppedTimestamps(count, earliest_ns, latest_ns)`，保留原有行序，最早和最晚值为精确的有符号 Unix 纳秒。未丢弃时计数为零，两个边界均为 `None`。类型错误和超出有符号 64 位范围的值始终报错。
- `fertile_finance.epoch.process_bar_frame(wrangler.process, frame, mode="reject", ts_init_delta=0)` 在原生 wrangler 前检查 `DatetimeIndex`，无时区的索引按 UTC 解释，拒绝 `NaT` 和越界的事件或初始化时间。返回值是原生 Bar 列表和同一丢弃报告。历史导入可直接用 `filter_pre_epoch` 检查数据，并用 `Price` / `Quantity` 精确构造 Bar。
