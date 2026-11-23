# Python 研究与分析

用于研究 notebook、数据分析和调用 Nautilus Python API，不负责重新实现 PyO3 绑定或交易核心。未来 notebook 需说明环境版本、输入 catalog 区间、参数、随机种子、成本和结果复现方式；可复用研究脚本需独立 issue 和必要验证。

数据通过配置读取 Parquet catalog；原始数据、大结果、凭据和 notebook 的敏感输出不进 git。TBD：研究环境锁定、notebook 命名、样例数据与结果保留期，由首个研究 issue 确定。当前 fertile_finance/backtest_sample.py 提供上游示例的批任务摘要包装，不实现策略或研究 notebook；入口与来源规则见 [API](../docs/api.md)。

finance-backtest-worker 使用已锁定源码环境，通过独立 Python 子进程调度上游示例和具名历史策略。接口请求、进程资源限制和恢复流程见 [回测作业](../docs/backtest-jobs.md)。

## 研究作业

源码构建的 Python 环境包含研究日线导入器和均线策略。配置、运行、费用与覆盖见 [历史回测](../docs/historical-backtest.md)。运行时测试同时检查原生 Nautilus 对负时间戳的异常及项目侧 epoch 边界。
