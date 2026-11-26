# 行情采集、历史数据与 Parquet 目录

finance-collector 在数据根的 catalog 下写入 Nautilus v1.231.0 可直接读取的 Bar，finance-api 读取同一数据根的 collector 状态。配置和启动命令见 [运行手册](../RUNBOOK.md)。测试数据源用 config/collector/replay.csv 的十进制 OHLCV 形成合成行情，用于持续演练。replay 状态不可用作真实市场行情。

获准持久化的 F1 多事件流使用独立的 [WAL 与市场事件 Parquet](../docs/market-wal.md)，投影目录位于 `catalog/market-events`。

## 输入与恢复

BarSource 按标的订阅、接收闭合 bar，并支持按时间区间回补。输入标识为完整 Nautilus 标的 ID，如 SPY.ARCA。价格与数量从源到存储均为十进制字符串 / 定点对象，时间为 UTC 纳秒。单个 writer 按标的、bar 规格和 UTC 日期分组。Nautilus v1.231.0 在 data/bar/<完整 bar type> 下扫描时间区间文件，跨日拆文件，文件名使用上游生成的起止时间。

writer 是通过 finance 构建入口安装的 Nautilus Python 包及其原生扩展，直接使用上游 ParquetDataCatalog.write_data。Rust 采集进程复用该持久子进程，每批确认后才更新已落盘进度。这样能使用已锁定版本的高精度 Arrow schema 与文件命名，避免重复链接另一套上游 persistence / DataFusion 依赖。目标 catalog 所在文件系统的暂存区写入完成并 fsync 后，原子 rename 发布文件，再同步父目录并原子更新 finance-state/collector/watermarks.json。每个实时采集存储区同时只允许一个 writer 持有排他锁。启动时按可见 Parquet 文件重建水位，截断 commit 后但 checkpoint 前的失败窗口；每个标的跳过已确认时间戳，批内去重按时间排序。停采后再做 catalog 和 finance-state 的一致备份。

## 本地运行

在 finance 仓库根目录完成 bash scripts/build.sh 与 cargo build --locked -p finance-collector，加载盒子的 env.sh。示例使用默认 127.0.0.1:8082、数据根 /home/box/fertile/data。FINANCE_BUILD_ROOT 指向包含 venv 的编译目录，也可用 FINANCE_PYTHON 显式指定源码 wheel 对应解释器。

collector 从仓库根按脚本路径启动 `python/fertile_finance/collector_store.py`。writer 从自身路径找到项目包，无需设置 `PYTHONPATH`，并和模块调用共用 Unix epoch 校验。

```sh
cargo run --locked -p finance-collector
curl -sS http://127.0.0.1:8082/healthz/live
curl -sS http://127.0.0.1:8082/healthz/ready
curl -sS http://127.0.0.1:8082/status
curl -sS http://127.0.0.1:8081/api/v1/market/status
```

FERTILE_PROFILE 选择 config/collector/cloudbox.yaml 或 stable.yaml。配置包含标的、bar_spec、catalog、写入周期、关闭超时与每次回补区间。相对 catalog 路径从 FERTILE_DATA_ROOT 解析。FINANCE_CONFIG_DIR 可替换 config 目录，FINANCE_COLLECTOR_CONFIG 可指定具体 YAML，FINANCE_COLLECTOR_BIND 可覆盖监听地址。默认每 60 秒回放一次，首个实际时间点持久保存于 finance-state/collector/replay-origin.json。两次启动使用同一 origin、标的、bar_spec、fixture 和间隔时得到同一序列，断线或重启先按水位回补，补齐后转实时。修改输入语义或在回放与真实 IB 之间切换时使用独立数据根、分开备份。catalog 和状态目录持久记录同一来源、bar_spec 和 UUID，拒绝在 replay 水位上改采 IB，或把已绑定的状态目录指向另一 catalog。

ready 逐项检查数据源连接、回补完成、catalog 实际能创建临时文件、每个标的的最后持久 bar 及年龄。任一失败返回 503；live 只检查进程。每秒原子替换 finance-state/collector/status.json，finance-api 的只读 market/status 计算当前年龄，超过 10 秒未更新显示 expired，缺文件为 missing，不能解码为 invalid。正常 SIGTERM/SIGINT 会停止接收、flush 当前缓冲、等待 writer 退出并发布 stopped 状态。SIGKILL 后复用原路径启动，自动从 catalog 复查水位并修复断点。清理演练数据须先停止 collector 并确认该路径为专用临时数据根；不可只删水位或手改 Parquet 文件。

