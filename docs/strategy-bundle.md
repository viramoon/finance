# StrategyBundle v1 策略配置

`StrategyBundle` 把离线研究所需的策略组件、数据与时钟、组合预算、风控和模拟执行模型固化为一份可校验、可编译为 JSON 的声明。当前引擎为 `nautilus_cash_equity`，模式为 `backtest`，范围是多头现金权益、整股和非负权重。编译出的计划供后续 F7 配置回测读取。现有 `DemoStrategy` 与 `SmaCrossover` 继续通过 [作业接口](backtest-jobs.md) 运行。

`finance-strategy compile-legacy-sma` 单独为现有的 `SmaCrossover` 生成单标的观察计划。计划版本、共享求值和回放方式见 [实时决策接口](decision-process.md)。

## 格式与组件

根对象包含 `apiVersion: fertile.finance/strategy/v1`、`kind: StrategyBundle`、`metadata` 和 `spec`。`metadata.id` 与 `metadata.version` 标识一份发布版本。版本为三段用点分隔的数字，每段最多 10 位，数值零写作 `0`，其余数值用无前导零的十进制表示。`metadata.title` 是 1 至 256 个 Unicode 字符的显示标题。组件版本使用注册表中的完整 `id@version`。格式、策略和组件各有独立版本。

| 字段 | v1 约定 |
| --- | --- |
| `spec.universe` | `ref` 引用已登记的点时成分集合，`membership` 为 `point_in_time`，集合须在对应会话开盘时可见。 |
| `spec.data` | `bars` 引用已登记的日线来源，`timeframe` 为 `1d`，`price_basis` 为 `raw_split_free_reviewed`。数据、股票池和初始现金使用 USD。 |
| `spec.clock` | `calendar` 引用已登记交易日历。按 `session_close` 决策，最早于 `next_session_close` 成交。声明 `warmup_sessions`，缺输入使用 `block_new_risk`。 |
| `spec.factors` | 以局部 ID 声明因子或指标。每项填写 `use` 和 `params`，可通过 `depends_on` 声明同一文件内的因子依赖，省略表示没有显式依赖。 |
| `spec.modules` | 以局部 ID 声明可复用模块。每项填写 `kind`、`use` 和 `params`，例如信号模块的 `params.factor` 引用已声明的因子。 |
| `spec.recipes` | 以局部 ID 声明至少一个配方。配方槽位引用 `modules` 中的局部 ID。 |
| `spec.portfolio` | 声明初始现金、至少一个子组合及全局风险上限。按 `fixed_budgets` 分配，余量留 `cash`，按 `by_instrument` 净额。 |
| `spec.policy` | 采用 `exit_first`、`defer_and_report` 和 `stable_instrument_id`。`same_session_reentry` 为必填布尔值，示例设为 `false`。 |
| `spec.execution` | 引用登记的模拟执行模型，整股单位来自 `instrument_master`，手续费和滑点以基点填写。 |

`entry_exit` 填写 `entry`、`exit`、`filters`、`sizing`、`risk`。`target_weights` 填写 `allocator`、`filters`、`risk`。`filters` 和 `risk` 各自可以为空数组。两种配方采用各自封闭的字段集合，共用账户资金、按标的净额和全局限额。

注册表逐项列出组件类型、参数 schema、输入序列与单位、输出、预热、持仓状态依赖、支持资产和引擎。参数可以填写注册表定义的常量与局部引用。配置文件只能使用已登记组件。收盘收益的阈值以比率表示，例如 `"0.02"` 表示 2%。Wilder RSI 的阈值使用 RSI 点数，例如 `"30"`。

精确小数使用带引号的十进制字符串，整数部分最多 10 位，小数部分最多 18 位，可用负号。整数部分的前导零仅允许单个 `0`。这些字段禁用数字字面量和科学计数法。权重须在 0 到 1 之间，初始资金须为正且能精确表示为美元分，费用与滑点均在 0 至 10000 基点之间。周期等整数字段采用可精确表示为整数的数值。预热为 0 至 2520，收益与持有期窗口为 1 至 2520，RSI 周期为 2 至 2520，`top_n` 为 1 至 1000，优先级为 0 至 2147483647。预热还须满足组件和依赖声明，计数口径为决策时刻之前已完成的会话。

