# 面向 web 的 HTTP API

finance-api 是独立的 Rust / axum 进程；web 通过 HTTP 调用，frontend 只调用 web。当前不启动 Nautilus、trader、collector 或 IBKR，也不在 HTTP 请求中执行回测。服务部署唯一来源仍为 fertile deploy/，本仓库 service.yaml 记录组件接口。

用于接口测试和文档的行情样本须来自已登记的合成 fixture。API 的 PR 作业以 PR 基分支 SHA 的硬规则检查 Git 对象，样本来源和 SHA-256 由候选树的清单提供。防护启用前已发布分支的合成样本也须通过该检查。将实际 API 响应、运行日志和数据库留在工作树之外，目录和样本准入方式见 [数据入仓防护](data-guard.md)。

## 运行与配置

```sh
cargo build --locked -p finance-api
# 先建立可读数据根目录；API 不会自动创建它掩盖挂载错误。
mkdir -p /home/box/fertile/data
FERTILE_PROFILE=cloudbox cargo run --locked -p finance-api
```

从仓库根运行；其他工作目录须设置 FINANCE_CONFIG_DIR 为 config 的绝对路径。默认监听 127.0.0.1:8081，覆盖顺序为 config/base.yaml → config/<FERTILE_PROFILE>.yaml → 环境变量。profile 默认 cloudbox，只允许 cloudbox / stable，不决定运行模式或实盘权限。

| 环境变量 | 作用 |
| --- | --- |
| FINANCE_CONFIG_DIR | 配置目录，默认当前工作目录下 config |
| FERTILE_PROFILE | cloudbox / stable |
| FINANCE_API_BIND_HOST | IP 地址，默认 127.0.0.1 |
| FINANCE_API_PORT | 1～65535，默认 8081 |
| FERTILE_DATA_ROOT | 绝对数据根目录，默认 /home/box/fertile/data |
| FINANCE_BINANCE_CONFIG | 沿用 Binance profile 覆盖，仅供 API 读取启用状态和用途许可，默认 `FINANCE_CONFIG_DIR/binance/<profile>.yaml` |

未知 YAML 字段、未知 profile、非法 IP / 端口、相对数据根路径在启动时拒绝。配置不读取账户密钥。finance API 默认绑定 loopback。web 负责调用方授权和限流，部署方只在受控内网显式覆盖绑定地址。

## 接口与状态

- GET /healthz/live：进程存活返回 200，status=ok，checks 含 process=true。
- GET /healthz/ready：实际 read_dir 数据根目录成功返回 200 / ready，否则 503 / not_ready；checks 含 data_root_readable 布尔值。目录没有回测结果仍可 ready；这不是交易、行情新鲜度或 Gateway 健康证明。
- GET /api/v1/backtests/sample：读取数据根下 finance-state/backtests/sample/latest.json。200 返回 run_id、strategy、instrument、start、end、total_orders、total_positions、pnl、currency、generated_at。时间为 UTC RFC3339（序列化时可规范化等价格式）；计数为整数，pnl 为精确十进制字符串，禁止 JSON 数字。当前示例 PnL 是 USD 账户最终余额减初始余额，不使用浮点统计接口；上游示例不下单，因此当前订单 / 持仓计数和 PnL 为零。

没有结果为 404，统一错误体包含 error.code=sample_not_found 和 error.message。不可读为 500 / sample_unreadable，损坏 JSON、字段缺失或金额非法为 500 / sample_invalid；响应不暴露文件路径和底层错误。liveness / readiness 使用健康体及逐项 checks。

## 行情与作业

