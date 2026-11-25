# 多标的组合回测

`fertile_finance.portfolio_backtest` 用 Nautilus 已有的多标的能力运行目标权重和横截面组合。一个 `BacktestEngine` 接收全部标的和日线，在 XNYS 的模拟 USD 现金账户中成交。接口供 Python 研究脚本和 notebook 调用，HTTP 作业由 [注册策略](backtest-jobs.md) 提供。

## 输入与再平衡

- `run_portfolio_backtest(*, assets, universe, start, end, config=PortfolioConfig(), targets=None, factors=None, top_n=None, rf=None)` 返回 `PortfolioResult`。起止为包含边界的 `date`，至少覆盖两个 XNYS 交易会话，支持 1970 年起的日线。输入价格、数量、净值和权重使用 Nautilus 精确类型或 `Decimal`。标的 ID 使用完整字符串，例如 `IBM.XNYS`。
- `PortfolioAsset(instrument, bars, raw_split_free_reviewed=True)` 接收至少两个不同的、按美分报价且一股一手的 USD 权益标的。外部 `1-DAY-LAST` 日线必须唯一并在官方收盘时可用，`ts_event` 和 `ts_init` 均等于收盘 UTC 纳秒。调用方需核实原始价格未经复权且区间无拆股，因子也需保持原始的当时可得数据口径。现金分红不入账。
- `UniverseSnapshot(date, available_ts, members)` 是从该交易日开盘起生效的完整成员表，必须在该次开盘前或同时已知。每次成员变化提供新的完整快照，表会持续生效到下一次变更。首个回测会话开盘必须已有生效快照，仅提供期末快照会报错。新成员从生效日起参与排名。退出后撤销该标的的待执行意图。若退出时仍持仓，保留原生账本中的头寸并报告净值缺口。调用方可用较早的目标指示，在退出前最后一个可交易会话完成减仓。
- `TargetWeights(date, available_ts, weights)` 与 `factors` 二选一。目标权重必须在所标会话的收盘前或同时可得，使用该信号日或此前最近的目标表。所有权重之和至多为 `1`，余额为现金。排除成员资格或数据不满足的标的后，剩余权重保持原值。新加入或重新加入的成员需要其当前成员期内的目标表。
- `FactorValue(date, available_ts, instrument_id, value)` 必须在所标会话的收盘前或同时可得。对当次合格标的按因子值从高到低排序，同值按完整标的 ID 排序，最多取 `top_n` 个等权持有。因子计算和数据的发布日期由调用方提供。
- `PortfolioConfig` 默认用 `initial_cash=Decimal("100000.00")`、`rebalance_every=1` 和 `weight_drift_threshold=Decimal("0")`。再平衡从第一个交易会话开始，每隔配置的会话数生成信号。每个标的只在当前权重与目标权重的绝对差超过阈值时调整。`Decimal("0.05")` 表示差值须大于 5 个百分点。漂移判断与股数换算使用精确分数，最后向下取整股。横截面等权在计算中为精确的 `1 / 入选标的数`，循环小数仅在 `rebalances[].targets` 审计中以 `Decimal` 向下舍入展示。

## 时点与数据缺口

每次在官方收盘生成信号，含当日有效收盘价和在该时点已知的宇宙、权重、因子。`PortfolioConfig.fill_timing` 可选 `next_open`（默认）和 `next_close`，分别按下一 XNYS 会话的开盘价和收盘价模拟成交。周末、休市、夏令时和提前收盘按 XNYS 日历处理。末日信号没有后续执行会话。

开盘模型将有实际成交的日线开盘价视作开盘时可得的成交价，零成交量日没有可用成交价。开盘订单的大小由信号和开盘时的可用现金决定。模型在同一执行时点先卖后买，组内按标的 ID 排序。若执行时现金不足，买入缩减为可负担的整股。模拟价直接来自日线，假设按该价可足额成交，佣金和滑点均为零。

初始现金必须能由 Nautilus USD `Money` 精确表示到分。每笔模拟买卖在提交原生订单前，以执行价乘整股数量计算精确金额，并逐分核对 Nautilus 的名义金额及其现金金额转换，同时检查预计余额可由原生账户表示。若原生计算有分差或金额超界，整次回测以 `ResearchError(code="native_cash_precision")` 结束，错误包含标的、日期及相关金额。成功回测仍逐分核对期末现金。

默认遇到必要的信号价格或因子缺失时排除该标的，并通过 `gaps` 报告。持仓缺少可用收盘标记，或整日只有现金且所有当期成员均无有效日线时，该会话的净值为 `None`，当次跳过按权重下单。意图仅对指定的下一会话有效。缺少当次会话的成交价格、成交量为零或执行日成员已退出时，丢弃当次意图。

