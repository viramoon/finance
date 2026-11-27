# Binance 公共行情适配

`finance-binance` 读取 Binance 国际站的 BTC、ETH、SOL 对 USDT 现货与所选 USDⓈ-M 线性永续，使用 Nautilus 源码构建提供的 WebSocket 与 HTTP 客户端。程序提供公共行情读取和 F3 发布入口，访问路径限于公共 `exchangeInfo`、`depth`、`openInterest` 和公共行情流。历史价量归档由 F11 提供。

## 部署配置

默认配置文件为 `config/binance/<profile>.yaml`。仓库中的 `cloudbox`、`stable`、缺失配置文件与缺省 `enabled` 均为关闭。关闭时以 `disabled_by_profile` 成功退出，既不打开行情存储，也不启动传输子进程。沿用 `FERTILE_PROFILE`、`FINANCE_CONFIG_DIR`、`FERTILE_DATA_ROOT` 和 `FINANCE_PYTHON`。可用 `FINANCE_BINANCE_CONFIG` 指定经审核的 profile 文件。运行产物需要 Nautilus 源码构建环境，`PYTHONPATH` 指向本仓库 `python/`，`FINANCE_PYTHON` 指向该环境的解释器。

仅在公共行情可用的部署和用途经审批的产品上启用。下面的标签和权限只示意字段，部署时填入实际审批结果。现货固定采集三个交易对。永续白名单为 `BTCUSDT`、`ETHUSDT`、`SOLUSDT` 的非空子集。可省略整个现货或永续配置块，只启用另一侧。

```yaml
enabled: true
spot:
  license: approved.example
  display_live: true
  record_local: false
  derive_non_display: false
  backup_personal_drive: false
perpetual:
  symbols: [BTCUSDT, ETHUSDT, SOLUSDT]
  market:
    license: approved.example
    display_live: true
    record_local: false
    derive_non_display: false
    backup_personal_drive: false
  context:
    license: approved.example
    display_live: true
    record_local: false
    derive_non_display: false
    backup_personal_drive: false
```

每个启用的产品都须明确给出许可标签和四项用途权限。至少具有展示或本地记录权限，云备份要求本地记录权限。适配器按 `(source, product)` 分别打开 [F3 MarketIngress](../market-wal.md) 并提交事件。获准落盘的前缀每 250 ms 尝试提交，断连和退出时也提交。仅展示数据进入 F3 的短时内存窗口。上层可使用库的 `Observer::display` 接收已获准展示的事件，CLI 只输出状态。许可政策变更按 F3 的新数据根流程处理。

## 对接标识和精度

| source / product | 标的示例 | channel | 事件 |
| --- | --- | --- | --- |
| `binance/spot` | `BTC-USDT.BINANCE` | `depth`、`trades` | F1 v1.0 盘口和原始成交 |
| `binance/usdt-perp` | `BTC-USDT-PERP.BINANCE` | `depth`、`trades` | F1 v1.0 盘口和聚合成交 |
| `binance/usdt-perp-context` | `BTC-USDT-PERP.BINANCE` | `funding_rate`、`open_interest` | [v2.0 永续上下文](../market-protocol.md#永续上下文-20) |

从对应市场的 `exchangeInfo` 读取价格和数量步长，并验证 USDT 线性永续类型和保证金币种。标的使用 UTC 24×7 日历，永续以 USDT 现金结算，固定乘数为 1。现货成交使用 `trade` 的 `t`，永续使用 `aggTrade` 的 `a`。买方为 maker 时主动方为卖出。相同流在进程内按已观察到的原生 ID 去重。永续资金费率来自 `markPrice`，`rate` 是无单位比率，`next_funding_ts` 是交易所给出的下次资金时间。源端 `T` 缺失或非正数时填 `null`，仍发布该费率。OI 来自公共 REST，立即读取后以每轮约 60 秒轮询，`quantity` 按交易所提供的基础资产数量保留，`exchange_ts` 用源返回的观测时间。金额相关字段在 F1 和 F3 中保留十进制字符串，交易所 ID 保留原生整数值。快照原生 ID 为 `lastUpdateId`，增量用 `u`。现货快照无源时间，`exchange_ts` 使用桥接帧的事件时间作为序列对齐上界。

## 深度同步和状态

- 订阅差分深度后缓存收到的更新，再请求每侧最多 1,000 档的 REST 快照。现货首条可用帧覆盖 `lastUpdateId + 1`，之后覆盖下一 ID。USDⓈ-M 首条帧覆盖 `lastUpdateId`，之后必须满足 `pu == 上一条 u`。快照已覆盖的旧帧及重复帧丢弃。每档更新为绝对数量，零数量删除价位。
- 桥接成功才发布快照和增量。先检查原生序号，再用与 F1 相同的精确运算核对增量价量步长，通过后才缓存或应用。连续序号的增量若不符合价格或数量步长，使用 `binance_depth_invalid_increment` 报告缺口。断序、无法应用的增量、缓冲溢出或会话断开时丢弃簿，发出 `gap` 和新 epoch 的 `reset`，随后重新连接并请求快照。单个深度流发生缺口会重建本进程的公共会话，其余已启动流标记为断连。恢复后 F3 记录当前簿从快照恢复及历史缺口不可补齐两种结果。历史缺量未知，恢复簿计数为 1。
- CLI 的 JSON 状态为 `disabled_by_profile`、`connecting`、`synchronizing`、`live`、`recovering`、`rate_limited`、`region_restricted` 或 `stopped`。 `live` 表示所选深度流已完成快照桥接。暂时断开以 1–60 秒退避重连。418/429 至少等待 60 秒，`Retry-After` 秒数或 HTTP 日期可延长等待，单次等待上限 24 小时。
- REST 或 WebSocket 握手返回 403/451 时，关闭公共会话，记录 `region_restricted` 及 `http_status`，成功退出并停止自动重试。部署守护应设置为仅失败重启。核对适用地区与产品可用性后，运维可通过新一次显式启动重新取证。

进程使用原有 `finance-state/market-wal` 与投影目录，不占用监听端口。现有 HTTP 行情状态读取 bar collector；F4 状态目前通过日志和本地库接口提供，产品水位、缺口和封段仍通过 F3。调试时使用[合成报文](../../crates/finance-collector/fixtures/binance/README.md)。
