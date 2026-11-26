# IB Gateway paper 接入

finance-ib 使用 Rust ibapi 3.2.0 的本地源码补丁，保留 Nautilus 锁定的 SDK 版本。独立 Rust 采集进程可直接消费 session 和恢复钩子，不需要在 Python TradingNode 中运行适配器。Nautilus 仍负责回测和 Parquet catalog。当前连接层仅请求账户清单、server time 及只读行情，trader 尚未实现。补丁边界及来源见 [ADR-0038](../adr/ADR-0038-ibapi-market-data.md)。

启动时按 base → FERTILE_PROFILE（cloudbox / stable）→ 环境变量合并 ib 段。client_id 分别为 probe 91、collector 92、预留 trader 93；多客户端同机运行时须使用不同 ID。可用 IB_GATEWAY_ADDRESS、IB_PROBE_CLIENT_ID、IB_COLLECTOR_CLIENT_ID、IB_TRADER_CLIENT_ID 和 IB_PAPER_ACCOUNT_PREFIX 覆盖。默认端点 127.0.0.1:4002。4001 和 TWS live 7496 在网络连接前拒绝。所有受管账户都必须匹配 DU 开头的 paper 前缀，未取到账户时不可订阅。probe 在终端输出完整账户列表用于人工核验，保存或分享证据前须脱敏。

| 配置字段 | 环境覆盖 | 单位 |
| --- | --- | --- |
| connect_timeout_ms | IB_CONNECT_TIMEOUT_MS | 毫秒，含连接握手与受管账户验证 |
| heartbeat_ms、read_timeout_ms | IB_HEARTBEAT_MS、IB_READ_TIMEOUT_MS | 毫秒，应用层请求 server time |
| backoff_initial_ms、backoff_max_ms、jitter_ms | IB_BACKOFF_INITIAL_MS、IB_BACKOFF_MAX_MS、IB_JITTER_MS | 毫秒，指数增长加随机抖动后的总间隔有上限 |

Gateway 可能仍能响应 server time，但交易所行情连接已经中断。连接管理器从握手前订阅 IB 系统通知，收到 1100/1101/1102、1300、2103、2105 或 2110 就关闭旧会话并重走账户校验、订阅、补数与对账；正常的 2104 行情场连接成功通知不会触发重连。Finance 的 paper 客户端禁用 SDK 内部自动重连，TCP 断开即结束该连接的账户核验和订阅。SDK 在握手和运行期分发时识别这些故障码，全局通知、带请求 ID 的通知以及空闲时到达的通知都能结束旧连接。collector 由连接管理器按退避策略新建连接，每次先重新校验受管账户。新连接返回空清单、混合账户或实盘账户时，管理器关闭连接并停止恢复。

## 手工核验

以下命令仅用于获授权的私有部署，Gateway 须已登录 paper 并开启只读 API。登录和凭据归 fertile 管理，探针不加载账户环境文件。真实探针测试须显式设置 `IB_LIVE_TEST=1`。

```sh
cargo run --locked -p finance-ib -- probe
IB_LIVE_TEST=1 cargo test --locked -p finance-ib --test paper opt_in_paper_probe -- --ignored --exact --nocapture
```

无法连接时检查 TCP 端点和 Gateway 日志；认证或双因素未完成时完成 paper 登录后重试。混合/实盘账户立即退出，修复 Gateway 会话后重新启动，不能以更换 profile 替代账户校验。连接超时和 server time 读超时均产生非零退出码。成功 probe 只表示当前连接与身份检查成功；订阅权限、连续采集、补数以及真实交易时段的数据新鲜度须另行验证。

## 美股 L1 和 OPRA 权限验收

