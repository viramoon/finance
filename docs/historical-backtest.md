# 单标的历史回测

历史作业使用与示例相同的 SQLite 队列、worker 超时和崩溃恢复，通过源码构建的 Nautilus BacktestEngine 读取独立的日线 Parquet catalog。策略名为 `SmaCrossover`。运行仅发生在本地模拟账户。

## 准备数据

管理员准备经许可的 NYSE 上市 USD 股票或 ETF 日线，使用 UTF-8 CSV，列顺序为 `date,open,high,low,close,volume`。日期是交易所当地日期，价格是可按一美分表示的原始 OHLC，成交量为整股。标的示例为 `IBM.XNYS`。指数、市值指数的代理、分钟行情和包含拆股的区间需要独立数据和研究方案。

先核对来源和区间内拆股记录。当前导入协议只接收未按现金分红复权且经复核无拆股的区间。现金分红不入账，结果是价格收益。复权、拆股处理和盘中成交模拟仍需单独设计。不要用已有分钟行情、用户的研究目录或来源不明的 CSV 填补缺口。

从已保存的 Yahoo chart JSON 导出源 quote OHLCV，或从显式指定历史数据根中的 Yahoo 序列导出。示例中的请求区间须按数据源调整，可用 `--start` 和 `--end` 显式选择经核实无拆股的闭区间。

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

转换器拒绝所选区间内已记录的拆股并列出日期。输出价格保留源 quote 数值，无法按分表示时退出并报告日期。Yahoo 可能按拆股归一化其源 quote，仍须依据来源和公司行为记录独立确认价格口径。官方导入器要求 `.XNYS` 标的，Yahoo 的 `SPY.ARCA` 仍用于公开历史数据归档。将标准输出的血缘 JSON 与 CSV 一同保存，核对 `adjustment_as_of`、实际首尾、观测数、缺口和 CSV 的 SHA-256。审核后导入所选研究根。

```sh
"$FINANCE_PYTHON" -m fertile_finance.research_catalog /path/to/ibm.csv \
  --instrument IBM.XNYS --asset-type stock --kind historical \
  --source "Yahoo Finance chart IBM" --raw-split-free-reviewed
```

合成数据用 `--kind fixture`。清单记录输入 CSV SHA-256、标的类别、数据来源和覆盖首尾。API 和 worker 用相同的 `FERTILE_RESEARCH_ROOT`。导入器在同一数据根内先写临时 Parquet，再发布完整标的目录。每个标的的 catalog 是不可覆盖的快照。需要更新时，先停止接单且等作业结束，在独立数据根准备新快照并备份旧快照，再由运维共同切换 API 和 worker 的配置。程序拒绝 catalog 内重复日期、非交易日、无有效数据，或请求区间内任何缺失的交易日。

官方作业不使用 FRED，无需配置 FRED 序列 ID、`cosd` 或 `coed`。按 2024 年示例区间导入的 `DGS10` 共 261 条，供公开历史归档和研究读取，其覆盖与四窗口验收无关。验收也无需 Ken French 日频归档。本次 French 文件截止日为 2026-09-30。归档流程见 [可选扩展 FRED 归档以对齐 SPY 样本](../RUNBOOK.md#可选扩展-fred-归档以对齐-spy-样本)。

## 信号、执行和绩效

- NYSE 日历使用锁定版本的 exchange-calendars。Bar 事件时间为官方交易日收盘，正常冬令时为 21:00 UTC、美东夏令时为 20:00 UTC，半日市依日历提早。交易日按日历验证，包括缺失和休市。日线区间从 Unix epoch 起，拒绝负数纳秒和早于 1970-01-01 的日期。导入器在调用 Nautilus 构造器之前验证。
- 请求起止日期包含边界。快慢均线使用截至当次收盘的窗口。快线大于慢线则目标为配置的整股数，否则为空仓。先加载慢线天数作 warmup，最后一个 warmup 收盘为绩效净值基线，随后才有机会成交。
- 信号最早在下一交易日收盘执行。研究引擎先消化上一日意图，再使用本日收盘更新信号。成交模拟使用当日收盘加减 5 bp，滑点金额向上对齐到一美分，单价至少一美分。每个完整成交收 1.00 USD。初始现金 100000.00 USD，无杠杆、无借钱和做空。收盘日成交量是模拟容量上限，真实收盘盘口和可成交性需要更高分辨率数据。
- 零量日保留在输入和审计中。零量、量不足或现金不足的意图不会提交给模拟撮合。单个订单只允许一次足额成交，并核对原生订单、时间、数量、价格、佣金及成交标识。末日持仓按收盘标记，末日的新信号不强制成交。
- 汇总返回净损益、净值变动率和最大回撤。满窗计算 CAGR、Sharpe、波动、最大回撤和换手。官方 HTTP 作业四窗口的 Sharpe 使用 `rf=0`，无需读取 Ken French 日 RF。会话换手按实际成交名义金额除以前一会话收盘净值计，零成交日为零。组合和分红再投资仍需独立研究方案。
- 相对实际最后一个绩效日分别回看 50、10、5 个日历年，并独立报告 `longest_sample`。每行包含请求起点、实际首尾、跨度、观测数、应有和缺失交易日及缺失比例。初始边界最多缺五个交易日，内部及末端须完整且至少两条观测才标记 `full`。请求起点早于 epoch 时保留原始日期，从 `calendar_start=1970-01-01` 统计交易日并将整窗标记不足。覆盖不足时 `metrics` 为 `null`。详见 [日线窗口](../python/README.md#日线窗口)。

从独立数据根和研究根通过 HTTP 验收四窗口，见 [隔离验收官方历史作业](../RUNBOOK.md#隔离验收官方历史作业)。接口、鉴权与超时确认见 [API](api.md) 和 [作业手册](backtest-jobs.md)。
