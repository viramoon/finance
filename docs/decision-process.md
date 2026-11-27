# 实时决策进程与共享策略求值

`scripts/finance-decision` 按编译计划读本地闭合行情，生成单标的或组合策略目标。该进程独立于采集、API 和回测 worker。启动器默认调用 `python3`，可用 `FINANCE_PYTHON` 指定解释器。SMA 模式使用 Python 标准库。`StrategyBundle` 模式使用现有源码构建 Python 环境的 Nautilus 行情类型和 XNYS 日历，以及同一构建的 `finance-strategy`。准备计划和回放文件的读权限，以及决策目录的写权限。进程入口、停止与恢复方式见 [运行手册](../RUNBOOK.md#实时决策与并行回测)。

## 共享求值接口

`fertile_finance.strategy_evaluation` 接收已编译计划和依序到达的 `ClosedBar`。记录标的、UTC 收盘纳秒和 `Decimal` 收盘价。`open_evaluator` 默认使用 CPU 后端，也接受实现 `EvaluationBackend.open(plan)` 的替身。每次打开建立独立状态。`Evaluator.evaluate(bar)` 在预热期间返回空，之后返回该 bar 对应的 `Decision`，含标的、时间和 `Decimal` 目标股数。

单标的计划版本是 `fertile.finance/sma-crossover-plan/v1`。`finance-strategy compile-legacy-sma` 复用历史回测的标的与参数规则。要求现有 XNYS 股票或 ETF 格式、`2 ≤ fast_period < slow_period ≤ 250`、`1 ≤ shares ≤ 1000`。计划含 `instrument_id`、`strategy`、`plan_version`、`parameters` 和 `content_sha256`。哈希覆盖其余字段的按键排序、紧凑 UTF-8 JSON。回测从现有 `SmaCrossover` 作业参数构造同一版本的计划。

SMA CPU 求值器用两个有限窗口比较收盘均线，窗口和以 27 位小数的定点整数保存，比较时使用交叉相乘。收到第 `slow_period` 根闭合 bar 时开始输出，快线严格高于慢线时目标为配置股数，其余为零。回测适配器依原有规则在下一会话使用目标做模拟成交。上游 `DemoStrategy` 仍用于源码构建示例。

`finance-strategy compile` 生成 `fertile.finance/strategy-plan/v1` 配置计划。`StrategyBundle` 的两条生产路径都调用 `BundleSignalEvaluator`。它用同一份收盘序列计算指标，按会话点时成分和组合快照应用入场、退出、排名和组合约束，输出各配方对各标的的整股目标及可选原因。回测用 Nautilus 模拟成交，更新下一会话求值所需的现金、持仓和在途减仓。实时入口使用提供方给出的这些只读快照，输出成交前目标。两条路径在计划、行情、时钟和组合快照一致时逐点产出相同目标。回测报告和提交方式见 [配置策略作业](backtest-jobs.md#配置策略作业)。

配置策略以本地标准输入和标准输出管道调用 `finance-strategy evaluate-indicators`。可执行文件默认从 `PATH` 查找，也可用 `FINANCE_STRATEGY_BIN` 设绝对路径。主图和该子进程使用同一 Rust `IndicatorEngine`。会话开盘、收盘、交易日和 OHLCV 按精确文本传入。每个因子选择已编译的 `output`，使用与主图相同的声明精度。每个标的及因子保留增量状态，首次求值和重启时回放所需闭合历史。缺失 bar 或退出点时成分后清除该标的的策略序列，再进入时重新预热。[指标定义与精度](indicators.md)

实时与回测在使用计划前校验 `bundle` 摘要和运行字段。预算、金额、权重、阈值和费率使用编译器输出的十进制字符串。顶层 `sleeves` 必须与 `bundle` 中的声明一致，并按优先级和配方 ID 排序。两条路径也核对执行模型、输入引用和同会话再入场规则。校验拒绝包含 JSON 浮点值或运行字段不一致的计划，实时进程在打开决策库前退出，回测作业返回 `strategy_plan_invalid`。

## 回放输入和重启

`--replay` 指向普通文件，每帧是一个以换行结尾的 UTF-8 JSON 对象，至多 1 MiB。运行时根据计划版本识别输入。运维向进程提供已获准计算与留存的文件。

### 单标的 F1 输入

每行是一个 [F1 事件](market-protocol.md#流分区消息与更正)。文件承载计划标的的单一有序流分区，生产方按固定周期和价格口径提供完整的闭合历史。每帧的 F1 `license` 必须与该回放第一帧相同，包含预热和控制帧。遇到不同许可即停止，并保留上一个已提交的检查点。F1 的 `license` 随目标保留作血缘记录。

启动时读取已提交的文件前缀来重建策略状态。`bar_closed` 按收盘时间严格递增推进窗口，其他行情和常规控制帧只推进消费检查点。显式缺口、重置、更正和 F1 序号不连续会使进程退出，等待运维核对并重建基线。序号连续是已提供文件的判定，上游数据覆盖由生产方确认。

### 配置策略会话输入

每行是一份完整的 XNYS 收盘快照，包含下表全部字段。按日历的连续交易会话追加，使用官方收盘时间，包括提早收盘。行情采用与 F7 配置回测相同的经审核 USD 原始、无拆股口径。提供方在会话开盘时冻结点时成分，在收盘时冻结组合状态。价格、数量、现金和时间使用精确十进制字符串。

| 字段 | 内容 |
| --- | --- |
| `schema` | `fertile.finance/decision-session/v1`。 |
| `session` | `YYYY-MM-DD` 格式的交易会话。 |
| `warmup` | 布尔值。预热帧作为连续前缀，达到计划要求的 `warmup_sessions` 后可开始输出。 |
| `members` | 会话开盘时已知的成分标的 ID 数组。 |
| `bars` | 标的 ID 到 `open`、`high`、`low`、`close`、`volume`、`first_available_ts` 的映射。纳秒时间须等于该会话的官方收盘。缺少或晚到的 bar 保持缺失。 |
| `cash` | 收盘时已知的非负组合可用现金。 |
| `sleeves` | 配方 ID 到 `shares`、`sessions_held`、`exited` 的映射。前两项按标的提供正整股持仓和距首次成交的会话数，`exited` 是当日已退出的标的数组。 |
| `pending_reductions` | 尚未完成的减仓数组，每项含 `instrument_id`、`recipe` 和负整股 `quantity`。 |

### 跟随与恢复

回放解析错误以单行记录字节偏移和错误类别。F1 保留固定协议字段位置，未知及重复字段名以 `<field>` 标记。配置会话诊断使用固定消息。

默认从回放起点按顺序推进并在文件结束后退出。`--follow` 按约 100 ms 的间隔等待新增完整行，并等到闭合 bar 或会话的决策时点再求值。SIGTERM 或 SIGINT 会中断配置策略等待中的指标管道读写。进程回收指标子进程后退出。中断的指标请求对应的回放帧留待重启时重新求值，检查点保持在最后一次成功提交的位置。跟随模式须保留原文件及其已提交前缀。进程在重启时核对前缀 SHA-256，发现前缀不一致、文件截短或运行期间替换即退出。更换计划或重做历史时，使用独立的新决策目录保留原记录。

## 本机输出

`$FERTILE_DATA_ROOT/finance-state/decisions/decisions.sqlite3` 中的 `decisions` 表记录 SMA 目标、计划哈希、源、产品、频道、标的、`license`、F1 `epoch`、`seq` 与 `cursor`，以及 `exchange_ts`、`first_available_ts`、`decision_ts` 和 `target_shares`。时钟与流位置使用十进制字符串，目标为精确整股的十进制字符串。`decision_ts` 为截至该 bar 的收盘和首次可用时间的最大值。

`bundle_decisions` 保存配置策略的 `replay_offset`、`plan_sha256`、`session`、`signal_ts`、`recipe`、`instrument_id`、`target_shares` 和 `reason`。计划哈希覆盖完整编译计划的规范 JSON。目标股数与纳秒时钟为十进制字符串，`reason` 可为空。按 `replay_offset`、`recipe`、`instrument_id` 查询可得到稳定顺序。

`decision_checkpoint` 保存求值身份哈希和已消费字节前缀的终点与哈希。配置策略的求值身份绑定计划哈希和指标求值器版本。每帧检查点与其全部目标在同一事务内提交，重启重建前缀时跳过已持久化的行。 SMA 还在同一事务更新 `sma_replay_license`，记录与检查点偏移和摘要绑定的统一许可。重启时会重新校验已提交前缀，旧 SMA 库在完整验证后补写许可记录。遇到混合许可的旧回放时，停止发布并在独立数据根重建经审核的同许可回放。升级至 F6 求值器时，保留旧配置策略的决策库，在新的数据根重新消费获准的原始回放。每个数据根配置一个计划和一个来源。

决策进程使用本地文件和 SQLite。finance-api 可按已发布的计划与许可只读查询已提交信号。发布格式、分页和错误见[第二批契约](api.md#决策信号发布与分页)。

## 后续集成

- F3 已提供 `projected_partitions`，可按 WAL 顺序枚举已提交的 Parquet 分区。准备回放时，按一个 F1 流分区导出 `event_json`，保留原始可用时间与控制帧，核对计算许可 `derive_non_display` 和覆盖质量报告。`read_latest_event` 返回最新修订，适用于点查。直接跟随投影需将断点绑定到投影源和已提交位置，并明确缺口、更正和保留边界。当前决策入口使用追加式 NDJSON 的字节断点。见 [F3 投影接口](market-wal.md#parquet-与投影游标)。
