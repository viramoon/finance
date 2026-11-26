# IBKR 行情线预算与期权链轮询

`finance_collector::line_budget` 提供美股和 OPRA 的只读进程内接口。调用方以 `LineScheduler::new(BudgetConfig::default(), watchlist())` 创建调度器，取出可克隆的 `LineHandle`，用 `LineScheduler::run_paper(&manager, stop)` 显式启动一个调度器。`manager` 是现有 `ConnectionManager<IbTransport>`，每次连接都核验 paper 账户。调用方持有唯一的专用行情 client ID。默认沿用 probe client ID，运行调度时该 ID 留给调度器。`stop` 是 `tokio::sync::watch` 的布尔接收端，发送 `true` 或关闭发送端时停止调度。

## 预算与轮询

| Rust 配置 | 默认值 | 含义 |
| --- | --- | --- |
| `account_lines` | 100 | TWS 和 API 共用的用户额度，按账户实测配置 |
| `stock_lines` | 9 | 常驻股票和 ETF L1 |
| `hot_lines` | 16 | 当前选择的热点期权 |
| `rotation_lines` | 45 | 可并发轮询的期权 |
| `reserve_lines` | 30 | TWS、深度相关占用及其他余量 |
| `request_interval` | 250 毫秒 | 发现、确认、订阅及取消的共享请求间隔 |
| `rotation_dwell` | 20 秒 | 从开启轮换订阅到达到取消条件的最长等待 |
| `chain_refresh` | 15 分钟 | 一轮发现结束后刷新合约链的间隔 |
| `reconnect_initial` / `reconnect_max` | 2 秒 / 60 秒 | 重连与单只股票确认重试的起始和最大退避 |
| `pacing_cooldown` | 15 秒 | 收到请求级或全局节流通知后的请求冷却 |
| `coverage_retention` | 1 小时 | 本地查询覆盖区间的时间窗口 |

三个 L1 池的额度之和须在账户额度扣除预留后的范围内。`watchlist()` 默认返回 QQQ、SPY、AAPL、MSFT、NVDA、AMZN、GOOGL、META、TSLA。可在启动时传入自定义股票列表与预算。热点行按 `set_selection(focus, hot_options)` 的选取顺序分配，重复的正整数 conId 合并为一个位置，超出的选择留在普通轮询范围。已有的轮换流进入热点时可直接提升为热点，同一个请求键只占一行。默认预算以无 Booster 的账户配额配置。

调度器在会话内确认标的并通过 `reqSecDefOptParams` 发现 SMART 系列。系列的到期日和行权价是查询线索。调度器请求指定到期日、交易类及乘数的合约详情，逐一验证返回的实际 conId、标的、币种、到期日、行权价及 C/P，只将确认的未过期期权放入候选。到期日按美东日历判断。轮询优先页面关注的标的，其次近到期和对已知股票价近平值的合约。每轮给其余候选机会。轮换使用请求 OI 的短时 streaming，得到 OI 和可用的完整 Greeks 或达到驻留阈值后，按共享请求节拍取消。取消等待和冷却会使实际占行时间长于驻留阈值。取消成功表示报文已写入且会话仍可用，之后才将该行用于新的合约。取消结果不确定则结束该会话，随后按退避重建并在新会话重新确认。旧会话关闭后，清空该会话的本地占用。

全局节流通知（无请求 ID，例如 IB 100、101、420）与请求级节流共用冷却规则。最新通知可延长冷却，已发出的请求继续等待结果。冷却期间暂停新的确认、订阅及取消，断连重试也须等到冷却截止。即使账户验证或行情类型选择失败，该次尝试收到的冷却仍对后续连接生效。

股票确认超时或响应丢帧时，该股票在原会话单独退避并排到等待确认的队尾。重试之间给其他就绪标的和链发现或轮换请求留出执行机会，退避短于请求间隔时也适用。确认恢复后开启常驻 L1，并把该标的纳入期权链发现。期权链元数据读取必须收到完整回调和结束标记。检测到丢帧时丢弃该次响应，发现状态保持不完整，下次刷新重试。

