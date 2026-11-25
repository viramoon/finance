# 行情采集与 Parquet 目录

finance-collector 在数据根的 catalog 下写入 Nautilus v1.231.0 可直接读取的 Bar，finance-api 读取同一数据根的 collector 状态。配置和启动命令见 [运行手册](../RUNBOOK.md)。测试数据源用 config/collector/replay.csv 的十进制 OHLCV 形成合成行情，用于持续演练。replay 状态不可用作真实市场行情。

## 输入与恢复

BarSource 按标的订阅、接收闭合 bar，并支持按时间区间回补。输入标识为完整 Nautilus 标的 ID，如 SPY.ARCA。价格与数量从源到存储均为十进制字符串 / 定点对象，时间为 UTC 纳秒。单个 writer 按标的、bar 规格和 UTC 日期分组。Nautilus v1.231.0 在 data/bar/<完整 bar type> 下扫描时间区间文件，跨日拆文件，文件名使用上游生成的起止时间。

writer 是通过 finance 构建入口安装的 Nautilus Python 包及其原生扩展，直接使用上游 ParquetDataCatalog.write_data。Rust 采集进程复用该持久子进程，每批确认后才更新已落盘进度。这样能使用已锁定版本的高精度 Arrow schema 与文件命名，避免重复链接另一套上游 persistence / DataFusion 依赖。目标 catalog 所在文件系统的暂存区写入完成并 fsync 后，原子 rename 发布文件，再同步父目录并原子更新 finance-state/collector/watermarks.json。一个 catalog 同时只允许一个 writer 持有排他锁。启动时按可见 Parquet 文件重建水位，截断 commit 后但 checkpoint 前的失败窗口；每个标的跳过已确认时间戳，批内去重按时间排序。停采后再做 catalog 和 finance-state 的一致备份。

## 本地运行

在 finance 仓库根目录完成 bash scripts/build.sh 与 cargo build --locked -p finance-collector，加载盒子的 env.sh。示例使用默认 127.0.0.1:8082、数据根 /home/box/fertile/data。FINANCE_BUILD_ROOT 指向包含 venv 的编译目录，也可用 FINANCE_PYTHON 显式指定源码 wheel 对应解释器。

collector 从仓库根按脚本路径启动 `python/fertile_finance/collector_store.py`。writer 从自身路径找到项目包，无需设置 `PYTHONPATH`，并和模块调用共用 Unix epoch 校验。

```sh
cargo run --locked -p finance-collector
curl -sS http://127.0.0.1:8082/healthz/live
curl -sS http://127.0.0.1:8082/healthz/ready
curl -sS http://127.0.0.1:8082/status
curl -sS http://127.0.0.1:8081/api/v1/market/status
```

FERTILE_PROFILE 选择 config/collector/cloudbox.yaml 或 stable.yaml。配置包含标的、bar_spec、catalog、写入周期、关闭超时与每次回补区间。相对 catalog 路径从 FERTILE_DATA_ROOT 解析。FINANCE_CONFIG_DIR 可替换 config 目录，FINANCE_COLLECTOR_CONFIG 可指定具体 YAML，FINANCE_COLLECTOR_BIND 可覆盖监听地址。默认每 60 秒回放一次，首个实际时间点持久保存于 finance-state/collector/replay-origin.json。两次启动使用同一 origin、标的、bar_spec、fixture 和间隔时得到同一序列，断线或重启先按水位回补，补齐后转实时。修改输入语义或在回放与真实 IB 之间切换时使用独立数据根、分开备份。catalog 和状态目录持久记录同一来源、bar_spec 和 UUID，拒绝在 replay 水位上改采 IB，或把已绑定的状态目录指向另一 catalog。

ready 逐项检查数据源连接、回补完成、catalog 实际能创建临时文件、每个标的的最后持久 bar 及年龄。任一失败返回 503；live 只检查进程。每秒原子替换 finance-state/collector/status.json，finance-api 的只读 market/status 计算当前年龄，超过 10 秒未更新显示 expired，缺文件为 missing，不能解码为 invalid。正常 SIGTERM/SIGINT 会停止接收、flush 当前缓冲、等待 writer 退出并发布 stopped 状态。SIGKILL 后复用原路径启动，自动从 catalog 复查水位并修复断点。清理演练数据须先停止 collector 并确认该路径为专用临时数据根；不可只删水位或手改 Parquet 文件。

## 复现兼容性及故障测试

```sh
source /home/box/fertile/env.sh
cargo build --locked -p finance-collector
export FINANCE_PYTHON="${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}/venv/bin/python"
PYTHONPATH=python UV_PROJECT_ENVIRONMENT="$(dirname "$(dirname "$FINANCE_PYTHON")")" uv run --no-sync --with pytest==8.4.2 --python "$FINANCE_PYTHON" python -m pytest -q tests/test_collector_store.py
FINANCE_COLLECTOR_BIN="${CARGO_TARGET_DIR:-target}/debug/finance-collector" "$FINANCE_PYTHON" tests/collector_drill.py
```

故障测试启动真实子进程，使用独立临时目录和可加速的有限 CSV 回放。它在非空缓冲时发送 TERM，之后用 KILL 中断和重启，再等数据陈旧，验证 ready 从 200 转 503。最后使用 Python ParquetDataCatalog.bars() 核对每个预期时间戳及高精度价格。跨日分组、排他锁、迟到重复数据和水位替换失败后恢复另由 test_collector_store.py 覆盖。

## 研究日线

经审核的原始日线用独立 [研究 catalog](../docs/historical-backtest.md) 导入。导入从交易日历生成时间戳，拒绝负数纳秒、预 epoch 日线、重复日和非交易会话。1970-01-01T00:00:00Z 与之后的有效纳秒可通过底层存储校验。NYSE 日线还必须落在官方交易日收盘。

应用端应在构造 Bar 或调用 wrangler 之前检查有符号时间戳。显式丢弃模式返回条数及最早、最晚被丢弃时间，接口见 [Python 研究与分析](../python/README.md#unix-epoch-边界)。系统无法使用预 1970 历史，最长可用区间从首根被接受的 bar 起算。^GSPC 始于 1927 年的 98.89 年原始历史不能作为已回测覆盖，SPY 约 33.81 年的数据也不能称作 50 年回报。满窗最多允许起始边界缺五个交易日，窗口内部及末端应完整。详见 [日线窗口](../python/README.md#日线窗口)。
