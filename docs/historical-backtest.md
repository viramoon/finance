# 单标的历史回测

历史作业使用与示例相同的 SQLite 队列、worker 超时和崩溃恢复，通过源码构建的 Nautilus BacktestEngine 读取独立的日线 Parquet catalog。策略名为 `SmaCrossover`。运行仅发生在本地模拟账户。

## 准备数据

管理员准备经许可的 NYSE 上市 USD 股票或 ETF 日线，使用 UTF-8 CSV，列顺序为 `date,open,high,low,close,volume`。日期是交易所当地日期，价格是可按一美分表示的原始 OHLC，成交量为整股。标的示例为 `IBM.XNYS`。指数、市值指数的代理、分钟行情和包含拆股的区间需要独立数据和研究方案。

先核对来源和区间内拆股记录。当前导入协议只接收未按现金分红复权且经复核无拆股的区间。现金分红不入账，结果是价格收益。复权、拆股处理、盘中成交模拟和批量迁移都有待单独设计。不要用已有分钟行情、用户的研究目录或来源不明的 CSV 填补缺口。

```sh
FERTILE_RESEARCH_ROOT=/home/box/fertile/data/research \
  PYTHONPATH=python /path/to/source-built/venv/bin/python -m fertile_finance.research_catalog \
  /path/to/approved.csv --instrument IBM.XNYS --asset-type stock \
  --kind historical --source "Approved provider IBM" --raw-split-free-reviewed
```

合成数据用 `--kind fixture`。清单记录输入 CSV SHA-256、标的类别、数据来源和覆盖首尾。API 和 worker 用相同的 `FERTILE_RESEARCH_ROOT`。导入器在同一数据根内先写临时 Parquet，再发布完整标的目录。每个标的的 catalog 是不可覆盖的快照。需要更新时，先停止接单且等作业结束，在独立数据根准备新快照并备份旧快照，再由运维共同切换 API 和 worker 的配置。程序拒绝 catalog 内重复日期、非交易日、无有效数据，或请求区间内任何缺失的交易日。

## 信号、执行和绩效

- NYSE 日历使用锁定版本的 exchange-calendars。Bar 事件时间为官方交易日收盘，正常冬令时为 21:00 UTC、美东夏令时为 20:00 UTC，半日市依日历提早。交易日按日历验证，包括缺失和休市。日线区间从 Unix epoch 起，拒绝负数纳秒和早于 1970-01-01 的日期。导入器在调用 Nautilus 构造器之前验证。
- 请求起止日期包含边界。快慢均线使用截至当次收盘的窗口。快线大于慢线则目标为配置的整股数，否则为空仓。先加载慢线天数作 warmup，最后一个 warmup 收盘为绩效净值基线，随后才有机会成交。
- 信号最早在下一交易日收盘执行。研究引擎先消化上一日意图，再使用本日收盘更新信号。成交模拟使用当日收盘加减 5 bp，滑点金额向上对齐到一美分，单价至少一美分。每个完整成交收 1.00 USD。初始现金 100000.00 USD，无杠杆、无借钱和做空。收盘日成交量是模拟容量上限，真实收盘盘口和可成交性需要更高分辨率数据。
- 零量日保留在输入和审计中。零量、量不足或现金不足的意图不会提交给模拟撮合。单个订单只允许一次足额成交，并核对原生订单、时间、数量、价格、佣金及成交标识。末日持仓按收盘标记，末日的新信号不强制成交。
- 汇总返回净损益、净值变动率和最大回撤。风险自由利率披露为零，当前不计算 Sharpe、不下载利率序列，也不进行组合或分红再投资。
- 相对实际最后一个绩效日分别回看 50、10、5 个日历年，并报告各窗口的请求起点、实际首尾、跨度、应有/缺失交易日和比例。仅当初始边界缺失不超过五个交易日、内部及末端没有缺失且有两点以上时标记 `full`。未覆盖的预 epoch 区间明确保留请求起点，日历计数从 `calendar_start` 开始，整窗标记不足。任何不足窗口的绩效指标为空。来源、warmup 与真实绩效区间随结果存储。

接口、鉴权与超时确认见 [API](api.md) 和 [作业手册](backtest-jobs.md)。
