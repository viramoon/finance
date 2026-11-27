# ADR-0038：IBKR 顶档行情 SDK 源码补丁

状态：已采纳

日期：2026-11-27

适用范围：finance Rust workspace 的 IBKR paper 行情接入

关联：[issue #38](https://github.com/viramoon/finance/issues/38)

## 背景

`ibapi` 3.2.0 的实际 `marketDataType` 带有请求 ID，但 SDK 未按该 ID 分发，订阅收不到回调。SDK 还会把顶档十进制数量字符串预先转换成 binary64，并将取消报文的写入错误仅记入日志。这三处行为分别影响产品权限结论、数量精度和订阅行回收。

## 决策与备选

通过 workspace 的 Cargo patch 使用 MIT 许可的 ibapi 3.2.0 本地源码。复用 SDK 的握手、合约查找、Protobuf 和网络分发，在原路径修复请求路由，并增加精确顶档订阅和返回写入结果的取消方法。既有 SDK 市场数据和取消接口继续供原调用方使用。本地缓存只有该版本。现有公开 SDK API 无法取得被丢弃的回调和数量，也无法检查取消发送结果。

本地源码来自已锁定的 `ibapi-3.2.0.crate`，其 SHA-256 为 `61cfb4b8eefd8dda23e2e327187a6ae837018c15d7ca6a79199c6cd475e5f169`，发布源码 commit 为 `0674e5c455081b5a3c6b4d17acf6b5f1b7703df9`。只引入依赖的库源码和许可证，排除上游开发示例、测试 fixture 和对应模块声明。生成的 Protobuf 源码原样引入。Nautilus 源码构建和依赖选择由其独立 workspace 管理。[MIT 许可证](../../third_party/ibapi/LICENSE)

## 影响与验证

[Issue #39](https://github.com/viramoon/finance/issues/39) 的异步合约详情和期权链定义使用要求完整响应的 SDK 读取路径。发生请求缓冲覆盖或未收到协议结束标记时，整次结果失败，避免用部分合约计算覆盖分母。既有行情流保留其原有缓冲行为。内存 I/O 回放通过实际 SDK 分发器填满请求缓冲，覆盖结束标记已送达但部分结果已丢失的情况。

finance-ib 和 collector 从同一补丁 SDK 构建。Finance 的 paper 客户端禁用 SDK 内部自动重连，账户核验绑定到物理连接。连接管理器负责新建连接和重新校验账户。权限探针在取消发送失败时停止扫测，订阅会话失效。Finance 将使会话失效的 IB 通知码交给 SDK，在握手和报文分发时结束对应连接。合约、订阅读取或取消先发现断线均终止本轮扫测。SDK 关闭会中断进行中的报文写入、结束分发并释放 socket，持有旧句柄仍可结束会话。SDK 默认自动重连模式仍响应关闭信号。内存 I/O 回放经过 SDK 实际分发器，覆盖请求隔离、四档实际回调、原始数量、取消失败和无响应握手关闭。同一合成重连脚本验证 SDK 默认模式能完成握手，并验证 finance 的 paper 客户端断线后不拨号且拒绝旧请求。连续建立连接的回放覆盖合成混合账户被拒以及新会话重新确认合约。取消回放覆盖已写出报文后才发现断线、独立写入错误、内部时限和调用方中断。真实 paper 测试仅为获授权私有部署的显式 opt-in。HTTP 和流协议采用既有版本。[IBKR 接入](../integrations/ib.md)

两份手写 Rust 合成测试中的连续数字参数，以及 SDK 消息源码注释中的数据场状态码表，会触发数据入仓防护的表格检测。导出脚本读取当前检出提交（`HEAD`）中的这三个文件。同一提交的[来源清单](../../.data-guard/sources.json) 记录各文件的用途和 SHA-256。在仓库根目录可将归档写到临时目录，并运行回归测试。测试将暂存的脚本、清单和源码放入只有一个提交的独立仓库，核对导出的路径和摘要。

```sh
out=$(mktemp -d)
sh scripts/export-ib-review-sources.sh > "$out/ib-review-sources.tar"
python3 -B -m unittest discover -s tests -p test_data_guard.py -k test_ib_review_export -v
```

## F9c 的 Nasdaq 场所深度扩展

F9c 复用同一 Cargo patch。IBKR 的深度行按买卖侧和位置更新，漏掉更新后无法确认后续位置对应的盘口行。异步 `MarketDepthBuilder::subscribe_exact()` 使用现有 Protobuf 类型解析回调，原样保留数量字符串，并以可选字段表达缺失的价格或数量。源端价格仍以 binary64 传输，finance 在适配边界转换为可往返的 `DecimalText`。深度发送和取消复用既有 SDK 报文路径，取消沿用 `try_cancel()` 检查本地写入结果。[issue #40](https://github.com/viramoon/finance/issues/40)

精确订阅将队列溢出报告为 `SubscriptionLagged` 并结束旧读取流，finance 收到该错误后清簿、取消并重新建立订阅。既有同步和异步 `subscribe()` 继续返回 `MarketDepths`，沿用各自的解码与队列处理。使用深度位置重建盘口的调用方应选择精确订阅。内存回放验证直接场所请求、原始数量、过载后的重置和已校验取消。部署沿用原工作区依赖和 paper 会话配置。相关接口见 [Nasdaq TotalView 场所深度](../integrations/ib.md#nasdaq-totalview-场所深度)。

深度测试由固定 Rust 输入生成合约、深度行和状态通知。用 `sh scripts/export-totalview-review-sources.sh` 从当前提交导出三份合成测试的原始源码，校对登记的 SHA-256。运行手册列出执行回放的命令。[来源清单](../../.data-guard/sources.json) · [离线验收](../../RUNBOOK.md#离线验收-nasdaq-totalview-场所深度)

## F9e 的期权历史逐笔扩展

F9e 在同一补丁中复用历史逐笔的 Protobuf 请求、解码和分发。`trade_exact()` 与 `bid_ask_exact()` 保留原始十进制数量和字段缺失状态，只有收到完成标记且完整读到请求回调才交付一页。请求队列溢出、断流或错误会丢弃当页的部分结果。既有 SDK 历史逐笔接口沿用原有行为。源价格以 binary64 传输，finance 以最短往返十进制表示进入 `DecimalText`，后续计算与传输使用十进制类型。[issue #42](https://github.com/viramoon/finance/issues/42)

历史读取使用同一已校验的 paper 会话和已确认的 SMART USD 期权 conId，只请求常规交易时段的 `TRADES` 和 `BID_ASK`。它复用 F11 的本地跨进程历史限速状态。内存回放经生产 SDK 分发器核对请求字段、原始数量、空条件码和完成标记。[历史限速](../historical-pull.md#预计耗时与连接约束)

## 后续工作

升级 SDK 时优先使用包含这些修复的上游版本。移除本地补丁前，运行同一套内存回归，核对顶档实际回调、顶档和深度数量、深度队列溢出后的清簿语义、历史逐笔大小与完整页、取消写入结果、重连期间关闭和 paper 会话边界。
