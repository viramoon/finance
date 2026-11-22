# 异步示例回测

finance-api 的写入口仅接受本机调用。经 web 远程提交时使用 web 的写接口鉴权。先按[源码构建](build.md)准备策略运行环境，从 finance 仓库根在两个终端分别运行下列进程，指向同一已挂载的数据根。

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

队列和已完成的摘要存于数据根的 finance-state/backtests/jobs.sqlite3。SQLite 的唯一键与 IMMEDIATE 事务使并发提交、去重和容量检查不可分割。子进程使用已验证的本地 wheel，执行 uv run --no-sync --active python -m fertile_finance.backtest_job。每次运行在独立临时目录生成并原子替换结果，worker 在同一事务内写入最终摘要和 succeeded 状态。失败保留 failed 或 timed_out 状态与错误代码。

同一队列只运行一个 worker，使用操作系统文件锁防止双重恢复。FINANCE_WORKER_CONCURRENCY 默认 1，范围 1～16。FINANCE_JOB_TIMEOUT_SECONDS 默认 600，范围 1～3600。FINANCE_JOB_MEMORY_MB 默认 8192，范围 128～65536，限制 Linux 执行进程的虚拟地址空间。子进程继承死亡信号，超时时 worker 终止整个执行进程组。SIGTERM 后停止领取任务，等待每个在途任务完成或到各自时限。进程守护器的优雅退出宽限应覆盖配置的单作业超时。

使用 GET 详情核对 status、attempt、summary、error。刚启动时 worker 持有独占锁，再检查 running 记录。首次中断的记录可再执行一次，第二次中断则转 failed，error=worker_lost_twice。诊断长期 running 时先查守护日志与原 worker PID，恢复方式为重启 worker；不要手动改数据库状态或删除锁文件。worker 存活由守护器检查，作业通路用提交并轮询到终态验证。

如需清理历史记录，先停止 API 和 worker，确认没有 queued、running 的作业，用 sqlite3 的 .backup 备份至受保护路径。按已确认的保留截止日期在事务中仅删除终态的旧记录，运行 PRAGMA integrity_check 后再启服务。被删除记录的幂等键将释放，不应清理仍在客户端重试窗口内的作业。

在隔离环境复验崩溃恢复，先 cargo build --locked -p finance-api，设置 FINANCE_API_BIN 为构建出的二进制绝对路径，运行 python3 scripts/test-job-recovery.py。该脚本对临时数据根和独立端口发真实 curl，验证去重、SIGKILL 后子进程回收、一轮重入、终态摘要以及真实超时。FINANCE_DRILL_PORT 可固定演练端口，运行前须确保未被服务占用。
