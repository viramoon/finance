# 行情标的、事件与流协议

finance 定义行情流契约，供采集适配器、投影、研究和 web 消费。传输格式为 UTF-8 JSON。当前交付包括类型、校验、独立 schema 和合成样本。接入与闭合 bar 写入继续使用现有 [BarSource 通路](../catalog/README.md)。HTTP 路由和 OpenAPI 见 [API 文档](api.md)。

## 标的与产品

| 字段 | 约定 |
| --- | --- |
| `instrument_id` | `<LOCAL>.<VENUE>`，区分大小写，按最后一个点分割。LOCAL 由点分隔的非空片段组成，总长最多 96 个字符。每个片段首字符为 ASCII 大写字母或数字，后续允许 ASCII 大写字母、数字、下划线和连字符。分配后保持稳定。 |
| `venue` | 1–32 字符，首字符为 ASCII 大写字母或数字，后续也允许下划线和连字符。与 ID 末段相同。可表示交易场所、指数发布场所或研究命名空间。 |
| `asset_kind` | `stock`、`etf`、`index`、`spot`、`perpetual`、`option`、`future`。 |
| `quote_currency` | 1–16 位 ASCII 大写字母或数字。报价单位可用 USD、USDT 等代号。 |
| `tick_size`、`lot_size`、`contract_multiplier` | 正十进制字符串。分别为价格步进、数量步进、每单位数量的合约乘数。 |
| `settlement_kind`、`settlement_currency` | `cash` 或 `physical` 时填写结算支付币种。无结算的 `none` 携带 `null`。 |
| `calendar_id` | 与 `venue` 使用相同的字符和长度规则，语义独立。`XNYS` 对应现有锁定版本 exchange-calendars 的 NYSE 会话，含节假日和提早收盘。`24X7` 表示全年连续，按 UTC 午夜分交易日。消费者须按已登记代码解析会话。 |
| `supplier_symbols` | 每项为 `{source, product, symbol}`，一个标的在同一 source/product 下至多有一个供应商 symbol。允许空列表。 |

`SPY.ARCA` 和 `SPY.BATS` 可同时存储。`SPY-SPLIT-NORMALIZED.RESEARCH` 是独立研究日线标识，可映射目录中的 `SPY.XNYS`，并使用 `XNYS` 日历。`SPX.CBOE` 是指数，`BTC-USDT-PERP.BINANCE` 是 USDT 报价的永续。复用标的 ID 时保留场所、合约和价格口径所代表的身份。标的格式检查与日历代码分配是两项独立工作。F2 可共用这套格式并将策略输入绑定到具体标的和日历。

`source`、`product`、`channel` 和 `license` 均为 1–64 位 ASCII 标识，首字符为小写字母或数字，后续也允许点、下划线和连字符。产品是源内的能力与授权边界。每个 source/product 的能力记录包含：

- `message_kinds` 声明至少一种受支持的行情数据种类，每种至多出现一次。
- `max_depth` 用无符号 32 位 JSON 整数表示每侧档数。产品支持快照或增量时大于零，其他产品为零。增量批次可包含多次档位变化，批长与档数口径不同。
- `can_backfill` 表示产品具备回补能力，实际可回补范围由适配器和授权决定。
- `delay_tier` 为 `realtime`、`delayed` 或 `replay`。档位描述产品，显示实际延迟时读事件时钟。

`license` 是每条消息的许可策略标识，样本标记为合成数据。生产者应保留源和产品的许可血缘，web 根据策略另行判断展示、导出、留存和再分发权限。

## 精确值与时钟

价格、数量、金额和权重共用无指数十进制字符串规则。允许负号与尾随零，整数部分为 `0` 或以非零数字开头，若有小数点则后面至少一位数字。整数位和小数位合计最多 28 位，含整数部分的 `0`。编码往返保留字面值，例如 `"101.2300"`。校验使用精确运算，价格是 `tick_size` 的整数倍，数量是 `lot_size` 的整数倍。涉及 JS 时用字符串承载，计算使用定点或十进制类型。

`exchange_ts`、`receive_ts`、`first_available_ts` 和区间端点均为 UTC Unix 纳秒。时间戳、`epoch`、`seq` 和 `revision` 使用 `0` 到 `18446744073709551615` 的 ASCII 十进制整数字符串，除 `0` 外禁止前导零。比较按整数值。时钟来源分别为：