## 完整示例

A 使用 20 日收盘收益信号进出，B 使用 RSI 入场并按持有期退出，A1_B2 把 A 的入场与 B 的退出组合起来。示例只给 A1_B2 分配资金。把 `portfolio.sleeves` 的 `recipe` 改成 A 或 B 可选用另一套配方。若该份配置已绑定版本，改动需要新的版本号。可直接编译 [entry-exit.yaml](../strategies/fixtures/strategy-bundle/v1/entry-exit.yaml)。

```yaml
apiVersion: fertile.finance/strategy/v1
kind: StrategyBundle

metadata:
  id: example.equity-momentum
  version: "1.0.0"
  title: "日线动量入场与持有期退出"

spec:
  engine: nautilus_cash_equity
  mode: backtest
  universe:
    ref: "example.us-cash-equities@1.0.0"
    membership: point_in_time
  data:
    bars: "example.reviewed-us-daily@1.0.0"
    timeframe: 1d
    price_basis: raw_split_free_reviewed
    currency: USD
  clock:
    calendar: XNYS
    decide_on: session_close
    earliest_fill: next_session_close
    warmup_sessions: 30
    missing_input: block_new_risk

  factors:
    mom20:
      use: "factor.close_return@1.0.0"
      params: {lookback_sessions: 20}
    rsi14:
      use: "indicator.rsi_wilder@1.0.0"
      params: {period: 14, field: close}

  modules:
    A1:
      kind: entry
      use: "entry.above_when_flat@1.0.0"
      params: {factor: mom20, threshold: "0.02"}
    A2:
      kind: exit
      use: "exit.below_when_long@1.0.0"
      params: {factor: mom20, threshold: "0.00"}
    B1:
      kind: entry
      use: "entry.below_when_flat@1.0.0"
      params: {factor: rsi14, threshold: "30"}
    B2:
      kind: exit
      use: "exit.holding_sessions@1.0.0"
      params: {sessions: 10, start_at: first_fill}
    F1:
      kind: filter
      use: "filter.complete_session_bar@1.0.0"
      params: {require_positive_volume: true}
    S1:
      kind: sizing
      use: "sizing.equal_weight_capped@1.0.0"
      params: {max_weight_per_asset: "0.25"}
    R1:
      kind: risk
      use: "risk.long_only_cash@1.0.0"
      params: {max_gross_weight: "1.00", allow_borrow: false}

  recipes:
    A:     {type: entry_exit, entry: A1, exit: A2, filters: [F1], sizing: S1, risk: [R1]}
    B:     {type: entry_exit, entry: B1, exit: B2, filters: [F1], sizing: S1, risk: [R1]}
    A1_B2: {type: entry_exit, entry: A1, exit: B2, filters: [F1], sizing: S1, risk: [R1]}

  portfolio:
    initial_cash: {currency: USD, amount: "100000.00"}
    sleeves:
      - {recipe: A1_B2, budget_weight: "1.00", priority: 10}
    allocation: fixed_budgets
    residual: cash
    netting: by_instrument
    max_gross_weight: "1.00"
    max_weight_per_asset: "0.25"
  policy:
    on_entry_exit_collision: exit_first
    same_session_reentry: false
    on_missing_execution_price: defer_and_report
    tie_break: stable_instrument_id
  execution:
    model: "execution.next_close_cash_equity@1.0.0"
    lot_size_policy: instrument_master
    commission_bps: "2.00"
    slippage_bps: "5.00"
```

另一个完整示例使用 `allocator.top_n@1.0.0` 按 `mom20` 分数选前五个可用标的并输出等权意见。该声明对应现有 Python 组合输入的 `FactorValue` 与 `top_n` 路径，输出可接入 `TargetWeights` 契约。全局单标的和总仓限制仍适用。参见 [target-weights.yaml](../strategies/fixtures/strategy-bundle/v1/target-weights.yaml) 和 [组合回测](portfolio-backtest.md)。

