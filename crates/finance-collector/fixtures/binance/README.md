# Binance 合成报文

两个 JSON 文件按本仓库所固定的 Nautilus Binance 公共报文字段手工构造。无行情录制，无随机种子。UTC 毫秒取 999–2000，价格取约 10 USDT。现货更新 ID 和成交 ID 刻意超过 JavaScript 安全整数上界。永续快照和首条更新的末尾 ID 都是 800，后续更新以 `pu=800` 接续，即使 `U` 跳到 805。断档帧分别在现货遗漏 ID，在永续引用不匹配的 `pu`。测试中的 `exchangeInfo` 和 HTTP 错误也由代码手工生成。