| 字段 | 含义 |
| --- | --- |
| `exchange_ts` | 交易所或发布方的事件时间。bar 用收盘边界。控制消息仍携带此键，在无交易所时间时填 `null`。 |
| `receive_ts` | 当前生产者接收该帧的时间，回放可记录本次读取时间。 |
| `first_available_ts` | 当前版本按该产品口径首次可用的时间，研究按此执行时点过滤。延迟源保留实际可用时间，回放保留所回放版本的首次可用时间。更正有自己的首次可用时间。 |

时钟可能来自不同系统，也可能来自历史记录，协议对三者独立编码。合成样本包含超过 JS 安全整数上限的时间戳与序号。

## 流分区、消息与更正

每条事件携带 `schema_version`、`channel`、`source`、`product`、`instrument_id`、`asset_kind`、`venue`、三种时钟、`epoch`、`seq`、`cursor`、`revision`、`license`、`kind` 和 `payload`。供应商提供原生 ID 时可增加 `native_id`，以非空字符串保留，省略表示未提供。`cursor` 为生产者给定的非空不透明字符串，消费端只保存并传回。

`(source, product, channel, instrument_id)` 标识一个流分区。消费者从快照或任意已知位置初始化。同一分区、同一 epoch 的相邻序号必须加一，因而同秒或同纳秒的成交也各有独立位置。序号重复或回退、缺号，以及未声明的 epoch 变化均报错。正整数 epoch 在显式 `reset` 时才能增加，新 epoch 的 `seq` 为 `"0"`。低于当前 epoch 和原地重置均报错。流内序号覆盖只说明该协议分区的连续性，上游覆盖由适配器在有原生依据时声明。

| `kind` | `payload` |
| --- | --- |
| `bar_closed` | `open_ts`、`open`、`high`、`low`、`close`、`volume`。开始时间早于收盘时间，低价和高价包含开盘和收盘价，成交量可为零。 |
| `trade` | `price`、正数 `quantity`、`side`（`buy`、`sell`、`unknown`）。方向缺少可靠依据时填 `unknown`。 |
| `snapshot` | `bids`、`asks` 各为 `{price, quantity}` 列表。数量为正数，买价严格降序、卖价严格升序，可为空。 |
| `delta` | 非空 `updates` 列表，每项为 `{side, price, quantity}`，side 为 `bid` 或 `ask`。`quantity` 为更新后绝对数量，零用于删除该档。 |
| `correction` | `target_epoch`、`target_seq`、`replacement`。替换内容为四种行情数据之一的 `{kind, payload}`。 |
| `gap` | `from_ts`、`to_ts`（含端点且从早到晚）、非空 `reason`。消息本身占用一个连续协议序号，用于报告已知覆盖缺口。 |
| `reset` | 非空 `reason`。收到后清除该分区的增量状态并重新取得基线。 |
| `heartbeat` | 空对象。 |
| `status` | `state` 为 `connected`、`degraded` 或 `disconnected`。 |

`option_chain`、`classified_trade` 为预留名称。它们的链定义、OI/报价时点、分类依据与覆盖待后续版本定义。

原始事件的 `revision` 为 `"0"`。更正事件占用独立位置并携带正修订号。其 `exchange_ts` 是替换行情的事件时间，`first_available_ts` 是本次修订的可用时间。目标是同一流分区内位置更早的原始消息，可来自旧 epoch。校验替换内容的行情种类、tick/lot、盘口深度和产品能力。应用更正的消费者需查到原始消息，核对替换种类相同，按原始位置检查修订递增。历史查询可据此保留「当时所见」和「最新修订」两种结果。

