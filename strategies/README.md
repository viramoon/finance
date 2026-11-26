# 策略

自研策略基于 NautilusTrader Strategy / Actor 扩展接口，可使用 Rust 或 Python；交易、回测、模拟复用 Nautilus 核心。这里管理策略逻辑、参数、精确领域类型和验证约定。

声明式策略配置从 [StrategyBundle v1 格式](../docs/strategy-bundle.md) 开始。可将 [入场退出配方](fixtures/strategy-bundle/v1/entry-exit.yaml) 和 [目标权重配方](fixtures/strategy-bundle/v1/target-weights.yaml) 编译后提交至 [离线回测作业](../docs/backtest-jobs.md#配置策略作业)。

每项研究记录输入数据、运行模式、风险限制、依赖版本及复现方式。已交付的离线研究见 [时序因子动量](2026-11-25-factor-momentum/README.md)，其输入是 Ken French 因子收益，运行模式为非系统月频研究。

月频动量作为 `research_only` 登记，目录项固定引用原结论和独立账本。查看登记、校验和复算规则见 [因子研究接口](../docs/factor-research.md#月频动量)。
