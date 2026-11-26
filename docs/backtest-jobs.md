# 异步示例回测

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

## worker 运行与恢复

队列和已完成的摘要存于数据根的 `finance-state/backtests/jobs.sqlite3`。SQLite 的唯一键与 IMMEDIATE 事务使并发提交、去重和容量检查不可分割。worker 使用启动它的源码构建 Python 环境执行 `python -m fertile_finance.backtest_job`。每次尝试在独立临时目录生成结果，worker 在同一事务内提交最终摘要和 `succeeded` 状态。失败保留 `failed` 或 `timed_out` 状态与错误代码。

同一宿主的多个 worker 可共用本地 SQLite 队列。领取时在同一写事务内递增尝试号、置为 `running` 并建立租约。只有持有作业锁且尝试号匹配的 worker 才能续租或提交结果，引擎子进程继承该锁。租约到期后，持锁者仍可完成同一次尝试，其他 worker 需等锁释放后才能恢复。共享数据根须位于支持 SQLite 和 `flock` 锁语义的本地文件系统。

`FINANCE_WORKER_LEASE_SECONDS` 或 `--lease-seconds` 设租约时长，默认 15 秒，范围 1～3600 秒，按时长的三分之一续租。`FINANCE_WORKER_CONCURRENCY` 按进程配置，默认 1，范围 1～16。`FINANCE_JOB_TIMEOUT_SECONDS` 默认 600，范围 1～3600。`FINANCE_JOB_MEMORY_MB` 默认 8192，范围 128～65536，限制 Linux 执行进程的虚拟地址空间。超时时 worker 终止执行进程组。SIGTERM 后停止领取任务，继续持有在途租约至作业完成或超时。进程守护器的优雅退出宽限应覆盖单作业超时。

启动时自动创建 `job_leases`，已有请求和终态记录继续使用原格式。升级先排空并停止旧版 worker，再启动新版。队列升级时剩余的旧版 `running` 作业按中断恢复。租约过期且锁释放后，首次中断重排一次，第二次中断转 `failed`，错误为 `worker_lost_twice`。使用 GET 详情核对 `status`、`attempt`、`summary`、`error`。长期处于 `running` 时先核对 worker 与其引擎子进程的存活、进度及终止状态，保留数据库和锁文件以便自动恢复。

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
