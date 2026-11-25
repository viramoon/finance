# 源码构建与回测冒烟

## 前置与一条命令

在云盒子先按 fertile 的 scripts/bootstrap-box.sh 安装锁定工具并 source /home/box/fertile/env.sh。然后在 finance 执行 `bash scripts/build.sh`，它使用 uv.lock、uv path source 和上游 PEP 517 backend 构建本地 wheel，再安装到持久虚拟环境。没有 PyPI Nautilus wheel 回退。第三方 Python 依赖可以使用 uv.lock 中锁定的普通 wheel。

默认 BUILD_MODE=ci-pr，使用上游 ci-pr-wheel profile，减少骨架验证的构建成本；优化生产构建可显式 BUILD_MODE=release，缓存键会变化。本 issue 不把 CI profile 当作正式发布产物。Rust 数字版本由 rust-toolchain.toml 锁定；上游 build.py 的 RUSTUP_TOOLCHAIN 仅接受 stable/nightly，因此入口清掉继承的同名覆盖，让 cargo 读取锁文件。

源码在独立 git worktree 构建，不编辑 vendor checkout。轮到客制化时必须先把补丁推入私有镜像，见 [nautilus.md](nautilus.md)。

## 精简研究工具链

Ubuntu 24.04 x86_64 上可以使用 [bootstrap-research.sh](../scripts/bootstrap-research.sh)。它使用 `toolchain.env` 中锁定的 Rust、rustup、uv、Python 和 LLVM 版本，通过 apt 准备系统编译与下载依赖，并复用源码构建的 LLVM 安装脚本。精简入口跳过 PostgreSQL、Node.js 和 pnpm。

```bash
bash scripts/bootstrap-research.sh --dry-run
bash scripts/bootstrap-research.sh
source "${FINANCE_RESEARCH_TOOLCHAIN_ROOT:-$HOME/.local/share/finance-research}/env.sh"
bash scripts/build.sh
```

`--dry-run` 只显示版本、软件包和目标路径。正式安装会联网，apt 需要系统安装权限。工具默认位于 `$HOME/.local/share/finance-research`，可通过 `FINANCE_RESEARCH_TOOLCHAIN_ROOT` 指定绝对路径。默认构建根为该目录的 `build/`，也可在 bootstrap 时设置绝对路径 `FINANCE_BUILD_ROOT`。生成的环境文件指定 `FINANCE_TOOLCHAIN_ENV`，源码构建优先加载这个环境文件。

研究分析依赖按 [Python 研究环境](../python/README.md#研究环境) 安装到已构建的回测 venv。

## 缓存、来源与空间

默认 FINANCE_BUILD_ROOT=/home/box/fertile/cache/finance；内含 wheels、cargo-target、uv-cache、venv、sources 和 logs。所有运行缓存、生成文件与编译产物位于持久根。CI 显式覆盖为 runner 临时目录，Linux x86_64 / Ubuntu 24.04 是当前验证平台。CARGO_BUILD_JOBS 默认 2、C 扩展编译串行，避免小内存 runner 同时编译大量模块。

缓存键包含 Nautilus commit、实际 Rust / clang 版本、Python ABI、构建模式、high precision、toolchain.env、uv.lock 和构建脚本。缓存 wheel 的摘要与 source-commit 每次核对；安装时明确传 file:// 本地 wheel，verify-source.py 验证 direct_url 和已安装的 .so 扩展位置。日志输出 SOURCE_COMMIT / SOURCE_WHEEL / BUILD_CACHE_HIT / BUILD_SECONDS，可证明来源和冷 / 热耗时。

`research` 依赖组与源码构建共享 `uv.lock`，以统一 NumPy、pandas 等公共依赖版本。CI 在完成两次源码构建与缓存校验后同步该组，再执行研究测试。研究组进入锁文件会改变源码 wheel 和 GitHub 编译缓存键，后续更新该组的锁文件也会使这些缓存失效。

本地实测（Ubuntu 24.04 x86_64、3 vCPU、约 9.7 GiB 内存、ci-pr profile）：完整源码构建与安装 2171 秒（36 分 11 秒），其中 Rust 阶段 17 分 11 秒；不含前置工具链安装。编译后 cargo-target 约 8.7 GiB、隔离 worktree 约 6.9 GiB、已安装虚拟环境约 4.1 GiB，wheel 658 MiB。不同硬件不可直接套用这些耗时，GitHub 冷 / 热运行另见 PR 与 artifact。应预留至少 40 GiB 可用磁盘和 8 GiB 内存，用于完整 Python / Rust 构建、下载与缓存临时文件；清理缓存只删除确认不再使用的旧 key，不删除数据 catalog 或订单状态。保留上次可用 wheel 可加快回退。

## 官方最小示例

执行 `bash scripts/smoke.sh`，直接运行 vendor 内 examples/backtest/example_01_load_bars_from_custom_csv/run_example.py；使用上游自带 CSV 和计数策略，核对处理了 9 根 bar。无网络请求、无真实账户、无自研策略，测试不可用导入成功代替实际 BacktestEngine 运行。

CI 使用 GitHub 托管 ubuntu-24.04 runner，checkout submodules，缓存 LLVM、cargo 编译产物和源码构建 wheel；setup-uv 的缓存目录与构建入口的 uv-cache 一致。首次构建后立即第二次验证 wheel 缓存，之后的 CI 运行可恢复 GitHub cache；日志、回测与两次耗时上传 artifact。缓存命中仍运行来源校验和上游回测。

首次 CI 源码构建与安装 1457 秒（24 分 17 秒），同一 job 的第二次安装与校验 3 秒；这两项不含前置 LLVM 安装、GitHub 缓存传输和 runner 准备时间。该次源码构建、回测及编译缓存保存成功，setup-uv 的默认缓存路径与入口不一致导致收尾失败，已统一路径；最终完整 CI 与跨运行缓存恢复证据见本 issue 的 PR。

## 故障与恢复

下载失败重跑同一命令，保留 uv / Cargo 缓存；版本不符先同步派生文件，不能放宽版本检查。源码构建失败修复根因，不回退 PyPI wheel。发现 vendor 脏修改时停止，在镜像按流程提交后更新指针；缓存损坏清理该 key 后重新构建。uv.lock 通过 uv lock 生成，生成文件只经生成器更新。

安装后的调用使用入口输出的 FINANCE_PYTHON 或持久 venv/bin/python；若使用 uv run，带 --no-sync，避免它绕过 wheel 缓存入口重新同步 path 依赖。构建缓存的入口始终是 scripts/build.sh。
