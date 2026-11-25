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

### 日线窗口

`fertile_finance.research_windows.coverage_windows(equity, end=None, *, calendar="XNYS", rf=None, rf_source=None, turnover=None)` 接收以交易会话日期为键、正的有限 `Decimal` 为值的日终净值，按顺序返回 `50y`、`10y`、`5y` 和 `longest_sample`。前三行从最后一个观测日期按日历年回推，闰日目标年无 2 月 29 日时使用 2 月 28 日。可显式指定 `end` 作为固定窗口统计截止日。最长样本行覆盖截止日前（含）第一至最后一条可用观测，`requested_years` 为 `null`，`requested_start` 为第一条观测日期。交易所日历可用 exchange-calendars 注册名指定。

行内 `window` 是稳定键，展示使用 `label`，例如 `50y (insufficient)`。覆盖字段包括 `requested_start`、`actual_start`、`actual_end`、`actual_years`、`observations`、`calendar_start`、`expected_sessions`、`missing_sessions` 和 `missing_pct`。实际跨度为首尾相差天数除以 365.2425，单位为年。固定窗口请求起点早于 epoch 时保留原日期，从 `calendar_start=1970-01-01` 开始统计交易日，覆盖标记为 `insufficient`。其余窗口仅在起始边界至首条观测之前最多缺五个交易日、内部及末端无缺失且至少两条观测时标记 `full`。周末、休市日按交易所日历处理。覆盖判定不使用跨度百分比、不填补观测。

`metrics` 为 `null` 时，该行全部绩效不可用。满窗的 `return_pct`、`cagr_pct`、`max_drawdown_pct`、`volatility_pct`、`turnover_pct` 和无量纲 `sharpe` 均用十进制字符串，单项不可用时为 `null`。CAGR 按实际跨度计算，最大回撤是正的峰谷跌幅。波动用日简单收益样本标准差乘以 √252。Sharpe 用日超额收益均值除以其样本标准差，再乘以 √252。少于两条日收益时波动和 Sharpe 为空，超额收益零方差时 Sharpe 也为空。

- `rf` 按收益结束的会话日提供已转换为日简单收益率的 `Decimal`，例如 `0.0001` 表示 1 bp。缺任何所需日的 RF 时 Sharpe 为 `null`。默认 `rf_source="rf=0"`，提供序列时默认标记为 `provided`，也可用 `rf_source` 指明公开来源。
- `turnover` 按收益结束的会话日提供日换手比率，显式用 `Decimal(0)` 表示零换手。窗口汇总用各日比率的总和并以百分比表示，首条净值作为基线，其当日换手不计入。未提供序列或缺任一所需会话时换手为 `null`。

例如结束于 2026-11-20 的 SPY 类样本从 1993-01-29 起约 33.81 年，只能在达到覆盖要求时报告 10 年及 5 年绩效，50 年显示不足，实际样本绩效使用独立的 `longest_sample` 行。即使长历史指数 ^GSPC 的原始数据始于 1927 年，98.89 年也不能称为已回测覆盖。50 年窗口的请求起点为 1976-11-20，只有可用部分符合上述规则才可报告。官方作业范围仍是经审核的 XNYS 股票／ETF，拒绝指数代理。

现有 `fertile_finance.backtest_research.coverage_windows(equity, end)` 入口继续返回三个固定窗口，使用同一套计算。SmaCrossover 成功摘要和新的研究调用使用四行入口。
