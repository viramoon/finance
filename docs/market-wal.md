# 本地行情 WAL、投影与封段接口

[F1 事件协议](market-protocol.md) 的生产者可以把已授权事件交给 `finance_collector::market_ingress::MarketIngress`。每个 source/product 有自己的 WAL 和投影游标。[Hyperliquid](integrations/hyperliquid.md) 同时发布 v1.0 事件和 v2.0 永续上下文，投影在现有 `event_json` 列保留完整事件并索引原始位置。旧 `BarSource`、Nautilus bar catalog 和 [旧采集状态](../catalog/README.md) 继续使用原有路径。本接口供后续适配器、查询实现和 fertile 备份接入。

[IBKR 历史期权流](option-flow.md) 通过相同入口发布 v3.0 原始逐笔证据。Parquet 与 SQLite 索引保留新种类及各自的流位置。

## 生产者与许可

- `MarketIngress::open(data_root, policy)` 接受调用方从已审核配置解析出的 `ProductPolicy`。策略绑定 `source`、`product`、`license` 以及 `display_live`、`record_local`、`derive_non_display`、`backup_personal_drive` 四个布尔值。项目禁用再分发。落盘前须准备数据根，打开时策略与该产品的 WAL 根绑定。权限或许可范围变动时，协调方应停写、按许可处置已有归档并选择新的数据根，再启用新策略。
- `publish(event, instrument, capability)` 校验 F1 结构、标的、产品和精度，并核对许可标签。生产者负责 F1 协议序号的分配、上游连续性判断和缺口报告。重投沿用同一流分区、epoch、seq、cursor 和内容，接收时钟可以更新。本地 WAL 记录每次合规投递，接收顺序与 F1 流位置独立。
- `record_local=true` 时，`publish` 返回 `Staged(position)`。调用方按时间或批大小调用 `flush()`，用其返回的持久位置确认投递。文件达到滚动上限时写入端也会提交并封闭前一段，确认仍以成功的 `flush` 或 `seal` 返回值为准。`seal()` 提交当前批、封闭非空活动段并返回本地清单。`seal_for_backup()` 额外检查云备份授权。退出前调用方负责主动提交或封段。
- 仅展示的产品只在进程内提供 `display_events()`，窗口最多 5 秒、256 条并受 16 MiB 的编码大小约束。调用方可在空闲时调用 `expire_display()` 释放内存。此路径不创建任何行情文件或持久状态。缺少展示与落盘权限时返回 `Denied`。投影只把已获准落盘的事件转换为归档与查询索引，F6 等非展示计算由调用方另行检查 `derive_non_display`。
- `report_quality(QualityReport)` 记录流的 `channel`、`instrument_id`、含端点的 UTC 纳秒区间、原因、恢复类别与计数，重复报告同一区间和相同内容时幂等。原因包括 `license_skip`、`rotation`、`source_rate_limit`、`disconnected`、`source_gap` 和 `storage_unavailable`。恢复类别为 `tick_backfilled`（已逐笔补齐）、`bar_backfilled`（仅 bar 补齐）、`book_restored_from_snapshot`（从新快照恢复当前簿）、`history_unrecoverable`（历史缺口不可补）。`outcome: null` 表示待恢复，`count: null` 表示数量未知。计数为恢复的条目数，或不可恢复区间内已知缺失的条目数。有落盘权限的产品将报告保存在本机。其余产品仅在内存中保存最多 256 条，`MarketIngress::quality()` 只返回收到后 5 秒内的记录，期间的重复报告沿用首次到期时间。写入时清理过期质量记录，空闲适配器可调用 `expire_display()` 释放这些记录的内存。

