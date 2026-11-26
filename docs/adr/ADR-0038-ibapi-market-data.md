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

## 后续工作

升级 SDK 时优先使用包含这些修复的上游版本。运行同一套内存回归，确认实际回调、数量、取消语义、重连期间关闭和 paper 会话边界后，移除本地补丁。
