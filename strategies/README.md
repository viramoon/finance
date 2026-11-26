# 策略

自研策略基于 NautilusTrader Strategy / Actor 扩展接口，可使用 Rust 或 Python；交易、回测、模拟复用 Nautilus 核心。这里管理策略逻辑、参数、精确领域类型和验证约定。

每项研究记录输入数据、运行模式、风险限制、依赖版本及复现方式。已交付的离线研究见 [时序因子动量](2026-11-25-factor-momentum/README.md)，其输入是 Ken French 因子收益，运行模式为非系统月频研究。