探针用现有 paper 配置和 probe client ID。Gateway 须已登录 paper 并启用只读 API。程序拒绝标准 live 端口，确认所有受管账户符合 paper 前缀后才请求合约详情和行情。它逐标的确认 SMART USD 合约并开启一条顶档流，在最多 8 秒的观察窗后写入取消请求。只有本地写入成功才开始下一项。观察窗之外的连接、合约确认和开始订阅使用现有配置的连接或读超时。IB 请求延迟模式以允许回退，SDK 补丁按 `reqId` 把 Gateway 的 Protobuf `marketDataType` 回调送到对应订阅。

`equity_l1` 覆盖 QQQ、SPY、AAPL、MSFT、NVDA、AMZN、GOOGL、META、TSLA。`opra` 使用操作者选定的、尚未到期且活跃的 SMART USD 美股期权。操作者须在账户产品清单核实 A/B/C 股票网络持续行情或 OPRA 的 API 产品权限及 paper 数据共享，再分别传入相应的 `--confirm-...-permission`。免费美股实时报价和回调 `1` 本身不足以确认综合覆盖。

```sh
# 在已获授权的 paper 盒子上，设 OPRA_CON_ID 为选定合约的正整数 conId。
IB_LIVE_TEST=1 scripts/check-ib-entitlements.sh \
  --opra-con-id "$OPRA_CON_ID" \
  --confirm-equity-l1-permission --confirm-opra-permission
```

可省略各项人工确认标志。省略 `--opra-con-id` 时 OPRA 保持 `pending/option_not_selected`，启动失败仍记录于报告的 `failure` 和股票结果。脱敏 JSON 按产品给出结论和各目标原因，附实际档位、双边报价有效性、接收时间、交易时段和期权 OI/Greeks 是否收到。期权用固定目标标签。摘要省略账户、地址、合约 ID、行情值和供应商原始错误文本。双边时间取两个价格接收时间中较早者。可识别的权限、数据共享和限频通知映射为固定原因。退出码 0 表示扫测完成，产品是否通过由 JSON 判断。启动失败或因连接、订阅超时或取消失败中断时返回非零。若已有非空的 `IBAPI_RECORDING_DIR`，探针以 `invalid_configuration` 拒绝连接，以免 SDK 写出原始报文。

探针在关闭连接并等待 SDK 分发任务退出后输出报告。扫测中断时，已完成标的保留原结论，当前标的保留已收到的证据，未开始的标的记为 `sweep_stopped`。未选择的 OPRA 合约始终记为 `option_not_selected`。当前请求已收到明确无许可通知时，标的保留 `not_entitled`，顶层 `failure` 另记中断原因。

| 扫测中观察到的故障 | 顶层 `failure` 与后续动作 |
| --- | --- |
| 合约确认、订阅、读取或取消首次检测到断线，或收到使会话失效的 IB 通知 | `gateway_disconnected`，停止扫测。取消报文已写出后发现断线也适用。读取已发现断线时，后续取消结果保留该原因。 |
| 开始订阅时超时，或出现其他传输错误 | 分别为 `subscription_timeout` 或 `transport_failure`。订阅行的分配结果未知，停止扫测。 |
| 取消写入遇到其他错误或超过时限 | `cancel_failed`，停止扫测。 |
| 单个合约确认超时，连接仍可用 | 当前标的记为 `request_timeout`。此阶段尚未分配行情行，继续下一项。 |
| 读取遇到请求级错误或流结束，取消成功且会话可用 | 当前标的保留请求原因，继续下一项。 |

| 结论 | 判断依据 |
| --- | --- |
| `passed`（通过） | 常规交易时段的实际类型 `1`、同请求的有效双边报价、已独立核实的账户产品权限。 |
| `delayed`（延迟） | 常规交易时段的实际类型 `3` 或 `4`，且有相应的有效报价。`4` 表示延迟冻结。 |
| `pending`（待验证） | 类型 `2`（冻结）、无回调、未知类型、报价不完整、权限待确认、休市、时段未知或会话故障等，详见 `reason`。 |
| `not_entitled`（无许可） | 当前请求收到明确的 IB 无权限证据。 |

