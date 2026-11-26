# Hyperliquid F5 合成回放报文

所有值由开发者按公共消息结构手写，无随机种子。使用固定的合成时钟 `1700000000000` 毫秒和虚构价量。第一组包括三个标的各自的盘口、成交和永续上下文。第二组模拟断线后收到的旧快照、替代快照、重发成交、不同时间复用的成交 ID，以及缺省方向。BTC 成交 ID 刻意选在 JavaScript 安全整数上界之外。钱包字段使用占位字符串，用于检验 F1/WAL 只保留公共成交所需字段。测试根据订阅请求在内存构造确认帧，并复用首张 BTC 快照字节模拟同时间戳重发。HTTPS 重放 `meta.json`。

从仓库根运行 `python3 -B scripts/generate_synthetic_fixtures.py /tmp/finance-synthetic-fixtures`，可在输出目录的 `tests/fixtures/hyperliquid/` 重建三份报文。种子是生成器内的固定常量，来源与摘要见 [sources.json](../../../.data-guard/sources.json)。