## 复现兼容性及故障测试

```sh
source /home/box/fertile/env.sh
cargo build --locked -p finance-collector
export FINANCE_PYTHON="${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}/venv/bin/python"
PYTHONPATH=python UV_PROJECT_ENVIRONMENT="$(dirname "$(dirname "$FINANCE_PYTHON")")" uv run --no-sync --with pytest==8.4.2 --python "$FINANCE_PYTHON" python -m pytest -q tests/test_collector_store.py
FINANCE_COLLECTOR_BIN="${CARGO_TARGET_DIR:-target}/debug/finance-collector" "$FINANCE_PYTHON" tests/collector_drill.py
```

故障测试启动真实子进程，使用独立临时目录和可加速的有限 CSV 回放。它在非空缓冲时发送 TERM，之后用 KILL 中断和重启，再等数据陈旧，验证 ready 从 200 转 503。最后使用 Python ParquetDataCatalog.bars() 核对每个预期时间戳及高精度价格。跨日分组、排他锁、迟到重复数据和水位替换失败后恢复另由 test_collector_store.py 覆盖。

## 官方回测与时间边界

官方回测导入器把经许可并审核的 NYSE 股票或 ETF 日线写入独立、不可变的 [研究 catalog 快照](../docs/historical-backtest.md)。快照接收经复核无拆股的原始日线，或带血缘的 Yahoo 供应商拆股归一化日线。日线时间戳使用交易所会话收盘，导入拒绝负数纳秒、预 epoch 日线、重复日和非交易会话。1970-01-01T00:00:00Z 与之后的有效纳秒可通过底层时间范围校验。