已知休市和未知交易时段依据合约详情 `liquidHours` 及其时区区分。采样只能说明所选请求的观测结果。OPRA 权限通过与 OI/Greeks 新鲜度、可用性和许可分别核验。默认测试使用内存合成 Protobuf 和合成探针结果。真实 paper Gateway 测试默认跳过，真实产品权限全部待验证。以下 opt-in 命令仅用于获授权的私有部署，可通过 `IB_OPRA_TEST_CON_ID` 选择合约。

```sh
IB_LIVE_TEST=1 cargo test --locked -p finance-ib --test paper opt_in_paper_entitlements -- --ignored --exact --nocapture
```

### 行情请求接口

`finance_ib::market` 提供 `MarketDataSource::confirm` 和 `subscribe`。实时实现通过 `ConnectionManager::open_market` 打开已验证的专用 paper 会话，并固定该会话的请求档位以允许 IB 回退。订阅只接受本会话内已确认的合约。`MarketRequest::key()` 提供 `(con_id, product, parameters)`，其中参数包含是否请求期权 OI。`MarketSubscription` 提供请求 ID、事件读取和显式取消。取消成功表示报文已在本地写完且返回时会话仍可用，Gateway 不提供取消回执。成功后可重复取消，读取返回流结束。订阅或取消写入失败、超时、被调用方中断，或调用方直接释放活动订阅，都会使整个会话失效，唤醒等待中的读取并拒绝新请求。要继续复用会话，调用方须显式等待 `cancel` 成功。会话拥有者可通过 `PaperMarket::disconnected` 等待失效，通过 `close` 结束分发并释放底层连接。断线后须重新调用 `open_market` 核验账户，再在新会话中确认合约。F9b 接管限速、并发预算、排队、去重和期权链发现。

行情流提供实际类型、L1 bid/ask/last 价格与数量、期权 OI、带来源的 Greeks 和脱敏通知。顶档 Protobuf 数量和 `TickSize` 传来的 OI 用原十进制字符串构造 F1 `DecimalText`，保留有效范围内的全部位数及小数位。期权 OI 使用 streaming generic tick `101`，显式零值与缺失分别处理。`TickGeneric` 的数值在供应商报文中为 binary64，OI 只接受能精确转成整数的值。顶档价格和期权计算字段由 Protobuf 传输 binary64，在进入 finance 时转成可往返的 `DecimalText`。Greeks 区分 bid、ask、last 和 model，无法表示或缺失的字段留空。事件保留本地 UTC 接收时间。last 更新为顶档采样。期权实时异动线索、历史逐笔核验和场所深度由后续任务处理。

F9a 的行情和报告驻于会话内存，私有脚本只输出摘要。许可策略及后续投影接入由相应 issue 处理。HTTP OpenAPI 与 `protocol/v1` schema 维持原版本。

## 采集模式与数据目录

在已加载环境、从源码构建 Nautilus 的盒子上运行。Gateway 由 fertile 托管，只开启 paper 的只读 API。collector 使用独立持久目录；finance-api 读取同一目录。请为 replay、ib、ib-delayed 分别分配目录，否则 writer 会拒绝来源或水位不匹配的启动。

```sh
source /home/box/fertile/env.sh
export FINANCE_COLLECTOR_CONFIG=config/collector/ib-cloudbox.yaml
export FERTILE_DATA_ROOT=/home/box/fertile/data-paper
mkdir -p "$FERTILE_DATA_ROOT"
cargo run --locked -p finance-collector
```

在配置中填写一至三个美股或 ETF 的完整标识，如 SPY.ARCA、AAPL.NASDAQ。collector 通过 SMART 路由，标识后缀指定 primary exchange，币种 USD。当前支持 1-MINUTE-LAST。实时模式请求 IB keepUpToDate 的一分钟 TRADES，包括扩展时段，重复的当前分钟更新替换累积值。收到下一分钟的更新后交付前一根完整 bar。持久化时间戳是收盘时刻 UTC 纳秒；休市、无成交或权限不足时不会写入模拟数据。单分钟的最后一根可能在下一笔或重连历史查询时确认。

