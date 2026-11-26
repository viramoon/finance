# IB Gateway paper 接入

finance-ib 使用与锁定 Nautilus 源码一致的 Rust ibapi 3.2.0。独立 Rust 采集进程可直接消费 session 和恢复钩子，不需要在 Python TradingNode 中运行适配器。Nautilus 仍负责回测和 Parquet catalog。当前连接层仅请求账户清单、server time 及只读行情，trader 尚未实现。

启动时按 base → FERTILE_PROFILE（cloudbox / stable）→ 环境变量合并 ib 段。client_id 分别为 probe 91、collector 92、预留 trader 93；多客户端同机运行时须使用不同 ID。可用 IB_GATEWAY_ADDRESS、IB_PROBE_CLIENT_ID、IB_COLLECTOR_CLIENT_ID、IB_TRADER_CLIENT_ID 和 IB_PAPER_ACCOUNT_PREFIX 覆盖。默认端点 127.0.0.1:4002。4001 和 TWS live 7496 在网络连接前拒绝。所有受管账户都必须匹配 DU 开头的 paper 前缀，未取到账户时不可订阅。probe 在终端输出完整账户列表用于人工核验，保存或分享证据前须脱敏。

| 配置字段 | 环境覆盖 | 单位 |
| --- | --- | --- |
| connect_timeout_ms | IB_CONNECT_TIMEOUT_MS | 毫秒，含连接握手与受管账户验证 |
| heartbeat_ms、read_timeout_ms | IB_HEARTBEAT_MS、IB_READ_TIMEOUT_MS | 毫秒，应用层请求 server time |
| backoff_initial_ms、backoff_max_ms、jitter_ms | IB_BACKOFF_INITIAL_MS、IB_BACKOFF_MAX_MS、IB_JITTER_MS | 毫秒，指数增长加随机抖动后的总间隔有上限 |

Gateway 可能仍能响应 server time，但交易所行情连接已经中断。连接管理器从握手前订阅 IB 系统通知，收到 1100/1101/1102、1300、2103、2105 或 2110 就关闭旧会话并重走账户校验、订阅、补数与对账；正常的 2104 行情场连接成功通知不会触发重连。

## 手工核验

在已完成 paper 登录的盒子运行下列命令。登录和凭据归 fertile 管理，探针不加载账户环境文件。仅当显式启用 IB_LIVE_TEST 时才执行真实探针测试。

```sh
cargo run --locked -p finance-ib -- probe
IB_LIVE_TEST=1 cargo test --locked -p finance-ib --test paper opt_in_paper_probe -- --ignored --exact --nocapture
```

`--test paper` 只运行探针。分钟线的真实 Gateway 验证使用独立测试目标 `paper_historical_minute`，须另行确认数据拉取与本地记录授权。运行参数见 [历史价量拉取](../historical-pull.md)。

无法连接时检查 TCP 端点和 Gateway 日志；认证或双因素未完成时完成 paper 登录后重试。混合/实盘账户立即退出，修复 Gateway 会话后重新启动，不能以更换 profile 替代账户校验。连接超时和 server time 读超时均产生非零退出码。成功 probe 只表示当前连接与身份检查成功；订阅权限、连续采集、补数以及真实交易时段的数据新鲜度须另行验证。

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

[历史价量批任务](../historical-pull.md) 另提供常规交易时段的一分钟 `TRADES`。命令复用 Gateway 端点和受管账户校验，使用 probe client ID。使用同一个已配置的外部数据根维持批任务跨进程和重启的 pacing 状态。

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