应用端应在构造 Bar 或调用 wrangler 之前检查有符号时间戳。显式丢弃模式返回条数及最早、最晚被丢弃时间，接口见 [Python 研究与分析](../python/README.md#unix-epoch-边界)。系统从首根被接受的 bar 起计算最长可用行情区间，官方作业从 warmup 后的第一条净值起算绩效。`^GSPC` 始于 1927 年的 98.89 年原始历史不能作为已回测覆盖，SPY 约 33.81 年的数据也不能称作 50 年回报。满窗最多允许起始边界缺五个交易日，窗口内部及末端应完整。详见 [日线窗口](../python/README.md#日线窗口)。

## 公开历史数据

`HistoricalCatalog` 使用 `data_root/catalog/historical`。`data_root` 可由调用方传入，也可用 `FERTILE_DATA_ROOT` 配置，默认是 `/home/box/fertile/data`。Yahoo 按序列 ID 分区，Ken French 使用固定分区 `daily`，FRED 按 series ID 分区。各分区的 `current` 指向当前版本，目录结构如下。

```text
catalog/historical/yahoo/IBM.XNYS/current
catalog/historical/yahoo/GSPC.INDEX/current
catalog/historical/french/daily/current
catalog/historical/fred/DGS10/current
```

每个版本都是独立的 Nautilus catalog。自定义数据的 `data/` 类名目录和 Parquet 文件名由 `ParquetDataCatalog.write_data` 生成。研究 API 在读取同一版本期间持有共享锁。写入按序列持有排他锁，在暂存目录构建并读回验证新版本，再原子切换 `current`。同一 Yahoo 序列的源价、因子和事件随同一个版本发布。后续写入同一序列时会清理过期版本。备份或复制时暂停对应写入，保留 `current` 及其目标目录。

- `append` 在传入区间与当前序列覆盖首尾相交时抛错，含边界重叠。计算原覆盖范围时同时计入主记录和事件日期，因此范围内缺少记录的日期仍计入覆盖。
- `replace` 重写指定的闭区间。区间外原有记录保留，区间内原有的观察值与事件按日期整体替换。传入记录须在指定范围内。省略边界时使用传入主记录和事件共同覆盖的首尾，详细签名见 [Python 研究接口](../python/README.md)。
- Yahoo、Ken French 和 FRED 加载器共用 [Unix epoch 校验](../python/README.md#unix-epoch-边界)，默认 `pre_epoch="reject"`，拒绝早于 1970-01-01 的样本。调用方可显式指定 `pre_epoch="drop"`，查看 `dropped_pre_epoch` 的 `count`、`first`、`last`，分别对应丢弃数、最早和最晚日期。`WriteReport.first` 和 `last` 报告此次实际写入主记录的首尾，区间含两端。没有主记录的输入无法提交。

公开历史数据分区供研究读取，`HistoricalCatalog` 不注册可下单工具。Yahoo 股票和 ETF 使用权益序列 ID，指数 `^GSPC` 对应参考序列 `GSPC.INDEX`。`SPY` 是独立标的，有自己的价格与公司行为。先使用 `convert_research_csv` 从允许的 Yahoo 序列导出 CSV 和血缘，经审核再导入官方 `SmaCrossover` 使用的历史回测快照。官方准入与命令见 [准备数据](../docs/historical-backtest.md#准备数据)。

### 日线时间戳

Yahoo 适配器接收纽约时区的美元日线，并以锁定的 `exchange-calendars==4.13.3` XNYS（NYSE）会话表作为支持序列的收盘约定。时区为 `America/New_York`。`session` 是美国东部当地交易日期，`ts_event` 和 `ts_init` 均为该日会话收盘的 UTC 纳秒。全日市标准时间收盘对应 21:00 UTC，夏令时对应 20:00 UTC。提早收盘日期由 `fertile_finance.research_catalog.nyse().early_closes` 提供，可按年份筛选。例见下表。

| XNYS 会话日期 | 收盘 UTC | 情况 |
| --- | --- | --- |
| 2024-01-02 | 21:00 | 标准时间全日市 |
| 2024-07-01 | 20:00 | 夏令时全日市 |
| 2024-07-03 | 17:00 | 夏令时半日市 |
| 2024-11-29、2024-12-24 | 18:00 | 标准时间半日市 |

分红及拆股事件保留 Yahoo 的源时间戳与对应日期。Ken French 和 FRED 使用观察日期的 UTC 00:00 作为存储索引。`ts_init` 在这些归档中等于 `ts_event`。判断当时是否已发布需要单独的发布时间证据。

### 存储 schema 与价格口径

源日线、公司行为和宏观因子使用 Nautilus 自定义数据。时间戳列 `ts_event`、`ts_init` 为非负 `int64` UTC 纳秒，日期列为 ISO `YYYY-MM-DD` 字符串，金额、价格、成交量、收益与因子使用精确十进制字符串，Python 加载与读取接口返回 `Decimal`。拆股比例两端和源事件秒数用整数保存。允许缺失的字段以空值保存。每版 `dataset.json` 记录 schema 版本、来源、序列 ID 和口径元数据。以下业务字段随 Nautilus 的 `type`、`ts_event`、`ts_init` 通用列保存。

| 数据类型 | 字段与含义 |
| --- | --- |
| `YahooSourceBar`（股票与 ETF）、`YahooIndexBar`（指数参考） | `session`、`open`、`high`、`low`、`close`、`volume`、`adjusted_close`、`adjustment_factor`、`adjustment_as_of` |
| `YahooDividend` | 事件 `session`、`source_timestamp`（Unix 秒）、`amount`（源币种每股分红）、`source_id`（源事件键，可空） |
| `YahooSplit` | 事件 `session`、`source_timestamp`（Unix 秒）、`numerator` / `denominator`（新股数 / 旧股数）、`source_id`（源事件键，可空） |
| `KenFrenchDaily` | `session`、`mkt_rf`、`smb`、`hml`、`rf`、`mom`（日收益率小数）、`available_on`（源文件没有可用日期，返回空值） |
| `FredDailyObservation` | `observation_date`、`value`（源单位）、`realtime_start`、`realtime_end`（若源文件提供） |

Yahoo 默认读取 chart `indicators.quote` 的 OHLCV，保留 `events.dividends` 与 `events.splits`，并保存 chart `indicators.adjclose` 的调整收盘价。源价的 `price_basis` 为 `yahoo_chart_quote`，表示适配器施加复权因子前的 Yahoo 值。Yahoo 源价本身可能已经过供应商的拆股归一化。若有有效的 `close` 和 `adjclose`，复权因子为 `adjclose / close`，使用 34 位十进制有效数字及 `ROUND_HALF_EVEN`。`DailyBar.adjusted()` 将因子乘到源 OHLC，派生记录的 `price_basis` 为 `yahoo_chart_adjusted_close_ratio`。`volume` 在两种视图中都保留源值，适配器不按拆股比率缩放成交量。

官方研究转换独立于上述归档口径。无拆股原始模式在 `0.00002 USD` 容差内消除 OHLC 美分浮点残留，记录 `normalized_price_count`。跨拆股模式要求同一 `adjustment_as_of` 且经审核的供应商归一化 OHLCV，沿用供应商股份单位，保留最后一个相关拆股日前的精确报价及其亚美分字段计数。归一化研究快照的 `yahoo-lineage.json` 保存源路径、存档截止日、拆股事件、审核依据和输出 CSV 哈希。转换与准入详见 [研究回测手册](../docs/historical-backtest.md#准备数据)。

- OHLCV 全为 `null` 时，向 catalog 写入 XNYS 会话日收盘时点的缺失记录。`DailyBar.is_missing_quote` 为真，`WriteReport.missing_quote_rows` 计入这类记录，`observations` 和实际写入区间也包含它们。非会话日的 OHLCV 和 `adjclose` 全为空时跳过，并计入 `WriteReport.skipped_missing_non_sessions`。非会话日若有源值则报错。若跳过后无主记录，写入失败并报告跳过数。
- 单日 `adjclose` 为空或文件缺少整个 `adjclose` 序列时，源 OHLCV 照常入库。`adjusted_close`、`adjustment_factor` 和 `DailyBar.adjusted()` 都返回 `None`。若 OHLCV 全空而会话日有源 `adjclose`，仍保留该源字段，复权因子为空。禁止用相邻数据前填或插值。
- 股票和 ETF 的部分 OHLCV 缺失会报错，确保含有行情的记录具备完整价格与成交量。指数是参考序列，可原样归档早期历史中的部分缺值以及为零的开盘、最高或最低价。

`adjustment_as_of` 是调用方记录的源文件数据截止日期，按行保存。同一序列追加或局部替换后，派生历史可能混合不同截止日期的复权因子。历史复权序列可能包含后来才知道的公司行为，可用于明确标注口径的事后分析。用于当时可得的信号研究须另有可用时点证明。锁定的 Nautilus v1.231.0 回测仓位调整类型只有佣金和资金费，未发现股票现金分红或拆股的一等回测事件。当前归档和读取事件，现金入账、持仓股数调整和权益连续性验证仍需要引擎接入。

### 既有 SPY 固定 21:00 UTC 数据

识别旧数据时先核对来源和价格口径，再以 XNYS 会话表比对多个交易日。若冬令时、夏令时及半日市的日线均标为 21:00 UTC，则符合旧路径的固定时点特征。一条冬令时记录不足以区分两种约定。旧数据的时间和复权口径应保留在原分析记录中。

先停止旧 catalog 写入并备份，记录源文件、标的 ID、日期范围、价格口径和原消费方。将经许可的 Yahoo SPY 日线及公司行为存为覆盖旧交易日的 JSON。下例的 `--source-catalog` 指向含原生 Nautilus 日线 Bar 的目录。默认检查 `SPY.ARCA-1-DAY-LAST-EXTERNAL`，其余 Bar 类型见 `--help`。

```sh
PYTHONPATH=python "$FINANCE_PYTHON" -B -m fertile_finance.migrate_spy \
  --source-catalog /path/to/legacy-nautilus-catalog \
  --target-data-root /path/to/new-data-root \
  --yahoo-json /path/to/archive/yahoo-SPY.json --adjustment-as-of YYYY-MM-DD \
  --dry-run
```

`--dry-run` 只读并输出检查结果。旧数据缺少可区分时点的证据或归档无法覆盖所需的源行情时，命令退出非零并说明原因。检查通过后用相同参数去掉 `--dry-run`。命令通过 Yahoo 适配器将源 OHLCV、调整收盘价及公司行为重导到目标根的 `catalog/historical/yahoo/SPY.ARCA`。应选择尚无该序列的独立数据根，并保留旧 catalog。

核对报告中源路径、标的 ID、新旧首尾和条数，以及冬令时、夏令时和半日市抽查日期与 UTC 时点。核对分红、拆股计数和覆盖缺口后，才将研究读取配置切换至新数据根。
