# 面向 web 的 HTTP API

web 通过 HTTP 调用 finance 的 api 进程，frontend 只调用 web。TBD：查询、回测任务、策略状态与受控交易操作的具体资源、路由、认证、错误模型及权限，后续接口 issue 确定；本次不手写接口契约或响应样例。

Rust handler 使用 utoipa / utoipa-axum 导出 OpenAPI，生成快照为 fertile `contracts/finance.openapi.json`。更新顺序：改 finance 代码并验证 → 从同一 commit 导出 → 在 fertile PR 更新快照及子模块指针 → CI 比对差异 → 更新 web 生成客户端。禁止手改快照和生成客户端；破坏性改动要有兼容与迁移计划，规则见 [fertile contracts](https://github.com/viramoon/fertile/tree/main/contracts)。

价格、数量、金额使用精确类型，序列化格式和客户端精度规则 TBD，不能默认为浮点。写操作以唯一请求 / 客户端订单 ID 关联状态，超时先查后试；健康接口 /healthz 区分 live / ready。实时流若有另定消息协议，不假设 OpenAPI 覆盖。

导出命令、工具版本和 API 进程与 Nautilus 的内部边界 TBD；这些在实现时明确，当前不生成空的 OpenAPI 文件。
