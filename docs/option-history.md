# 历史期权链

`finance_collector::option_history` 是本地 Rust 库。调用者提供候选合约集，查询返回各合约的闭合价量 bar 或可得性状态。调用在已有进程内完成，价量结果返回调用方。节流状态使用既有历史任务目录。

## IBKR 未到期期权

在获准读取的环境中，由调用者通过 `ConnectionManager::open_market()` 打开已验证的只读 `PaperMarket` 会话，将 `IbkrOptionHistory::new(&market, data_root)` 传给 `fetch_ibkr_chain`。读取完成后调用 `market.close().await` 关闭专用会话。该会话使用 probe client ID，须与占用该 ID 的权限探针和 F11 批任务错开。

每个 `ConfirmedOption` 包含 F1 `InstrumentId`、正整数 IBKR `con_id` 及权威合约元数据提供的 UTC 到期时刻 `expires_at`。调用者提供返回 UTC Unix 纳秒的请求时钟 `now`。对于尚未到期的合约，适配器先在同一会话通过 F9a `ContractQuery::Option` 确认合约，再经 `PaperPeer::historical_data` 读取。F9a 提供的美东到期日期另作校验，精确截止时刻取自 `expires_at`。

- `BarWindow` 用 UTC Unix 纳秒表示 `[start, end)`，要求 `start < end <= now()`。保留开始时间不早于 `start` 且收盘时间不晚于 `end` 的完整 bar。结果按收盘时间排序，合并完全相同的重复 bar。矛盾的重复值或非法价量使对应合约查询失败。
- `now() >= expires_at` 时返回 `Expired`（不可得），并跳过确认和历史请求，即使查询区间完全早于到期。每页实际请求前在节流等待结束后重查准确时刻，排队期间到期也返回 `Expired`。F9a 返回的到期错误归入同一状态。
- `Available` 含一根或多根有效 bar。`NoBars` 表示成功查询后该区间没有闭合 bar。权限、连接和数据错误作为单合约错误返回，其余合约仍按输入顺序处理。重复合约标识、重复 `con_id` 或非法窗口在发起任何请求前拒绝。
- bar 使用 F1 `ClosedBar`，由 `TradeBar.close_ts` 补充闭合时刻。JSON 序列化保留时间戳和 OHLCV 的十进制字符串。

每页通过 SMART/USD 请求常规交易时段的一分钟 `TRADES`，使用 `1 D` 历史请求并裁剪至该页完整分钟。单页超时取自 paper 会话的读超时配置。F12 与 F11 使用同一绝对路径 `data_root`，共用 `finance-state/historical/ibkr-pacing.*` 的跨进程锁和预约。供应商明确返回无数据时，该页为空。权限失败、断线和不完整的响应作为错误返回。

SDK 将历史价量字段解码为 binary64。适配器立即转换为最短可往返的十进制文本，并拒绝超出 F1 `DecimalText` 表示范围的值。SDK 转换前丢失的精度无法恢复。真实数据留在获准的本地用途和数据根内。[IB 接入](integrations/ib.md) · [数据入仓防护](data-guard.md)

## Databento OPRA 适配器与估算

`DatabentoOpra::default()` 或 `DatabentoOpra::new(None)` 的状态是 `Disabled`。传入 `EstimateConfig` 后状态为 `EstimateOnly`。两种状态均无账户、密钥、网络传输和购买入口，估算器也不读取环境变量。

v2 §D4 于 **2026-11-26** 记录 OPRA 历史按数据量计价，以及 OPRA Standard 的公开挂牌价 **199 USD/月**。估算器无内置按量单价，调用者通过 `EstimateConfig` 提供经核对的 `price_source`、`price_as_of` 和各 `schema` 的预算常量。实际启用 Databento 请求前须由 **vira 确认费用、预算与可用授权**。[Databento OPRA 价格说明](https://databento.com/blog/introducing-new-opra-pricing-plans) · [OPRA.PILLAR 数据集](https://databento.com/datasets/OPRA.PILLAR)

`EstimateRequest` 输入 F1 标的 ID 到预计合约数的映射、开始日期、排他结束日期和一个配置过的 `schema`。每个 schema 由调用方填写 `bytes_per_contract_day` 和 `usd_micros_per_gb`。后者每百万分之一美元为一单位，例如 `1250000` 表示 `1.25 USD/GB`。这是单位换算示例，实际常量来自调用者提供的日期与来源。

1. `估算字节数 = 所有标的合约数之和 × 日历天数 × 每合约每日字节数`。日期区间含起始日，排除结束日，含休市日。合约数和每日数据量在区间内按输入常量估算。
2. `估算美元 = 估算字节数 × 每 GB 微美元费率 ÷ 10^15`，按十进制 GB（10⁹ 字节）计算。返回精确 `Decimal` 和用于复算的日期、量价参数、数据集、schema、合约总数与日历天数。跨接口传递金额时使用十进制字符串。

估算会随查询覆盖和成交活跃度波动。模型只覆盖历史数据量。接入未来的真实供应商报价前需复核费率、schema 可用性、计费口径和用途权限。

## 合成复测

从仓库根运行 `cargo test -p finance-collector --test option_history --locked --offline`。IBKR 合成数据由仓库生成器按固定时间和十进制输入生成，覆盖乱序、重复、窗边界与长小数。到期测试核对已过到期时刻及恰在到期时刻的两份合约都按输入顺序返回 `Expired`，且请求数为零。F9a 替身测试验证确认请求、历史窗口、准确到期时刻、节流数据根以及权限错误的传递。估算测试使用明示的合成费率和日期，覆盖合约数、天数、schema、来源和错误输入。[生成与登记](data-guard.md#合成-fixture-与例外登记)

运行 `cargo test -p finance-ib --lib verified_paper_option_history_requests_and_decodes_synthetic_bars --locked --offline` 可复测生产历史请求路径。F9a 内存传输替身回放 paper 账户校验及期权确认。测试调用实际 `PaperMarket::option_trade_minutes`，核对出站 Protobuf 的合约、时段、周期及 `TRADES` 参数，回送固定合成 bars 和结束标记，验证闭合边界、精确价量以及到期后无新请求。替身只在 SDK 的测试功能下编译。