## 共享事件与覆盖查询

前台和后台分别调用 `LineHandle::reader(key)` 读取同一个 `(con_id, product, parameters)` 流。批量分析可用 `LineHandle::updates()` 接收所有键的尾部更新。股票使用 `Product::EquityL1` 和默认请求参数。期权使用 `Product::Opra`，参数 `option_open_interest = true`。读者只接收注册以后的事件，读者数量与排队、行数分配无关。`LineUpdate` 含请求键、调度器内的订阅代次以及 `Opened`、`Event` 或 `Closed` 消息。开始读取时可从首条收到的事件建立本地状态。收到 `Closed`、新代次或滞后错误时清空本地状态，再从后续事件重建。事件沿用 `finance_ib::market::MarketEvent` 的数据类型与精确小数表示。

`LineHandle::status()` 返回 `as_of`、连接阶段、账户预算、预留、各池使用量、已确认候选数和保留窗口内实际观察到的候选数。`option_contracts()` 返回当前已确认的合约。只有当 `discovery_complete` 为真时，才可把 `confirmed_options` 当作本轮配置范围的已确认合约分母。正在发现、确认失败或重连时，候选数可能只是已确认的子集，且可暂留上一轮确认的合约。候选列表和两个计数在所有连接阶段按查询时刻的美东日期排除过期合约，旧合约的覆盖记录仍按原键查询。新会话只订阅在该会话重新确认的合约。任一候选分母都以当前配置和 IB 返回的合约为边界。

`LineHandle::coverage(key, start, end)` 查询半开 UTC 区间 `[start, end)`。输入须满足 `start < end <= as_of`，`as_of` 可从 `status()` 取得。返回 `last_observed_at` 和三类区间。

| 字段 | 含义 |
| --- | --- |
| `covered` | 从该次订阅首个合格本地回调起至读取结束的时段，仍在读取时截至查询时间 |
| `uncovered` | 已纳入追踪的合约尚未得到有效回调、未轮到、已取消或处于连接缺口的时段 |
| `unknown` | 纳入追踪前、查询了未知键、超出保留窗口或历史已被淘汰的时段 |

覆盖开始于符合实际行情类型的有效顶档或有效 last、带值的 OI（含零）或带 gamma 的 Greeks 回调，使用调度器的本地接收时间。覆盖截止于取消、读取终止或断连。`covered` 表示本地存在观察机会。报价类型、OI、Greeks 的可用性仍以事件内容为准，逐笔连续性仍未知。区间最多保留 128 段／键，最多追踪 16,384 个键；达到键上限时先淘汰最旧的非活跃历史。`last_observed_at` 记录仍有历史的键的最近观察时刻，可能早于区间保留窗口。进程重启后的旧时段为未知。

## 集成与数据去向

- F9c 可用 `DepthBudget::allocate` 对已校验的 Nasdaq 股票 conId 分配独立深度名额。默认 `pinned_slots = 2`、`page_slot = true`。视图选中固定席位时复用对应名额。深度连接、资格校验和盘口事件由 F9c 处理。
- F9d 和 F9e 可在同一进程读取 `LineHandle` 的合约、L1 回调和覆盖报告。F9d 据回调和缺失情况组成 OI/Greeks 估值输入。F9e 以观察窗口为边界描述实时线索，历史逐笔核验使用独立数据请求。
- 行情和覆盖记录暂存内存，HTTP OpenAPI 和 `protocol/v1` 流契约维持现有版本。启用由 Rust 调用方负责，使用已有 paper 配置。部署、下游发布与持久化分别走对应集成和产品许可流程。[数据入仓防护](../data-guard.md)

离线测试入口见 [运行手册](../../RUNBOOK.md#离线验收-ibkr-行情线预算)。