GET /api/v1/market/status 从 bar collector 持久状态读取各标的水位和新鲜度。没有状态文件时返回明确的缺失状态，collector 停止后会反映过期。可用性和精度验证见 [行情目录](../catalog/README.md)。Binance 的 profile 状态和 IBKR 的私有查询见 [第三批读契约](#第三批读契约)。

IBKR 顶档订阅与权限验收由 `finance-ib` 的 Rust 接口和私有 CLI 提供。共享行情所有者通过 `finance_collector::line_budget::LineHandle` 提供行情线预算、确认的期权链与覆盖，在核对许可后接入第三批查询。Nasdaq TotalView 的页面焦点由所有者在进程内控制。[IBKR 顶档权限](integrations/ib.md#美股-l1-和-opra-权限验收) · [行情线预算](integrations/ib-line-budget.md) · [场所深度](integrations/ib.md#nasdaq-totalview-场所深度)

期权流观察及历史核验使用进程内 `finance_collector::option_flow` 接口。启用历史产品本地记录时，已有 WAL、投影和质量查询可读取已获准的原始证据。本期的领域报告由调用方在进程内取得，`/options/flow` 的专用 HTTP 查询交由后续读契约接入。[期权流接口](option-flow.md)

POST /api/v1/backtests 接收带 Idempotency-Key 的任务并返回 202。GET /api/v1/backtests/{id} 查看执行进度与摘要，GET /api/v1/backtests 按 cursor 和 limit 翻页。提交使用同库事务完成幂等检查和容量检查。多个 worker 可独立领取并续租，租约是服务端状态。完整输入范围、超时和恢复见 [作业运维](backtest-jobs.md)。公网入口由 web 鉴权并限流。

`finance-decision` 的信号与检查点保存在本地决策库。信号发布和分页见[第二批契约](#第二批策略回测与研究契约)，进程边界见[实时决策接口](decision-process.md)。

## 配置策略提交与结果

`POST /api/v1/strategies` 接受 `{"format":"yaml","content":"<配置文件内容>"}`，也接受带 `format: "json"` 的 JSON 配置文档。通过校验时返回 200 和 `{id, version, content_sha256}`。同一版本的等价文档可重复登记。登记或内联入队遇到已绑定不同内容的版本时返回 409 `VERSION_CONFLICT`，`error.path` 为 `/metadata/version`，`error.location` 指向本次提交的 YAML 或 JSON 配置中的版本值，行列从 1 起算。配置校验错误返回 422，`error` 包含编译器错误码、消息、字段 `path` 和行列 `location`。原始配置上限为 256 KiB。

`POST /api/v1/backtests` 支持如下请求，继续使用 `Idempotency-Key`。配置作业使用 UTC 午夜端点和半开区间 `[start, end)`，可覆盖 1970-01-01 至 2100-12-31，最晚结束端点为 2101-01-01。决策及成交按 XNYS 当地会话时钟执行。

```json
{
  "strategy": "StrategyBundle",
  "start": "2024-03-01T00:00:00Z",
  "end": "2024-04-15T00:00:00Z",
  "source": {"kind": "registered", "id": "example.equity-momentum", "version": "1.0.0"}
}
```

使用 `source: {"kind":"inline","format":"yaml","content":"<配置文件内容>"}` 可直接编译并登记。内联文件和相同已登记版本使用同一编译路径。同一键、相同区间和相同规范化配置哈希返回原作业 ID。使用同一键提交另一份有效配置或区间返回 409 `key_conflict`。指定未知登记版本返回 404 `strategy_version_not_found`，缺少已发布的本地回放快照返回 422 `no_data`。

列表与详情以已登记版本和 `content_sha256` 表示配置作业请求。成功摘要保留通用作业字段，`instrument` 为 `PORTFOLIO`，并带有 `run_manifest` 与 `strategy_report`。前者锁定实际执行版本与回放数据，后者包含净值、仓位、成交、子账、冲突归因和覆盖。金额、价格、数量和权重以十进制字符串表示，回放时钟的 UTC 纳秒时间戳也以字符串传输。已有 `DemoStrategy` 和 `SmaCrossover` 请求、详情及历史摘要仍可读取。参见 [配置策略作业](backtest-jobs.md#配置策略作业)。

## 第二批策略回测与研究契约

路径使用 `/api/v1` 前缀。以下为新增操作，返回 `Cache-Control: private, no-store`。网关按会话授权，回测取消与策略登记需要写权限。因子快照、决策发布文件及数据库、策略输入和回测队列须位于配置数据根内的实体目录。API 遇到这些路径中的符号链接时返回存储错误。

| 请求 | 参数和行为 | OpenAPI 响应 schema |
| --- | --- | --- |
| `POST /strategies/validate` | 接收原有 `StrategyDocument` YAML/JSON 包装，编译预览。校验不写入注册表或队列。 | `StrategyPreview`（`RegisteredStrategy`、`CompiledPlan`） |
| `GET /strategies` | 可选 `id`、`after`、`limit`。按 `id`、`version` 的文本顺序排序，`after` 使用上一页返回的 `id@version`。 | `StrategyVersionPage`（`RegisteredStrategy`） |
| `GET /strategies/{id}/versions/{version}` | 选择已登记的精确版本，以固定输入元数据编译。不存在返回 `404 / strategy_version_not_found`。 | `StrategyPreview` |
| `POST /backtests/{id}/cancel` | 空请求体。取消排队或运行中的回测，重复取消返回同一作业。已完成且未取消的作业返回 `409 / job_already_finished`，未知 ID 返回 `404 / job_not_found`。 | 既有 `Job` |
| `GET /backtests/worker-status` | 读取队列和当前租约数，有效租约按 `as_of` UTC 判断。租约缺失或过期的运行作业计入 `unleased_running`。队列未初始化时 `queue_present` 为 false，计数为 0。 | `WorkerStatus` |
| `GET /decisions/signals` | 分页读取获准发布的已提交目标。可选 `after`、`limit`。 | `DecisionSignalPage`（`DecisionAccess`、`DecisionSignal`、`SmaSignal`、`BundleSignal`） |
| `GET /factors/{id}/evaluations/{evaluation_id}` | 必须指定 `version`，按因子 ID、版本、评价 ID 精确匹配同一个 F8 快照。无报告返回 `404 / factor_evaluation_not_found`。 | `FactorEvaluationDetail`（原有 `FactorEvaluationItem`，含报告和同行因子的许可） |

策略和决策列表的 `limit` 默认 20，取值 1～100。带上其他筛选项并原样传回 `next_cursor` 以续页，值为 `null` 时结束。尚未建立策略注册表时列表为空。策略预览和选择返回同一个编译器生成的计划。等价 YAML 和 JSON 的规范化配置与 `content_sha256` 一致，`source_sha256` 标记各自的原始字节。版本选择使用首次登记的源码及输入元数据。原始配置限 256 KiB，编译错误使用既有 `ErrorBody` 的字段位置和行列。预览中的 `plan.bundle` 遵循代码导出的 [StrategyBundle v1 JSON Schema](../strategies/schema/strategy-bundle-v1.schema.json)。登记仍调用 `POST /strategies`，版本冲突由登记和入队返回。

沿用 `POST /backtests`、`GET /backtests/by-key`、`GET /backtests` 和 `GET /backtests/{id}`。详情的 `summary` 承载成功结果，配置作业的 `run_manifest` 和 `strategy_report` 保留精确十进制字符串与现有 F7 字段。取消后的详情沿用原有状态枚举，`status` 为 `failed`，`error` 为 `job_cancelled`，`summary` 为 `null`。同一请求和幂等键重交会返回原 ID，重新运行须使用新键。取消和完成按 SQLite 事务先后决定终态。worker 状态查询反映队列与租约，进程存活由运维监控。

### 决策信号发布与分页

API 只读打开现有 `finance-state/decisions/decisions.sqlite3`。运维或已授权的回放提供方核对允许本地留存和非展示计算后，在同一目录原子发布普通文件 `read-v1.json`。下列为固定合成配置。部署时登记 `decisions` 或 `bundle_decisions` 表的 `plan_sha256`。配置策略使用信号行的完整计划哈希，检查点哈希另绑定指标求值器版本。SMA 的发布许可还须与生产者校验整个已提交回放前缀后记录的许可一致，范围包括预热与控制帧。首次遇到不同许可的帧即停止，保留上一份已提交检查点。API 仅返回许可证明覆盖的信号。旧 SMA 库需要用原始同许可回放启动新版决策进程，完整验证旧检查点后才能发布。证明缺失、过期或许可不匹配时返回 `404 / decision_not_published`。配置策略由发布方为全部输入核对共同授权并登记适用许可。

```json
{
  "schema_version": 1,
  "kind": "sma_crossover",
  "plan_sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
  "access": {
    "license": "fixture.synthetic",
    "record_local": true,
    "derive_non_display": true,
    "redistribute": false
  }
}
```

`kind` 可用 `sma_crossover` 或 `strategy_bundle`。许可与计划哈希一同出现在响应中。记录按 SMA 的回放字节偏移，或配置策略的回放字节偏移、配方 ID 和标的 ID 升序排列。游标绑定发布计划。`replay_offset`、事件 `epoch`、`seq` 和各纳秒时钟为精确整数的字符串，`target_shares` 为十进制字符串。每行公开目标及其输入血缘字段，配置策略另保留可空的 `reason`。缺少发布文件、数据库或获准用途时返回 `404 / decision_not_published`。查询或游标非法返回 400，发布文件或数据库损坏返回 500。仅展示产品不得提供给离线回放、回测、因子或决策发布通道。

第二批新增 REST 操作。首批的 WS/SSE schema 和样本继续由原生成器校验。场所深度、GEX/0DTE 和历史期权查询见[第三批读契约](#第三批读契约)。web 的代理、浏览器授权和策略页面由下游交付。

## 首批读契约

以下查询使用 `/api/v1` 前缀，返回 `Cache-Control: private, no-store`。finance-api 读取本地数据根，web 负责调用方授权、限流和客户端可见范围。新增查询只使用 GET。状态、健康与原回测路由仍按上述约定调用。

| GET 路径 | 查询和分页 | 返回 |
| --- | --- | --- |
| `/market/products` | 可选 `source`、`offset`、`limit` | 按 source/product 排序的已登记、可落盘产品许可 `items`，以及 `next_offset`。 |
| `/market/wal` | `source`、`product`，可选排除式 `after`、含端点的 `through`、`limit` | `access`、`records`、`durable_through`、本次固定前缀上界 `through`、`next_after`。一页默认 512 条，上限 4096 条，F3 帧预算约 16 MiB，可能提前分页。续页传上次的 `through` 和 `next_after`，后者为 `null` 即结束。 |
| `/market/projection/latest` | `source`、`product`、`channel`、`instrument_id`、`epoch`、`seq` | `access`、`projected_through`、`record`。按原始事件位置查 F3 SQLite 已提交索引，再从持久 WAL 取同一记录。已有更正时返回最新更正信封。 |
| `/market/watermarks` | `source`、`product` | `access` 和 F3 的 `watermarks`。`source_latest` 取最后一条已持久事件，`remote_verified` 在远端回执接入前为 `null`。 |
| `/quality/gaps` | `source`、`product`，可选 `channel`、`instrument_id`、`from_ts`、`to_ts`、`offset`、`limit` | `access`、按报告写入顺序的 `items`、`next_offset`。返回与 `[from_ts, to_ts)` 相交的报告，报告自身仍保存含两端的 UTC 纳秒区间。 |
| `/factors` | 可选 `factor_id`、`version`、`after`、`limit` | 因子定义、研究状态、状态迁移、历史研究登记以及许可。游标为 `factor_id@version`。 |
| `/factors/{id}/evaluations` | 可选 `version`、`after`、`limit` | 评价报告摘要，含样本窗、IC/衰减、覆盖、费前费后及换手数据。游标为 `version@evaluation_id`。 |

行情查询的 `as_of` 是 API 的 UTC 观察时间。事件时间与覆盖分别由 F1 时钟、持久和投影水位、质量报告说明。`position`、`generation`、`epoch`、`seq`、`revision` 与 UTC 纳秒时间在 JSON 和查询参数中均使用十进制字符串，`after` 默认 `"0"`。价格与数量沿用 F1 的精确十进制字符串。`offset` 默认 0，`limit` 默认 100、最大 500，`next_offset: null` 表示已到列表末尾。[F1 事件](market-protocol.md) · [F3 存储](market-wal.md)

WAL 的 `records[].event` 和投影的 `record.event` 保留原始 `schema_version`。同一 WAL 页面可包含多个版本，消费端按每条记录的版本与消息种类解码。Hyperliquid 使用 `2.0` 的 `perpetual_context`。Binance 使用 `2.0` 的 `funding_rate`、`open_interest` 及该产品的控制消息。期权逐笔证据使用 `3.0` 的 `option_history_trade` 和 `option_history_quote`，其重置消息使用 `1.0`。OpenAPI 含上述事件类型。独立导出的 WS/SSE 消息使用 `1.0`。[版本化行情协议](market-protocol.md)

市场响应的 `access` 来自 F3 的落盘策略，含 `license`、`display_live`、`record_local`、`derive_non_display`、`backup_personal_drive`。事件另携带原 `license`。仅展示产品由生产者在内存处理，不进入这些查询和生成物。项目禁止再分发。参数不合法返回 `400 / market_query_invalid`。产品未登记或目标投影位置缺失返回 `404 / market_product_not_found` 或 `market_projection_not_found`。策略、索引或 WAL 损坏返回 `500 / market_storage_error`，响应省略本地路径和数据正文。

因子查询读取由离线研究程序显式发布到 `FERTILE_DATA_ROOT/finance-state/factor-research/read-v1.json` 的本地快照。响应包含 `schema_version: 1`、快照的 UTC `as_of`、`items` 和 `next_cursor`。`after` 排除游标本身，按完整游标文本的字典序翻页，`limit` 默认 50、最大 100。接续时保留筛选项，若 `as_of` 变化则重新分页。条目保留授权，普通因子和评价附 `data_version`。报告 Decimal 字段为字符串，未知值用 `null`。无快照时返回空列表和 `as_of: null`。请求无效为 `400 / invalid_factor_query`，已发布快照内找不到评价请求指定的因子或因子版本时为 `404 / factor_not_found`，快照无效或不可读为 500。[离线因子评价与发布](factor-research.md)

## 主图行情与指标

| 请求 | 响应 |
| --- | --- |
| `GET /api/v1/market/bars` | 精确 OHLCV，可在同一行附带零至 16 个指标。 |
| `GET /api/v1/indicators` | `definitions` 列出版本化指标、参数、输出及算法口径。 |
| `GET /api/v1/indicators/values` | 使用同一投影和计算过程，每行保留 bar 时钟和指标，须请求 1 至 16 个指标。 |

两个时间序列接口都要求 `instrument_id`、`source`、`product`、`timeframe`、`adjustment`、`start` 和 `end`。前三项沿用 [F1 标的与产品](market-protocol.md#标的与产品) 格式。`timeframe` 支持 1 至 86400 秒（`s`）、1 至 1440 分钟（`m`）、1 至 24 小时（`h`）和 `1d`，数字无前导零。`adjustment` 是发布方登记的 F1 标识，例如 `raw`。查询与周期和复权标签精确匹配，跨口径转换由发布方处理。

区间使用带时区的 RFC3339 时间，按 `open_ts` 落在 `[start, end)` 选取，按 `open_ts` 升序返回。`limit` 默认为 500，范围为 1 至 1000。存在 `next_start` 时将其作为下页的 `start`，保留其余参数。分页读取最新快照，`as_of` 改变时应重新开始。`as_of` 是投影发布时间。历史点时查询由策略输入方按实际可见版本回放。

`indicators` 是 URL 编码的 JSON 数组，例如 `[{"use":"indicator.sma@1.0.0","params":{"period":2}}]`。指标声明及逐行 `indicator_values` 按请求顺序排列。区间内首点会使用同一投影中的前置闭合历史。`include_provisional` 默认为 `false`。请求预览时，可在末尾返回一根未闭合 bar。每行的 `provisional` 标签独立于指标的 `status`。`status` 为 `ready` 时，`values` 提供各列的十进制字符串，`warmup` 和 `undefined` 都返回空 `values`。OHLCV 也保留源十进制字符串。交易时钟与精度见 [v1 指标口径](indicators.md)。

响应携带 `series`、`provenance`、`as_of`、`coverage`、`next_start`、`indicators` 和 `rows`。`provenance` 保留 `license`、`delay_tier`、`calendar_id`、`market_timezone` 和发布方的展示授权标记。每行的 `first_available_ts` 属于这一版源 bar。最新快照的指标可能包含后到的前置 bar 或更正。图表可用源 bar 的时钟显示延迟，由 web 对浏览器执行身份和产品权限检查。三个端点均设置 `Cache-Control: private, no-store`。

`coverage` 包含 `history_complete`、`first_closed_open_ts`、`last_closed_end_ts`、`closed_bars`、`returned_closed_bars` 和 `returned_provisional`。快照首尾为已观测闭合 bar 的边界，两者之间可能有缺口。bars 端点的 `rows[].bar` 含 OHLCV 和时钟，values 端点把 `open_ts`、`end_ts`、`first_available_ts`、`trading_day` 放在每行顶层。OpenAPI 用 `ChartCoverage` 描述两个时间序列端点的覆盖率对象，历史回测窗口的 `coverage` 使用 `Coverage` 字符串枚举（`full`、`insufficient`）。

| HTTP 状态 | `error.code` | 含义 |
| --- | --- | --- |
| 400 | `invalid_bars_query`、`invalid_indicator` | 查询参数、范围、分页或指标声明非法。 |
| 404 | `bars_not_found` | 未发布匹配序列。 |
| 422 | `indicator_history_unavailable` | 匹配投影缺少从起点计算所需的历史。 |
| 422 | `indicator_work_limit` | 从序列起点到本页末尾（含返回的预览）的估算工作量超出 5,000,000。每个指标每行至少计 1，已过预热的布林带和实现波动率按各自周期计。可减小 `limit` 或选择更少、周期更短的指标。 |
| 500 | `bars_invalid`、`bars_unreadable`、`indicator_calculation_failed`、`bars_query_failed` | 投影无效、不可读，或计算失败。指标计算错误可来自本页或更早的回放前缀。 |

投影的 OHLCV 字符串遵守 [F1 十进制文本规则](market-protocol.md#精确值与时钟)，整数位和小数位合计最多 28 位。整数部分至少占一位，所以小数部分最多 27 位。超出位数的投影在解析时返回 500 `bars_invalid`。

### 只读展示投影

finance-api 读取现有 `FERTILE_DATA_ROOT` 下的 `finance-state/bars/v1/<source>/<product>/<instrument_id>/<adjustment>/<timeframe>.json`。受信的投影发布方按整份快照原子替换普通文件，并递增该序列的 `as_of`。投影目录下的路径使用实际目录和文件。快照结构为 `schema_version: "1.0"`、`series`（与请求相同的五项序列键）、`provenance`（含 `display: "authorized_display"`）、`as_of`、`history_complete`、`closed` 和可为空的 `provisional`。`closed` 最多 50,000 行，文件最多 32 MiB。编制投影时由发布方核对产品是否同时允许本地留存与展示。文件只放在受控数据根，访问权限交由部署配置管理。

每根 bar 包含 `open_ts`、`end_ts`、`first_available_ts`、`trading_day`（`YYYY-MM-DD`）和十进制 `open`、`high`、`low`、`close`、`volume`。同一序列的区间按时间递增且互不重叠。闭合 bar 要求 `end_ts ≤ first_available_ts ≤ as_of`，未闭合 bar 以 `open_ts ≤ first_available_ts ≤ as_of` 验证，允许延迟源在理论收盘后继续输出未闭合预览。`24X7` 序列的 `market_timezone` 为 `UTC`，单根 bar 的半开区间落在同一 UTC 交易日，允许在次日零点结束。`history_complete` 表示已包含声明序列起点以来的历史。缺少起点历史时仍可请求原始 bars，指标请求返回 422。交易日内缺 bar 的处理由 [指标口径](indicators.md) 定义。

当前查询端提供 `BarsSource` 读取边界。[F3 投影](market-wal.md#parquet-与投影游标)发布获准落盘的 F1 事件 Parquet 和修订索引。受信的主图发布方可用该索引构建上述 `bars/v1` 快照，并在事件更正后重新发布。长期分钟历史需要向指标内核提供完整闭合前缀，或接入与同一组件版本对应的等价状态检查点。现有 collector 闭合 bar Parquet 和研究目录使用各自管道。

## 第三批读契约

以下路径使用 `/api/v1` 前缀，均为 GET，成功和错误响应均设置 `Cache-Control: private, no-store`。符合展示权限的 web 调用在鉴权和产品授权后向浏览器转发。

| 路径 | 查询 | OpenAPI 响应与含义 |
| --- | --- | --- |
| `/market/bars` | 标的、复权、时窗等 | 复用 F6 的 `ChartBars`、`ChartContext`、`ChartCoverage`。 |
| `/indicators`、`/indicators/values` | 注册表或带指标的时窗 | 复用 F6 的 `IndicatorCatalog`、`ChartValues`。见 [主图行情与指标](#主图行情与指标)。 |
| `/market/binance/status` | 无 | `BinanceReadStatus`。禁用或 profile 文件缺失时为 `disabled_by_profile`、`products: []`。启用时为 `enabled_by_profile` 及配置中各产品的 `ProductPolicy`。配置无法读取或校验时返回 `500 / binance_profile_invalid`。 |
| `/market/book` | `symbol` 为 QQQ、AAPL、MSFT、NVDA、AMZN、GOOGL、META、TSLA | `DepthBook`。Nasdaq TotalView 的具体场所、L1/TotalView/EDS 确认状态、请求和返回档数、同步状态及精确价量。 |
| `/market/subscriptions` | 无 | `SubscriptionBudget`。账户上限、预留、各池实际占用、确认数、观察数和发现状态。 |
| `/market/subscriptions/coverage` | 正整数 OPRA `con_id`，UTC RFC3339 的 `start`、`end` | `SubscriptionCoverage`。半开区间内的 `covered`、`uncovered`、`unknown`，以及最近一次观察时间。未知合约在有效区间内返回 `unknown`。报告键须匹配所查合约和带 OI 的共享 OPRA 行。 |
| `/options/chain` | 正整数 `underlying_con_id`，可选 `expiry`（YYYY-MM-DD）、排他游标 `after_con_id` 和 `limit` | `OptionChainPage`。返回已确认未过期的合约、乘数与执行价等，按 `con_id` 升序。API 在分页前排除早于候选集 `as_of` 对应美东日期的到期日，保留当日到期合约。每页默认 100 条，最大 500 条，`next_after_con_id: null` 表示查询到末页。 |
| `/options/exposure` | F1 `underlying_id`，可选 UTC RFC3339 `as_of` 和 `scenario` | `OptionExposureResponse` 包装 `ExposureReport`，包含情景、估值时点、`all`、`zero_dte`、`later` 的 GEX、wall、`GammaFlip` 和 `ExposureCoverage`。默认 `absolute`，另有 `dealer_short`、`dealer_long` 和 `call_long_put_short`。 |
| `/options/history` | 正整数 `underlying_con_id`，UTC Unix 纳秒字符串 `start`、`end`，可选 `offset`、`limit` | `OptionHistoryPage`。每个 `HistoricalOptionResult` 保留 `available`、`no_bars`、`expired` 或 `request_failed`，可用时附一分钟 `OptionMinuteBar`（F1 `ClosedBar` 外加 `close_ts`）。按已发布批次的合约顺序分页，默认 20 条，最多 50 条，`next_offset: null` 表示末页。 |

Binance profile 状态是配置意图。启用后的采集进程状态仍由 F4 日志报告。行情正文通过 F3 的 `/market/products`、`/market/wal`、`/market/projection/latest`、`/market/watermarks` 和 `/quality/gaps` 读取，`source=binance`，`product` 依次可选 `spot`、`usdt-perp`、`usdt-perp-context`。读取仍受 F3 已登记的记录许可约束。永续上下文用 v2.0 的 `funding_rate` 和 `open_interest`。策略状态包含许可元数据，仅展示品种的行情正文留在内存。相关标识与降级行为见 [Binance 公共行情](integrations/binance.md)。

IBKR 响应的 `access` 是 API 的显示用途许可，来源为 `ibkr`，产品为 `opra` 或 `nasdaq-totalview`，含所有者提供的 `license`、`display_live: true`、`record_local: false`、`derive_non_display: false`、`backup_personal_drive: false`。深度仅在三项权限已确认且 `state=receiving` 时提供行；`market_data_type` 为 `null`，时间为本地 `received_at`。链分页基于当前候选集，继续请求需保留筛选条件；一轮分页期间确认集可能变化。`discovery_complete` 是调度器自选范围的发现状态。覆盖始于合格的本地观察，止于取消或中断；`end` 须不晚于调度器当前时间。敞口的 `as_of` 缺省取最近快照，指定时查询完全相同的估值时点，见 [模型和覆盖口径](option-exposure.md)。

历史请求只查所有者已取得的精确 `[start, end)` 窗口，完整 bar 可在 `end` 处闭合。`as_of` 表示该批次的取得时间；换批后应从第一页开始。失败只返回状态，省略供应商错误详情。时间戳及已有流序号沿用 `U64Text`，价格、数量、乘数和敞口沿用 `DecimalText`。接口不会触发 paper 历史请求或 Databento 下载。[F12 窗口与限制](option-history.md)

### 行情所有者接入

独立 `finance-api` 进程提供本地 bars、Binance profile 和 F3 记录查询。IBKR 查询需由拥有已验证 paper 只读会话、确认产品展示许可的一方将数据接入 `router_with_market_reads` 的 `MarketReadSources`。

- 期权链和预算接入现有 `LineHandle`（实现 `OptionLineSource`），GET 不分配行情行。
- 场所深度将 `run_depth` 的显示回调交给 `DepthFrames::publish`，包括 reset、inactive 和冷启动空簿。
- 敞口由 `ExposureSource` 查找估值快照、时序阈值和可选价格网格，计算走 F9d 的 `evaluate`。历史由 `HistorySource` 查找 F12 的 `ContractHistory` 结果。两个查找接口读取内存，由所有者提供生命周期与留存策略。

数据源未接入时返回 `503 / market_read_source_unavailable`。数据源已接入但深度、估值或历史窗口不可用时返回 `404 / market_read_not_found`。查询参数无效返回 `400 / market_read_query_invalid`。覆盖键、估值快照键或历史标的和窗口与请求不匹配，链快照时钟无法转换为美东日期，或估值计算失败时，返回 `500 / market_read_failed` 和固定消息 `Could not read market data`，省略底层详情。IBKR 响应和敞口计算结果只在进程内与授权的私有请求中使用。F9e 的原始逐笔证据沿用已授权的 F3 查询。专用 `/options/flow`、页面焦点写接口和行情所有者的部署接线留待后续集成。本批复用 F1/F3 数据通道与既有 WS/SSE 导出。[期权流观察与历史核验](option-flow.md)

## 示例批任务

先完成 [源码构建](build.md)，执行一条命令：

```sh
FERTILE_DATA_ROOT=/home/box/fertile/data bash scripts/backtest-sample.sh
```

入口验证本地源码 wheel，调用 #3 同一个上游 CSV 示例和 DemoStrategy，不编辑 vendor、不写自研策略。在 engine.dispose 前提取真实结果与 Money.as_decimal() 余额，最后释放引擎；临时文件与 latest.json 在同一目录，flush / fsync 后 os.replace 原子替换，失败保留旧结果并清理临时文件。只在回测成功后写结果。worker 独立领取 HTTP 提交的作业。批任务的数据路径使用 FERTILE_DATA_ROOT 或上述默认值，不读取 API profile 文件；调度时向批任务与 API 注入相同根路径。

## OpenAPI 与契约

```sh
cargo run --locked --offline -q -p finance-api -- export-openapi > /tmp/finance.openapi.json
cargo run --locked --offline -q -p finance-api -- export-read-stream /tmp/finance-read-stream
```

utoipa 从 handler 注解和实际响应类型导出。流生成器使用 F1 v1.0 行情事件和已有的行情状态 DTO。导出不依赖部署数据根、账户或网络。Finance API CI 比较两次导出的字节，并将流产物与 `protocol/v1/` 中的文件逐一比对，成功后上传 `finance-openapi` 和 `finance-read-stream-v1` artifact。兼容测试用旧客户端类型检查现有字段，流样本来自已登记的固定合成输入。[WS/SSE 消息契约](market-protocol.md#首批读契约的-ws-与-sse-消息)

finance 合并后，web 先固定 `openapi/finance.commit` 的完整 SHA，从同一提交更新快照与客户端，并扩充 web 自己的契约。frontend 随后固定 `openapi/web.commit` 并生成客户端。fertile 最后更新已合并的子仓 gitlink 和集成快照 `contracts/finance.openapi.json`，并验证契约链。生成物由相应生成流程更新，破坏性接口变更需要兼容迁移计划。[fertile contracts](https://github.com/viramoon/fertile/tree/main/contracts)

历史价量补拉通过本地命令运行。入口、JSON 运行报告和 Parquet 目录见批任务文档。[历史价量批任务](historical-pull.md)

历史期权读取与 OPRA 费用估算通过 collector 的 Rust 库调用。IBKR 读取使用 `IbkrOptionHistory` 和 F9a 的只读 `PaperMarket` 会话。已取得的批次可接入第三批 HTTP 查询，费用估算和期权流证据各自沿用独立边界。[历史期权链](option-history.md)

## 历史回测与提交确认

GET /api/v1/backtests/datasets 返回具名日线 catalog 的可选标的、来源和日期范围。POST /api/v1/backtests 接受 `SmaCrossover`、一个已导入的 `instrument`、UTC 午夜的 `start` / `end`，以及整数 `fast_period`、`slow_period`、`shares`。要求 `2 ≤ fast_period < slow_period ≤ 250` 与 `1 ≤ shares ≤ 1000`。研究结果的 `research` 字段含数据血缘、执行模型、覆盖与成交审计。无匹配 catalog 的新请求返回 422。缺失会话或 warmup 不够的作业进入失败终态，错误码为 `missing_sessions` 或 `insufficient_history`。不替换为示例数据。

新成功作业的 `summary.research.windows` 依序返回四行，`window` 为 `50y`、`10y`、`5y` 或 `longest_sample`。使用 `label` 展示覆盖不足的固定窗口，例如 `50y (insufficient)`。每行包含 `requested_years`、`requested_start`、`actual_start`、`actual_end`、`actual_years`、`calendar_start`、`calendar_end`、`observations`、`expected_sessions`、`missing_sessions`、`missing_pct`、`coverage` 和 `rf_source`。请求区间超出所选日历的可用范围时，行内附有 `coverage_reason="calendar_out_of_range"`。最长样本行的 `requested_years` 为 `null`。日期采用 `YYYY-MM-DD`，实际跨度及比例使用十进制字符串，数量为整数。

`coverage=insufficient` 时 `metrics` 为 `null`。满窗的 `metrics` 包含 `return_pct`、`cagr_pct`、`max_drawdown_pct`、`volatility_pct`、`turnover_pct` 和无量纲的 `sharpe`，计算值为十进制字符串，无法计算的单项为 `null`。已有的成功摘要仍可读取，可能只有三个固定窗口及原有字段。判定和计算口径见 [日线窗口](../python/README.md#日线窗口)。

官方作业不使用 FRED，HTTP 作业无需 FRED 序列 ID、`cosd` 或 `coed`。2024 年示例的 `DGS10` 和本次截止 2026-09-30 的 Ken French 日频均供归档与研究。官方四窗口无需导入这两类数据，Sharpe 使用 `rf=0`，结果标记 `rf_source="rf=0"`。

日历构造失败的行记录 `coverage_reason="calendar_unavailable"` 和 `calendar_diagnostic`，该行的 `calendar_start`、应有交易日数、缺失交易日数及缺失比例为 `null`。`calendar_end` 省略。

GET /api/v1/backtests/by-key 使用提交时的同一 `Idempotency-Key` 请求头。存在时返回原作业 ID，不存在时返回 404。经 web 查询时也使用提交凭据。详情继续用 GET /api/v1/backtests/{id}。回测条件和风险边界见 [历史回测](historical-backtest.md)。

通过独立的本机 API 实例验收官方历史作业时，提交和轮询仍使用上述路由。使用方法见 [隔离运行官方历史作业](backtest-jobs.md#隔离运行官方历史作业)。
