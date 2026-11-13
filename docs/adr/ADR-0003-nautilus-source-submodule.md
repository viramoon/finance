# ADR-0003：Nautilus 源码 submodule 与构建

状态：已采纳；取代 [ADR-0002](ADR-0002-nautilus-dependency.md) 的依赖获取与禁止修改决定。

依据：[finance issue #3](https://github.com/viramoon/finance/issues/3)。

## 决策与边界

在 vendor/nautilus_trader 引入上游源码 submodule，锁定最新非预发布 release v1.231.0、commit 27a8e54e7ac3c57d6cbf8891f0283dfbaee97317；上游项目仍标 Beta。Python 工程通过 uv 的 path source 解析该目录，由上游 PEP 517 backend 从源码构建 wheel，不从 PyPI 安装 Nautilus 预编译 wheel。Rust 侧以后若直接用 nautilus crate，也只使用同一 submodule 的 path 依赖。

当前指向 https://github.com/nautechsystems/nautilus_trader.git，没有补丁。首次需要客制化时，由 CTO 创建私有镜像 viramoon/nautilus_trader（不使用会公开的 GitHub fork）；镜像的 fertile/main = 上游 release tag + 我们的补丁。修改先提交并推送镜像 fertile/main，再通过 finance PR 切换 URL 与指针。禁止在 vendor 留本地修改后直接提交 finance 指针。

## 理由与代价

源码与构建工具可控、方便后续客制化。代价是首次构建时间与磁盘占用较大、工具链和 Python ABI 要锁定、升级时需要 rebase 补丁和处理冲突。缓存按源码 commit、工具链、依赖锁、平台 ABI 与构建脚本区分，命中后仍校验 wheel 摘要 / 来源并跑上游示例，不牺牲验证。

改动前仍优先查上游现成组件和扩展点，不自研交易内核，不在本 issue 写策略或业务代码。源码构建在隔离 worktree 进行，生成内容不进入 submodule 的主 checkout。

## 升级与许可

升级选择新的稳定 release，先在新 tag 上 rebase 镜像补丁，再更新版本派生文件 / uv.lock / 指针，重新构建并跑回测与后续连接回归；经 PR 与 fertile 集成后发布，保留旧指针用于回退。LGPL-3.0-or-later 来源与许可保留；私有使用和内部修改不触发对外分发义务，对外分发修改版须按 LGPL 提供修改部分源码并允许替换库，届时按分发方式落实完整义务。

构建与实测证据见 [build.md](../build.md)，镜像切换步骤见 [nautilus.md](../nautilus.md)。
