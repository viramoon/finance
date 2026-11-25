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
