# Python 研究与分析

用于研究 notebook、数据分析和调用 Nautilus Python API，不负责重新实现 PyO3 绑定或交易核心。未来 notebook 需说明环境版本、输入 catalog 区间、参数、随机种子、成本和结果复现方式；可复用研究脚本需独立 issue 和必要验证。

数据通过配置读取 Parquet catalog。原始数据、大结果、凭据和 notebook 的敏感输出不进 git。历史适配器测试使用 [构造的源格式样本](../tests/fixtures/historical)。研究环境沿用源码构建与依赖锁定。notebook 命名与结果保留期仍为 TBD，由后续 notebook 流程确定。当前 `fertile_finance/backtest_sample.py` 提供上游示例的批任务摘要包装，入口与来源规则见 [API](../docs/api.md)。

finance-backtest-worker 使用已锁定源码环境，通过独立 Python 子进程调度上游示例和具名历史策略。接口请求、进程资源限制和恢复流程见 [回测作业](../docs/backtest-jobs.md)。

## 研究环境

只需要本地回测时，可以运行 `bash scripts/bootstrap-research.sh` 准备 Rust、uv、Python、LLVM 和编译依赖。此入口跳过 PostgreSQL、Node.js 和 pnpm。环境文件、安装权限和无写入预览见 [精简研究工具链](../docs/build.md#精简研究工具链)。

在仓库根目录加载所选工具链环境并运行 `bash scripts/build.sh`。随后把 `research` 依赖组同步到同一个回测 venv。该组包含 SciPy 和 statsmodels，`--inexact` 保留已安装的 Nautilus 源码 wheel。

```bash
build_root=${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}
UV_CACHE_DIR="$build_root/uv-cache" UV_PROJECT_ENVIRONMENT="$build_root/venv" \
  uv sync --locked --only-group research --inexact --python "$(cat .python-version)"
```

准备好上述 Python 环境和本地 Cargo 依赖缓存后，在仓库根目录构建共享指标求值器并运行测试。显式传入 `FINANCE_STRATEGY_BIN` 的绝对路径，供测试及其以精简 `PATH` 启动的子进程使用。

```bash
build_root=${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}
target_dir=$(realpath -m -- "${CARGO_TARGET_DIR:-target}")
cargo build --locked --offline -p finance-api --bin finance-strategy --target-dir "$target_dir"
FINANCE_STRATEGY_BIN="$target_dir/debug/finance-strategy" \
  FERTILE_DATA_ROOT="$(mktemp -d)" PYTHONPATH=python \
  "$build_root/venv/bin/python" -B -m unittest discover -s tests -v
```

## 研究作业

源码构建的 Python 环境包含研究日线导入器和均线策略。配置、运行和费用见 [历史回测](../docs/historical-backtest.md)。

从仓库根设置 `PYTHONPATH=python`，用源码构建的解释器调用以下模块。每个入口支持 `--help`。

| 模块 | 用途 |
| --- | --- |
| `fertile_finance.download_yahoo_daily` | 将公开 Yahoo chart 日线下载到指定文件，有限退避并报告实际 URL |
| `fertile_finance.isolated_historical_job` | 在独立数据根、研究根和环回端口上通过官方 HTTP API 执行一次作业 |
| `fertile_finance.migrate_spy` | 核对旧 SPY 固定 21:00 UTC 时点并用本地 Yahoo 存档重导，支持 `--dry-run` |
| `fertile_finance.convert_research_csv` | 从 Yahoo 文件或历史数据根生成官方导入 CSV，输出价格口径、规范计数、拆股血缘和 SHA-256 |
| `fertile_finance.research_catalog` | 将经审核的原始无拆股或 Yahoo 拆股归一化 OHLCV 快照发布到指定研究根 |

完整操作见 [运行手册](../RUNBOOK.md#隔离验收官方历史作业)、[SPY 迁移](../catalog/README.md#既有-spy-固定-2100-utc-数据) 和 [CSV 导出与审核导入](../docs/historical-backtest.md#准备数据)。

### Unix epoch 边界

本系统日线时间戳下界是 `1970-01-01T00:00:00Z`，该时刻及之后的有效时间戳均可表示。预 1970 历史不可用。最长样本从第一条可用观测起算，研究作业从 warmup 完成后的第一条净值起算。原生 Nautilus 对负时间戳的运行时验证见 [日线时间戳下界](../docs/nautilus.md#日线时间戳下界)。

- `fertile_finance.epoch.filter_pre_epoch(rows, timestamp=..., mode="reject")` 接收返回 Python 整数的时间戳提取函数。传入值是转换为无符号类型之前的 UTC 纳秒。`mode="drop"` 返回保留行与 `DroppedTimestamps(count, earliest_ns, latest_ns)`，保留原有行序，最早和最晚值为精确的有符号 Unix 纳秒。未丢弃时计数为零，两个边界均为 `None`。类型错误和超出有符号 64 位范围的值始终报错。
- `fertile_finance.epoch.process_bar_frame(wrangler.process, frame, mode="reject", ts_init_delta=0)` 在原生 wrangler 前检查 `DatetimeIndex`，无时区的索引按 UTC 解释，拒绝 `NaT` 和越界的事件或初始化时间。返回值是原生 Bar 列表和同一丢弃报告。历史导入可直接用 `filter_pre_epoch` 检查数据，并用 `Price` / `Quantity` 精确构造 Bar。

### 日线窗口

`fertile_finance.research_windows.coverage_windows(equity, end=None, *, calendar="XNYS", rf=None, rf_source=None, turnover=None, longest_through_end=False)` 接收以交易会话日期为键、正的有限 `Decimal` 为值的日终净值，按顺序返回 `50y`、`10y`、`5y` 和 `longest_sample`。前三行从最后一个观测日期按日历年回推，闰日目标年无 2 月 29 日时使用 2 月 28 日。可显式指定 `end` 作为固定窗口统计截止日。最长样本行默认覆盖截止日前（含）第一至最后一条可用观测，`requested_years` 为 `null`，`requested_start` 为第一条观测日期。`longest_through_end=True` 将最长样本的日历截止日延伸到 `end`，最后一条有效观测之后的缺失会话也计入覆盖。`calendar` 使用 exchange-calendars 注册名或别名，未注册的名称会报 `InvalidCalendarName`。

行内 `window` 是稳定键，展示使用 `label`，例如 `50y (insufficient)`。覆盖字段包括 `requested_start`、`actual_start`、`actual_end`、`actual_years`、`observations`、`calendar_start`、`calendar_end`、`expected_sessions`、`missing_sessions` 和 `missing_pct`。实际跨度为首尾相差天数除以 365.2425，单位为年。固定窗口请求起点早于 epoch 时保留原日期，仅统计 epoch 当日及之后的可用交易日，覆盖标记为 `insufficient`。其余窗口仅在起始边界至首条观测之前最多缺五个交易日、内部及末端无缺失且至少两条观测时标记 `full`。周末、休市日按交易所日历处理。覆盖判定不使用跨度百分比、不填补观测。

注册日历的规则可能只覆盖有限日期。例如锁定的 XSHG 日历从 1990-12-03 起可用。只要某行的 epoch 后请求区间超出这些规则的起止范围，该行就报告 `coverage=insufficient`、`coverage_reason="calendar_out_of_range"` 和 `metrics=null`，即使日历内缺失交易日数为零。其他行独立判定。`calendar_start` 和 `calendar_end` 给出该行统计交易日使用的上下界。无重叠时上下界逆序，应有与缺失交易日数为零，缺失比例为 `null`。实际起止和观测数仍包括请求区间内的输入净值，日历规则未覆盖的日期不参与交易日计数。

已注册的日历也可能只在部分区间能构造，或在锁定依赖中尚未实现。每行独立尝试构造所需区间。构造失败的行返回 `insufficient`、`coverage_reason="calendar_unavailable"` 和 `metrics=null`，`calendar_diagnostic` 记录日历名、尝试区间与错误类型和摘要。统计上下界、应有交易日数、缺失交易日数和缺失比例均为 `null`，实际净值起止和观测数照常报告。例如 XTAE 的 2026-01-12 至 2026-01-16 可报告 `longest_sample`，同期回推的三个固定窗口会因构造失败而标记不足。

`metrics` 为 `null` 时，该行全部绩效不可用。满窗的 `return_pct`、`cagr_pct`、`max_drawdown_pct`、`volatility_pct`、`turnover_pct` 和无量纲 `sharpe` 均用十进制字符串，单项不可用时为 `null`。CAGR 按实际跨度计算，最大回撤是正的峰谷跌幅。波动用日简单收益样本标准差乘以 √252。Sharpe 用日超额收益均值除以其样本标准差，再乘以 √252。少于两条日收益时波动和 Sharpe 为空，超额收益零方差时 Sharpe 也为空。

- `rf` 按收益结束的会话日提供已转换为日简单收益率的 `Decimal`，例如 `0.0001` 表示 1 bp。缺任何所需日的 RF 时 Sharpe 为 `null`。默认 `rf_source="rf=0"`，提供序列时默认标记为 `provided`，也可用 `rf_source` 指明公开来源。
- `turnover` 按收益结束的会话日提供日换手比率，显式用 `Decimal(0)` 表示零换手。窗口汇总用各日比率的总和并以百分比表示，首条净值作为基线，其当日换手不计入。未提供序列或缺任一所需会话时换手为 `null`。

例如结束于 2026-11-20 的 SPY 类样本从 1993-01-29 起约 33.81 年，只能在达到覆盖要求时报告 10 年及 5 年绩效，50 年显示不足，实际样本绩效使用独立的 `longest_sample` 行。即使长历史指数 ^GSPC 的原始数据始于 1927 年，98.89 年也不能称为已回测覆盖。50 年窗口的请求起点为 1976-11-20，只有可用部分符合上述规则才可报告。官方作业范围仍是经审核的 XNYS 股票／ETF，拒绝指数代理。

现有 `fertile_finance.backtest_research.coverage_windows(equity, end)` 入口继续返回三个固定窗口，使用同一套计算。SmaCrossover 成功摘要和新的研究调用使用四行入口。

### 月度研究窗口

`fertile_finance.research_windows.monthly_coverage_windows(observed, end)` 复用四个窗口名，输入唯一的月历月末日期标签和月末截止日。固定窗口从请求的净值基线之后计入 600、120 或 60 条月收益，`longest_sample` 从首个有效收益月计到截止日。`monthly_coverage(observed, name=..., start=..., end=...)` 用同样规则检查显式的起止收益月。

每行分别披露首个请求和实际收益月、各自的前一月末净值基线、实际末月以及应有、实际和缺失的月份数。任何缺月均使窗口标记为 `insufficient`，研究报告的 `metrics` 为 `null`。月度标签可用于 1970 年前的离线研究补充。Nautilus 及日线 catalog 仍使用 [Unix epoch 边界](#unix-epoch-边界)。

## 历史数据接口

加载器以本地文件为输入，返回精确的 `Decimal` 数值和 `date` 日期。加载器使用同一个 [`filter_pre_epoch`](#unix-epoch-边界) 检查观察或事件日期。`pre_epoch` 默认为 `"reject"`。选用 `"drop"` 时，`dropped_pre_epoch` 的 `count`、`first` 和 `last` 分别记录丢弃条数、最早和最晚日期。Yahoo 的条数同时包含行情记录和公司行为。获取经许可的源文件见 [运行手册](../RUNBOOK.md#手工保存公开历史数据)。

### Yahoo 日线

```python
# fertile_finance.yahoo_daily
yahoo_chart_url(symbol: str, start: date, end: date) -> str
load_yahoo_daily(path: str | Path, *, adjustment_as_of: date, pre_epoch="reject") -> YahooDaily
```

`yahoo_chart_url` 接收 Yahoo 标的代码和含首尾的纽约当地日期，生成 `period1` 含起点、`period2` 不含终点的 `interval=1d` 请求，同时请求分红、拆股和调整收盘价。公开归档下载命令和重试策略见 [手工保存公开历史数据](../RUNBOOK.md#手工保存公开历史数据)。`adjustment_as_of` 是存档的观测截止日期，应覆盖文件内的行情及事件日期。

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

## 多标的研究

`fertile_finance.portfolio_backtest.run_portfolio_backtest` 在一个 Nautilus 引擎中运行 XNYS 权益组合，支持目标权重、横截面排序、逐会话净值和成交审计。`result.windows` 使用上方统一的四行合同，最长样本覆盖检查使用请求截止日。该 Python 入口把 Nautilus 已有的多标的能力封装成可复用的研究工具。输入契约、执行时点、估值缺口和无风险利率口径见 [多标的组合回测](../docs/portfolio-backtest.md)。HTTP 作业继续运行已注册的策略，`SmaCrossover` 的数据与执行合同见 [历史回测](../docs/historical-backtest.md)。

经审核的 Yahoo 日线可以通过 `PortfolioAsset.from_yahoo` 供组合研究使用。`RiskFreeSeries.from_french` 接收日频 RF 与独立提供的准确发布时间。FRED 序列的单位及版本处理见 [历史数据与组合输入](../docs/portfolio-backtest.md#历史数据与组合输入)。

## Catalog 回测入口

```text
fertile_finance.run_backtest(strategy, instruments, start, end, *, benchmark=None, costs=None,
             rf="kenfrench", fill_timing="next_open", catalog=None, universe=...,
             raw_split_free_reviewed=False, rf_available_ts=None, fixture_zero_volume=False)
```

- `instruments` 是一个或多个不同的 Yahoo catalog 标的 ID。可成交标的为 XNYS 美元股票或 ETF。`start`、`end` 是含边界的 `date` 或 ISO 日期字符串，至少覆盖两个 XNYS 会话。默认 `catalog=None` 按 `FERTILE_DATA_ROOT` 打开 `HistoricalCatalog`。调用方先核实供应商 OHLCV 的原始、未复权及无拆股归一化口径，再设置 `raw_split_free_reviewed=True`。
- `universe` 是调用方提供的 `UniverseSnapshot` 序列。首个会话开盘前须已有成员表，后续按实际可得时点更新成员。`strategy` 接收 `TargetWeights` 序列，或接收 `SignalContext` 并返回 `{标的 ID: Decimal 权重}` 的函数。每次回调只有当期会话的 `date`、收盘 `signal_ts`、`members`、可用原始收盘价 `closes` 和 `equity`。缺少持仓估值时 `equity` 为 `None`。函数可自行保留先前回调的状态。目标权重在 0 到 1 之间且总和至多为 1。
- `fill_timing` 默认 `next_open`，可选 `next_close`。收盘信号仅在下一 XNYS 会话执行，默认把零量或缺少执行价的意图记录在 `gaps`。初始现金为 `100000.00` USD。佣金为零，正成交量行情按指定开盘或收盘价成交，`costs` 接受 `None`、整数 `0` 或 `Decimal(0)`。非零成本会报错。
- `fixture_zero_volume=True` 仅用于隔离的合成研究 fixture。调用方可用合成价格和零成交量构造具备 XNYS 权益规格的指数风格日线，将其写入临时 catalog，使零量 OHLC 参与信号、估值和执行。执行时给原生 L1 引擎提供一股的报价，市价单的余量由引擎按对买卖方向不利的一跳价格即时成交。系统在下单前核对各档金额与可用现金，并逐笔核对方向、时间、数量、价格和佣金。真实参考指数仍仅用于基准，常规组合及官方单标的作业继续执行原有零量和成交规则。 [公开回测回归](../tests/test_run_backtest.py)
- `rf="kenfrench"` 从 catalog 读取 French 日 RF，并要求 `rf_available_ts` 为按观测日期索引、已核实的 UTC 纳秒发布时间。较早的观测可通过在该映射中提供较早日期来纳入读取范围，读取到的每条非空 RF 都需要时间证据。组合引擎按每段收益的起始收盘对齐已知利率，缺少当时可得 RF 的窗口 Sharpe 为 `None`。也可传入已构造的 `RiskFreeSeries`，或用 `rf=None` 显式选择 `rf=0`。
- `benchmark` 可传入独立的 Yahoo catalog ID，包括只用于比较的美元参考指数。基准按源收盘价序列计算价格收益，窗口内换手为零。基准报告使用其自身有效观测和相同的区间截止日与 RF 对齐规则。缺失的基准观测保留为缺口。

`BacktestResult.metadata` 记录标的、请求区间、成交时点、初始现金、成本、RF 来源及频率、基准 ID 和零量 fixture 开关。`nav` 按会话返回组合净值，`weights` 返回当日已持有头寸的收盘权重，现金为剩余权重。估值缺失的净值和权重为 `None`。`targets` 提供解析后的目标权重审计，`positions` 和 `cash` 分别为与原生模拟账户核对后的期末持仓和现金，`total_orders` 为实际订单数。

`fill_audit` 中每条记录提供 `signal_date`、`signal_ts`、`fill_date`、`fill_ts` 和归并后的 `fill`，每笔均满足 `signal_ts < fill_ts`。换手使用每笔逻辑成交的精确名义金额除以信号日净值。零量 fixture 的同订单、同纳秒分笔合并为一笔逻辑成交，`fill.parts` 提供各分笔的数量、价格、佣金、成交 ID 和时间。`windows` 依次给出 50 年、10 年、5 年和最长样本，覆盖不足的行 `metrics` 为 `None`。`benchmark_nav` 和 `benchmark_windows` 独立呈现基准结果，省略基准时两者为 `None`。

`result.analyze_factors(factors, as_of=..., regression=False)` 复用下方的相关工具，以相邻会话净值计算日收益。任一净值缺失的区间保留缺值。分析默认截在请求结束日，也可显式截在更早日期。

### 离线最小示例

从仓库根目录设置 `PYTHONPATH=python`，使用源码构建的 Python 运行以下合成 fixture。输入写入一个临时 catalog。

```python
from datetime import date
from decimal import Decimal
from tempfile import TemporaryDirectory

from fertile_finance import run_backtest
from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.historical_types import DailyBar, EpochDiscard
from fertile_finance.portfolio_backtest import UniverseSnapshot
from fertile_finance.research_catalog import nyse, sessions
from fertile_finance.yahoo_daily import YahooDaily

start, end = date(2024, 11, 27), date(2024, 12, 2)
symbol = "FIXTURE.XNYS"
rows = tuple(
    DailyBar(day, price, price, price, price, Decimal(1000), None, None, end)
    for day, price in zip(sessions(start, end), map(Decimal, ("100", "101", "104")), strict=True)
)
opening = int(nyse().schedule.at[start.isoformat(), "open"].value)
universe = (UniverseSnapshot(start, opening, (symbol,)),)

with TemporaryDirectory() as directory:
    catalog = HistoricalCatalog(directory)
    catalog.write_yahoo(YahooDaily("FIXTURE", "stock", "USD", "XNYS", end, rows, (), (), EpochDiscard()))
    result = run_backtest(
        lambda context: {symbol: Decimal("0.50")}, [symbol], start, end,
        catalog=catalog, universe=universe, raw_split_free_reviewed=True, rf=None,
    )
    print(result.metadata.fill_timing, result.nav)
    print([(row["window"], row["coverage"]) for row in result.windows])
```

## Ken French 日因子相关

`fertile_finance.factor_analysis.analyze_factors(returns, factors, *, as_of=None, regression=False)` 接收一个日收益 `pandas.Series`，因子可以直接传入 `load_ken_french(...)` 或 `HistoricalCatalog.read_french(...)` 返回的 `FrenchDaily`。已归一化的内存数据也可以用 `pandas.DataFrame` 传入。Pandas 索引使用唯一的交易日标签，支持 ISO 日期字符串、`datetime.date` 或无时区的午夜时间戳。收益用小数表示，例如 1% 写作 `0.01`。DataFrame 至少包含 `Mkt-RF`、`SMB`、`HML`，存在 `Mom` 列时一起计算。对 `FrenchDaily`，只有分析日期区间内可用的因子行至少提供一个非空动量值时才纳入 `Mom`。相关与回归直接使用传入收益，如需以超额收益为因变量，请先按已对齐的 RF 计算后传入。

按日期取完整交集，要求同日策略收益和所选因子均有值。缺日或缺值会缩小所有统计量的共同样本，计算不前填。结果提供 `sample_start`、`sample_end`、`observations` 和 `dropped_observations`，后者统计在策略收益索引首尾之间、至少一侧有该日标签但未入样的日期。两边同时缺少标签的日期无法从输入识别。空样本的首尾为 `None`。样本不足两点或任一变量为常数时，对应 Pearson 系数为 `None`。

历史分析传入 `as_of`，两侧只使用截至该日（含）的观测。`FrenchDaily` 中已知 `available_on` 晚于截点的记录会排除。Ken French 源文件未提供发布日期。若用结果驱动历史决策，调用方需按当时实际可用的发布日期和数据版本准备输入。

`regression=True` 用同一交集做含截距的 OLS，返回日截距、各因子系数和 R²。回归需要样本数大于参数数、因变量有变动且设计矩阵满秩。结果用 `regression_status` 披露 `ok`、`insufficient_observations`、`constant_returns` 或 `rank_deficient`。默认只计算 Pearson 系数。

```python
from pathlib import Path

import pandas as pd
from fertile_finance.factor_analysis import analyze_factors
from fertile_finance.research_factors import load_ken_french

fixtures = Path("tests/fixtures/historical")
factors = load_ken_french(
    fixtures / "french_daily.csv",
    momentum_path=fixtures / "french_momentum_daily.csv",
    pre_epoch="drop",
)
returns = pd.Series({row.session: 2 * row.mkt_rf for row in factors.rows if row.mkt_rf is not None})
result = analyze_factors(returns, factors, as_of="2024-07-05")
print(result.sample_start, result.sample_end, result.observations, result.correlations)
```

## 原生成交审计

研究模拟可收集 Nautilus 的 `OrderFilled` 回调，并把同一次运行的事件交给 `fertile_finance.fill_audit.merge_order_fills(events)`。组件按账户、客户端订单 ID 和 `fill_ts`（事件 UTC 纳秒）归并，原始分笔保存在 `raw_fills`。成交时间沿用 [Unix epoch 边界](#unix-epoch-边界)，接受从 0 到有符号 64 位整数上限的值。同一订单跨时间的分笔各保留一笔逻辑成交。重复 trade ID（同账户同订单）或订单标的、方向、币种不一致时抛出 `ValueError`。

每笔逻辑成交累加 `quantity`、`notional`（各分笔价格乘数量的和）和 `commission`。展示价格 `average_price` 为成交量加权平均价，使用 Decimal 的 28 位有效数字和四舍六入五成双，保留小于最小价格跳动的部分。组合和基准窗口、分笔明细、归并金额及因子数值转换使用独立的 Decimal 上下文，调用方的精度、舍入方式、异常开关和状态标志保持原值。查看稳定的十进制分笔请使用 `parts`。直接格式化 `raw_fills` 原生对象时沿用 Nautilus 的规则。统计换手金额时使用 `notional`。在成交价与佣金同币种的现金股票模拟中，可用 `cash_delta` 和 `position_delta` 对账。

[原生回归 fixture](../tests/test_fill_audit.py) 用相同的两根日线和一笔 10 股市价买单对比两档成交量。Nautilus 1.231.0 的 L1 日线模拟按标的最小数量生成零量 bar 的合成档位，并按偏移一跳的价格撮合耗尽第一档后的市价单余量。fixture 中零量产生同一时刻的 1 股 @ 100.50 和 9 股 @ 100.51，归并后为 10 股、成交金额 1005.09 USD。成交量 100 时产生一笔 10 股 @ 100.50，成交金额 1005.00 USD。两组各计一笔逻辑成交，并核对引擎的现金和持仓。官方历史作业继续使用 [零量意图不提交及一单一笔足额成交](../docs/historical-backtest.md#信号执行和绩效) 的合同。

## 月频因子研究案例

[时序因子动量](../strategies/2026-11-25-factor-momentum/README.md) 演示有符号的因子名义、月 RF 抵押和独立账本对账。该研究读取本地公开归档并复用月频覆盖规则，运行入口和数据校验见研究目录。

[离线因子研究接口](../docs/factor-research.md)说明 PIT 定义和截面、带费前费后明细的评估、按版本的研究状态，以及月频动量 `research_only` 登记的只读入口。
