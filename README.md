# finance

fertile 的主项目与 submodule，基于 NautilusTrader 的交易与数据系统。负责策略、做市商数据 adapter、配置、trader / collector / projector / backtest / api 入口、运维和研究；以源码 submodule 引入上游，不自研交易核心。

本仓库按 [issue #1 方案 v2](https://github.com/viramoon/finance/issues/1#issuecomment-6834604073) 初始化。当前提供 [Nautilus 源码构建](docs/build.md)、上游离线回测批任务、[配置策略回测](docs/backtest-jobs.md#配置策略作业) 及 [行情与回测 HTTP API](docs/api.md)。TBD 项由后续 issue 明确。开发从 [AGENTS.md](AGENTS.md) 开始。

`finance-ib` 提供 paper 只读的美股及 OPRA 顶档订阅接口，以及需显式启用的私有权限探针。真实账户各产品权限仍待验收。操作见 [IBKR 接入](docs/integrations/ib.md#美股-l1-和-opra-权限验收)。

本地行情链路支持 F1 事件追加、恢复和独立 Parquet 投影，Hyperliquid 公共永续行情已接入。[本地行情 WAL、投影与封段接口](docs/market-wal.md)

## 系统中的位置

| 仓库 | 在 fertile 系统中的职责 |
| --- | --- |
| [fertile](https://github.com/viramoon/fertile) | 金融看板 + 自动交易系统的总仓库；初始化、集成、监控、运维和发布 |
| [finance](https://github.com/viramoon/finance) | 主项目；基于 NautilusTrader 的策略、数据采集、交易、回测和 HTTP API |
| [web](https://github.com/viramoon/web) | axum 后端网关；调用 finance HTTP API，管理 PostgreSQL 业务与博客数据，托管前端静态文件 |
| [frontend](https://github.com/viramoon/frontend) | React + TypeScript + Vite 单页应用；只调用 web |
| [agent](https://github.com/viramoon/agent) | 独立的开发流程知识库；通用 skill、prompt、工作流、评估和复盘 |


系统关系、版本、契约、配置、健康和环境规则见 [docs/README.md](docs/README.md)。服务部署唯一来源是 fertile deploy/，agent 只分发知识；只有 fertile 对外发版，Maas · Ruler 只按 fertile tag 部署 / 回滚。
## 目录说明

```text
finance/
├── README.md / AGENTS.md / RUNBOOK.md / service.yaml
├── .agents/skills/          本仓库专属 skill 约定
├── strategies/             策略配置、示例和研究
├── adapters/mm_api/        做市商数据 adapter 边界
├── nodes/                  trader / collector / backtest / api
├── config/                 base / cloudbox / stable 字段设计
├── python/                 Python API、notebook 与研究约定
├── catalog/                Parquet 数据约定，真实数据不进 git
├── docs/                   架构、依赖、连接、IB、接口与 ADR
└── .github/                模板与文档检查
```

目录中的 README 说明职责和边界。策略目录提供配置契约、示例和离线研究。vendor 源码与构建入口按 issue #3 实现。

## 文档索引

- [AGENTS.md](AGENTS.md)：开发流程与硬规则
- [docs/README.md](docs/README.md)：知识放置、系统约定与文档索引
- [架构与进程边界](docs/architecture.md)
- [Nautilus 使用与升级](docs/nautilus.md)
- [连接恢复与熔断](docs/connectivity.md)
- [IBKR 接入](docs/integrations/ib.md)
- [Hyperliquid 公共永续行情](docs/integrations/hyperliquid.md)
- [HTTP 与代码导出](docs/api.md)
- [行情标的、事件与流协议](docs/market-protocol.md)
- [历史价量补拉与逐笔归档](docs/historical-pull.md)
- [本地行情 WAL、投影与封段接口](docs/market-wal.md)
- [期权 GEX、gamma wall、flip 与 0DTE 计算接口](docs/option-exposure.md)
- [StrategyBundle v1 配置与编译](docs/strategy-bundle.md)
- [离线因子评价与研究登记](docs/factor-research.md)
- [技术栈决策](docs/adr/ADR-0001-tech-stack.md)
- [依赖、许可与替换](docs/adr/ADR-0002-nautilus-dependency.md)
- [分层配置字段](config/README.md)
- [数据与备份](catalog/README.md)
- [数据入仓防护与 fixture 来源](docs/data-guard.md)
- [运行手册](RUNBOOK.md)
- [服务设计清单](service.yaml)

## 协作与检查

一个任务对应一个 issue，使用 issue-<号>-<简述> 分支和 PR，CI 通过后合并，禁止直接推送 main。PR 需含 Closes #<号>、测试结果和逐条验收自检；开发问题在原分支解决，文档随行为在同一 PR 更新。文档 CI 检查必需文件、非空内容、YAML、相对链接及 PR 描述；Finance API CI 编译和测试 API，Nautilus CI 构建源码并执行上游回测、批任务和 HTTP 端到端验证。没有真实交易验证。

开发克隆先在仓库根运行 `sh scripts/install-data-guard-hooks.sh`。Git 钩子和 Repository Data Guard CI 检查提交的文件与合成 fixture 来源。提交、推送和 PR 分别从 `HEAD`、远端跟踪默认分支和 PR 基分支 SHA 读取硬规则。PR 和推送同时检查提交历史与净差异。防护启用前已发布的历史经单独评审后可豁免逐提交检查。真实行情、截图、数据库、日志导出和备份均存放在工作树之外。规则、故障处置与例外登记见 [数据入仓防护](docs/data-guard.md)。

## 源码构建入口

```sh
git clone --recurse-submodules https://github.com/viramoon/finance.git
cd finance
bash scripts/build.sh
bash scripts/smoke.sh
```

先准备与 fertile/deploy/toolchain.env 一致的工具链；方法、持久缓存、耗时证据见 [构建](docs/build.md)。源码位置 vendor/nautilus_trader，升级和私有镜像见 [Nautilus 维护](docs/nautilus.md) 与 [ADR-0003](docs/adr/ADR-0003-nautilus-source-submodule.md)。

## API 与示例回测

```sh
FERTILE_DATA_ROOT=/home/box/fertile/data bash scripts/backtest-sample.sh
cargo run --locked -p finance-api
curl http://127.0.0.1:8081/healthz/ready
curl http://127.0.0.1:8081/api/v1/backtests/sample
```

端口默认 8081，配置优先级、错误状态、精确金额与 OpenAPI 导出见 [API 文档](docs/api.md)。示例接口读取 latest.json。提交的作业由 worker 独立执行，collector 的行情状态可在 /api/v1/market/status 查询。

`StrategyBundle` 作业用同一编译器接受内联 YAML/JSON 或已登记版本，支持入场/出场与目标权重两类配方。作业锁定本地回放快照，可从结果查看各配方的资金、仓位、净额和成本归因。离线复验方法见 [运行手册](RUNBOOK.md#隔离验收配置策略作业)。
