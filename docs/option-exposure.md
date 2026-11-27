# 期权 GEX、gamma wall、flip 与 0DTE

F9d 提供进程内 Rust 计算接口 `finance_collector::option_exposure::evaluate(&OptionChainSnapshot, DealerScenario, TimingPolicy, Option<&GammaGrid>) -> Result<ExposureReport, ExposureError>`。调用方提供一只标的的快照和估值参数，得到按执行价和到期日汇总的敞口。计算的运行范围为内存，部署时无需专用进程、端口、环境变量或数据目录。

## 快照输入

| 字段 | 约定 |
| --- | --- |
| `underlying_id`、`instrument_id` | 标的与各合约使用 F1 `InstrumentId`。一份快照的合约均属于同一标的，同一合约只出现一次。 |
| `as_of`、`session_date` | 估值时点用 UTC。交易会话日期由调用方按交易日历确定，用于识别 0DTE。 |
| `spot` | 标的美元现价及其 UTC 观察时间。调用方负责确认计价币种与合约口径。 |
| `universe_complete`、`contracts` | `contracts` 是候选合约清单，已确认合约即使缺 OI 或 gamma 也应列入。确认清单覆盖调用方声明的整个范围时，才将 `universe_complete` 置为真。 |
| `right`、`expiry`、`strike`、`multiplier` | 合约方向、到期日、执行价和实际合约乘数。调整合约按实际乘数计算。 |
| `open_interest` | 非负整数张数及独立的 UTC `observed_at`。供应商明确提供 OI 业务日期时填写 `business_date`，否则留空。 |
| `gamma` | 每单位标的价格变化对应的多头 delta 变化，与当前标的价格和乘数使用同一口径，并携带独立的 UTC `observed_at`。 |

价格、数量、乘数和 Greeks 使用 [F1 `DecimalText`](market-protocol.md) 输入。观测值 `0` 表示已收到零值，仍需满足同一行的其他输入条件。缺值保留为空。输入校验覆盖重复合约、晚于估值时点的观察、早于会话日期的到期日和越界数值。

`TimingPolicy` 由调用方设置 `max_spot_age_seconds`、`max_gamma_age_seconds` 和 `max_spot_gamma_skew_seconds`，边界值可用。单独保留 OI 的观察时间和业务日期。日结 OI 可与盘中 gamma 配对，报告保留两者的时差以便判断局部估算的输入口径。

## 金额、投影与 wall

单合约无方向集中度为 `OI × 实际合约乘数 × 单位 gamma × spot² × 0.01`，单位是标的变化 1% 对应的美元 delta 变化。以合成输入 `spot=100`、`OI=3`、`multiplier=25`、`gamma=0.02` 为例，集中度为 150 美元。方向金额再乘以下表中的假设比例。

| 情景 | call、put 比例 | 含义 |
| --- | --- | --- |
| `absolute` | 集中度均按 1 汇总 | 返回单边 OI 的无方向集中度。 |
| `dealer_short` | −1、−1 | 客户净买、做市商净卖的情景。 |
| `dealer_long` | +1、+1 | 做市商净买的反向情景。 |
| `call_long_put_short` | +1、−1 | call 正、put 负的经验约定。 |

报告带情景、模型版本 `gex-v1` 及估值时点。情景比例是模型假设，供解释敞口对假设的敏感度。无方向集中度在各情景间相同，同一执行价的 call 和 put 在有方向敞口中可以相抵。计算使用可检查的定点十进制操作，超出计算精度或 F1 `DecimalText` 表示范围时返回算术错误。金额经 Serde 编码为十进制字符串。

报告的 `all`、`zero_dte`、`later` 分别对应全候选链、到期日恰好等于会话日期、到期日在会话日期之后。三个视图各自按执行价、到期日排序汇总，并各自计算 wall。集中度 wall 取无方向集中度最大的执行价，正 gamma wall 取最大正净值，负 gamma wall 取最负净值。并列选较低执行价，零值档无 wall。绝对情景提供集中度和集中度 wall，方向金额为空。某个投影范围没有有效输入时，金额为空。

## 覆盖率与 OI 日期

- 每个视图报告 `valid_contracts`、`candidate_contracts` 以及 `exclusions`。同一合约可计入多个排除原因。缺字段或超出时序阈值的合约不贡献敞口。候选范围不完整或候选项未全部有效时，`partial` 标记局部估算。候选数仅涵盖快照提供的合约，展示时同时使用 `universe_complete`。
- OI 覆盖分子为 `eligible_open_interest`，即有效合约的 OI 之和。完整候选清单且其中所有 OI 均已观察时，`total_open_interest` 提供分母。未知分母为空，全零分母下比例无定义。
- `observed` 汇总现价及该视图候选合约的输入观察时间范围，另有独立的 OI 与 gamma 范围。`max_oi_gamma_skew` 汇总同一合约两个观察时间之差的最大值。`open_interest_dates` 去重并排序，`unconfirmed_open_interest_dates` 统计已观察 OI 但业务日期未确认的合约，显示时保留观察时间。

## 现货价格网格与 flip

调用方可传入 `GammaGrid`。`as_of` 与链快照的估值时间一致。`spots` 至少包含两个严格递增的正现货价格。每个合约的 `ContractGammaGrid` 以 `instrument_id` 对应候选链，带正的年化 `implied_volatility` 及按 `spots` 顺序排列的非负重估单位 `gammas`。每个合约的 IV 在网格内固定。`model` 记录定价模型名称和版本，IV、`annual_risk_free_rate` 和 `annual_dividend_yield` 统一使用年化小数，例如 `0.05` 表示 5%。计算在每个价格点用该点的 spot 与 gamma 求和，并应用所选有方向情景。使用相同网格可比较不同持仓假设。

`GammaFlip` 的 `crossings` 状态给出夹住每个符号反转的 `lower_spot` 和 `upper_spot`。两侧异号且中间有零值网格点时，两个端点分别是首尾零值点，单个零值点则两个端点相同。`no_flip` 表示该视图的有效合约在已求值范围内未出现符号反转。`unavailable` 会区分网格缺失、有效合约的网格行缺失、范围内没有有效合约及绝对情景。查看 flip 时应同时展示估值时间、定价模型、利率、股息率、逐合约 IV 与视图覆盖情况。

## 集成边界

F9a 在 paper 只读链路确认期权合约并提供带时间的 OI 与 Greeks，F9b 提供候选清单与轮转覆盖。集成方负责来源与计价口径、交易会话日期、定价输入和产品许可。第三批读契约使用该库计算 `GET /api/v1/options/exposure`，由行情所有者提供已授权的估值快照。[第三批接口](api.md#第三批读契约)

HTTP 敞口响应在获准的私有会话中使用。实时展示核对 `display_live`，非展示衍生用途核对 `derive_non_display`。若将期权链或衍生结果持久化，先评审专用事件契约，取得 `record_local` 许可后经 [F3 `MarketIngress`](market-wal.md) 写入。个人盘备份还需核对 `backup_personal_drive`。
