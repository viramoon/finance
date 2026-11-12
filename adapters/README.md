# 适配器边界

优先查 Nautilus 现成组件，IB 使用上游适配器，禁止复制或修改 Nautilus 源码。自研仅覆盖上游缺少的数据源；做市商 API 约定见 [mm_api](mm_api/README.md)。

TBD：其他数据源只有在账户资格、服务地区和协议验证后才单独立项。错误分类、重试、精确数值、连接观测和脱敏遵守 [连接设计](../docs/connectivity.md)，不创建 v1 的 connector / data 核心 crate。
