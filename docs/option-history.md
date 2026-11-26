# 历史期权链

`finance_collector::option_history` 是本地 Rust 库。调用者提供已确认的合约集。查询返回各合约的闭合价量 bar 或可得性状态，供研究和后续读契约集成。运行时无独立进程、端口、环境变量、迁移、落盘或自动回补任务。

## IBKR 未到期期权

调用 `fetch_ibkr_chain(provider, contracts, window, now)`。每个 `ConfirmedOption` 包含 F1 `InstrumentId`、已确认的正整数 IBKR `con_id` 及权威合约元数据提供的 UTC 到期时刻 `expires_at`。调用者提供返回 UTC Unix 纳秒的请求时钟 `now`。到期按实际时刻判断。合约集来自逐合约确认。

- `BarWindow` 用 UTC Unix 纳秒表示 `[start, end)`，要求 `start < end <= now()`。保留开始时间不早于 `start` 且收盘时间不晚于 `end` 的完整 bar。结果按收盘时间排序，合并完全相同的重复 bar。矛盾的重复值或非法价量使对应合约查询失败。
- 每次调用 `PaperOptionHistory::fetch_trade_bars` 前重新检查到期时刻。`now() >= expires_at` 时返回 `Expired`（不可得），并跳过请求，即使查询区间完全早于到期。到期日当日仍以准确时刻为界。
- `Available` 含一根或多根有效 bar。`NoBars` 表示成功查询后该区间没有闭合 bar。权限、连接和数据错误作为单合约错误返回，其余合约仍按输入顺序处理。重复合约标识、重复 `con_id` 或非法窗口在发起任何请求前拒绝。
- bar 使用 F1 `ClosedBar`，由 `TradeBar.close_ts` 补充闭合时刻。JSON 序列化保留时间戳和 OHLCV 的十进制字符串。

F9a 合并后接入 `PaperOptionHistory`。桥接层负责已验证的只读 paper 会话、合约确认、请求节流、分页与超时，并在排队后实际发请求前再次判断到期。请求口径为合约级一分钟 `TRADES`，桥接层须区分供应商确认为无数据的响应和权限等错误。本期以合成回放验证这条调用链，生产会话桥接仍待集成。真实数据留在获准的本地用途和数据根内。[IB 接入](integrations/ib.md) · [数据入仓防护](data-guard.md)

## Databento OPRA 适配器与估算

`DatabentoOpra::default()` 或 `DatabentoOpra::new(None)` 的状态是 `Disabled`。传入 `EstimateConfig` 后状态为 `EstimateOnly`。两种状态均无账户、密钥、网络传输和购买入口，估算器也不读取环境变量。

v2 §D4 于 **2026-11-26** 记录 OPRA 历史按数据量计价，以及 OPRA Standard 的公开挂牌价 **199 USD/月**。估算器无内置按量单价，调用者通过 `EstimateConfig` 提供经核对的 `price_source`、`price_as_of` 和各 `schema` 的预算常量。实际启用 Databento 请求前须由 **vira 确认费用、预算与可用授权**。[Databento OPRA 价格说明](https://databento.com/blog/introducing-new-opra-pricing-plans) · [OPRA.PILLAR 数据集](https://databento.com/datasets/OPRA.PILLAR)

`EstimateRequest` 输入 F1 标的 ID 到预计合约数的映射、开始日期、排他结束日期和一个配置过的 `schema`。每个 schema 由调用方填写 `bytes_per_contract_day` 和 `usd_micros_per_gb`。后者每百万分之一美元为一单位，例如 `1250000` 表示 `1.25 USD/GB`。这是单位换算示例，实际常量来自调用者提供的日期与来源。

1. `估算字节数 = 所有标的合约数之和 × 日历天数 × 每合约每日字节数`。日期区间含起始日，排除结束日，含休市日。合约数和每日数据量在区间内按输入常量估算。
2. `估算美元 = 估算字节数 × 每 GB 微美元费率 ÷ 10^15`，按十进制 GB（10⁹ 字节）计算。返回精确 `Decimal` 和用于复算的日期、量价参数、数据集、schema、合约总数与日历天数。跨接口传递金额时使用十进制字符串。

估算会随查询覆盖和成交活跃度波动。模型只覆盖历史数据量。接入未来的真实供应商报价前需复核费率、schema 可用性、计费口径和用途权限。

## 合成复测

从仓库根运行 `cargo test -p finance-collector --test option_history --locked --offline`。IBKR 合成数据由仓库生成器按固定时间和十进制输入生成，覆盖乱序、重复、窗边界与长小数。估算测试使用明示的合成费率和日期，覆盖合约数、天数、schema、来源和错误输入。[生成与登记](data-guard.md#合成-fixture-与例外登记)
