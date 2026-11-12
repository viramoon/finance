# IBKR 接入设计

使用 Nautilus 上游 IB adapter（基于社区 ibapi），当前仅 IBKR paper。IB Gateway paper API 端口规划为 **4002**，必须与实际 Gateway 配置核对；不把其他 API 的网络现象推定为 Gateway 可用。Gateway 的容器生命周期可使用 Nautilus 提供的能力，当前没有容器或启动实现。

| 配置 | 规划与未定项 |
| --- | --- |
| Gateway 地址 | 从 IB_GATEWAY_ADDRESS 或配置读取；host TBD，禁止把凭据放入仓库 |
| client_id | trader、collector 使用独立 ID，具体编号与冲突检测 TBD；api 不直接建立 IB 会话，除非另行 ADR |
| 只读模式 | collector / 研究仅行情权限；trader 的写权限须显式配置，测试不读取真实账户环境变量 |
| 重连 | 心跳、读超时、指数退避与抖动，参数与上游字段映射 TBD；恢复后核对订阅和订单 |
| 数据订阅 | 标的、交易所、账户行情权限、延迟 / 实时类型、速率限制 TBD；在独立接入验证中确认 |
| 订单 | 唯一客户端 ID / orderRef；订单、成交、持仓对账；超时先查后试 |

真实协议连接验证、账户认证、行情权限、长连接和故障注入须分别留证据。读请求与写请求不能共用盲重试规则；准备好暂停开仓与恢复对账后才能进入 paper 联调。部署环境不决定运行模式，stable profile 也不能隐式启用实盘。

参考：[Nautilus IB 文档](https://github.com/nautechsystems/nautilus_trader/blob/develop/docs/integrations/interactive_brokers.md)、[rust-ibapi](https://github.com/wboayue/rust-ibapi)。职责与测试设计见 [connectivity.md](../connectivity.md)。
