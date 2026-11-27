# Binance 合成报文

两个 JSON 文件按本仓库所固定的 Nautilus Binance 公共报文字段手工构造。无行情录制，无随机种子。UTC 毫秒取 999–2000，价格取约 10 USDT。现货更新 ID 和成交 ID 刻意超过 JavaScript 安全整数上界。永续快照和首条更新的末尾 ID 都是 800，后续更新以 `pu=800` 接续，即使 `U` 跳到 805。断档帧分别在现货遗漏 ID，在永续引用不匹配的 `pu`。测试中的 `exchangeInfo` 和 HTTP 错误也由代码手工生成。

运行以下命令可按固定常量生成完全相同的报文字节。摘要与生成入口登记在 [数据来源清单](../../../../.data-guard/sources.json)。复合断序负例和结算时间缺失负例由测试从这些报文修改字段得到。

```sh
python3 -B scripts/generate_synthetic_fixtures.py /tmp/finance-synthetic-fixtures
```
