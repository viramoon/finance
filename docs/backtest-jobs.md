# 异步回测作业

finance-api 默认监听 127.0.0.1:8081，写入口由本机 web 调用。盒子部署保持 loopback 绑定，远程提交由 web 验证写入令牌。先按[源码构建](build.md)准备策略运行环境，从 finance 仓库根在两个终端分别运行下列进程，指向同一已挂载的数据根。

```bash
source /home/box/fertile/env.sh
export FERTILE_DATA_ROOT=/home/box/fertile/data
cargo run --locked -p finance-api
# 另一个终端同样加载 env.sh 并设置 FERTILE_DATA_ROOT
scripts/finance-backtest-worker
```

当前白名单包含锁定上游的 DemoStrategy，其唯一支持的标的是 6EH4.XCME。随源码交付的 CSV 覆盖 2024-01-01 UTC 23:01 至 23:09，允许任意两个其中的整分钟端点，开始须早于结束。引擎实际按请求区间运行。策略没有可调参数，提交空对象。没有覆盖、参数错误、请求超过 8 KiB、缺少幂等键均返回 422。价格与 PnL 使用精确数值，HTTP 摘要的 pnl 为十进制字符串。

```bash
curl -i -X POST http://127.0.0.1:8081/api/v1/backtests \
  -H 'Content-Type: application/json' -H 'Idempotency-Key: k1' \
  -d '{"strategy":"DemoStrategy","instrument":"6EH4.XCME","start":"2024-01-01T23:02:00Z","end":"2024-01-01T23:06:00Z","params":{}}'
# 用上面 202 响应的整数 id 替换 1
curl http://127.0.0.1:8081/api/v1/backtests/1
curl 'http://127.0.0.1:8081/api/v1/backtests?limit=20'
```

同一键和相同的规范化请求返回同一个 id。复用键提交不同参数返回 409。待执行与执行中合计最多 128 个作业，达到上限返回 429。按 id 升序分页，limit 范围 1～100。将 next_cursor 原样作为下次的 cursor，空值表示该页为末页。

## 配置策略作业

