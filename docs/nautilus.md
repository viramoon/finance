# NautilusTrader 源码维护

当前来源：[上游 v1.231.0](https://github.com/nautechsystems/nautilus_trader/releases/tag/v1.231.0)，commit 27a8e54e7ac3c57d6cbf8891f0283dfbaee97317，submodule 路径 vendor/nautilus_trader。GitHub 将该 release 标为 Latest 且 prerelease=false，名称仍含 Beta。gitlink 锁 commit，不能用 master / develop 的移动头代替版本。

```sh
git clone --recurse-submodules https://github.com/viramoon/finance.git
cd finance
bash scripts/build.sh
bash scripts/smoke.sh
```

源码构建、缓存与持久路径见 [build.md](build.md)。Nautilus 提供 Strategy / Actor、TradingNode、Backtest、风险 / 执行、IB adapter、Parquet catalog 和 Python API；当前 [API](api.md) 已提供只读示例回测查询，包装此处同一上游示例生成 JSON；实际交易连接验证仍由后续 issue 实现，Nautilus 源码没有修改。

## 版本唯一来源

fertile/deploy/toolchain.env 为维护源，finance 的 toolchain.env、rust-toolchain.toml、.python-version、.uv-version 是派生文件。运行 `python scripts/sync-toolchain.py /path/to/fertile/deploy/toolchain.env` 同步，--check 校验；uv.lock 通过 uv lock 生成，禁止手改。选择的 Rust 1.97.1 与上游精确一致，uv 0.11.33 符合上游 required-version，Python 3.12.13 落在 >=3.12,<3.15 内；LLVM 22.1.8 提供 clang / lld，满足上游未指定最低版本的编译器要求。finance CI 可独立读取派生文件，不需跨私有仓库 token；系统集成时必须与 fertile 维护源核对。

## 首次补丁时切私有镜像

1. CTO 在 GitHub 创建私有空仓库 viramoon/nautilus_trader；公开仓库的普通 fork 会公开，因此使用私有镜像。
2. 将选定上游 release 及必要历史推送镜像，以 release 为基线建立 fertile/main；记录 upstream remote 和补丁清单。
3. 在镜像 fertile/main 提交、验证并推送补丁，禁止依赖本地未推送的对象。
4. 在 finance PR 更新 .gitmodules URL 为镜像地址，执行 git submodule sync，然后把 gitlink 切到已推送的 fertile/main commit；更新工具链源中的 NAUTILUS_COMMIT 并重新生成派生文件。
5. 重新运行源码构建、上游示例与相关回归；配置 GitHub Actions 的只读镜像访问凭据，不把凭据写进 URL、仓库或日志。当前上游公开 submodule 不需要额外 token。

升级时在新 release tag 上 rebase 我们的补丁，重新生成锁文件、构建并比较行为；冲突在镜像解决后再更新 finance。涉及数据或协议时先验证兼容、迁移和回退，不直接部署 develop。

[ADR-0003](adr/ADR-0003-nautilus-source-submodule.md) 已取代 ADR-0002 的依赖方式；保留历史决定供追溯，不把旧的“不修改源码”解释为禁止本 issue 已授权的镜像维护流程。