相邻事件的校验和游标字段供生产者和消费者复用。[本地 WAL 与投影](market-wal.md) 用此分区键与 epoch、seq 识别逻辑位置，持久水位使用独立的产品内 WAL 位置。F6 提供 [主图 HTTP 查询](api.md#主图行情与指标)，复用标的和十进制类型，从授权的展示快照读取。带游标的 WS 快照续接、游标过期的 `reset_required` 响应、订阅路由和 web 的 SSE 投递及授权由后续流接口定义。

Nasdaq TotalView 在 `finance-ib` 内存展示接口保留 IBKR 按位置更新的深度行。返回行数按现有位置计，允许同侧多行价格相同。F1 `snapshot` 仍使用严格有序的唯一价位，价位口径与场所展示行分别由各自的类型表达。场所深度通过专用接口消费，后续外部投递由 web 集成工作定义。[Nasdaq TotalView 场所深度](integrations/ib.md#nasdaq-totalview-场所深度)

## Schema 导出与兼容规则

v1 的标的、能力和事件都带 `schema_version: "1.0"`。仓库交付以下独立的 JSON Schema Draft 2020-12 和跨仓样本。

| 文件 | 内容 |
| --- | --- |
| [instrument.schema.json](../protocol/v1/instrument.schema.json) | 标的主数据。 |
| [capability.schema.json](../protocol/v1/capability.schema.json) | 数据源产品能力。 |
| [event.schema.json](../protocol/v1/event.schema.json) | 事件信封和消息种类。 |
| [examples.json](../protocol/v1/examples.json) | 合成标的、产品和流消息，可供 Rust、Python 与下游读者往返。 |

从仓库根运行以下命令。导出从 Rust 类型生成 schema，并通过类型往返生成样本。Rust 测试比较二次导出的字节，Python 测试读取同一份样本。运行时还负责 JSON 重复键、tick/lot、结构之外的语义和相邻位置校验。

协议样本的固定合成输入和导出产物在同一 [fixture 来源清单](../.data-guard/sources.json) 登记。更新样本时先运行导出器，再校对来源说明与摘要，流程见 [数据入仓防护](data-guard.md#合成-fixture-与例外登记)。

```sh
cargo run --locked -q -p finance-market-protocol --bin export-market-protocol -- protocol/v1
cargo test --locked -p finance-market-protocol
PYTHONPATH=python python -m unittest discover -s tests -p test_market_protocol.py -v
```

读取端按完整 `schema_version` 选择 schema，拒绝未知版本、未知字段、未知种类和缺少必填字段。任意增删字段或种类、缩窄值域、改变单位或事件语义都需要显式协商并以新的主版本发布，旧 schema 和样本保留给旧读者。只修订文档或更正生成物中与既定 v1.0 规则不一致的部分时保留原协议版本。协议版本独立于代码包版本和 fertile 发布版本。

跨仓消费方应从同一个 finance 提交取 schema 和样本，并在自身测试中核对识别的版本、消息分支以及精确字段。finance → web → frontend 的 HTTP OpenAPI 指针继续按原有流程更新。

### v2.0 永续上下文

`schema_version: "2.0"` 的事件种类为 `perpetual_context`，适用于 `asset_kind: "perpetual"`。保留原信封、位置和时钟定义；缺源端时钟时 `exchange_ts` 为 null。`payload` 由三个十进制字符串组成。资金费率 `funding_rate` 可为负，未平仓量 `open_interest` 非负，标记价格 `mark_price` 大于零。标记价格与未平仓量保留源值的精度，可超出交易价格和数量的网格。原始事件修订为 `"0"`。

能力声明包含 `perpetual_context` 时使用 2.0，可同时声明 v1.0 市场数据种类。标的及 `reset`、`gap` 等控制消息沿用 v1.0。读者须按每条记录的版本选择 schema，跨版本控制消息也参与同一流分区的 epoch、seq 校验。

生成 [事件 schema](../protocol/v2/event.schema.json)、[能力 schema](../protocol/v2/capability.schema.json) 和 [合成样本](../protocol/v2/examples.json) 的命令如下。

```sh
cargo run --locked --offline -q -p finance-market-protocol --bin export-market-protocol -- protocol/v2 --v2
```

### v3.0 期权历史证据

`option_history_trade` 记录原始价格、张数、合约乘数、成交场所、条件码及映射版本。`option_history_quote` 记录报价、数量和供应商标记，缺值使用 `null`。两者都保留历史请求页号、请求起点、可用的请求 ID 和完整秒内的位置。`exchange_ts` 为 UTC 源秒对应的纳秒整数，`receive_ts` 为取得回调的 UTC 纳秒。请求 ID 的使用范围限同一 Gateway 会话，秒内位置用于区分重复成交。事件内交易数值用十进制字符串，批次位置用无符号十进制字符串。

版本 3.0 的能力只声明上述历史证据种类，标的和 reset 仍使用版本 1.0。既有 v1 WS 和 SSE 继续使用各自的 v1 模式。业务核验结果通过 [期权流接口](option-flow.md) 返回。

生成 [事件 schema](../protocol/v3/event.schema.json)、[能力 schema](../protocol/v3/capability.schema.json) 与 [固定合成样本](../protocol/v3/examples.json)。

```sh
cargo run --locked --offline -q -p finance-market-protocol --bin export-market-protocol -- protocol/v3 --v3
```

接口与恢复规则见 [Hyperliquid](integrations/hyperliquid.md) 及 [ADR-0034](adr/ADR-0034-hyperliquid-context-stream.md)。

## 首批读契约的 WS 与 SSE 消息

`finance-api export-read-stream DIRECTORY` 从代码导出以下 1.0 版文件。更新仓库产物时用同一命令指向 `protocol/v1`，同步样本来源和摘要。导出器只读已提交的合成事件种子与代码中的确定性状态样本。

| 文件 | 内容 |
| --- | --- |
| [market-ws.schema.json](../protocol/v1/market-ws.schema.json) | 一帧 WS 文本消息承载一个完整的 F1 v1.0 `Event`。`snapshot`、`delta`、`reset`、分区和游标沿用 F1 约定。 |
| [market-status-sse.schema.json](../protocol/v1/market-status-sse.schema.json) | `event: market_status`，`id` 为发布流内的无符号 64 位十进制字符串，`data` 为含 `schema_version` 和现有 `status` 对象的 JSON。Schema 的 `event`、`id`、`data` 分别对应三个 SSE 字段。 |
| [read-stream-examples.json](../protocol/v1/read-stream-examples.json) | 固定合成 WS 事件与 SSE 状态，包括超过 JavaScript 安全整数范围的序号和事件 ID。 |

首批交付消息格式和编码。实时订阅、过期游标重置、SSE 保留窗和 `Last-Event-ID` 的续接由传输集成方实现。WS 游标按 F1 分区解释，SSE 事件 ID 按发布流解释。web 应在握手、订阅及重连时重新授权，并遵循产品的展示、留存和导出许可。[首批 REST 查询](api.md#首批读契约)

## 解码与错误定位

Rust 使用 `finance_market_protocol::decode_json::<T>(input)`，其中 `T` 为 `Instrument`、`Capability` 或 `Event`。通过错误的 `path()` 和 `kind().as_str()` 读取诊断。Python 使用 `fertile_finance.market_protocol` 的同名类型及 `from_json(input)`，通过 `ProtocolError.path` 和 `ProtocolError.kind` 读取诊断。Python 的 `Kind` 枚举对应 v1.0 消息种类，永续上下文使用 `ContextKind.PERPETUAL_CONTEXT`。

路径使用相对于所读对象的 JSON Pointer，数组下标从零开始。语法错误等定位到整个输入的情况使用空路径。调用方可按错误种类分支，并用可读错误消息显示原因。

| 输入问题 | 路径 | 错误种类 |
| --- | --- | --- |
| 逐笔价格为 `"6.6312e2"` | `/payload/price` | `invalid_decimal` |
| 逐笔数量为 JSON 数字，包括 `1e1000000000000000000` | `/payload/quantity` | `invalid_type` |
| 序号为 `"18446744073709551616"` | `/seq` | `out_of_range` |
| `max_depth` 的整数字面量超出无符号 32 位范围 | `/max_depth` | `out_of_range` |
| 更正的盘口替换中第一档价格非法 | `/payload/replacement/payload/bids/0/price` | `invalid_decimal` |

解码按字段判断 JSON 数字的类型和范围，极大的整数和指数仍有字段路径。`max_depth` 接受范围内的无符号整数字面量，`-0`、小数及指数写法报告 `invalid_type`。`NaN`、`Infinity` 和 `-Infinity` 属于 JSON 语法错误，报告空路径和 `invalid_json`。

结构错误种类包括 `missing_field`、`unknown_field`、`duplicate_field` 和 `invalid_json`。非规范的无符号整数字符串报告 `invalid_integer`，未知枚举值等报告 `invalid_value`。读取事件后调用 `validate_against(instrument, capability)` 校验元数据和步进，再用 `validate_successor(previous)` 核对已有流位置。此类语义约束错误报告 `constraint_violation`。