`StrategyBundle` 作业使用 [F2 编译器](strategy-bundle.md) 统一处理文件和登记版本，将编译计划和回放快照关联到同一队列记录。现有 worker 用 Nautilus `BacktestEngine` 执行现金权益的离线模拟成交。提交和查询见 [HTTP 接口](api.md#配置策略提交与结果)。

### 本地回放数据

在既有数据根的 `finance-state/backtests/strategy-inputs/<bars_ref>/<universe_ref>.json` 发布受信任的回放文件。单文件上限 32 MiB，路径中的两个引用来自已编译的版本化数据和股票池 ID。文件由运维或研究侧从已审查的本地数据生成，使用原子替换发布。测试数据可用 [合成生成器](../tests/config_strategy_replay_fixture.py) 写入独立临时根。默认使用三个每会话上涨一美元的标的，并分别在入场和退出的执行会话设置一条晚到 bar。`--target-weights` 生成另一组固定价格序列，让前三名的次序在回测中变化，用于核对 Top-N 截断、等权名义金额和调仓。

输入目录、目录文件和回放文件使用实体路径。如果单个路径组件含后缀超过 255 字节，使用 `~` 加原始引用的 SHA-256 小写十六进制，再拼接原后缀。API 在入队时核对快照元数据，worker 校验完整记录、覆盖区间和原始字节哈希。

| 回放字段 | 约定 |
| --- | --- |
| `schema` | `fertile.finance/strategy-replay/v1` |
| `bars_ref`、`universe_ref`、`calendar`、`currency`、`timeframe`、`price_basis` | 必须匹配编译计划。运行口径为 XNYS、USD、`1d` 与 `raw_split_free_reviewed`。 |
| `first_session`、`last_session` | 数据声明覆盖的首尾交易日，使用 `YYYY-MM-DD`。运行区间所含实际会话须落在覆盖内。 |
| `instruments` | 声明 USD XNYS 现金权益标的，使用 `AAA.XNYS` 形式，最小价格单位一分，整股成交。 |
| `membership` | 按生效交易日提供 `{date, first_available_ts, members}`。成分在该会话开盘前或开盘时可用，沿用至下一份点时成分。 |
| `bars` | 每个标的和交易日最多一条，含 `instrument_id`、`date` 和 `first_available_ts`。`open`、`high`、`low`、`close` 用正的精确美元字符串，`volume` 用非负整股字符串。 |

`first_available_ts` 使用 UTC Unix 纳秒数字字符串。按 XNYS 日历检查会话早收盘、假日与夏令时。只使用在对应交易日收盘时已经可用的闭合 bar，晚到记录计入数据血缘和缺口。预热使用请求区间之前的闭合会话。历史会话不足时作业以 `insufficient_history` 结束，指标历史遇到成分中断或缺 bar 时从连续数据重新建立。运行器先逐标的求值因子的 `depends_on`。上游任一依赖缺值时，下游因子也缺值，依赖该值的新增风险被阻止，结果记录 `missing_factor`。缺失执行价时记录延期，保留减仓意图，并在下一次合格决策时重新评估加仓。持有期在首次确认成交后开始。

可选的 `strategy-inputs/catalog.json` 为受信任的本地编译目录，补充 F2 的示例 ID。顶层 `universes` 和 `bars` 分别列出元数据。两类条目都有 `id`、`calendar`、`currency`、`asset`、`available_at`。股票池另有 `point_in_time`，日线另有 `timeframe`、`price_basis`。用 `cash_equity` 表示资产类别。股票池和日线的可用性分别设为 `session_open` 与 `session_close`。同类 ID 不得重复；实际快照独立按两个引用发布。

### 组合裁决与归因

入场/出场配方只在空仓时按入场条件建仓，按退出条件或持有期减仓。目标权重配方按登记的排序因子生成 Top-N 等权目标，同分按稳定标的 ID 处理。信号使用当日闭合数据，模拟成交最早发生在下一交易日收盘。过滤器阻止新增风险，组合先处理全局风控和减仓，再分配买入预算。套用 `exit_first` 与 `same_session_reentry` 策略时，退出优先级高于同一决策的入场。

每个配方按 `budget_weight` 分配初始资金并独立维护虚拟现金和持仓，余量保留在组合现金。执行裁决采用以下确定性步骤。

1. 固定当次有执行价的减仓。按 `priority` 从小到大逐级处理加仓。同一级先按预算比例和收盘价分配，在各配方内沿用申请金额的比例，然后取整股，余量按标的 ID、配方 ID 分配。每一级完成后固定其接受股数，后续级别只可使用剩余容量。
2. 对试算数量重新按标的撮合。相反意图按收盘价内撮，内撮份额也按优先级及同级预算比例分配。同级整股余量按配方 ID 分配。外部净额按滑点报价成交。每一侧的外部手续费按实际累计整股和报价计算，按分四舍五入。各级承担加入该级数量后增加的费用，同级按外部成交量分摊，分币余量按配方 ID 分配。
3. 核对预计子账现金和扣除实际外部成本后的现金、净值及敞口上限。现金不足时先按外部报价收紧当级的试算，费用造成的不足按候选整股重新计算实际净单手续费。风控超限时收紧当级的估值额度，并复核可保留的内撮份额。每次调整重新结算，高优先级已确认的股数不参与裁剪。
4. 当级得到可执行结果后，移除试算限额，按标的 ID、配方 ID 依次复核有缺口的意图。每次固定其余股数并检查原申请量。在内撮范围内，先根据买方现金及随该笔买入增加的敞口找出整股上界，再对该数量复核所有持仓。外部增量按候选整股重算实际净单手续费和风控。单笔订单按实际分币手续费求最大可负担整股。此步骤允许收回其他子账裁剪后释放的内撮份额。裁决是逐级的局部算法，不追求跨配方、跨标的的全局最优。

同级合并单的分币手续费可能使相邻股数的可行性反转。外部净买入先用扣费后的敞口限定搜索上界。撮合份额固定时，按分币分摊发生变化的费用比率区间搜索，使用精确整数计数找出满足子账现金的最大股数。撮合份额变化时，在现金和敞口上界内按股数从大到小试算。每次加试都使用对应的净额和实际手续费。若加试一股和三股可行，两股和四股因同级子账多分到一分手续费而失败，复核接受三股。

| 场景 | 确定性结果 |
| --- | --- |
| 权益 $1.00，单标的上限 50%。优先级 1 申请 50 股 AAA，收盘和成交均为 $0.01。优先级 2 申请 1 股 BBB，收盘 $0.10，外部买价 $0.11。 | AAA 成交 50 股。若加入 BBB，扣除滑点后的权益为 $0.99，原 AAA 持仓将超过上限。BBB 被拒，记录 `portfolio_asset_limit`。 |
| 同级 A、B 的预算各 45%，分别申请 10 股和 8 股 AAA。AAA 和 BBB 均收盘 $10，外部卖价 $2、买价 $18，手续费为零。已有卖方退出 10 股 AAA。A 有 $100，B 卖 8 股 BBB 得 $16，其余子账现金为零。 | B 可内撮 1 股。复核时 A 可接住余下的 9 股。AAA 全量内撮，只有 BBB 对外卖出 8 股。A、B、AAA 卖方现金分别为 $10、$6、$100。 |
| 子账现金 $1.00，收盘 $0.50、外部买价 $1.00，手续费 40 bps，申请 2 股。 | 两股费用为 $0.01。一股的费用舍入为 $0.00，实付 $1.00，接受 1 股并记录 `sleeve_cash`。 |

引擎向 Nautilus 提交最终外部净额。无执行价的持仓继续等待，必要减仓优先执行，仍超限的留仓由随后收盘决策纳入风控减仓。对裁剪或拒绝的买入，归因固定其余最终股数，加试第一股未获分配的数量，重新撮合并计算实际费用、现金和敞口。现金不足记为 `sleeve_cash`，随后检查敞口约束。加试仍满足约束时记为 `allocation_order`，表示该股未在逐级裁决和单次顺序复核中分配。`strategy_report` 将原因与请求量、接受量、内外部成交、费用及子账变化关联，并记录组合现金、估值和覆盖。

### 快照固定与恢复

入队事务按 SHA-256 固定快照原始字节，并按哈希复用同一份数据。`jobs.sqlite3` 中新增的 `strategy_backtest_versions` 保存版本绑定和编译输入元数据，`strategy_backtest_snapshots` 保存经过固定的快照字节。worker 只从队列关联读取快照，校验哈希后运行；同一作业的恢复尝试使用首次入队的数据。运维备份队列时须包含这些表。修改发布文件后，新提交按当前文件固定快照。

成功结果 `run_manifest` 记录规范化方案和组件、Nautilus 版本和源码提交、请求区间、预热、复权口径、日历与时区、费用及滑点、可用性规则、回放字节与每标的数据分区及成分哈希。`strategy_report.pnl_as_of` 标明用于 PnL 的最后完整估值日期。查询摘要可用 `gaps` 排查缺口，可用 manifest 核对版本与数据。隔离复验见 [运行手册](../RUNBOOK.md#隔离验收配置策略作业)。

## worker 运行与恢复

队列和已完成的摘要存于数据根的 finance-state/backtests/jobs.sqlite3。SQLite 的唯一键与 IMMEDIATE 事务使并发提交、去重和容量检查不可分割。子进程使用已验证的本地 wheel，执行 uv run --no-sync --active python -m fertile_finance.backtest_job。每次运行在独立临时目录生成并原子替换结果，worker 在同一事务内写入最终摘要和 succeeded 状态。失败保留 failed 或 timed_out 状态与错误代码。

同一队列只运行一个 worker，使用操作系统文件锁防止双重恢复。FINANCE_WORKER_CONCURRENCY 默认 1，范围 1～16。FINANCE_JOB_TIMEOUT_SECONDS 默认 600，范围 1～3600。FINANCE_JOB_MEMORY_MB 默认 8192，范围 128～65536，限制 Linux 执行进程的虚拟地址空间。子进程继承死亡信号，超时时 worker 终止整个执行进程组。SIGTERM 后停止领取任务，等待每个在途任务完成或到各自时限。进程守护器的优雅退出宽限应覆盖配置的单作业超时。

使用 GET 详情核对 status、attempt、summary、error。刚启动时 worker 持有独占锁，再检查 running 记录。首次中断的记录可再执行一次，第二次中断则转 failed，error=worker_lost_twice。诊断长期 running 时先查守护日志与原 worker PID，恢复方式为重启 worker；不要手动改数据库状态或删除锁文件。worker 存活由守护器检查，作业通路用提交并轮询到终态验证。

如需清理历史记录，先停止 API 和 worker，确认没有 queued、running 的作业，用 sqlite3 的 .backup 备份至受保护路径。按已确认的保留截止日期在事务中仅删除终态的旧记录，运行 PRAGMA integrity_check 后再启服务。被删除记录的幂等键将释放，不应清理仍在客户端重试窗口内的作业。

在隔离环境复验崩溃恢复，先 cargo build --locked -p finance-api，设置 FINANCE_API_BIN 为构建出的二进制绝对路径，运行 python3 scripts/test-job-recovery.py。该脚本对临时数据根和独立端口发真实 curl，验证去重、SIGKILL 后子进程回收、一轮重入、终态摘要以及真实超时。FINANCE_DRILL_PORT 可固定演练端口，运行前须确保未被服务占用。

## 隔离运行官方历史作业

使用 `python -m fertile_finance.isolated_historical_job --help` 查看一次性运行入口。按 [运行手册](../RUNBOOK.md#隔离验收官方历史作业) 指定已构建的 finance-api、独立数据根和已导入标的的独立研究根。两个根须为已存在、互不包含的绝对目录，路径组件使用实体目录。API 和 worker 共用本次调用的数据路径，队列位于数据根的 `finance-state/backtests/jobs.sqlite3`。运行器直接使用本机环回的 finance-api，从同一源码构建的 Python 环境启动 worker。服务配置只影响本次进程，部署环境文件、web 和 PostgreSQL 不参与。

运行器提交 `SmaCrossover` 后轮询官方作业终态，把响应 JSON 写到标准输出。失败或超时返回非零状态，保留专用队列供诊断。结束或中断时回收本次启动的进程并释放端口。结果与 [HTTP 契约](api.md#历史回测与提交确认) 一致。

官方作业不使用 FRED，运行器无需 FRED 序列 ID、`cosd` 或 `coed`。2024 年示例的 `DGS10` 和本次截止 2026-09-30 的 Ken French 日频均供归档与研究。验收无需导入或扩展这些归档。四窗口按标的实际净值和交易日覆盖判定，Sharpe 使用 `rf=0`。单独归档 DGS10 的流程见 [可选扩展 FRED 归档以对齐 SPY 样本](../RUNBOOK.md#可选扩展-fred-归档以对齐-spy-样本)。

在允许本机环回监听的环境中，用构造的日线 fixture 验证真实 HTTP 提交、终态和端口释放。测试自动创建临时数据根和研究根，并使用空闲的高位端口。

```sh
FINANCE_API_BIN=/absolute/path/to/target/debug/finance-api PYTHONPATH=python \
  "$FINANCE_PYTHON" -B tests/isolated_historical_job_drill.py -v
```
