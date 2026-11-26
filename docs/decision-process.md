# 实时决策进程与共享策略求值

`scripts/finance-decision` 读取本地 F1 回放，按闭合 bar 生成单标的策略目标。该进程独立于采集、API 和回测 worker。运行时使用 Python 标准库，启动器默认调用 `python3`，可用 `FINANCE_PYTHON` 指定解释器。准备计划和行情文件的读权限，以及决策目录的写权限。进程入口、停止与恢复方式见 [运行手册](../RUNBOOK.md#实时决策与并行回测)。

## 共享求值接口

`fertile_finance.strategy_evaluation` 接收已编译计划和依序到达的 `ClosedBar`。记录标的、UTC 收盘纳秒和 `Decimal` 收盘价。`open_evaluator` 默认使用 CPU 后端，也接受实现 `EvaluationBackend.open(plan)` 的替身。每次打开建立独立状态。`Evaluator.evaluate(bar)` 在预热期间返回空，之后返回该 bar 对应的 `Decision`，含标的、时间和 `Decimal` 目标股数。

当前求值计划是 `fertile.finance/sma-crossover-plan/v1`。`finance-strategy compile-legacy-sma` 复用历史回测的标的与参数规则。要求现有 XNYS 股票或 ETF 格式、`2 ≤ fast_period < slow_period ≤ 250`、`1 ≤ shares ≤ 1000`。计划含 `instrument_id`、`strategy`、`plan_version`、`parameters` 和 `content_sha256`。哈希覆盖其余字段的按键排序、紧凑 UTF-8 JSON。回测从现有 `SmaCrossover` 作业参数构造同一版本的计划。

CPU 求值器用两个有限窗口比较收盘均线，窗口和以 27 位小数的定点整数保存，比较时使用交叉相乘。收到第 `slow_period` 根闭合 bar 时开始输出，快线严格高于慢线时目标为配置股数，其余为零。对当前 bar 生成的目标仅供观察，回测适配器依原有规则在下一会话使用目标做模拟成交。上游 `DemoStrategy` 仍用于源码构建示例。通用 `StrategyBundle` 计划的执行由 F7 集成。

## 回放输入和重启

`--replay` 指向普通文件，每行是一个含换行符结尾的 UTF-8 [F1 事件](market-protocol.md#流分区消息与更正)，单帧至多 1 MiB。文件承载计划标的的单一有序流分区，生产方负责按固定周期和价格口径提供完整的闭合历史。运维向进程提供已获准计算与留存的输入，F1 的 `license` 字段会随目标保留作血缘记录。

启动时读取已提交的文件前缀来重建策略状态。`bar_closed` 按收盘时间严格递增推进窗口，其他行情和常规控制帧只推进消费检查点。显式缺口、重置、更正和 F1 序号不连续会使进程退出，等待运维核对并重建基线。序号连续是已提供文件的判定，上游数据覆盖由生产方确认。

回放解析错误以单行记录字节偏移、错误类别和协议字段定位，未知及重复字段名以 `<field>` 标记。

默认从回放起点按事件顺序推进并在文件结束后退出。`--follow` 按约 100 ms 的间隔等待新增完整行，并等到闭合 bar 的可用时点再求值。跟随模式支持追加，须保留原文件及其已提交前缀。进程在重启时核对前缀 SHA-256，发现前缀不一致、文件截短或运行期间替换即退出。更换计划或重做历史时，使用独立的新决策目录保留原记录。

## 本机输出

`$FERTILE_DATA_ROOT/finance-state/decisions/decisions.sqlite3` 中的 `decisions` 表记录计划哈希、源、产品、频道、标的、`license`、F1 `epoch`、`seq` 与 `cursor`，以及 `exchange_ts`、`first_available_ts`、`decision_ts` 和 `target_shares`。时钟与流位置使用十进制字符串，目标为精确整股的十进制字符串。`decision_ts` 为截至该 bar 的收盘和首次可用时间的最大值。`decision_checkpoint` 保存计划哈希和已消费字节前缀的终点与哈希。每帧检查点与其决策在同一事务内提交，重启重建前缀时跳过已持久化的行。每个数据根配置一个计划和一个来源，按 `replay_offset` 读取记录的先后顺序。

进程没有监听端口、IBKR 或下单入口。决策库是本地进程接口。行情流 schema、HTTP OpenAPI 与历史回测请求格式不变。

## 后续集成

- F3 已提供 `projected_partitions`，可按 WAL 顺序枚举已提交的 Parquet 分区。准备回放时，按一个 F1 流分区导出 `event_json`，保留原始可用时间与控制帧，核对计算许可 `derive_non_display` 和覆盖质量报告。`read_latest_event` 返回最新修订，适用于点查。直接跟随投影需将断点绑定到投影源和已提交位置，并明确缺口、更正和保留边界。当前决策入口使用追加式 NDJSON 的字节断点。见 [F3 投影接口](market-wal.md#parquet-与投影游标)。
- F6 将 `IndicatorEngine::push_closed` 及其批量入口接入求值后端。适配时核对预热、点时可用性和均线比较精度，策略求值使用未为展示舍入的值。
- F7 将 `run_config_strategy_backtest` 内的策略目标生成与模拟成交分开，把已编译 `StrategyBundle`、闭合会话和所需的只读持仓及股票池快照接到共享求值接口。实时和回测共用目标生成，分别验证 `A1_B2` 与 `target_weights` 的逐点及重启一致性。
