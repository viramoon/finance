# 做市商 API 数据 adapter

按 Nautilus adapter 的 config / data / factories / providers 边界组织未来代码：config 定义可配置连接与凭据引用，data 转为 Nautilus 数据模型，factories 构建客户端，providers 解析标的元数据。TBD：实际 API、认证方式、速率限制、交易日历和字段映射，需先完成独立接入调研。

这里只接入数据，不自研执行内核；价格、数量使用精确类型。长连接须有心跳、读超时、退避重连及断点补数验证；读请求和写请求采用不同策略。原始凭据和实际行情数据不进 git，本次只放本说明。
