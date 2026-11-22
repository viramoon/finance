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
IB_LIVE_TEST=1 cargo test --locked -p finance-ib --test paper -- --ignored --nocapture
```

无法连接时检查 TCP 端点和 Gateway 日志；认证或双因素未完成时完成 paper 登录后重试。混合/实盘账户立即退出，修复 Gateway 会话后重新启动，不能以更换 profile 替代账户校验。连接超时和 server time 读超时均产生非零退出码。成功 probe 只表示当前连接与身份检查成功；订阅权限、连续采集、补数以及真实交易时段的数据新鲜度须另行验证。
