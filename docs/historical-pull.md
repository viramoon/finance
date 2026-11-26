# 历史价量批任务

F11 提供按日期补拉闭合 bar 和导入 Binance 公开 Spot 逐笔日归档的命令。仅在获准访问源、保存该产品并使用指定本地数据根的部署中运行。每次命令须给出 `--allow-download`，Binance 还须给出 `--enable-binance`。启用 Binance 之前，命令返回 `disabled_by_profile`，网络和数据根均不会被访问。Binance HTTP 451 会结束本次任务并报告 `region_restricted`。

| 来源 | 默认标的 | 口径 |
| --- | --- | --- |
| `yahoo-daily` | QQQ、SPY、AAPL、MSFT、NVDA、AMZN、GOOGL、META、TSLA | Yahoo chart 源 OHLCV，供应商适用的拆股口径，使用美股现金会话实际收盘时间，包括节假日与提前收盘 |
| `ibkr-minute` | 同上 | IBKR SMART USD `TRADES` 一分钟 bar，常规交易时段（RTH），仅用 paper 只读 Gateway |
| `hyperliquid` | BTC、ETH、SOL | 公共 `candleSnapshot`，USD 永续，`1m` 或 `1d` |
| `binance` | BTC、ETH、SOL | Binance Spot 对 USDT 的公共 K 线，`1m` 或 `1d` |
| `binance-trades` | BTC、ETH、SOL | Binance Spot 对 USDT 的公共 daily `trades` ZIP 与 SHA-256 校验，按 UTC 日导入原生逐笔 |

美股标识用 `SPY.ARCA` 和其余八个 `.XNAS` 标识。加密分别用 `BTC-USD-PERP.HYPERLIQUID` / `BTCUSDT.BINANCE` 等场所标识。各源独立保存。Yahoo 使用 `quote.close` 和同组 OHLCV。源历史后续可能因拆股改变，已落盘行保留首次发布值。需要按同一新口径重建时，使用另一数据根重新从头拉取。

## 命令

在 finance 仓库根执行。先准备 [源码构建的 Python](build.md)，IBKR 分支另运行 `cargo build --release --locked -p finance-ib` 并指定产物。`FERTILE_DATA_ROOT` 或 `--data-root` 必须是明确的绝对路径，且位于 Git 工作树之外。示例中的路径和日期由运维按授权替换。

```sh
export PYTHONPATH=python
export FINANCE_PYTHON=/path/to/source-built/venv/bin/python
export FERTILE_DATA_ROOT=/path/to/authorized/history-data

"$FINANCE_PYTHON" -m fertile_finance.historical_pull yahoo-daily \
  --allow-download --start 2024-11-25 --end 2024-11-29

"$FINANCE_PYTHON" -m fertile_finance.historical_pull ibkr-minute \
  --allow-download --ib-binary target/release/finance-ib --start 2024-11-29 --end 2024-11-29

"$FINANCE_PYTHON" -m fertile_finance.historical_pull hyperliquid \
  --allow-download --interval 1d --symbol BTC --start 2024-11-25 --end 2024-11-29

"$FINANCE_PYTHON" -m fertile_finance.historical_pull binance \
  --allow-download --enable-binance --interval 1m --start 2024-11-29 --end 2024-11-29

"$FINANCE_PYTHON" -m fertile_finance.historical_pull binance-trades \
  --allow-download --enable-binance --start 2024-11-29 --end 2024-11-29
```

`--symbol` 可重复。`--start` 和 `--end` 都包含所给 UTC 日期。对 bars 按闭合时刻选取，日线加密在下一天 00:00 UTC 闭合。逐笔归档按成交发生的 UTC 日期选取。`--interval` 默认 `1m`，只对加密 K 线生效。美股日线只发布已收盘的交易会话，IBKR 分钟线与加密只发布已闭合的分钟或日 bar。

## 进度与断点

- bar 使用 `BarSource` 的七个闭合记录字段和 `collector_store.py` 的 Nautilus 写入、单写者锁、暂存、`fsync`、原子发布与水位重建。Python 有限批次按闭合时间用双闭区间扫描，IBKR adapter 转为 Rust 单次请求所需的左开右闭区间。目录为 `$FERTILE_DATA_ROOT/catalog/historical/<source>/<bar-spec>`，含 `data/bar/<instrument>-<bar-spec>-EXTERNAL/`。`$FERTILE_DATA_ROOT/finance-state/historical/<source>/<bar-spec>` 保存按标的和请求起止时间划分的扫描游标及水位。
- bar 输出逐标的的 `scanned_through_ns`、`watermark_ns` 和缺口列表。每个缺口给出实际缺少的预期 bar 数及首末闭合时刻。美股预期时刻来自锁定的 XNYS 会话日历，按 UTC 24 小时网格检查加密。休市和当前未闭合的区间不产生缺口。上市前、IBKR 留存限制、停牌、零成交以及明确无数据的响应可能留下缺口，输出只证明缺少预期时间戳。Yahoo 已识别的结构化空窗口会记录缺口并继续后续窗口。权限失败或无法识别的响应终止本次任务。
- 同一参数重跑时先处理上次遗留的缺口，再从扫描游标推进。网络失败和写入失败会保留已提交进度。扫描游标在 Parquet 发布后更新，重启会先按实际 Parquet 行查缺。需要核对恢复后的文件、切换查询范围或从起点复核时可加 `--rescan`。既有行保持不变，补入缺口时按原文件区间原子发布，最大水位不回退。
- 逐笔使用 `$FERTILE_DATA_ROOT/catalog/historical/binance-spot-trades/<instrument>/<UTC-date>.parquet`。每个已校验的日文件是一个恢复单位。原生 `trade_id` 和价格、数量、quote quantity 为字符串，`ts_ns` 为 UTC 纳秒，保留 maker 标记并附来源 URL 和 SHA-256 元数据。2025-01-01 起 Spot 文件时间戳按微秒处理，更早的文件按毫秒处理。导入时验证日内原生 ID 严格递增，重跑跳过已提交的 UTC 日期。同一时间戳可以有多笔成交。日归档 404 会列入 `missing_days`，下次运行继续尝试。损坏归档不会发布。

