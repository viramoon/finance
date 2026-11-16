# Python 研究与分析

用于研究 notebook、数据分析和调用 Nautilus Python API，不负责重新实现 PyO3 绑定或交易核心。未来 notebook 需说明环境版本、输入 catalog 区间、参数、随机种子、成本和结果复现方式；可复用研究脚本需独立 issue 和必要验证。

数据通过配置读取 Parquet catalog；原始数据、大结果、凭据和 notebook 的敏感输出不进 git。TBD：研究环境锁定、notebook 命名、样例数据与结果保留期，由首个研究 issue 确定。当前 fertile_finance/backtest_sample.py 提供上游示例的批任务摘要包装，不实现策略或研究 notebook；入口与来源规则见 [API](../docs/api.md)。
