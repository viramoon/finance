# 单标的历史回测

历史作业使用与示例相同的 SQLite 队列、worker 超时和崩溃恢复，通过源码构建的 Nautilus BacktestEngine 读取独立的日线 Parquet catalog。策略名为 `SmaCrossover`。运行仅发生在本地模拟账户。

## 准备数据

管理员准备经许可的 NYSE 上市 USD 股票或 ETF 日线，使用 UTF-8 CSV，列顺序为 `date,open,high,low,close,volume`。日期是交易所当地日期，成交量为整股。现金分红不入账，两种价格口径都报告价格收益。指数、分钟行情和现金分红再投资仍需独立研究方案。不要用其他行情、调整收盘价或人工数据填补缺口。

### IBM 2024 原始日线

先核对 Yahoo 存档和公司行为记录，确认整个 `2024-01-02` 至 `2024-12-31` 区间无拆股，且更晚的公司行为也未改变该区间的原始价格与成交量口径。用已保存的 chart JSON 或显式指定的 `HistoricalCatalog` 导出 `indicators.quote`。默认转换命令遇到区间内的拆股会列出日期并退出。导入时 `--raw-split-free-reviewed` 是对原始 OHLCV 及无拆股的确认。

```sh
export PYTHONPATH=python
export FINANCE_PYTHON=/path/to/source-built/venv/bin/python
export FERTILE_RESEARCH_ROOT=/path/to/isolated/research

"$FINANCE_PYTHON" -m fertile_finance.convert_research_csv \
  --yahoo-json /path/to/archive/yahoo-IBM.json --adjustment-as-of 2024-12-31 \
  --start 2024-01-02 --end 2024-12-31 --output /path/to/ibm.csv \
  > /path/to/ibm.lineage.json

# 已归档到 HistoricalCatalog 时可改用这个输入
"$FINANCE_PYTHON" -m fertile_finance.convert_research_csv \
  --historical-data-root /path/to/history-data --series-id IBM.XNYS \
  --start 2024-01-02 --end 2024-12-31 --output /path/to/ibm-from-catalog.csv \
  > /path/to/ibm-from-catalog.lineage.json
```

原始价格用 `Decimal` 按 `ROUND_HALF_EVEN` 取最近美分。与该美分的绝对差小于或等于 `0.00002 USD` 时接受，否则退出并报告日期与 OHLC 字段。例如 `162.8300018310547` 输出为 `162.83`。血缘的 `output_price_basis` 为 `cent_normalized_yahoo_chart_quote`，`cent_tolerance_usd` 记录容差。`normalized_price_count` 统计数值改变的 OHLC 字段数，单日最多计四次。保留血缘 JSON 和 CSV，核对 `adjustment_as_of`、实际首尾、观测数、缺口和 CSV 的 SHA-256。官方导入要求 `.XNYS` 标的，`SPY.ARCA` 可继续用于公开历史归档。

```sh
"$FINANCE_PYTHON" -m fertile_finance.research_catalog /path/to/ibm.csv \
  --instrument IBM.XNYS --asset-type stock --kind historical \
  --source "Yahoo Finance chart IBM" --raw-split-free-reviewed
```

### 跨拆股的 IBM 长历史

