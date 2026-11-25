# Python 研究与分析

用于研究 notebook、数据分析和调用 Nautilus Python API，不负责重新实现 PyO3 绑定或交易核心。未来 notebook 需说明环境版本、输入 catalog 区间、参数、随机种子、成本和结果复现方式；可复用研究脚本需独立 issue 和必要验证。

数据通过配置读取 Parquet catalog。原始数据、大结果、凭据和 notebook 的敏感输出不进 git。TBD 的 notebook 命名与结果保留期由首次提交 notebook 的 issue 确定。当前 fertile_finance/backtest_sample.py 提供上游示例的批任务摘要包装，入口与来源规则见 [API](../docs/api.md)。

finance-backtest-worker 使用已锁定源码环境，通过独立 Python 子进程调度上游示例和具名历史策略。接口请求、进程资源限制和恢复流程见 [回测作业](../docs/backtest-jobs.md)。

## 研究作业

源码构建的 Python 环境包含研究日线导入器和均线策略。配置、运行、费用与覆盖见 [历史回测](../docs/historical-backtest.md)。运行时测试同时检查原生 Nautilus 对负时间戳的异常及项目侧 epoch 边界。

## 研究环境

只需要本地回测时，可以运行 `bash scripts/bootstrap-research.sh` 准备 Rust、uv、Python、LLVM 和编译依赖。此入口跳过 PostgreSQL、Node.js 和 pnpm。环境文件、安装权限和无写入预览见 [精简研究工具链](../docs/build.md#精简研究工具链)。

在仓库根目录加载所选工具链环境并运行 `bash scripts/build.sh`。随后把 `research` 依赖组同步到同一个回测 venv。该组包含 SciPy 和 statsmodels，`--inexact` 保留已安装的 Nautilus 源码 wheel。

```bash
build_root=${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}
UV_CACHE_DIR="$build_root/uv-cache" UV_PROJECT_ENVIRONMENT="$build_root/venv" \
  uv sync --locked --only-group research --inexact --python "$(cat .python-version)"
FERTILE_DATA_ROOT="$(mktemp -d)" PYTHONPATH=python \
  "$build_root/venv/bin/python" -m unittest discover -s tests -v
```

## Ken French 日因子相关

`fertile_finance.factor_analysis.analyze_factors(returns, factors, *, as_of=None, regression=False)` 接收一个日收益 `pandas.Series` 和一张日因子 `pandas.DataFrame`。两边的索引都使用唯一的交易日标签，支持 ISO 日期字符串、`datetime.date` 或无时区的午夜时间戳。因子表至少包含 `Mkt-RF`、`SMB`、`HML`，存在 `Mom` 时一起计算。收益用小数表示，例如 1% 写作 `0.01`。相关与回归直接使用传入收益，如需以超额收益为因变量，请先按已对齐的 RF 计算后传入。

按日期取完整交集，要求同日策略收益和所选因子均有值。缺日或缺值会缩小所有统计量的共同样本，计算不前填。结果提供 `sample_start`、`sample_end`、`observations` 和 `dropped_observations`，后者统计在策略收益索引首尾之间、至少一侧有该日标签但未入样的日期。两边同时缺少标签的日期无法从输入识别。空样本的首尾为 `None`。样本不足两点或任一变量为常数时，对应 Pearson 系数为 `None`。

历史分析传入 `as_of`，两侧只使用截至该日（含）的观测。因子表以观测日为索引。若用结果驱动历史决策，调用方还须按当时实际可用的发布日期和数据版本准备输入。

`regression=True` 用同一交集做含截距的 OLS，返回日截距、各因子系数和 R²。回归需要样本数大于参数数、因变量有变动且设计矩阵满秩。结果用 `regression_status` 披露 `ok`、`insufficient_observations`、`constant_returns` 或 `rank_deficient`。默认只计算 Pearson 系数。

```python
import pandas as pd
from fertile_finance.factor_analysis import analyze_factors

factors = pd.read_csv("tests/fixtures/french_daily.csv", index_col="date")
returns = 2 * factors["Mkt-RF"]
result = analyze_factors(returns, factors, as_of="2024-01-10", regression=True)
print(result.sample_start, result.sample_end, result.observations, result.correlations)
```

## 原生成交审计

研究模拟可收集 Nautilus 的 `OrderFilled` 回调，并把同一次运行的事件交给 `fertile_finance.fill_audit.merge_order_fills(events)`。组件按账户、客户端订单 ID 和 `fill_ts`（事件 UTC 纳秒）归并，原始分笔保存在 `raw_fills`。同一订单跨时间的分笔各保留一笔逻辑成交。重复 trade ID（同账户同订单）或订单标的、方向、币种不一致时抛出 `ValueError`。

每笔逻辑成交累加 `quantity`、`notional`（各分笔价格乘数量的和）和 `commission`。展示价格 `average_price` 为成交量加权平均价，使用 Decimal 的 28 位有效数字，保留小于最小价格跳动的部分。统计换手金额时使用 `notional`。在成交价与佣金同币种的现金股票模拟中，可用 `cash_delta` 和 `position_delta` 对账。

[原生回归 fixture](../tests/test_fill_audit.py) 用相同的两根日线和一笔 10 股市价买单对比两档成交量。Nautilus 1.231.0 的 L1 日线模拟按标的最小数量生成零量 bar 的合成档位，并按偏移一跳的价格撮合耗尽第一档后的市价单余量。fixture 中零量产生同一时刻的 1 股 @ 100.50 和 9 股 @ 100.51，归并后为 10 股、成交金额 1005.09 USD。成交量 100 时产生一笔 10 股 @ 100.50，成交金额 1005.00 USD。两组各计一笔逻辑成交，并核对引擎的现金和持仓。官方历史作业继续使用 [零量意图不提交及一单一笔足额成交](../docs/historical-backtest.md#信号执行和绩效) 的合同。
