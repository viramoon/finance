# 配置分层设计

部署配置按 base → 选定 profile 覆盖 → 受控环境变量 / 密钥引用加载；未知字段、非法路径和缺失必要项应在未来启动校验中拒绝。当前 API 已实现 base.yaml / profile 文件及环境变量覆盖，实际字段仅 api.bind_host、api.port 和 paths.data_root，详见 [API](../docs/api.md)；下面其他交易字段仍为未来设计，不混入当前严格配置。

- `base.yaml`：跨环境字段、单位、默认值和验证规则。
- `cloudbox.yaml`：云电脑根目录与网络参数覆盖，当前只允许 IBKR paper。
- `stable.yaml`：稳定机器路径和网络覆盖；不自动切换账户，实盘需单独显式选择和验证。
- `FERTILE_PROFILE`：仅 cloudbox / stable。环境与运行模式是两个独立维度：Backtest / Sandbox / Live；paper 和实盘属于 Live 的账户配置。

## 规划字段

以下为字段说明用 YAML，TBD 的项目待 Nautilus 版本锁定和联调后填充；字段名属于本项目设计，不宣称与某个上游构造函数同名。

```yaml
profile: cloudbox
paths:
  root: /home/box/fertile
  catalog: /home/box/fertile/data/catalog
  state: /home/box/fertile/data/finance-state
  logs: /home/box/fertile/logs/finance
  backtest_results: /home/box/fertile/data/backtests
  backups: /home/box/fertile/backups/finance
runtime:
  mode: Live # Backtest / Sandbox / Live，与 profile 独立
  account_kind: paper # 实盘不能由 stable profile 隐式启用
  account_secret_ref: TBD # 账户凭据仅引用，不存值
ib_gateway:
  host: TBD # 部署配置确认的 Gateway 地址
  port: 4002 # paper Gateway API 端口，连接前验证配置
  client_ids:
    trader: TBD # 各进程唯一，联调分配
    collector: TBD
  read_only: TBD # collector 为只读，trader 按权限显式配置
  subscriptions: TBD # 账户权限、标的、行情种类
connectivity:
  heartbeat_interval_ms: TBD
  read_timeout_ms: TBD
  reconnect_initial_ms: TBD
  reconnect_max_ms: TBD
  reconnect_jitter: TBD
  resubscribe: true
  historical_backfill: TBD # 数据范围、限流和断点
reconciliation:
  enabled: true
  startup_timeout_ms: TBD
  recover_open_orders: true
  snapshot_sources: TBD # 账户、持仓、订单、成交核对
risk:
  max_market_data_age_ms: TBD
  pause_on_uncertain_orders: true
  resume_after_reconciliation: true
catalog:
  format: parquet
  writer_ownership: collector
  backup_schedule: TBD
  retention: TBD
api:
  bind_host: 127.0.0.1
  port: 8081
  auth_secret_ref: TBD
health:
  path: /healthz
  live: process
  ready_dependencies: TBD
observability:
  metrics_address: TBD
  log_level: TBD
  redact_fields: TBD
```

上述路径均为配置默认值，不能写死在代码里。环境变量包括 `FERTILE_PROFILE`、`FERTILE_DATA_ROOT`、`IB_GATEWAY_ADDRESS`，实际命名和优先级在实现时统一验证；账户密钥以运行环境的受控引用提供，测试不读取真实账户环境变量。profile 仅覆盖部署差异，不能出现 cloudbox / stable 两套业务代码。
