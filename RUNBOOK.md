# finance 运行手册

本仓库已提供源码构建、示例批任务、行情采集、回测 worker 与 HTTP API。交易服务尚未实现，其启动命令、恢复阈值和凭据配置仍为 TBD；下述交易运维段落是后续设计。服务部署以 [fertile deploy/](https://github.com/viramoon/fertile/tree/main/deploy) 为唯一准绳，Maas · Ruler 只按 fertile tag 部署和回滚，不部署任意子模块分支。

## 数据目录与 Git 检查

把行情根目录、研究目录、数据库、截图、日志、崩溃转储和备份放在 Git 工作树之外。IBKR paper 与回放各用独立数据根。开发克隆运行 `sh scripts/install-data-guard-hooks.sh` 安装暂存区和推送检查。处理阻断时先用 `python3 -B scripts/data_guard.py --show-paths range origin/main HEAD` 定位 PR 范围内的问题。清理尚未发布的违规历史。防护启用前已发布的提交可经单独评审登记历史豁免，再追加提交移除仍留在目标树中的违规文件或补齐合成 fixture 来源。合成样本复现和登记流程见 [数据入仓防护](docs/data-guard.md)。

`range` 的第一个参数同时确定差异基准和硬规则版本。复现 CI 时，使用 PR 事件中的基分支 SHA 与合并提交 SHA。推送遇到 `missing-trusted-ref` 时，先获取目标远端的默认分支，更新本地跟踪引用，再重试。

## 部署、启停与回滚

按 fertile tag 获取 finance 指针与配置；确认 cloudbox / stable profile 和运行模式独立，当前只使用 IBKR paper；检查配置化持久目录、备份、Gateway 与权限。先启动依赖和采集，恢复交易状态并对账，再满足 ready 后启用策略，最后开放查询 API。TBD：具体命令和对账核对表。

停止前暂停开新仓，确认未决订单与持仓，保存恢复状态及采集水位，再按依赖顺序停止；撤单仍须唯一 ID 与先查后试。回滚使用上一个经过验证的 fertile tag，同时核对 catalog 和状态格式兼容，不能只换可执行文件。

## 常见故障

| 场景 | 处置与恢复依据 |
| --- | --- |
| Gateway 断连 | 暂停开新仓，检查认证、网络和重连日志；依赖上游重连和重订阅后完成补数 / 对账，ready 通过才能恢复 |
| 行情陈旧 | 核对数据时间与采集水位，触发熔断，只允许撤单和查询；补数并确认新鲜度后对账，不盲目重启下单 |
| 进程 / 云电脑重启 | 从 /home/box 持久状态恢复，重建依赖、订阅与快照，核对订单 / 成交 / 持仓，不能将内存清空当作没有订单 |
| API 或 web 故障 | 恢复入口查询，不重启健康的交易核心；finance 不依赖 Tunnel |

## 应急暂停

优先通过未来受控控制面暂停开新仓；TBD：操作入口、权限和人工确认步骤。检查未决写请求，先查订单与成交再决定撤单或重试。状态不可信时保持暂停，记录时间、fertile tag、profile、错误及脱敏证据，使用现网问题模板开 issue 并关联原需求。分支内问题在原分支修复。

本次未执行真实账户或 24～72 小时连接测试，验证设计见 [connectivity.md](docs/connectivity.md)。

## 源码构建与缓存恢复

source /home/box/fertile/env.sh 后运行 bash scripts/build.sh 和 bash scripts/smoke.sh。默认缓存、wheel、虚拟环境和独立 worktree 均位于 /home/box/fertile/cache/finance，不依赖 /tmp 或系统目录持久化。恢复时核对源码 commit、toolchain.env 和 uv.lock，检查 wheel 来源及摘要，不能从 PyPI 补装 Nautilus。首次时间和空间实测见 [docs/build.md](docs/build.md)；当前不启停任何真实交易进程或 Gateway。

若 GitHub Actions 的「系统编译依赖」失败，先查 apt 日志中的源文件路径、Ubuntu 镜像和失败的软件包。缺少官方源文件时核对 runner 镜像布局。官方镜像的临时错误在恢复后可重跑作业。若日志出现 `packages.microsoft.com`，检查 `update` 和 `install` 是否仍共用官方源选项。离线复测命令见 [源码构建 CI](docs/build.md#源码构建-ci-的系统编译依赖)。

## 当前 API 与批任务操作

在 finance 仓库根目录 source /home/box/fertile/env.sh，按 build.md 完成源码构建后：

```sh
export FERTILE_DATA_ROOT=/home/box/fertile/data
bash scripts/backtest-sample.sh
cargo build --locked -p finance-api
FERTILE_PROFILE=cloudbox cargo run --locked -p finance-api
```

另一个终端 curl http://127.0.0.1:8081/healthz/live、/healthz/ready 和 /api/v1/backtests/sample。批任务打印 SUMMARY_PATH；ready 要求根目录可读，sample 为 404 表示尚未生成结果，重新运行批任务；500 时检查文件格式、挂载和权限，不手写假结果。批任务失败会保留旧 latest.json；重启 API 不会清空状态。路径覆盖必须同时传给 API 和批任务。端口覆盖 FINANCE_API_PORT，配置目录 FINANCE_CONFIG_DIR，细节见 [API](docs/api.md)。

API 接受 SIGTERM 和 Ctrl-C 优雅停止；部署守护、日志收集和滚动重启以 fertile deploy/ 为准，本 PR 不部署。默认 loopback / 8081，无账户配置和 Gateway 依赖；不能把当前 ready 当成交易 ready。

## 首批读契约验收

用隔离数据根或直接运行进程内 HTTP 测试。出现 `market_product_not_found` 时核对 source/product 与 F3 的落盘策略。投影位置未知时查看 `local_durable` 和 `projected` 水位，必要时在同一隔离根运行一次投影。因子列表为空时核对离线快照是否已经发布。新查询读取已有 WAL 和投影，因子使用单独的可选快照文件。[查询与错误码](docs/api.md#首批读契约)

```sh
cargo test --locked --offline -p finance-api --test read_contract --test market_read --test factor_read --test read_stream
cargo build --locked --offline -p finance-collector --bin finance-market-wal
FINANCE_F3_TEST_PYTHON="$FINANCE_PYTHON" FINANCE_F3_TEST_WAL_BIN="$WAL_BIN" \
  cargo test --locked --offline -p finance-api --test market_read \
  python_projector_index_is_readable_over_http -- --include-ignored
FINANCE_F8_TEST_PYTHON="$FINANCE_PYTHON" \
  cargo test --locked --offline -p finance-api --test factor_read \
  python_factor_publication_is_read_by_rust_contract -- --include-ignored
cargo run --locked --offline -q -p finance-api -- export-openapi > /tmp/finance.openapi.json
cargo run --locked --offline -q -p finance-api -- export-read-stream /tmp/finance-read-stream
```

`FINANCE_PYTHON` 指向已构建 Nautilus 的 Python，`WAL_BIN` 指向刚构建的 `finance-market-wal`。联测在临时数据根用合成样本验证 Python 投影、因子发布和 Rust API 读取。生成器无需启动 API 或连接数据源。取用同一 finance 提交生成的 OpenAPI、WS/SSE schema 和合成样本供下游核对。[导出与下游更新顺序](docs/api.md#openapi-与契约)

## 第二批策略回测与研究契约验收

使用独立临时数据根运行以下测试。回测详情和取消都使用提交时返回的作业 ID。提交响应丢失时，以原 `Idempotency-Key` 调用 `GET /api/v1/backtests/by-key`。`job_cancelled` 是取消终态。重复取消返回同一终态，已完成作业返回 `409 / job_already_finished`。运行中取消时，worker 结束执行进程并清理临时文件，之后释放租约。异常中断后，恢复流程在租约缺失或到期且执行锁释放后清理临时文件和锁，旧版无租约作业通过遗留执行锁或当次运行目录识别。没有待恢复作业时，检查不获取队列写锁。

```sh
cargo test --locked --offline -p finance-api --test bundle_jobs --test jobs --test decision_read --test factor_read --test read_contract --test read_stream
PYTHONPATH=python "$FINANCE_PYTHON" -m unittest discover -s tests -p test_worker_leases.py -v
```

用 `GET /api/v1/backtests/worker-status` 核对队列和租约。出现缺失或过期的租约时，检查 worker 进程和本地锁，再按[回测作业恢复](docs/backtest-jobs.md#worker-运行与恢复)处置。发布决策信号前核对输入许可和计划哈希，在现有决策目录原子发布 `read-v1.json`。需要下线读取时撤下发布文件。出现 `decision_not_published` 时检查发布文件和数据库是否在同一数据根。升级旧 SMA 库时先用原回放启动新版决策进程，重验整个已提交前缀。出现许可变化时停止发布，在独立数据根用已审核的同许可回放重建。因子、决策和回测队列绑定数据根以下的实体路径。因子报告按因子版本和评价 ID 从 F8 快照定位。[查询参数、许可和发布格式](docs/api.md#第二批策略回测与研究契约)

### 公开回测列表验收

在隔离数据根运行以下测试，核对旧库升级、旧写入格式、索引范围查询以及跨私有历史的分页。

```sh
cargo test --locked --offline -p finance-api legacy
```

新版 API 首次打开队列或新版 worker 启动时，SQLite 自动为已有队列建立 `jobs_legacy_id` 部分索引。索引已存在时会直接复用。若队列较大，可在切换 web 匿名列表之前以新版 API 访问一次 `GET /api/v1/backtests?visibility=legacy&limit=1` 并确认返回 200。旧版 API 和 worker 仍可使用升级后的队列。web 匿名请求固定使用 `visibility=legacy`，并在续页时保留该参数。回滚到旧版 finance 时需同时回滚依赖此参数的 web 版本。[回测列表分页](docs/api.md#回测列表分页)

## 第三批读契约验收

用进程内路由和合成输入核对 bars、指标、Binance profile 与期权响应。测试使用临时数据根。

```sh
cargo test --locked --offline -p finance-api --test bars --test market_options --test market_read --test read_contract --test read_stream
```

若 `/api/v1/market/binance/status` 返回 `disabled_by_profile`，核对 API 的 `FINANCE_BINANCE_CONFIG` 和 `FERTILE_PROFILE`。若配置已启用，采集端实际状态查看独立日志，数据持久进度查看 F3 水位。独立 API 的期权查询返回 `market_read_source_unavailable` 时，检查部署是否接入获准的共享行情所有者。接入后，深度的 `permission_unconfirmed` 表示须核对 L1、TotalView 和 EDS，`reset` 时清空两侧。历史窗口 404 时核对所有者取得的窗口，`expired` 表示该合约在 F12 读取时已不可得。[第三批接口与权限](docs/api.md#第三批读契约)

排查期权链时，按快照 `as_of` 的美东日期核对到期日。订阅覆盖返回 `500 / market_read_failed` 时，在行情所有者侧核对响应键的 `con_id`、OPRA 产品和 OI 请求参数。

## 第四批期权流读契约验收

```sh
cargo test --locked --offline -p finance-api --test option_flow --test market_options --test read_contract --test read_stream
cargo test --locked --offline -p finance-collector option_flow
```

测试在临时数据根用合成观察、逐笔与时钟核对分页、精度、许可分流和已有流契约。若查询返回 `503 / market_read_source_unavailable`，核对是否由共享行情所有者给 `router_with_market_reads` 接入对应查询源。收到 `404 / market_read_not_found` 时核对合约、半开时窗、快照保留范围和相应产品的私有展示许可。历史核验缺口按响应的 `issues` 查明 TRADES 或 BID_ASK 及区间。`expired_contract` 表示该时段的逐笔已不可回补。

已获本地记录许可的 `ibkr/opra-history` 核验报告可用 `durable_through` 对照同一数据根和产品的 F3 水位。仅展示模式从进程内读取，核验水位为空。按[接口导出流程](docs/api.md#openapi-与契约)取用同一提交生成的 OpenAPI 和已有的流产物。[第四批接口与权限](docs/api.md#第四批期权流读契约)

## 行情订阅需求验收

`GET /api/v1/market/subscriptions/demand` 查看状态。`PUT` 同一路径提交完整快照，JSON `{}` 清除全部追加需求。独立 API 默认对有效请求返回 `200` 和 `state: disabled`，忽略该次需求，零公网请求。已有 `GET /api/v1/market/subscriptions` 继续查询实际行数。

```sh
cargo test --locked --offline -p finance-api --test subscription_demand --test market_options
cargo test --locked --offline -p finance-collector --lib line_budget::session::tests
```

验收使用进程内路由和合成 paper 会话。在 Linux x86_64 或 aarch64 上，关闭态回归会检测 `socket` 或 `connect` 调用。调度测试用 SDK 内存回放核对冷却中停止或断线后的实际写入。仅在已经核实 paper 只读账户及产品权限的部署中配置 `api.subscription_owner`。给所有者分配独占的 IB client ID，核对预算阶段和场所簿数据状态。API 退出时先关闭 paper 连接，再释放活动订阅。所有者进程重启后，由 web 重发已保存的自选和 focus。遇到 `400 / subscription_demand_invalid`，检查完整的 F1 ID、深度资格和 JSON 字段。出现 `enabled` 后，通过预算和场所簿状态判断实际覆盖。[配置与接入](docs/api.md#行情订阅需求)

## 离线因子研究验收

在仓库根目录用含研究依赖的现有 Python 环境执行。定向测试在内存生成截面和收盘价，月频登记读取仓库内原报告并核对证据哈希。

```sh
PYTHONPATH=python /path/to/research/venv/bin/python -m unittest discover -s tests -p 'test_factor_*.py' -v
PYTHONPATH=python /path/to/research/venv/bin/python -m unittest discover -s tests -p 'test_historical_research.py' -v
```

报告复算时固定因子版、数据版、交易会话、各段区间、分位数和成本档。用逐期参考权重及目标权重复算换手、费用及费前费后收益，再核对复利汇总。遇到未来披露诊断时核对数据源的真实发布时间和该版修订的可见区间。停止新回测选择时将相应因子版本退回 `observe_only`，记录理由并保留原报告引用。入参、状态和历史证据见 [因子研究接口](docs/factor-research.md)。

## 离线期权敞口验收

用锁定的 Rust 工具链在仓库根目录运行合成期权链测试。

```sh
cargo test --locked --offline -p finance-collector --test option_exposure
```

先按测试中的现货价、合约乘数、OI 和 gamma 手算单合约金额，再核对各执行价与到期日汇总以及 0DTE 分组。诊断局部估算时，查看候选范围、缺失原因、OI 业务日期和输入观察时间。flip 结果随估值价格网格和模型输入解释。该计算通过 Rust 库调用，测试直接在内存中构造数据。输入及输出约定见 [期权敞口接口](docs/option-exposure.md)。

## 行情与作业

从 finance 仓库根运行。先按源码构建文档准备虚拟环境，设置各进程共用的 FERTILE_DATA_ROOT，然后构建三个 Rust 程序。

```sh
cargo build --release --locked --workspace
cargo run --release --locked -p finance-ib -- probe
cargo run --release --locked -p finance-collector
# 另一个终端
curl --fail http://127.0.0.1:8082/healthz/ready
curl --fail http://127.0.0.1:8081/api/v1/market/status
scripts/finance-backtest-worker
```

Replay 可单独运行。paper 模式下 probe 用 client ID 91，collector 用 92，93 预留给 trader，凭据由盒子上的 Gateway 读取。连接、补数、权限与 pacing 故障定位见 [IB 接入](docs/integrations/ib.md)，水位、精度和杀进程恢复见 [catalog](catalog/README.md)，worker 的资源限制、队列恢复及幂等请求见 [作业说明](docs/backtest-jobs.md)。部署守护从 fertile 的 runbook 进入。

### 离线验收期权流观察与核验

```sh
cargo test --locked --offline -p finance-ib option_ticks
cargo test --locked --offline -p finance-collector option_flow
cargo test --locked --offline -p finance-market-protocol option_history
PYTHONPATH=python "$FINANCE_PYTHON" -m unittest discover -s tests -p 'test_market_projector.py' -v
```

测试用固定合成合约、逐笔和时钟，在临时目录检查观察与核验计数、同秒重复成交、方向估计、权限分流和 F3 回放。调用方在经授权的私有运行中提供 OPRA L1 与历史逐笔各自的许可策略。历史源按 paper 只读配置显式接入。记录模式用 `finance-market-wal` 的 `watermarks` 和 `quality` 对 `ibkr/opra-history` 核对持久位置及逐笔缺口。遇到 pacing 或权限问题，按原因处理并保留未核验区间。到期不可回补的区间保留覆盖状态。相关接口、字段和上下游边界见 [期权流观察与历史核验](docs/option-flow.md)。

### 私有验证美股 L1 与 OPRA

经授权后，在美股常规交易时段使用已登录的本机 paper Gateway，确认 Gateway 的 API 设置为只读。选择一份已知活跃的 SMART USD 美股期权，使用该合约的 `conId`，按 [产品权限验收](docs/integrations/ib.md#美股-l1-和-opra-权限验收) 运行 `scripts/check-ib-entitlements.sh`。权限探针占用现有 probe client ID 91，逐标的开一条流并在进入下一项前取消。`pending` 保留具体原因。遇到 `gateway_disconnected` 时，先恢复只读 paper 会话再重新运行探针。遇到 `cancel_failed` 时，本轮行数回收结果未知，探针关闭连接并保留已完成标的的证据。发现休市、无回调或报价不完整时，于常规交易时段复测。探针只输出脱敏摘要，原始行情和账户资料留在私有环境。

### 离线验收 IBKR 行情线预算

在仓库根执行下列测试。测试在进程内构造期权链和行情回调，并推进虚拟时间验证轮换、行数回收及恢复限速。

```sh
cargo test --locked --offline -p finance-collector --lib line_budget
cargo test --locked --offline -p finance-ib --lib connection_replay_tests
cargo test --locked --offline -p finance-ib --lib market::ib::chain
```

接入时由调用方显式运行 `LineScheduler::run_paper` 并持有专用行情 client ID。默认使用配置中的 probe client ID，同一 ID 需要在调度器和私有权限探针之间互斥使用。读取 `LineHandle::status()` 核对三类占用、预留和发现状态。股票确认超时会在同一会话退避并排到队尾。初始握手失败时从失败返回起至少等 15 秒才重连，较长的配置冷却或重连退避仍生效。账户验证及行情类型选择期间收到的全局节流也限制下一次重连时间。发生节流、链响应丢帧或重连时，可继续查已观测窗口和缺口，等 `discovery_complete` 后再使用完整分母。额度以账户实测为准，调低 `BudgetConfig` 并保留 TWS/深度余量即可按部署调整。行情正文和覆盖状态驻于本进程内存。[IBKR 行情线预算](docs/integrations/ib-line-budget.md)

### 离线验收 Nasdaq TotalView 场所深度

焦点测试使用独立列出的 QQQ、AAPL、MSFT、NVDA、AMZN、GOOGL、META、TSLA 逐一验证切换，另验证 SPY 拒绝、两个固定名额与一个页面名额、清簿再订阅和场所及档数标签。深度测试使用合成回调和内存 Gateway 回放，无需账户配置。

WAL 隔离测试在子进程中运行深度展示入口，将工作目录和 `FERTILE_DATA_ROOT` 都指向同一个临时目录。子进程在展示盘口时和退出展示循环后核对目录为空，并检查 WAL 产品和 TotalView 记录策略。随后在同一目录落盘一条获准记录的合成 F1 盘口快照作为对照，父进程核对只存在该对照产品且能原样回读快照。产品策略测试另行核对 `ibkr/nasdaq-totalview` 拒绝本地记录、非展示派生与 Drive 备份。

```sh
cargo test --locked --offline -p finance-ib depth
cargo test --locked --offline -p finance-collector --test nasdaq_totalview
cargo test --locked --offline -p finance-collector totalview_product_cannot_record_derive_or_back_up
```

授权的私有 paper 验收须独立核对综合股票 L1、TotalView-OpenView、EDS、paper 数据共享和实际 Nasdaq 深度回调。遇到休市或无回调时保留待验证状态，遇到名额不足时检查同一用户名的现有深度窗口。真实证据遵循 [数据入仓防护](docs/data-guard.md)。接口和故障状态见 [Nasdaq TotalView 场所深度](docs/integrations/ib.md#nasdaq-totalview-场所深度)。

### F1 行情 WAL 与投影

生产者通过 [本地行情接口](docs/market-wal.md) 绑定产品许可，并在确认采集前提交 WAL。在独立数据根可使用已有源码构建的 Python 环境追平投影并检查水位。下列命令把 `$DATA_ROOT`、`$SOURCE` 和 `$PRODUCT` 设为目标数据根与已登记产品，`$WAL_BIN` 设为构建的 `finance-market-wal` 路径。

```sh
cargo build --locked -p finance-collector
PYTHONPATH=python "$FINANCE_PYTHON" -m fertile_finance.market_projector \
  --data-root "$DATA_ROOT" --wal-bin "$WAL_BIN" --once
"$WAL_BIN" --data-root "$DATA_ROOT" watermarks --source "$SOURCE" --product "$PRODUCT"
"$WAL_BIN" --data-root "$DATA_ROOT" quality --source "$SOURCE" --product "$PRODUCT"
```

需要持续投影时去掉 `--once`，进程接受 SIGTERM/SIGINT，完成正在处理的批次后退出。写者异常退出后用同一数据根与许可重新打开，接着从已确认位置恢复。投影报错时保留 WAL 和游标，检查磁盘、校验错误或 F1 身份冲突，使用隔离数据根验证恢复。磁盘空间不足时先停可重建投影和低优先任务，记录未采区间。

经授权备份的产品可在写者运行时通过生产者封段，或在停写时使用 `finance-market-wal seal`。按 [封段清单 v1](docs/market-wal.md#t2b-封段清单-v1) 枚举并校验封段，再交给 fertile 的备份协调方。旧 collector 仍由上面的运行方式管理。

Nautilus 源码 CI 使用构建后的 Python 环境运行 Rust WAL 到 Parquet 的端到端测试。本地可用同一命令核对四条水位、同秒逐笔保留、去重和修订索引。`committed_wal_projects_through_python_cli` 在每次投影后检查退出状态与水位，并启动 8 个独立的 Python 校验进程读取分区和最新修订，逐次检查正常退出。

```sh
FINANCE_F3_TEST_PYTHON="$FINANCE_PYTHON" \
  cargo test --locked -p finance-collector --test market_wal_projector -- --include-ignored
```

### Binance 公共行情

`finance-binance` 在内置 profile 下输出 `disabled_by_profile` 并退出。只在经审核的部署中使用 [Binance profile](docs/integrations/binance.md) 显式开启。启用前确认产品许可、数据根与 Nautilus 源码构建环境，进程的 `PYTHONPATH` 指向 finance 的 `python/`。该任务无需本地监听。使用独立日志区分 `synchronizing`、`live`、`recovering` 和 `region_restricted`。看到 403/451 时保留状态码并停止自动重启。盘口出现缺口时，通过 F3 的 `quality` 核对 `source_gap`、恢复快照和历史缺口范围。资金费率的 `next_funding_ts: null` 表示交易所尚未给出下次结算时间，已收到的费率仍可使用。

```sh
cargo test --workspace --locked --offline
PYTHONPATH=python python -m unittest discover -s tests -p test_binance_transport.py -v
```

### Hyperliquid 公共行情

由 fertile 部署显式启动独立的 `finance-hyperliquid` 进程，配置参考 [hyperliquid.yaml](config/collector/hyperliquid.yaml)。启用前核对标的、产品条款和各用途权限。设置已有的 `FINANCE_PYTHON` 为源码构建的 Python、`PYTHONPATH=python`，并为 `FERTILE_DATA_ROOT` 明确指定数据根。启用落盘前应创建该目录。下列启动命令会连接公共 Hyperliquid。

```sh
cargo run --locked -p finance-collector --bin finance-hyperliquid -- /path/to/reviewed-hyperliquid.yaml
```

进程没有监听端口，SIGTERM/SIGINT 停止采集、清除流基线并刷新已暂存 WAL。记录模式下，用 `finance-market-wal --data-root "$FERTILE_DATA_ROOT" watermarks --source hyperliquid --product perp-public` 查持久水位，将子命令换为 `quality` 查质量报告。核对首张新快照的 `book_restored_from_snapshot` 和历史缺口的 `history_unrecoverable`。发生 WebSocket 订阅或帧校验失败时检查连接与订阅确认，采集器按上限退避重试。若重连后仅收到与断线前时间戳相同或更早的簿，当前簿继续等待新快照。遇到策略绑定或存储错误，检查数据根、许可与单写者，策略改变按 [F3 规则](docs/market-wal.md#生产者与许可) 处理。

离线验收使用 `cargo test --locked --offline -p finance-collector --test hyperliquid` 与 `PYTHONPATH=python "$FINANCE_PYTHON" -m unittest discover -s tests -p 'test_hyperliquid*.py' -v`。Rust 测试用 `finance-hyperliquid CONFIG --replay FILE` 重放合成报文。该模式读取 UTF-8 JSON Lines，`event` 为 `metadata`、`connected`、`disconnected` 或 `frame`，每行都含十进制字符串 `receive_ts`（UTC 纳秒）。`metadata` 和 `frame` 行的 `data` 是原始 JSON 文本字符串。回放使用隔离数据根。行情帧校验失败后，回放丢弃该连接剩余帧，直到下一条 `connected`。完整语义见 [Hyperliquid 行情接口](docs/integrations/hyperliquid.md)。

### 主图查询

F3 投影进程发布 F1 事件的 Parquet 和修订索引。主图读取 `finance-state/bars/v1/` 下由受信发布方按许可另行生成的展示快照。

用 `GET /api/v1/indicators` 核对已注册的算法版本。请求 `GET /api/v1/market/bars` 时提供与展示投影一致的标的、源、产品、周期和复权口径。404 时检查同一数据根下是否已发布该序列。收到 `indicator_history_unavailable` 时核对 `history_complete` 和序列起点，补足前置历史后重新发布。收到 `indicator_work_limit` 时减小 `limit` 后分页，若前置历史已超预算，则选择更少或更短周期的指标。收到 `indicator_calculation_failed` 时按同一组件参数检查整个闭合前缀的数值和精度。其余投影 500 错误按文件权限、时间次序和 OHLCV 类型排查。旧数据的问题按快照覆盖、`as_of`、`first_available_ts` 和 `delay_tier` 排查。API 启动参数与健康检查沿用原接口，部署须在受控数据根发布合法展示投影。详细契约见 [主图 API](docs/api.md#主图行情与指标)。

收到 500 `bars_invalid` 时，核对投影 OHLCV 是否符合 F1 十进制文本规则。每个字段的整数位和小数位合计最多 28 位，超限在投影解析时即会报错。合法输入的计算结果还须满足输出编码范围，详见 [数值域与舍入](docs/indicators.md#数值域与舍入)。

### 在盒子上验证 paper 行情恢复

先用已登录的 Gateway 运行上方 probe。给 paper collector 配置独立的已挂载数据根目录，例如 /home/box/fertile/data-paper；finance-api 用同一数据根，回放继续使用自己的目录。使用实时市场数据订阅时将 FINANCE_COLLECTOR_CONFIG 指向 config/collector/ib-cloudbox.yaml；选择延迟数据时使用 config/collector/ib-delayed.yaml。确认 SPY 对应交易时段有行情，按 [IB 接入](docs/integrations/ib.md) 检查订阅权限与 pacing 日志。

在 collector、finance-api 已运行时，记录 /healthz/ready、/api/v1/market/status 和当时的持久水位。连续采集至少 30 分钟，再从盒子执行：

```sh
# 在 finance 仓库内执行。只阻断本地 paper Gateway 端口，脚本到时撤销规则。
bash scripts/fault-ib-outage.sh 120
curl -sS http://127.0.0.1:8082/healthz/ready
curl -sS http://127.0.0.1:8081/api/v1/market/status
```

记录断开和恢复的 UTC 时间、订阅重建、补数区间、首尾 bar 时间和数量。核对 Nautilus 在受影响交易时段读回的时间戳与 IB 历史返回一致，再确认 ready 为 200。休市和无成交区间要单独标注，不能用合成行情填补。遇到 2FA、无权限或历史数据不可用时保存脱敏错误并保留未验证状态。

历史回测导入、信号和审计见 [研究回测手册](docs/historical-backtest.md)。部署时向 API 和 worker 配置同一只读研究 catalog；请求状态和结果仍由持久作业队列管理。

### 实时决策与并行回测

多个 `scripts/finance-backtest-worker` 实例可共用同一宿主本地盘上的 `FERTILE_DATA_ROOT`。升级前让旧 worker 完成在途作业并停止，再启动新 worker。每个实例单独配置并发和资源限制。作业租约默认 15 秒，实例用 `FINANCE_WORKER_LEASE_SECONDS` 或 `--lease-seconds` 设置续租周期的基准。配置作业的指标子进程也持有执行锁。租约过期后，恢复仍须等旧作业的执行进程释放该锁。迁移、超时和重复执行防护见 [作业手册](docs/backtest-jobs.md#worker-运行与恢复)。

使用 `finance-strategy` 编译单标的计划，并为 `finance-decision` 指定已有的绝对数据根和获准计算与留存的 F1 回放文件。以下 `TEST.XNYS` 为合成标的，回放需由输入方准备。

```sh
cargo build --locked -p finance-api --bin finance-strategy
target/debug/finance-strategy compile-legacy-sma TEST.XNYS 2 3 10 > /path/to/sma-plan.json
FERTILE_DATA_ROOT=/path/to/decision-data scripts/finance-decision \
  --plan /path/to/sma-plan.json --replay /path/to/test-bars.ndjson
```

运行 `StrategyBundle` 时，用同一个 `finance-strategy compile` 命令生成 F2 计划，准备按 XNYS 收盘顺序写入的会话 NDJSON，再通过已有源码构建的 Python 环境启动。配置回测 worker 与实时决策都需要同一 finance 构建的 `finance-strategy`，可置于 `PATH` 或在各自环境中设置 `FINANCE_STRATEGY_BIN` 为绝对路径。每帧包含闭合行情、点时成分和只读组合快照。格式见 [配置策略会话输入](docs/decision-process.md#配置策略会话输入)。

```sh
target/debug/finance-strategy compile /path/to/bundle.yaml > /path/to/bundle-plan.json
FINANCE_STRATEGY_BIN="$PWD/target/debug/finance-strategy" \
  FINANCE_PYTHON=/path/to/source-built/venv/bin/python FERTILE_DATA_ROOT=/path/to/bundle-decision-data \
  scripts/finance-decision --plan /path/to/bundle-plan.json --replay /path/to/decision-sessions.ndjson
```

升级配置策略的求值器版本时，保留原决策库，用新的数据根和原始获准回放重建基线。启动配置策略时若指标求值报错，核对 `FINANCE_STRATEGY_BIN`、二进制版本与执行权限，再核对闭合行情和计划的指标选择。

一次性回放在读完后退出。对持续追加的同一文件加 `--follow`。向决策进程发送 SIGTERM 可中断正在等待的指标管道读写。进程回收指标子进程后退出，使用同一参数即可恢复。保留回放文件已提交的字节前缀和 `finance-state/decisions/decisions.sqlite3`。重启会重建指标状态、核对前缀并从持久断点续读。遇到缺口、更正或输入文件替换，先核对源和覆盖范围。用已复核的新输入基线和新数据根建立独立记录。进程健康由守护器检查，消费进度用决策库中的持久检查点核对。此进程只读行情，写入本地策略目标和消费进度，无端口或账户配置。文件格式、边界与诊断见 [实时决策接口](docs/decision-process.md)。

### 历史价量补拉

在获准采集并落盘的部署上，使用 [历史价量批任务](docs/historical-pull.md) 拉取九只美股日线和 IBKR paper 只读分钟线、BTC/ETH/SOL 公共 K 线及 Binance Spot 逐笔归档。命令需显式授权，Binance 默认关闭。使用独立于工作树的配置数据根，记录 JSON 的水位和缺口，失败后用同一参数续拉。与 Gateway 的其他历史请求错开。来源口径、命令、退出状态和耗时估算见批任务文档。

### 离线验证历史期权链

在已准备好锁定的 Rust 工具链和依赖缓存后运行 `cargo test -p finance-collector --test option_history --locked --offline`。测试通过 F9a 接口的合成替身核对合约确认、历史请求和到期行为。真实读取仅使用已验证的只读 paper 会话，并与 F11 使用同一数据根以共用历史请求节流。权限或连接错误按单合约返回。

运行 `cargo test -p finance-ib --lib verified_paper_option_history_requests_and_decodes_synthetic_bars --locked --offline`，在内存传输上验证生产历史请求的发出、解码与到期阻断。

Databento 缺省禁用，即使配置估算常量也只进行离线估算。取得注明日期和来源的按量费率后才能用于预算。启用任何真实 Databento 请求之前须由 vira 确认费用、预算和数据用途授权。[历史期权链](docs/option-history.md)

### 隔离验收配置策略作业

使用源码构建的 Python 环境从仓库根运行以下测试。测试在临时数据根生成合成日线，通过 Axum 路由提交并在独立 worker 中执行，再查询结果。无需启动常驻服务或绑定端口。

```sh
FINANCE_STRATEGY_TEST_PYTHON=/path/to/source-built/venv/bin/python \
  cargo test --locked -p finance-api --test config_strategy_worker -- --ignored
```

在受控环境提交配置作业时，先将已审核的回放 JSON 发布到现有数据根下的 `finance-state/backtests/strategy-inputs/`，再按 [HTTP 接口](docs/api.md#配置策略提交与结果) 提交。API 与 worker 共享同一个 `jobs.sqlite3`。排查失败时先读 `status` 与 `error`。区间、预热、点时成分或收盘 bar 缺口的检查方法见 [配置策略作业](docs/backtest-jobs.md#配置策略作业)。客户端超时后用同一幂等键确认或重试。已入队作业使用该作业关联的快照，覆盖源文件只影响随后提交的新作业。

### 隔离验收官方历史作业

从 finance 仓库根运行。先按 [IBM 2024 原始日线](docs/historical-backtest.md#ibm-2024-原始日线) 转换、复核并导入日线快照，再指定与部署分开的数据根、只读研究根和可用的本机端口。

```sh
cargo build --locked -p finance-api
export FINANCE_PYTHON=/path/to/source-built/venv/bin/python
export PYTHONPATH=python
export FINANCE_ISOLATED_DATA=/path/to/isolated/job-data
export FERTILE_RESEARCH_ROOT=/path/to/isolated/research
mkdir -p "$FINANCE_ISOLATED_DATA"

"$FINANCE_PYTHON" -m fertile_finance.isolated_historical_job \
  --api-bin /absolute/path/to/target/debug/finance-api \
  --data-root "$FINANCE_ISOLATED_DATA" --research-root "$FERTILE_RESEARCH_ROOT" \
  --port 18081 --instrument IBM.XNYS --start 2024-01-02 --end 2024-12-31
```

运行器在环回地址启动专用 API 和 worker，通过 HTTP 提交并轮询。省略 `--port` 时自动选用高位端口。队列位于所选数据根的 `finance-state/backtests/jobs.sqlite3`。标准输出为终态作业 JSON，运行结束后 API、worker 和端口均释放。核对 `summary.research.windows` 中依序返回的 `50y`、`10y`、`5y`、`longest_sample`，并记录每行实际首尾、覆盖状态和观测数。覆盖不足时 `metrics` 为 `null`，最长样本的 `requested_years` 为 `null`。更多进程和队列说明见 [作业手册](docs/backtest-jobs.md#隔离运行官方历史作业)。

长样本验收使用不同于 2024 示例的研究根。先按 [跨拆股的 IBM 长历史](docs/historical-backtest.md#跨拆股的-ibm-长历史) 完成供应商归一化口径审核、转换和血缘导入，再以同一入口提交长区间。调整日期以实际已导入的首尾为准，并给最慢均线留出预热会话。

```sh
"$FINANCE_PYTHON" -m fertile_finance.isolated_historical_job \
  --api-bin /absolute/path/to/target/debug/finance-api \
  --data-root "$FINANCE_ISOLATED_DATA" --research-root "$FERTILE_RESEARCH_ROOT" \
  --instrument IBM.XNYS --start 1975-11-25 --end 2026-11-25
```

先核对终态 JSON 的 `summary.research.dataset.price_basis` 为 `yahoo_chart_split_normalized`，再检查四窗口。目录内的 `yahoo-lineage.json` 随快照归档，以复核所用的拆股事件和价格口径。

**官方作业不使用 FRED。** 四窗口验收无须 FRED series ID 或 Graph CSV 的 `cosd`、`coed`。本次已导入的 2024 年 `DGS10` 共 261 条，是公开历史归档示例，其区间不影响验收。Ken French 日频也无须提供，官方 Sharpe 固定使用 `rf_source="rf=0"`。本次 French 文件的观测截止日为 2026-09-30，实际写入 1970-01-02 至 2026-09-30，共 14,308 条，另丢弃 12,030 条早于 1970 年的记录。Yahoo 和 SPY 的截止日应从各自存档独立核对。

## 手工保存公开历史数据

先确认 Yahoo、Ken French Data Library、FRED 及底层提供商允许所需的下载、使用和本地归档，并确认有网络访问。以下是有许可的人工步骤。CI 使用仓库内构造的小样本。下载文件存放在仓库之外。若服务端拒绝请求，检查数据许可与端点要求。

从 finance 仓库根使用源码构建的 Python。示例下载 IBM 和 SPY 日线、French FF3、FF5 2x3 和动量日序列、以及 FRED 的 DGS10 日序列。若取得的是 FRED observations API JSON，可保存在本地后交给同一加载器读取。示例 Graph CSV 不需 API 密钥。

Yahoo 下载入口为 `python -m fertile_finance.download_yahoo_daily --help`。其 URL 使用 `yahoo_chart_url(symbol, start, end)` 的符号和日期约定，例如 `yahoo_chart_url("SPY", date(2024, 1, 1), date(2024, 12, 31))`。CLI 的 `--symbol` 是 Yahoo 代码，`--start` 和 `--end` 为含首尾的纽约当地日期。日线请求同时包含分红、拆股和调整收盘价。

默认 User-Agent 为 `fertile-finance/0.0.0 (historical daily archive)`，可通过 `--user-agent` 指定实际客户端标识。先请求 `query1.finance.yahoo.com`，遇到 HTTP 429、408、5xx 或连接失败时，下一次尝试切到同一路径的 `query2.finance.yahoo.com`，之后在两者间交替。总计最多尝试 4 次，连接与读取的 socket 超时为 15 秒。重试前按 1、2、4 秒退避，合法的 `Retry-After` 秒数或 HTTP 日期要求更久时取较长等待。累计退避等待最多 30 秒，超过预算即退出。成功报告实际使用的 URL。请指定已存在的归档目录和新文件名。

连续拒绝或超过预算时，命令退出非零。保留报错中的脱敏状态码、尝试端点和 `Retry-After`、`Date` 响应头，可按供应商要求稍后再试。归档和仓库均不应保存完整错误页。使用文档列出的端点和有失败检查的命令。供应商持续拒绝时，将对应数据和验收项保持为未验证。

```sh
export FINANCE_PYTHON=/path/to/source-built/venv/bin/python
export FINANCE_HISTORY_ARCHIVE=/path/to/permitted/archive
export FERTILE_DATA_ROOT=/path/to/new/data-root
export PYTHONPATH=python
mkdir -p "$FINANCE_HISTORY_ARCHIVE"

"$FINANCE_PYTHON" -m fertile_finance.download_yahoo_daily \
  --symbol IBM --start 2024-01-01 --end 2024-12-31 \
  --output "$FINANCE_HISTORY_ARCHIVE/yahoo-IBM.json"
"$FINANCE_PYTHON" -m fertile_finance.download_yahoo_daily \
  --symbol SPY --start 2024-01-01 --end 2024-12-31 \
  --output "$FINANCE_HISTORY_ARCHIVE/yahoo-SPY.json"

curl --fail --location 'https://mba.tuck.dartmouth.edu/pages/faculty/ken.french/ftp/F-F_Research_Data_Factors_daily_CSV.zip' \
  -o "$FINANCE_HISTORY_ARCHIVE/french-factors.zip"
curl --fail --location 'https://mba.tuck.dartmouth.edu/pages/faculty/ken.french/ftp/F-F_Research_Data_5_Factors_2x3_daily_CSV.zip' \
  -o "$FINANCE_HISTORY_ARCHIVE/french-ff5-2x3-daily.zip"
curl --fail --location 'https://mba.tuck.dartmouth.edu/pages/faculty/ken.french/ftp/F-F_Momentum_Factor_daily_CSV.zip' \
  -o "$FINANCE_HISTORY_ARCHIVE/french-momentum.zip"
curl --fail --location 'https://fred.stlouisfed.org/graph/fredgraph.csv?id=DGS10&cosd=2024-01-01&coed=2024-12-31' \
  -o "$FINANCE_HISTORY_ARCHIVE/fred-DGS10.csv"
```

记录各文件的获取日期、来源、实际首尾日期、观测数及缺口，核对 FRED 频率及单位。请求日期仅用于演示，是否达到 50 年满窗由官方作业按实际净值及会话覆盖判定。将下例的 `FINANCE_YAHOO_AS_OF` 改为 Yahoo 存档实际观测截止日。French 的完整存档通常含预 1970 记录，示例显式丢弃并报告。当前 `load_ken_french` 支持 FF3 和 Momentum，FF5 下载文件单独归档。重复导入已存在区间时，应先核对覆盖范围，再向 `write_*` 传入 `mode="replace"` 及确认过的 `start`、`end`。

```sh
export FINANCE_YAHOO_AS_OF=YYYY-MM-DD
"$FINANCE_PYTHON" - <<'PY'
from datetime import date
import os
from pathlib import Path

from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.research_factors import load_fred, load_ken_french
from fertile_finance.yahoo_daily import load_yahoo_daily

archive = Path(os.environ["FINANCE_HISTORY_ARCHIVE"])
catalog = HistoricalCatalog()
yahoo = load_yahoo_daily(
    archive / "yahoo-IBM.json",
    adjustment_as_of=date.fromisoformat(os.environ["FINANCE_YAHOO_AS_OF"]),
)
french = load_ken_french(
    archive / "french-factors.zip",
    momentum_path=archive / "french-momentum.zip",
    pre_epoch="drop",
)
fred = load_fred(archive / "fred-DGS10.csv", series_id="DGS10")
print(catalog.write_yahoo(yahoo))
print(catalog.write_french(french))
print(catalog.write_fred(fred))
PY
```

核对报告里的实际写入首尾、记录数与预 1970 丢弃范围。导入 Yahoo 时，同时核对写入的缺失行情行数 `missing_quote_rows` 和跳过的全空非会话行数 `skipped_missing_non_sessions`。读取约定见 [Python 研究接口](python/README.md#历史数据接口)。

### 可选扩展 FRED 归档以对齐 SPY 样本

独立研究如需将 `DGS10` 归档与旧 SPY 最长行情样本对齐，使用 `cosd=1993-01-29`（本机旧 SPY 起点约为该日），将 `coed` 设为已核实的 SPY 归档实际末日。下例用 `2024-12-31` 演示设置末日，并导入新的独立数据根。核对 FRED 元数据中的日频和百分比单位。DGS10 是美国 10 年期国债恒定期限市场收益率，按日观测，存档保留年化百分比报价。记录 FRED 实际覆盖首尾及缺值。这个选做区间不参与官方四窗口的覆盖判定或 Sharpe。

```sh
export FINANCE_FRED_FIRST_DATE=1993-01-29
export FINANCE_SPY_LAST_SESSION=2024-12-31  # 改为核实的 SPY 归档末日
export FINANCE_FRED_DATA_ROOT=/path/to/isolated/fred-spy-span
export FINANCE_FRED_SPY_CSV="$FINANCE_HISTORY_ARCHIVE/fred-DGS10-spy-span.csv"
curl --fail --location --get 'https://fred.stlouisfed.org/graph/fredgraph.csv' \
  --data-urlencode 'id=DGS10' --data-urlencode "cosd=$FINANCE_FRED_FIRST_DATE" \
  --data-urlencode "coed=$FINANCE_SPY_LAST_SESSION" --output "$FINANCE_FRED_SPY_CSV"
```

下载成功并核对归档后，按研究需要导入。

```sh
"$FINANCE_PYTHON" - <<'PY'
from datetime import date
import os
from pathlib import Path

from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.research_factors import load_fred

series = load_fred(Path(os.environ["FINANCE_FRED_SPY_CSV"]), series_id="DGS10")
catalog = HistoricalCatalog(Path(os.environ["FINANCE_FRED_DATA_ROOT"]))
start = date.fromisoformat(os.environ["FINANCE_FRED_FIRST_DATE"])
end = date.fromisoformat(os.environ["FINANCE_SPY_LAST_SESSION"])
print(catalog.write_fred(series, start=start, end=end))
print("Missing DGS10 values:", sum(row.value is None for row in series.rows))
PY
```

### 迁移旧 SPY 数据和准备官方输入

旧 SPY 迁移先对停写的旧 catalog 或备份执行 `--dry-run`，将 Yahoo 请求日期改为旧数据实际区间。核对报告后去掉 `--dry-run`，在独立数据根重导。完整命令、标的和时间抽查约定见 [历史数据目录](catalog/README.md#既有-spy-固定-2100-utc-数据)。

官方作业使用 `convert_research_csv` 从许可的 Yahoo 文件或 `HistoricalCatalog` 导出 CSV 和血缘。核对原始 OHLCV 与无拆股记录后，使用 `research_catalog --raw-split-free-reviewed` 导入。跨拆股时先核实 Yahoo 已统一 OHLCV 股份口径，再使用 `--yahoo-split-normalized-evidence` 转换，并用 `research_catalog --yahoo-split-normalized-lineage` 导入。两者均在独立研究根操作。价格容差、审核证据和复制命令见 [准备数据](docs/historical-backtest.md#准备数据)。

```sh
"$FINANCE_PYTHON" -m fertile_finance.download_yahoo_daily --help
"$FINANCE_PYTHON" -m fertile_finance.isolated_historical_job --help
"$FINANCE_PYTHON" -B -m fertile_finance.migrate_spy --help
"$FINANCE_PYTHON" -m fertile_finance.convert_research_csv --help
"$FINANCE_PYTHON" -m fertile_finance.research_catalog --help
```