## 组合及冲突次序

`B2` 从首次实际成交开始计持有会话，延迟成交会同步移动起算时刻。多个子组合各自维护虚拟子账，先各自计算目标，再按标的净额执行并分摊成本。同一标的若一个子组合退出而另一个保留，净减仓量只包含退出部分。

编译计划会记录下列执行次序，供后续 F7 同一决策时刻使用。

1. 冻结数据、现金和持仓快照，检查点时可用性、交易日历、数据质量和账户约束。缺失或过期输入阻止新风险。缺可信执行价时，暂存减仓意图并报告原因。
2. 先应用全局风控、强制减仓和已有仓位退出。`same_session_reentry: false` 时禁止同一会话退出后重入。设为 `true` 时允许同会话重入，同一决策时刻的进出冲突仍按 `exit_first`。重复意图仅处理一次。
3. 检查过滤条件，再处理入场与直接目标权重。过滤条件只限制新增风险。先应用各子组合的预算，再执行总仓及单标的上限。减仓优先于加仓。
4. 仅在剩余资金竞争时使用 `priority`，数值较小者优先。同优先级按各子组合的预算比例缩放。整股余量按稳定 `instrument_id`、再按配方 ID 处理。
5. 对净额、拒绝、延期和模拟成交记录原因、组件版本、目标变化和价格来源。v1 执行时钟仅按收盘判断。

例如两个子组合优先级相同且预算分别为 `"0.3"` 和 `"0.6"`，受限的加仓资金按 1 比 2 分配。提交文件时颠倒两个子组合的顺序，资金竞争次序仍按相同规则确定。

## 校验、错误与版本绑定

解析先检查单文档、UTF-8、256 KiB 上限、8192 个节点上限（映射键计入）和 48 层深度上限（根深度为 1）。YAML 拒绝显式标签、锚点、别名、非字符串映射键及重复键。布尔值使用 `true` 或 `false`，`yes` 和 `off` 解析为文本。JSON 按严格语法解析，重复键同样拒绝。输入只允许本地注册引用，禁止外部引用。

第一遍使用注册表的参数规则和封闭的 Bundle 结构检查必需字段、类型、枚举、范围和配方互斥。发布的 [StrategyBundle JSON Schema](../strategies/schema/strategy-bundle-v1.schema.json) 使用 Draft 2020-12，由相同规则导出。也可读取 [组件注册表](../strategies/schema/component-registry-v1.json) 了解参数和输入输出。

第二遍解析已登记引用并检查槽位、依赖图、币种、交易日历、预热和时间可见性。交易用数据和因子必须在决策时刻或之前可见。样例中延迟日线在收盘决策后才发布，因此 [future-data.yaml](../strategies/fixtures/strategy-bundle/v1/future-data.yaml) 无法编译。现金引擎禁止借贷和空头，预算为非负且总和最多为 1。实际区间的样本覆盖、成分和数据快照留给配置回测运行时再检查。