显式设置 `PortfolioConfig(fill_missing=True)` 可在同一个连续成员期内用更早的已知因子或回测区间内的收盘价补充信号，收盘价也可用于持仓估值。`PortfolioResult.fill_missing` 和 `DataGap.filled_from` 披露启用状态及所用日期。零成交量标的仍从当期信号选股排除。执行价始终要求当次会话的真实输入。

`PortfolioResult.equity` 按交易日给出净值，`positions` 是期末仍持有的原生头寸。`rebalances` 记录每个计划再平衡日和当时解析出的目标。`fills` 记录实际原生成交的标的、买卖方向、`Decimal` 数量和价格、订单与成交 ID、`signal_date`、`fill_date` 以及 UTC 纳秒 `signal_ts`、`fill_ts`。每笔逻辑成交均核对原生成交，且满足 `signal_ts < fill_ts`。

## 窗口与无风险利率

`windows` 复用现有 50 年、10 年、5 年的 [覆盖规则](historical-backtest.md#信号执行和绩效) 并报告实际首尾与观测数。有效观测要求当日至少一个当期成员有原始且成交量大于零的日线，且所有持仓可按当日价格估值。借用历史收盘价估值的会话仍计入 `missing_sessions`，补全后的净值保留在 `PortfolioResult.equity` 中。固定窗口以回测末交易会话为终点，内部或末端存在行情缺口时覆盖为 `insufficient`、`metrics` 为 `None`。`longest_sample` 从首个有效观测算到同一终点，至少两次有效观测且期间连续时计算指标。窗口换手 `turnover` 是窗口起始净值之后各笔买卖名义金额除以各自信号日组合净值的累计值，单位为倍数。

`risk_free` 在结果和窗口中披露来源与频率。默认来源为 `rf=0`。外部加载器可构造 `fertile_finance.research_metrics.RiskFreeSeries(source, basis, rates)`，其中 `basis` 取 `daily` 或 `annual`，`rates` 是 `RiskFreeRate(observation_date, available_ts, rate)` 序列。利率为 `Decimal` 小数，`available_ts` 为发布或修订可得的 UTC 纳秒。`annual` 表示有效年利率，以 252 个交易日折为日利率。

对每段收盘到收盘收益，以区间**起始收盘**为截止时点，先找最新已发布的观测日期，再取该日期届时已知的最新修订。较晚发布或修订的数据仅影响随后适用的区间。当显式 RF 序列在某区间尚不可得，或窗口内的超额收益不足两段或波动为零时，该窗口的 `sharpe` 为 `None`。其余窗口按每日超额收益的均值除以样本标准差再乘 `sqrt(252)` 计算。`excess_returns(equity, rf=None)` 和 `sharpe_ratio(returns)` 可在其它研究报表中复用，`equity` 接受逐会话的 `(date, close_ts, Decimal | None)`。

## 本地 fixture

从仓库根目录设置 `PYTHONPATH=python`，在源码构建的 Python 环境运行下例。所有价格和利率均由示例在内存中生成。

```python
from datetime import date
from decimal import Decimal

from nautilus_trader.model.data import Bar
from nautilus_trader.model.objects import Price, Quantity

from fertile_finance.portfolio_backtest import (
    PortfolioAsset, PortfolioConfig, TargetWeights, UniverseSnapshot, run_portfolio_backtest,
)
from fertile_finance.research_catalog import bar_type, cash_equity, nyse, sessions
from fertile_finance.research_metrics import RiskFreeRate, RiskFreeSeries

start, end = date(2024, 11, 27), date(2024, 12, 3)
closes = sessions(start, end)
first_open = int(nyse().schedule.loc[str(start), "open"].value)
ids = ("ALPHA.XNYS", "BETA.XNYS")

def asset(symbol, prices):
    bars = []
    for when, value in zip(closes.values(), prices, strict=True):
        price = Price.from_str(value)
        bars.append(Bar(bar_type(symbol), price, price, price, price,
                        Quantity.from_int(100_000), when, when))
    return PortfolioAsset(cash_equity(symbol), bars, raw_split_free_reviewed=True)

result = run_portfolio_backtest(
    assets=(asset(ids[0], ["100.00", "104.00", "102.00", "107.00"]),
            asset(ids[1], ["110.00", "109.00", "113.00", "111.00"])),
    universe=(UniverseSnapshot(start, first_open, ids),),
    targets=(TargetWeights(start, closes[start], dict.fromkeys(ids, Decimal("0.45"))),),
    start=start, end=end, config=PortfolioConfig(fill_timing="next_open"),
    rf=RiskFreeSeries("deterministic fixture", "daily", (
        RiskFreeRate(start, closes[start], Decimal("0.0001")),
    )),
)
print(result.total_orders, result.positions)
print([(row["requested_years"], row["coverage"]) for row in result.windows])
print(result.risk_free, result.longest_sample["metrics"])
```
