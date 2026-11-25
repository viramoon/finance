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
