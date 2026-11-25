# Python 研究与分析

用于研究 notebook、数据分析和调用 Nautilus Python API，不负责重新实现 PyO3 绑定或交易核心。未来 notebook 需说明环境版本、输入 catalog 区间、参数、随机种子、成本和结果复现方式；可复用研究脚本需独立 issue 和必要验证。

数据通过配置读取 Parquet catalog。原始数据、大结果、凭据和 notebook 的敏感输出不进 git。历史适配器测试使用 [构造的源格式样本](../tests/fixtures/historical)。研究环境沿用源码构建与依赖锁定。notebook 命名与结果保留期仍为 TBD，由后续 notebook 流程确定。当前 `fertile_finance/backtest_sample.py` 提供上游示例的批任务摘要包装，入口与来源规则见 [API](../docs/api.md)。

finance-backtest-worker 使用已锁定源码环境，通过独立 Python 子进程调度上游示例和具名历史策略。接口请求、进程资源限制和恢复流程见 [回测作业](../docs/backtest-jobs.md)。

## 研究作业

源码构建的 Python 环境包含研究日线导入器和均线策略。配置、运行、费用与覆盖见 [历史回测](../docs/historical-backtest.md)。运行时测试同时检查原生 Nautilus 对负时间戳的异常及项目侧 epoch 边界。

## 历史数据接口

加载器以本地文件为输入，返回精确的 `Decimal` 数值和 `date` 日期。`pre_epoch` 默认为 `"reject"`。选用 `"drop"` 时，`dropped_pre_epoch` 记录丢弃条数与首尾日期。Yahoo 的条数同时包含行情记录和公司行为。获取经许可的源文件见 [运行手册](../RUNBOOK.md#手工保存公开历史数据)。

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