历史策略 ID `research.factor-momentum` 保留给 `research_only` 目录项。以该 ID 上传 StrategyBundle 会在版本绑定前返回 `RESEARCH_ONLY_STRATEGY`，诊断指向 `/metadata/id`。[历史登记证据](factor-research.md#月频动量)

编译在首个错误处停止，错误 JSON 提供 `path`、`code`、`message` 和 `location`。`path` 沿用 JSON Pointer 的路径形状，`""` 指根节点。公开 schema 中的固定字段名和实际数组索引会保留。其余映射键（包括局部 ID 和未知字段名）用 `<key>` 占位，例如 `/spec/factors/<key>/use`。出现占位符时按 `location` 定位原文，行列从 1 起算，缺失字段指向最近的已有位置。公开诊断用固定文本解释原因，省略输入原值、原文片段、配置哈希和底层存储错误文本。

[duplicate-key.txt](../strategies/fixtures/strategy-bundle/v1/duplicate-key.txt) 在 `/metadata/id` 报 `DUPLICATE_KEY`。其他错误码包括 `FUTURE_DATA`、`UNKNOWN_COMPONENT`、`SLOT_MISMATCH`、`DEPENDENCY_CYCLE`、`BUDGET_EXCEEDED`、`VERSION_CONFLICT`。用于验证严格解析的纯文本文件含预期被拒绝的 YAML。

规范化保留数组顺序，按键稳定排序 JSON 对象，去除十进制字符串多余的尾零，并将零值统一为 `"0"`。`content_sha256` 是规范化 JSON 的内容哈希，`source_sha256` 是原始输入字节的哈希。空格、注释、对象键顺序与等值小数拼写可以改变原文哈希而保持内容哈希。每对 `(metadata.id, metadata.version)` 在通过全部校验后可绑定一个内容哈希。已绑定同一哈希时允许重试，绑定另一哈希时以 `VERSION_CONFLICT` 拒绝。

## 命令行与 Python worker

在仓库根目录执行以下命令。编译成功时标准输出是单个计划 JSON。编译失败时标准输出为空，标准错误输出诊断 JSON，退出码为 2。文件打开等命令行准备失败使用固定说明并以状态码 1 退出。

```sh
cargo run --locked -p finance-api --bin finance-strategy -- compile strategies/fixtures/strategy-bundle/v1/entry-exit.yaml
cargo run --locked -p finance-api --bin finance-strategy -- compile --bindings /tmp/strategy-versions.sqlite3 strategies/fixtures/strategy-bundle/v1/target-weights.yaml
cargo run --locked -p finance-api --bin finance-strategy -- schema
cargo run --locked -p finance-api --bin finance-strategy -- registry
```

`compile` 从指定文件或 `-` 所代表的标准输入读取，按 `.json` 文件名选择 JSON，其余按 YAML。可用 `--format yaml|json` 指定格式。省略 `--bindings` 时仅做单次预检。指定 SQLite 文件时，不同调用共享原子版本绑定。`schema` 与 `registry` 的标准输出分别用于重新生成仓库中的 schema 与注册表快照。

已构建的 `finance-strategy` 可作为 worker 子进程使用，Python 也可用标准库读取提前生成的计划。对精确值做运算时应使用 `Decimal`。

```python
import json
import subprocess
from decimal import Decimal

plan = json.loads(subprocess.check_output([
    "finance-strategy", "compile",
    "strategies/fixtures/strategy-bundle/v1/entry-exit.yaml",
]))
initial_cash = Decimal(plan["bundle"]["spec"]["portfolio"]["initial_cash"]["amount"])
```

`plan_version` 当前为 `fertile.finance/strategy-plan/v1`。计划包含规范化的 `bundle`、两个哈希、已解析的 `inputs`、按依赖拓扑排序的 `factors`、按局部 ID 排序的 `modules`、执行模型 `execution`、按优先级与配方 ID 排序的 `sleeves` 和生效的 `conflict_rules`。成功计划保留规范化配置的字段值和局部 ID，供 worker 复现完整配置。

`spec.clock.calendar` 与标的主数据的 `calendar_id` 共用日历代码。跨契约接入时采用 1–32 位 ASCII 大写字母或数字，首位之后也允许下划线和连字符。当前示例目录只登记 `XNYS`，编译器要求股票池和日线目录的日历与决策日历一致。`metadata.id` 中的 `example.*` 是策略 ID，`spec.universe.ref` 和 `spec.data.bars` 中的 `example.*@1.0.0` 是各自目录的引用。后续解析股票池成分和日线记录时，实际标的使用 `<LOCAL>.<VENUE>`，例如 `SPY.ARCA`。标的和日历的完整规则见 [行情标的、事件与流协议](market-protocol.md)。

F7 接入持久版本绑定、数据与成分的快照哈希、引擎提交、成本和运行区间，并将编译计划归入不可变 `run_manifest`。指标计算由 C4/F6 接入。F8 提供离线评价及版本选用检查，F7 接入研究因子时调用 `FactorResearchCatalog.select_for_backtest`。[因子研究接口](factor-research.md)