命令在 stdout 输出 JSON。退出 `0` 对应已完成当前闭合窗口、尚无已闭合窗口或 Binance 已禁用。退出 `2` 表示报告中仍有缺口，命令行参数错误也使用 `2` 并在 stderr 给出用法。退出 `1` 表示运行失败。读取 bar 用现有 `ParquetDataCatalog` 接口。批任务对账时核对扫描进度、实际水位、缺口与退出状态。

## 读取 Binance 逐笔日归档

日归档是 F11 独立发布的本地 Parquet。用 PyArrow 读取，以文件元数据的 `instrument_id` 和行中的原生 `trade_id` 标识成交。与 [F3 WAL](market-wal.md) 的接入留给后续项。

将 `archive_path` 设为已导入的某个日文件的绝对路径。下例读取首批成交。

```pycon
>>> import pyarrow.parquet as pq
>>> with pq.ParquetFile(archive_path) as archive:
...     instrument_id = archive.schema_arrow.metadata[b"instrument_id"].decode("ascii")
...     trades = next(archive.iter_batches(batch_size=1024, columns=["trade_id", "ts_ns", "price", "quantity"]))
>>> trades.num_rows > 0
True
```

## 预计耗时与连接约束

| 来源 | 估算方式 |
| --- | --- |
| Yahoo 日线 | 每标的每 31 UTC 日窗口最多一个逻辑请求。九个标的一年最多约 108 个逻辑请求，正常网络通常几分钟。连接和读取超时各按 15 秒 socket 超时处理，429 等暂态错误按 [运行手册](../RUNBOOK.md#手工保存公开历史数据) 的有限重试处理。 |
| IBKR 分钟线 | 每个标的每个现金会话最多一个请求，单次至多 24 小时，完成后至少等 16 秒。九个标的一个会话只算间隔约 2 分钟，约 252 个会话只算间隔约 10 小时，另加认证和 Gateway 响应时间。保留期、权限和历史数据限流会影响进度。 |
| Hyperliquid / Binance K 线 | 单次窗口上限分别为 5,000 / 1,000 根。Hyperliquid 只提供最近约 5,000 根，分钟线约 3 天半，远端保留期以实际返回为准。Binance 三个标的 30 天分钟线约 132 个请求。先试一天，用请求数和实际响应耗时估算。 |
| Binance 逐笔 | 每个标的每个可用 UTC 日获取校验和及 ZIP。下载、解压和 Parquet 时间取决于成交量与带宽，先试最近已归档的一个 UTC 日。 |

IBKR 历史任务使用 [paper 连接保护](integrations/ib.md) 及 probe client ID（默认 91），须与 probe 错开。一个 Gateway 的所有 F11 请求应使用同一配置数据根，以使用同一持久 pacing 锁。与实时 collector 回补等其他历史请求错开，避免共享 Gateway 的请求额度叠加。Rust 直连入口为 `finance-ib historical-minute --allow-download SYMBOL START_UNIX END_UNIX`，以 UTC 秒接收 `(START_UNIX, END_UNIX]` 的闭合时间范围。缺少授权参数时在读取连接配置前退出。成功输出一个 JSON 对象，`precision` 是转换口径，`bars` 是闭合 bar 数组。Linux 下使用单调时钟、启动标识和磁盘预约跨进程限速。单次历史读取限时 45 秒，请求失败后从批任务游标重跑。

真实 Gateway 的集成测试 `opt_in_paper_historical_minute` 默认忽略。仅在获准部署上设置 `IB_HISTORY_PAPER_TEST=1`，在 `IB_HISTORY_START_UNIX`、`IB_HISTORY_END_UNIX` 中填入已闭合且有成交的 SPY 常规交易时段，再运行 `cargo test --locked -p finance-ib --test paper_historical_minute opt_in_paper_historical_minute -- --ignored --exact`。准备已登录的只读 paper Gateway 和已存在的外部 `FERTILE_DATA_ROOT`。

## IBKR 分钟线精度

锁定的 ibapi 3.2.0 公开历史接口以 binary64 提供 OHLC 和成交量。线上 protobuf 的成交量是十进制文本，SDK 的公开历史 bar 无法取得该原文。F11 为九只 USD 美股和 ETF 声明历史存储网格，价格步长 `0.0001` USD，成交量步长 `0.00000001` 股。部署时须确认 Gateway 按股返回美股历史成交量。适配器保留该单位。场所订单最小价位另由标的定义。SDK 值经最短十进制表示进入 Decimal，按最接近网格、正中取偶舍入一次，输出十进制字符串。输入绝对值达到 `2^52` 个声明步长、非有限值或舍入到零的价格会被拒绝。该口径只覆盖 SDK 已交付的值。

批任务核对 Rust 响应中的转换口径，并将其写入历史 catalog 的 `.finance-collector/source.json` 和状态目录的 `catalog.json`，字段为 `source_metadata.decimal_policy`。若曾用旧实现发布 IBKR 分钟线，请换一个配置数据根拉取。旧 catalog 可以独立读取，批任务会拒绝混写未标注或精度策略不同的 catalog。

批任务没有服务端口、定时器和数据库迁移。线上流协议见 [行情协议](market-protocol.md)。