ibapi 3.2.0 的公开历史 / 流式 OHLCV 字段为 binary64。适配器按可往返的最短十进制表示转换并验证，无法用精确 Decimal 表示时拒绝入库。writer 以后续定点格式校验及写入。上游已丢失的原始精度无法从 SDK 恢复。

实时权限不足时，先停止 collector，再用单独数据根及 config/collector/ib-delayed.yaml 启动，同时调整读取方的数据根。此模式选择 IB 的 Delayed 数据类型并轮询至少滞后 20 分钟的 HMDS 历史成交，支持该交易所的账户权限仍是前提。状态标记 ib-delayed，每分钟轮询，按实际收盘时刻判断年龄，示例阈值 1500 秒。实时模式标记 ib-realtime，示例阈值 180 秒。354 / 10167 / 162 权限错误需核对订阅、paper 账户数据共享及历史数据许可。

## 重连与排查

每次会话对受管账户重新验证，再订阅所有标的并排队实时更新。逐标的从持久水位到切换时点按最长一小时查询历史，裁剪开闭边界、按时间去重后交给 writer。首次启动默认取最近两分钟的闭合 bar；延迟模式取延迟窗口之前两分钟。恢复期间 /healthz/ready 返回 503；全部回补、目录可写、每个标的数据均新鲜后才返回 200。/status 和 finance-api 的 /api/v1/market/status 给出每标的最后时刻和年龄。收盘后年龄会增长，持续休市导致的 503 应结合交易日历解释。

历史请求、实时订阅及重试共用限速，任意两个请求至少间隔 16 秒，重连时相同请求也避开 15 秒限制。100、420 和带 pacing 文本的 162 错误延长退避，单次历史请求最多四次尝试。若仍失败，保留已有水位并重新建立验证后的会话。日志中检查 IB historical recovery 的标的、起止纳秒与记录数；无数据时核对交易时段、标的映射和权限，不能通过修改 checkpoint 跳过失败区间。

## 纸账户断网演练

在确有成交的时段，按 [RUNBOOK](../../RUNBOOK.md) 采集至少 30 分钟。记录两端健康状态、目录持久水位、演练前后 UTC 采样及 IB 日志。另开终端运行下列命令，脚本使用 sudo iptables 只丢弃发往指定 IPv4 回环地址上的 paper Gateway 4002 的本机 TCP 包，默认 120 秒。它影响同一目的地址和端口的所有本地客户端，应选定演练窗口。中断或到期会删除带本次唯一标识的规则。

```sh
scripts/fault-ib-outage.sh 120
curl -sS http://127.0.0.1:8082/healthz/ready
curl -sS http://127.0.0.1:8081/api/v1/market/status
```

在演练期间另外轮询 ready，预期为 503，恢复后为 200。读回实际行情以报告断线窗口的闭合 bar 时间和数量；核对 IB 返回的相同时间窗、最后水位和重复键，标注休市或无成交分钟。示例按标的和 UTC 时段读取 Parquet：

```sh
export DRILL_START_NS=... DRILL_END_NS=...
"${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}/venv/bin/python" - <<'PY'
import os
from pathlib import Path
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog
catalog = ParquetDataCatalog(Path(os.environ["FERTILE_DATA_ROOT"]) / "catalog")
rows = catalog.bars(bar_types=["SPY.ARCA-1-MINUTE-LAST-EXTERNAL"],
                    start=int(os.environ["DRILL_START_NS"]), end=int(os.environ["DRILL_END_NS"]))
timestamps = [bar.ts_event for bar in rows]
assert timestamps and timestamps == sorted(set(timestamps)), timestamps
print(len(rows), timestamps)
PY
```

盒子外可运行 cargo test --locked -p finance-ib -p finance-collector。真实行情连续性、账户权限与 2FA 只有已登录的 Gateway 能验证；由 fertile#9 留存真实窗口的脱敏演练结果。
