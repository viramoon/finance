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

未知 YAML 字段、未知 profile、非法 IP / 端口、相对数据根路径在启动时拒绝。配置不读取账户密钥。当前无认证，默认 loopback；部署方只在受控内网显式覆盖绑定地址，不能作为公网写操作入口。更丰富的查询、认证和受控交易接口由后续独立 issue 确定。

## 接口与状态

- GET /healthz/live：进程存活返回 200，status=ok，checks 含 process=true。
- GET /healthz/ready：实际 read_dir 数据根目录成功返回 200 / ready，否则 503 / not_ready；checks 含 data_root_readable 布尔值。目录没有回测结果仍可 ready；这不是交易、行情新鲜度或 Gateway 健康证明。
- GET /api/v1/backtests/sample：读取数据根下 finance-state/backtests/sample/latest.json。200 返回 run_id、strategy、instrument、start、end、total_orders、total_positions、pnl、currency、generated_at。时间为 UTC RFC3339（序列化时可规范化等价格式）；计数为整数，pnl 为精确十进制字符串，禁止 JSON 数字。当前示例 PnL 是 USD 账户最终余额减初始余额，不使用浮点统计接口；上游示例不下单，因此当前订单 / 持仓计数和 PnL 为零。

没有结果为 404，统一错误体包含 error.code=sample_not_found 和 error.message。不可读为 500 / sample_unreadable，损坏 JSON、字段缺失或金额非法为 500 / sample_invalid；响应不暴露文件路径和底层错误。liveness / readiness 使用健康体及逐项 checks。

## 行情与作业

GET /api/v1/market/status 从 collector 持久状态读取各标的水位和新鲜度。没有状态文件时返回明确的缺失状态，collector 停止后会反映过期。可用性和精度验证见 [行情目录](../catalog/README.md)。

IBKR 顶档订阅与权限验收由 `finance-ib` 的 Rust 接口和私有 CLI 提供，运行时独立于 HTTP API 和 collector 持久状态。接口与权限结论见 [IBKR 接入](integrations/ib.md#美股-l1-和-opra-权限验收)。

IBKR 行情线预算、共享事件、已确认期权链和覆盖区间由 `finance_collector::line_budget::LineHandle` 提供。该接口属于进程内 Rust 调用，HTTP OpenAPI、`GET /api/v1/market/status` 的持久状态来源和 `protocol/v1` 流契约维持原约定。[IBKR 行情线预算](integrations/ib-line-budget.md)

POST /api/v1/backtests 接收带 Idempotency-Key 的任务并返回 202。GET /api/v1/backtests/{id} 查看执行进度与摘要，GET /api/v1/backtests 按 cursor 和 limit 翻页。提交使用同库事务完成幂等检查和容量检查，worker 独立执行。完整输入范围、超时和恢复见 [作业运维](backtest-jobs.md)。公网入口由 web 鉴权并限流。

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

WAL 的 `records[].event` 和投影的 `record.event` 保留原始 `schema_version`。同一 WAL 页面可以包含 F1 `1.0` 和 F5 `2.0` 事件。消费端按每条事件的版本解码，`2.0` 事件的 `kind` 为 `perpetual_context`。首批独立导出的 WS/SSE 消息继续使用 `1.0`。[版本化行情协议](market-protocol.md)

市场响应的 `access` 来自 F3 的落盘策略，含 `license`、`display_live`、`record_local`、`derive_non_display`、`backup_personal_drive`。事件另携带原 `license`。仅展示产品由生产者在内存处理，不进入这些查询和生成物。项目禁止再分发。参数不合法返回 `400 / market_query_invalid`。产品未登记或目标投影位置缺失返回 `404 / market_product_not_found` 或 `market_projection_not_found`。策略、索引或 WAL 损坏返回 `500 / market_storage_error`，响应省略本地路径和数据正文。

因子查询读取由离线研究程序显式发布到 `FERTILE_DATA_ROOT/finance-state/factor-research/read-v1.json` 的本地快照。响应包含 `schema_version: 1`、快照的 UTC `as_of`、`items` 和 `next_cursor`。`after` 排除游标本身，按完整游标文本的字典序翻页，`limit` 默认 50、最大 100。接续时保留筛选项，若 `as_of` 变化则重新分页。条目保留授权，普通因子和评价附 `data_version`。报告 Decimal 字段为字符串，未知值用 `null`。无快照时返回空列表和 `as_of: null`。请求无效为 `400 / invalid_factor_query`，已发布快照内找不到评价请求指定的因子或因子版本时为 `404 / factor_not_found`，快照无效或不可读为 500。[离线因子评价与发布](factor-research.md)

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

## 历史回测与提交确认

GET /api/v1/backtests/datasets 返回具名日线 catalog 的可选标的、来源和日期范围。POST /api/v1/backtests 接受 `SmaCrossover`、一个已导入的 `instrument`、UTC 午夜的 `start` / `end`，以及整数 `fast_period`、`slow_period`、`shares`。要求 `2 ≤ fast_period < slow_period ≤ 250` 与 `1 ≤ shares ≤ 1000`。研究结果的 `research` 字段含数据血缘、执行模型、覆盖与成交审计。无匹配 catalog 的新请求返回 422。缺失会话或 warmup 不够的作业进入失败终态，错误码为 `missing_sessions` 或 `insufficient_history`。不替换为示例数据。

新成功作业的 `summary.research.windows` 依序返回四行，`window` 为 `50y`、`10y`、`5y` 或 `longest_sample`。使用 `label` 展示覆盖不足的固定窗口，例如 `50y (insufficient)`。每行包含 `requested_years`、`requested_start`、`actual_start`、`actual_end`、`actual_years`、`calendar_start`、`calendar_end`、`observations`、`expected_sessions`、`missing_sessions`、`missing_pct`、`coverage` 和 `rf_source`。请求区间超出所选日历的可用范围时，行内附有 `coverage_reason="calendar_out_of_range"`。最长样本行的 `requested_years` 为 `null`。日期采用 `YYYY-MM-DD`，实际跨度及比例使用十进制字符串，数量为整数。

`coverage=insufficient` 时 `metrics` 为 `null`。满窗的 `metrics` 包含 `return_pct`、`cagr_pct`、`max_drawdown_pct`、`volatility_pct`、`turnover_pct` 和无量纲的 `sharpe`，计算值为十进制字符串，无法计算的单项为 `null`。已有的成功摘要仍可读取，可能只有三个固定窗口及原有字段。判定和计算口径见 [日线窗口](../python/README.md#日线窗口)。

官方作业不使用 FRED，HTTP 作业无需 FRED 序列 ID、`cosd` 或 `coed`。2024 年示例的 `DGS10` 和本次截止 2026-09-30 的 Ken French 日频均供归档与研究。官方四窗口无需导入这两类数据，Sharpe 使用 `rf=0`，结果标记 `rf_source="rf=0"`。

日历构造失败的行记录 `coverage_reason="calendar_unavailable"` 和 `calendar_diagnostic`，该行的 `calendar_start`、应有交易日数、缺失交易日数及缺失比例为 `null`。`calendar_end` 省略。

GET /api/v1/backtests/by-key 使用提交时的同一 `Idempotency-Key` 请求头。存在时返回原作业 ID，不存在时返回 404。经 web 查询时也使用提交凭据。详情继续用 GET /api/v1/backtests/{id}。回测条件和风险边界见 [历史回测](historical-backtest.md)。

通过独立的本机 API 实例验收官方历史作业时，提交和轮询仍使用上述路由。使用方法见 [隔离运行官方历史作业](backtest-jobs.md#隔离运行官方历史作业)。
