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

finance-ib 和 collector 从同一补丁 SDK 构建。权限探针在取消发送失败时停止扫测，订阅会话失效。SDK 异步分发器在自动重连中也响应关闭信号，探针能在后台任务退出后输出断线报告。测试通过内存 I/O 将合成 Protobuf 帧送入 SDK 的实际分发器，检查请求隔离、四档实际回调、原始数量、取消写入失败和无响应握手的关闭路径。真实 paper 测试仅为获授权私有部署的显式 opt-in。HTTP 和流协议采用既有版本。[IBKR 接入](../integrations/ib.md)

## 后续工作

升级 SDK 时优先使用包含这些修复的上游版本。运行相同的内存回归测试确认实际回调、数量、取消语义和重连期间关闭后，移除本地补丁。