当 Yahoo 源 `indicators.quote` 的 OHLC 和整股成交量已由供应商统一到同一拆股后股份口径时，可按供应商归一化价格做连续研究。先用许可存档、供应商价格和成交量口径资料、公司行为记录及拆股日前后有独立依据的样本核实这一点，核对从请求起点到存档截止日的事件记录。证据未能确认统一口径时，应保留此次验收为未验证。转换器要求所选行情使用同一 `adjustment_as_of`，适合使用同一份完整存档写入的历史分区。完整长归档含 1970 年前记录时，按 [历史归档步骤](../RUNBOOK.md#手工保存公开历史数据) 显式使用 `pre_epoch="drop"` 并核对丢弃报告。

归一化 CSV 延续供应商的股份口径。程序保留源成交量，用归档中的拆股事件界定报价精度政策，不再乘除拆股比率或应用 `adjclose`。最后一个相关拆股日前，按原数值保留拆股口径下的报价。构建使用的 Nautilus 高精度定点最多支持 16 位小数，无法精确存储的源值会报告日期和字段并退出。自最后一个拆股日起使用上述美分容差。运维需核对血缘中的 `split_events`，其 `session`、`numerator`、`denominator`、`source_timestamp` 和 `source_id` 记录来源事件，范围从请求起点到存档截止日，包含此范围内晚于请求结束的拆股。`split_review_evidence` 记录审核依据，`output_price_basis` 为 `yahoo_chart_split_normalized`，`preserved_subcent_price_count` 统计按源精度保留的亚美分 OHLC 字段数。

以 IBM 的长存档为例。将 `FINANCE_YAHOO_SPLIT_EVIDENCE` 设为实际审核引用，内容可写供应商说明和经核对的公司行为报告编号。使用脱敏引用，血缘会随研究快照保存。日期按已核实的实际存档覆盖设置。使用新的 CSV 和血缘路径，并将 `FERTILE_RESEARCH_ROOT` 指向独立且尚未导入 IBM 的研究根。

```sh
"$FINANCE_PYTHON" -m fertile_finance.convert_research_csv \
  --historical-data-root /path/to/history-data --series-id IBM.XNYS \
  --start 1975-11-25 --end 2026-11-25 --output /path/to/ibm-long.csv \
  --yahoo-split-normalized-evidence "${FINANCE_YAHOO_SPLIT_EVIDENCE:?set reviewed source references}" \
  > /path/to/ibm-long.lineage.json
```

转换成功后核对 CSV 与血缘，再执行导入。

```sh
"$FINANCE_PYTHON" -m fertile_finance.research_catalog /path/to/ibm-long.csv \
  --instrument IBM.XNYS --asset-type stock --kind historical \
  --source "Yahoo Finance chart IBM" \
  --yahoo-split-normalized-lineage /path/to/ibm-long.lineage.json
```

归一化导入核验 CSV 与血缘一致，并将 `yahoo-lineage.json` 随目录发布。研究清单的 `price_basis` 为 `yahoo_chart_split_normalized`，`corporate_actions` 为 `cash_dividends_excluded_yahoo_split_normalized_reviewed`。这个入口与 `--raw-split-free-reviewed` 互斥。跨拆股的未调整原始 OHLCV 不能作为连续价格导入。运行命令见 [隔离验收官方历史作业](../RUNBOOK.md#隔离验收官方历史作业)。价格口径的选择见 [ADR-0004](adr/ADR-0004-yahoo-split-normalized-research.md)。

### 研究快照和覆盖检查

合成数据用 `--kind fixture`。清单记录输入 CSV SHA-256、标的类别、数据来源和覆盖首尾。API 和 worker 用相同的 `FERTILE_RESEARCH_ROOT`。导入器在同一数据根内先写临时 Parquet，再发布完整标的目录。每个标的的 catalog 是不可覆盖的快照。需要更新时，先停止接单且等作业结束，在独立数据根准备新快照并备份旧快照，再由运维共同切换 API 和 worker 的配置。程序拒绝 catalog 内重复日期、非交易日、无有效数据，或请求区间内任何缺失的交易日。

官方作业不使用 FRED，无需配置 FRED 序列 ID、`cosd` 或 `coed`。按 2024 年示例区间导入的 `DGS10` 共 261 条，供公开历史归档和研究读取，其覆盖与四窗口验收无关。验收也无需 Ken French 日频归档。本次 French 文件截止日为 2026-09-30。归档流程见 [可选扩展 FRED 归档以对齐 SPY 样本](../RUNBOOK.md#可选扩展-fred-归档以对齐-spy-样本)。

## 信号、执行和绩效

- NYSE 日历使用锁定版本的 exchange-calendars。Bar 事件时间为官方交易日收盘，正常冬令时为 21:00 UTC、美东夏令时为 20:00 UTC，半日市依日历提早。交易日按日历验证，包括缺失和休市。日线区间从 Unix epoch 起，拒绝负数纳秒和早于 1970-01-01 的日期。导入器在调用 Nautilus 构造器之前验证。
- 请求起止日期包含边界。快慢均线使用截至当次收盘的窗口。快线大于慢线则目标为配置的整股数，否则为空仓。先加载慢线天数作 warmup，最后一个 warmup 收盘为绩效净值基线，随后才有机会成交。
- 信号最早在下一交易日收盘执行。研究引擎先消化上一日意图，再使用本日收盘更新信号。成交模拟使用当日收盘加减 5 bp，滑点金额向上对齐到一美分，买价再向上、卖价向下对齐到美分，单价至少一美分。归一化研究使用精确收盘价生成信号和标记净值，持仓与成交量上限使用所审核的归一化股份单位。每个完整成交收 1.00 USD。初始现金 100000.00 USD，无杠杆、无借钱和做空。真实收盘盘口和可成交性需要更高分辨率数据。
- 零量日保留在输入和审计中。零量、量不足或现金不足的意图不会提交给模拟撮合。单个订单只允许一次足额成交，并核对原生订单、时间、数量、价格、佣金及成交标识。末日持仓按收盘标记，末日的新信号不强制成交。
- 汇总返回净损益、净值变动率和最大回撤。满窗计算 CAGR、Sharpe、波动、最大回撤和换手。官方 HTTP 作业四窗口的 Sharpe 使用 `rf=0`，无需读取 Ken French 日 RF。会话换手按实际成交名义金额除以前一会话收盘净值计，零成交日为零。组合和分红再投资仍需独立研究方案。
- 相对实际最后一个绩效日分别回看 50、10、5 个日历年，并独立报告 `longest_sample`。每行包含请求起点、实际首尾、跨度、观测数、应有和缺失交易日及缺失比例。初始边界最多缺五个交易日，内部及末端须完整且至少两条观测才标记 `full`。请求起点早于 epoch 时保留原始日期，从 `calendar_start=1970-01-01` 统计交易日并将整窗标记不足。覆盖不足时 `metrics` 为 `null`。详见 [日线窗口](../python/README.md#日线窗口)。

从独立数据根和研究根通过 HTTP 验收四窗口，见 [隔离验收官方历史作业](../RUNBOOK.md#隔离验收官方历史作业)。接口、鉴权与超时确认见 [API](api.md) 和 [作业手册](backtest-jobs.md)。

## 公开因子研究

[时序因子动量](../strategies/2026-11-25-factor-momentum/README.md) 使用 Ken French 月收益和抵押现金进行离线研究。其日频档案只用于核验来源覆盖与标记信号、执行的收盘时点。1970 年以前的月收益单列为非系统补充。
