# Hyperliquid 公共永续行情

F5 使用主网公共 HTTPS `POST /info` 的 `meta` 请求读取标的精度，并在同一条公共 WebSocket 上订阅所选标的的 `l2Book`、`trades` 和 `activeAssetCtx`。传输层复用源码构建的 Nautilus WebSocket 客户端。请求只带公共频道名和币种。运行时无需账户、钱包、签名或 API key。

| coin | F1 instrument_id |
| --- | --- |
| BTC | `BTC-USD-PERP.HYPERLIQUID` |
| ETH | `ETH-USD-PERP.HYPERLIQUID` |
| SOL | `SOL-USD-PERP.HYPERLIQUID` |

合约以 USD 报价、USDC 现金结算，按基币数量计，乘数为 1，日历为 `24X7`。数量步进为 `10^-szDecimals`。F1 的固定 `tick_size` 记录盘口与成交价格可表达的最细小数网格 `10^-(6-szDecimals)`。交易所五位有效数字规则会使实际档距随价位变粗。元数据缺失、重复、已下架或精度无效时启动失败。

## 事件与许可

产品固定为 `hyperliquid/perp-public`。书写事件使用配置中的许可标签，F3 验证 source/product/license 后处理记账、展示和封段。示例配置只开启 F3 的短时内存展示。部署方按审核结果配置 `record_local`、`derive_non_display` 和 `backup_personal_drive`，分别授权落盘、非展示计算和私人 Drive 备份。F5 无历史下载，产品能力的 `can_backfill` 为 `false`。

| WS 频道 | F1 channel | 事件 |
| --- | --- | --- |
| `l2Book` | `book` | v1.0 `snapshot` 完整替换两侧，每侧最多 20 档。空侧清除旧侧，早于已知簿时间的快照丢弃。 |
| `trades` | `trades` | v1.0 `trade`。`B` 为主动买，`A` 为主动卖，其余字符串、缺失或 null 保留为 `unknown`。只提取币种、毫秒时钟、tid、价格、数量和方向。 |
| `activeAssetCtx` | `perpetual_context` | v2.0 `perpetual_context`。`funding_rate` 是源提供的资金费率比例，`open_interest` 是基币数量，`mark_price` 是 USD 标记价格。保留源十进制字符串的精度和尾随零。 |

盘口和成交使用交易所毫秒转换为 UTC 纳秒。永续上下文没有交易所时钟，`exchange_ts` 为 null。三类事件的 `first_available_ts` 取本次接收时钟。标记价格由交易所计算，可比交易网格更细。

成交 `native_id` 使用 `coin:time_ms:tid`。每币最多保留 32768 个已见标识，按接收时钟保留 120 秒。仅展示时为 5 秒。写者重启从最近 65536 条已确认 WAL 事件恢复去重窗口及可见的最后时钟。窗口与条数界外的覆盖无去重保证。CVD 消费者对买量减卖量，另保留未知方向量；总量包含三类。F5 发布原始规范化成交，由有许可的下游计算累计量。

## 断线与恢复

连接退出后发布 v1.0 `reset`，清空各流的旧状态，进程进行 1 到 30 秒有上限的指数退避，并在下次连接重发全部订阅。20 秒发送一次公共心跳，连接空闲超时为 45 秒。订阅确认超时、公共订阅被拒绝与接收队列溢出都结束本会话。

各流等待首条有效新事件。若存在间隙，先发布 v1.0 `gap`，其 `reason` 为 `continuity_unknown`（连续性未知）。区间是自上次已见接收时钟之后至本次恢复接收时钟之前的 UTC 纳秒。F3 的 `disconnected` 质量报告记录待恢复观察，随后用 `history_unrecoverable` 与 `count: null` 描述未知历史覆盖。新快照同时单独产生 `book_restored_from_snapshot` 与 `count: "1"`，用于标记当前簿恢复的时刻。上游没有原生连续序号时，本地 epoch/seq 只证明适配器的消息次序。持续连接期间也无法据此推断逐笔完整性。

Rust 侧消费 [F3 MarketIngress](../market-wal.md)。集成读者可从 `Hyperliquid::instruments()` 和 `capability()` 取得对应元数据，从同进程的 `ingress().display_events()` 消费获准展示的数据。获准落盘的读者通过 F3 WAL、质量报告和 Parquet 投影接入。F3 的格式、路径与保留规则适用。HTTP 状态与浏览器展示由下游集成。部署与本地检查见 [运行手册](../../RUNBOOK.md#hyperliquid-公共行情)。
