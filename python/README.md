# Python 研究与分析

用于研究 notebook、数据分析和调用 Nautilus Python API，不负责重新实现 PyO3 绑定或交易核心。未来 notebook 需说明环境版本、输入 catalog 区间、参数、随机种子、成本和结果复现方式；可复用研究脚本需独立 issue 和必要验证。

数据通过配置读取 Parquet catalog。原始数据、大结果、凭据和 notebook 的敏感输出不进 git。历史适配器测试使用 [构造的源格式样本](../tests/fixtures/historical)。研究环境沿用源码构建与依赖锁定。notebook 命名与结果保留期仍为 TBD，由后续 notebook 流程确定。当前 `fertile_finance/backtest_sample.py` 提供上游示例的批任务摘要包装，入口与来源规则见 [API](../docs/api.md)。

finance-backtest-worker 使用已锁定源码环境，通过独立 Python 子进程调度上游示例和具名历史策略。接口请求、进程资源限制和恢复流程见 [回测作业](../docs/backtest-jobs.md)。

## 研究作业

源码构建的 Python 环境包含研究日线导入器和均线策略。配置、运行和费用见 [历史回测](../docs/historical-backtest.md)。

### Unix epoch 边界

本系统日线时间戳下界是 `1970-01-01T00:00:00Z`，该时刻及之后的有效时间戳均可表示。预 1970 历史不可用。最长样本从第一条可用观测起算，研究作业从 warmup 完成后的第一条净值起算。 原生 Nautilus 对负时间戳的运行时验证见 [日线时间戳下界](../docs/nautilus.md#日线时间戳下界)。

- `fertile_finance.epoch.filter_pre_epoch(rows, timestamp=..., mode="reject")` 接收返回 Python 整数的时间戳提取函数。传入值是转换为无符号类型之前的 UTC 纳秒。`mode="drop"` 返回保留行与 `DroppedTimestamps(count, earliest_ns, latest_ns)`，保留原有行序，最早和最晚值为精确的有符号 Unix 纳秒。未丢弃时计数为零，两个边界均为 `None`。类型错误和超出有符号 64 位范围的值始终报错。
- `fertile_finance.epoch.process_bar_frame(wrangler.process, frame, mode="reject", ts_init_delta=0)` 在原生 wrangler 前检查 `DatetimeIndex`，无时区的索引按 UTC 解释，拒绝 `NaT` 和越界的事件或初始化时间。返回值是原生 Bar 列表和同一丢弃报告。历史导入可直接用 `filter_pre_epoch` 检查数据，并用 `Price` / `Quantity` 精确构造 Bar。

### 日线窗口

`fertile_finance.research_windows.coverage_windows(equity, end=None, *, calendar="XNYS", rf=None, rf_source=None, turnover=None)` 接收以交易会话日期为键、正的有限 `Decimal` 为值的日终净值，按顺序返回 `50y`、`10y`、`5y` 和 `longest_sample`。前三行从最后一个观测日期按日历年回推，闰日目标年无 2 月 29 日时使用 2 月 28 日。可显式指定 `end` 作为固定窗口统计截止日。最长样本行覆盖截止日前（含）第一至最后一条可用观测，`requested_years` 为 `null`，`requested_start` 为第一条观测日期。`calendar` 使用 exchange-calendars 注册名或别名，未注册的名称会报 `InvalidCalendarName`。

行内 `window` 是稳定键，展示使用 `label`，例如 `50y (insufficient)`。覆盖字段包括 `requested_start`、`actual_start`、`actual_end`、`actual_years`、`observations`、`calendar_start`、`calendar_end`、`expected_sessions`、`missing_sessions` 和 `missing_pct`。实际跨度为首尾相差天数除以 365.2425，单位为年。固定窗口请求起点早于 epoch 时保留原日期，仅统计 epoch 当日及之后的可用交易日，覆盖标记为 `insufficient`。其余窗口仅在起始边界至首条观测之前最多缺五个交易日、内部及末端无缺失且至少两条观测时标记 `full`。周末、休市日按交易所日历处理。覆盖判定不使用跨度百分比、不填补观测。

注册日历的规则可能只覆盖有限日期。例如锁定的 XSHG 日历从 1990-12-03 起可用。只要某行的 epoch 后请求区间超出这些规则的起止范围，该行就报告 `coverage=insufficient`、`coverage_reason="calendar_out_of_range"` 和 `metrics=null`，即使日历内缺失交易日数为零。其他行独立判定。`calendar_start` 和 `calendar_end` 给出该行统计交易日使用的上下界。无重叠时上下界逆序，应有与缺失交易日数为零，缺失比例为 `null`。实际起止和观测数仍包括请求区间内的输入净值，日历规则未覆盖的日期不参与交易日计数。

已注册的日历也可能只在部分区间能构造，或在锁定依赖中尚未实现。每行独立尝试构造所需区间。构造失败的行返回 `insufficient`、`coverage_reason="calendar_unavailable"` 和 `metrics=null`，`calendar_diagnostic` 记录日历名、尝试区间与错误类型和摘要。统计上下界、应有交易日数、缺失交易日数和缺失比例均为 `null`，实际净值起止和观测数照常报告。例如 XTAE 的 2026-01-12 至 2026-01-16 可报告 `longest_sample`，同期回推的三个固定窗口会因构造失败而标记不足。

`metrics` 为 `null` 时，该行全部绩效不可用。满窗的 `return_pct`、`cagr_pct`、`max_drawdown_pct`、`volatility_pct`、`turnover_pct` 和无量纲 `sharpe` 均用十进制字符串，单项不可用时为 `null`。CAGR 按实际跨度计算，最大回撤是正的峰谷跌幅。波动用日简单收益样本标准差乘以 √252。Sharpe 用日超额收益均值除以其样本标准差，再乘以 √252。少于两条日收益时波动和 Sharpe 为空，超额收益零方差时 Sharpe 也为空。

- `rf` 按收益结束的会话日提供已转换为日简单收益率的 `Decimal`，例如 `0.0001` 表示 1 bp。缺任何所需日的 RF 时 Sharpe 为 `null`。默认 `rf_source="rf=0"`，提供序列时默认标记为 `provided`，也可用 `rf_source` 指明公开来源。
- `turnover` 按收益结束的会话日提供日换手比率，显式用 `Decimal(0)` 表示零换手。窗口汇总用各日比率的总和并以百分比表示，首条净值作为基线，其当日换手不计入。未提供序列或缺任一所需会话时换手为 `null`。

例如结束于 2026-11-20 的 SPY 类样本从 1993-01-29 起约 33.81 年，只能在达到覆盖要求时报告 10 年及 5 年绩效，50 年显示不足，实际样本绩效使用独立的 `longest_sample` 行。即使长历史指数 ^GSPC 的原始数据始于 1927 年，98.89 年也不能称为已回测覆盖。50 年窗口的请求起点为 1976-11-20，只有可用部分符合上述规则才可报告。官方作业范围仍是经审核的 XNYS 股票／ETF，拒绝指数代理。

现有 `fertile_finance.backtest_research.coverage_windows(equity, end)` 入口继续返回三个固定窗口，使用同一套计算。SmaCrossover 成功摘要和新的研究调用使用四行入口。

## 历史数据接口

加载器以本地文件为输入，返回精确的 `Decimal` 数值和 `date` 日期。加载器使用同一个 [`filter_pre_epoch`](#unix-epoch-边界) 检查观察或事件日期。`pre_epoch` 默认为 `"reject"`。选用 `"drop"` 时，`dropped_pre_epoch` 的 `count`、`first` 和 `last` 分别记录丢弃条数、最早和最晚日期。Yahoo 的条数同时包含行情记录和公司行为。获取经许可的源文件见 [运行手册](../RUNBOOK.md#手工保存公开历史数据)。

### Yahoo 日线

```python
# fertile_finance.yahoo_daily
yahoo_chart_url(symbol: str, start: date, end: date) -> str
load_yahoo_daily(path: str | Path, *, adjustment_as_of: date, pre_epoch="reject") -> YahooDaily
```

`yahoo_chart_url` 接收含首尾的纽约当地日期，生成 `period1` 含起点、`period2` 不含终点的 `interval=1d` 请求，同时请求分红、拆股和调整收盘价。`adjustment_as_of` 是存档的观测截止日期，应覆盖文件内的行情及事件日期。

`YahooDaily` 提供 `symbol`、`series_id`、`asset_type`、`currency`、`listing_venue`、`adjustment_as_of`、`bars`、`dividends`、`splits` 和 `dropped_pre_epoch`。行情记录包含 `session`、源 `open/high/low/close/volume`、源 `adjusted_close`、`adjustment_factor` 和 `adjustment_as_of`。`DailyBar.is_missing_quote` 标记 OHLCV 全空的行，`YahooDaily.missing_quote_rows` 统计解析后这类行的数量，包含待写入时跳过的非会话日全空行。分红提供 `session`、`source_timestamp` 和源币种 `amount`。拆股提供 `session`、`source_timestamp`、`numerator` 和 `denominator`。两类事件都提供可空的源事件键 `source_id`，时间以 Unix 秒返回。读取 catalog 后，行情还提供会话收盘时间戳。`DailyBar.adjusted()` 返回标明历史复权口径的派生记录，源调整收盘价缺失时返回 `None`，源 OHLCV 仍可读取。缺值规则、复权口径与时点限制见 [目录约定](../catalog/README.md#存储-schema-与价格口径)。

### Ken French 和 FRED

```python
# fertile_finance.research_factors
load_ken_french(factors_path: str | Path, *, momentum_path: str | Path | None = None,
                pre_epoch="reject") -> FrenchDaily
load_ken_french_rf(factors_path: str | Path, *, pre_epoch="reject") -> FrenchRiskFree
load_fred(path: str | Path, *, series_id: str, pre_epoch="reject") -> FredDaily
```

- `FrenchDaily.rows` 是按 `session` 排序的 `FrenchFactor` 元组。`mkt_rf`、`smb`、`hml`、`rf` 和 `mom` 以收益率小数返回，源百分数 `0.12` 对应 `Decimal("0.0012")`。读取日频 CSV 或文本，也可读取含单个源文件的 ZIP。`Mom` 可来自同表或按日期外连接独立日频文件。空字段及源缺失标记 `-99.99` / `-999` 返回 `None`。数据格式不含首次可用日期，`available_on` 返回 `None`。
- `FrenchRiskFree.rows` 是同样按日期排序的 `FrenchRF` 元组，字段为 `session`、`rf` 和 `available_on`。`FrenchDaily` 与 `FrenchRiskFree` 均提供 `dropped_pre_epoch`。
- `FredDaily` 提供 `series_id`、`rows` 和 `dropped_pre_epoch`。各 `FredObservation` 的字段为 `observation_date`、`value`、`realtime_start` 和 `realtime_end`。`value` 保持供应商单位，缺值为 `None`。支持 FRED Graph CSV 和标准 observations API JSON（`output_type=1`、`units=lin`），每个观察日期对应一个版本。JSON 保留源版本日期区间，CSV 无此日期字段。版本日期不等于日内发布时间。获取数据时根据供应商元数据核实序列日频与单位。

### Parquet catalog 读写

```python
# fertile_finance.historical_catalog.HistoricalCatalog
HistoricalCatalog(data_root: Path | None = None)
write_yahoo(batch: YahooDaily, *, mode="append", start=None, end=None) -> WriteReport
write_french(batch: FrenchDaily, *, mode="append", start=None, end=None) -> WriteReport
write_fred(batch: FredDaily, *, mode="append", start=None, end=None) -> WriteReport
read_yahoo(series_id: str, *, start=None, end=None) -> YahooHistory
read_french(*, start=None, end=None) -> FrenchDaily
read_fred(series_id: str, *, start=None, end=None) -> FredDaily
```

`start`、`end` 为可选的 `date` 闭区间端点。`WriteReport` 提供 `series_id`、实际写入主记录的 `first` 与 `last`、`observations`、`dividends`、`splits`、`dropped_pre_epoch`。Yahoo 写入报告另用 `missing_quote_rows` 统计写入的 OHLCV 全空记录，用 `skipped_missing_non_sessions` 统计跳过的非会话日全空记录。`YahooHistory` 提供 `symbol`、`series_id`、`asset_type`、`currency`、`bars`、`dividends`、`splits` 和 `is_reference`。行情读取后有 `ts_event` 和 `ts_init` 两个 UTC 纳秒时间戳。读取 French catalog 返回完整因子行，其中 `rf` 与文件加载器单位相同。French 和 FRED 读取结果的 `dropped_pre_epoch` 为零，导入时的丢弃信息以写入报告为准。读取的日期是观察或交易标签，判断何时可用时应核对源发布元数据。存储、并发以及覆盖合同见 [目录约定](../catalog/README.md#公开历史数据)。