`ibkr/nasdaq-totalview` 为仅展示产品。共享入口按产品拒绝开启本地记录、非展示派生或 Drive 备份的策略。深度适配器通过内存接口交付场所簿，无需实例化 WAL 入口或提供数据根。[Nasdaq TotalView 场所深度](integrations/ib.md#nasdaq-totalview-场所深度)

## WAL 格式与恢复边界

WAL 根为 `<data-root>/finance-state/market-wal`。产品目录为 `<source>/<product>`，两部分都使用 F1 标识。对一个产品只有一个进程可持有 `writer.lock`。每次获得写锁并成功打开后递增持久化写者代际。WAL 本地位置从 1 开始按投递递增，同一产品内以 0 表示起点。写者代际与 F1 事件的 epoch 是独立坐标。

段格式 v1 从 8 字节魔数 `FINWAL1\n` 开始，每个帧依次为：

| 字段 | 格式 |
| --- | --- |
| 帧正文长度 | 小端 `u32` |
| 帧校验和 | 长度字节与帧正文的 SHA-256，32 字节原始摘要 |
| 帧正文 | 小端 `u64` WAL 位置、小端 `u64` 写者代际、UTF-8 F1 事件 JSON |

单个 F1 JSON 上限 4 MiB，单段上限 64 MiB。段文件与目录实体同步后，通过原子替换和目录同步发布内部 checkpoint。读者可以在写者工作期间读取一致的已提交前缀。启动恢复将活动段截断到已发布的字节偏移，忽略未确认的完整或半条尾记录。已提交的长度、校验、位置或 F1 数据不合格时恢复报错。确认调用遇到 I/O 错误后，重新打开写者并按已确认位置重投，保留 F1 原位置和修订。

`finance-market-wal` 是本地命令行工具，显式接受 `--data-root PATH`。下表除 `products`、`restore` 外都接受 `--source SOURCE --product PRODUCT`。标量位置以十进制字符串传输。读取一页时结果固定在该次调用选定的持久前缀。

| 命令 | 输出与行为 |
| --- | --- |
| `products` | 可读的持久产品列表，JSON 数组，每项有 `source` 和 `product`。 |
| `read [--after POSITION] [--limit N]` | JSON Lines，每行 `position`、`generation`、`event`。位置按升序，`after` 为排除式游标，默认 0。页记录数默认 512，范围 1–4096，且按约 16 MiB 的帧大小提前分页。 |
| `watermarks` | 四条水位 `source_latest`、`local_durable`、`projected`、`remote_verified`。 |
| `quality` | 查询该产品已记录的质量报告，JSON 数组。 |
| `manifest` | 返回已封段的可备份清单，写者运行中也可读取。 |
| `seal` | 写者停机时取得该产品的写锁、封段并返回可备份清单。 |
| `restore --manifest PATH` | 在隔离数据根中验证已复制的封段并激活该本地 WAL。 |

`source_latest` 包含最后一次收到的事件的 `channel`、`instrument_id`、`epoch`、`seq`、`exchange_ts`、`receive_ts`。`MarketIngress::watermarks()` 在进程内报告当前所见事件，CLI 使用最后一条已持久记录作为离线观察，缺少记录时为 `null`。`local_durable` 和 `projected` 分别是本地已提交和投影已处理的位置，初始为 `"0"`。`remote_verified` 预留给 T2b 的远端校验回执，目前返回 `null`。这些位置不代表供应商逐笔覆盖，缺口情况查质量报告。

## T2b 封段清单 v1

备份协调方应在写者运行时调用生产者的 `seal_for_backup()`，或停写后执行本地 `seal` 命令。`manifest` 可随时枚举上次封段的前缀。只有策略明确允许 `backup_personal_drive` 的产品能调用这些备份接口。清单格式如下。

| JSON 路径 | 约定 |
| --- | --- |
| `access` | 完整 `ProductPolicy`，用于核对持久化与备份范围。 |
| `wal.version` | JSON 整数 `1`，封段清单和文件格式版本。 |
| `wal.source`、`wal.product` | 源和产品标识。 |
| `wal.sealed_through` | 最后一个已封段的本地位置，十进制字符串，空清单为 `"0"`。可直接用作本产品的备份截点。 |
| `wal.segments` | 按 `first` 升序列出无缺口的已封段前缀，区间含两端。 |
| `wal.segments[].id` | 第一条本地位置左补零至 20 位，按 products 中的源和产品作用域解释。 |
| `wal.segments[].path` | 相对于 WAL 根的 `<source>/<product>/<id>.wal`。 |
| `wal.segments[].first`、`last`、`bytes` | 本地起止位置与文件字节数，十进制字符串。 |
| `wal.segments[].sha256` | 完整文件 SHA-256，64 个小写十六进制字符。 |

封段发布后文件保持不变。T2b 按同一源/产品的父清单和截点枚举新增段，并用长度、摘要及覆盖范围完成核验。仅复制当前清单列出的文件，活动文件、内部 checkpoint、锁和可重建投影不进入此 WAL 备份接口。质量记录是独立的本地状态。跨库清单、客户端加密、云端发布与远端已验证水位由 T2b 负责。封段 id 可以在独立的恢复分支复用，比较时同时核对摘要和恢复点父链。

先创建隔离数据根，将可信备份清单列出的封段复制成对应位置的独立普通文件，再执行 `restore --manifest PATH`。恢复以这个目录为边界逐级打开 WAL 目录，拒绝符号链接目录和文件，以及带多个硬链接的段文件或既有恢复元数据。恢复验证版本、产品、段范围、长度、SHA-256、逐帧数据和许可标签，然后发布策略绑定和截止于 `sealed_through` 的 checkpoint。相同封段清单可重试。目标产品已有其他活动或已提交内容时命令报错。随后运行投影重建 catalog 并核对水位。原数据根保留给恢复核验。

## Parquet 与投影游标

`python -m fertile_finance.market_projector --data-root PATH --wal-bin PATH` 轮询可读产品，`--once` 用于追平当前前缀后退出。默认每批 512 条，`--batch-size` 接受 1–4096，空闲等待由 `--poll-seconds` 设置，默认 1 秒。投影按产品加锁，先固定本批 WAL 位置范围，再原子发布 Parquet，最后提交 SQLite 游标和修订索引。文件发布后、游标提交前中断时，同一批次重放并校验已有文件。目录分别为：

- `<data-root>/catalog/market-events/<source>/<product>/positions-<first:020>-<last:020>.parquet`。
- `<data-root>/finance-state/market-projection/<source>/<product>/`，包括 `projection.sqlite3` 和原子更新的 `watermark.json`。

投影以 F1 流分区、epoch 和 seq 去重。同一秒或纳秒的不同 seq 分开保留，重复投递保留首次接收数据。位置相同却内容冲突、修订目标缺失、类型不符或修订号未递增时，该产品投影报错，游标停在上一成功批次。原事件与有效修订各自保留。完全重复的批次也推进 WAL 游标。

Parquet schema 版本 `finance.market_projection.version=1`。文件含 WAL 位置、F1 产品、流、标的、种类、epoch、seq、revision、三个时钟、修订目标以及 `event_json`。位置、序号和时钟列为精确 `uint64`，行情价格与数量保存在 `event_json` 的原始十进制字符串中。读取端用 `projected_partitions(data_root, source, product)` 按 SQLite 已提交的范围枚举 Parquet。用 `read_latest_event(data_root, source, product, channel, instrument_id, epoch, seq)` 读取某原始位置的原事件或最新更正信封。需要重建时先停止该产品投影，保留旧投影供核对，在隔离根从已恢复的 WAL 重新投影。

finance-api 可查询已获准落盘的产品、WAL 持久前缀、水位和缺口。投影查询使用 SQLite v1 索引定位已提交的原事件或最新更正，再从 WAL 读取信封及其许可。传入 `through` 可固定 WAL 翻页截点，`projected_through` 则表示该次查询读取的投影提交水位。[REST 首批读契约](api.md#首批读契约)
