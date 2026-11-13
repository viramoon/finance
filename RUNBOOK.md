# finance 运行手册（设计）

本仓库已提供源码构建与上游示例回测，交易 / HTTP 服务仍未实现。所有启动命令、端口、恢复阈值和凭据配置为 TBD，实施与演练后补齐。服务部署以 [fertile deploy/](https://github.com/viramoon/fertile/tree/main/deploy) 为唯一准绳，Maas · Ruler 只按 fertile tag 部署和回滚，不部署任意子模块分支。

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
