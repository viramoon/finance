# 数据入仓防护

付费数据、真实行情样本（包括延迟行情和对真实样本做的脱敏改写）、截图、数据库、日志导出、崩溃转储和备份均留在 Git 工作树之外。这个边界同样适用于 GitHub 的 PR、issue、附件和粘贴内容。每个数据产品独立记录 `display_live`、`record_local`、`derive_non_display`、`backup_personal_drive`、`redistribute` 及许可依据。项目始终关闭 `redistribute`。产品只获准实时展示时，通过内存展示；归档、派生计算和备份各自取得授权后再启用。

## 目录与开发检查

将 `FERTILE_DATA_ROOT`、`FERTILE_RESEARCH_ROOT` 和原始数据归档设置在工作树之外。回放与 paper 数据使用各自的根目录。临时测试可为每次运行分配独立的 `/tmp` 子目录。截图、日志、崩溃转储和备份也存放在工作树外，归档和备份按产品许可处理。部署目录和回放配置见 [运行手册](../RUNBOOK.md) 与 [catalog 约定](../catalog/README.md)。

在检出含防护脚本的分支后，从仓库根目录安装钩子。需 Git 2.36 或更新版和 Python 3.10 或更新版，检查器只使用 Python 标准库。

```sh
sh scripts/install-data-guard-hooks.sh
python3 -B scripts/data_guard.py --show-paths staged
python3 -B scripts/data_guard.py --show-paths range origin/main HEAD
python3 -B -m unittest discover -s tests -p test_data_guard.py -v
```

- 安装器写入当前仓库的默认 Git 钩子目录，可重复运行。已存在同名钩子或配置了 `core.hooksPath` 时，在现有钩子中先运行 [pre-commit](../.githooks/pre-commit) 或 [pre-push](../.githooks/pre-push) 对应的检查。后者须转交远端名和完整的 stdin ref 更新列表，检查成功后再把同一输入交给原钩子。新克隆需单独安装。
- `pre-commit` 按 Git 将要提交的文件树检查内容和来源清单，部分暂存时检查实际入仓字节。清单引用的 fixture 和生成器须已在该树中，仅执行 `git add -N` 仍需暂存内容。
- `pre-push` 逐个检查推送的 ref；已有远端 ref 以 Git 提供的旧提交为基准。新 ref 从该远端的本地跟踪默认分支（`refs/remotes/<remote>/HEAD`，缺失时尝试 `refs/remotes/<remote>/main`）划定已发布历史；缺少远端跟踪基准时检查完整历史。日常开发应使用当前的跟踪分支。收到 `git-read-failed` 或 `incomplete-history` 时，先从目标仓库补齐历史和跟踪 ref，再重新检查。
- PR 检查以事件提供的基分支 SHA 到 CI 检出的合并提交为范围。每个新提交与第一父提交比较，根提交检查全部文件，因此也覆盖合并进来的分支以及先添加、随后删除的对象。使用所检出目标的规则和各提交自己的来源清单。Gitlink 指针按路径检查，子模块对象由各仓库自己的检查覆盖。基准历史中已有研究产物，本检查从新增历史开始；修改、重加和改名在新位置重新检查。
- CI 只输出固定拒绝原因与计数。该检查没有 artifact，API、文档和 Nautilus 的 PR 作业也在读取候选文件或生成 artifact 前执行检查。本地的 `--show-paths` 显示转义后的路径，便于修正提交。

本地钩子依赖开发者安装和使用，格式特征只覆盖可识别的文件。新扩展名、文本内联、编码内容和来源真实性仍需人工审查。遇到违规文件，从暂存区移除并移到工作树外；若违规文件已经进入待推送历史，须在本地清除该提交历史中的对象后再推送。已上传时按现网问题处理，避免在问题描述中附上原始数据。

## 规则文件 v1

[rules.json](../.data-guard/rules.json) 与 [sources.json](../.data-guard/sources.json) 使用 UTF-8 JSON。各仓库可独立实现这一格式。两个文档都必须有整数 `version: 1`，单文件上限 256 KiB。拒绝未知键、重复 JSON 键及未知版本。路径使用 Git 仓库根相对路径，以 `/` 分段，禁止空段、`.`、`..`、反斜杠和控制字符。路径及后缀规则匹配时忽略 ASCII 大小写，来源清单的路径精确匹配。

| 规则键 | 值与匹配方式 |
| --- | --- |
| `max_blob_bytes` | 1 到 16,777,216 的整数，按 Git blob 字节数计；finance 使用 262,144 字节。 |
| `blocked_path_components` | 目录名数组。匹配路径中任一目录段。 |
| `blocked_suffixes` | 后缀数组。每项以点开始，以小写 ASCII 字母、数字、点、下划线或连字符组成；匹配完整路径结尾，例如 `.sqlite3-wal`。 |
| `blocked_magic` | `{"offset": 0, "hex": "50415231"}` 形式的数组。偏移为从零起计的字节位置，签名为至少一字节的小写十六进制，必须在大小上限内。匹配任一签名即拒绝。 |
| `fixture_paths` | 完整仓库相对路径的 glob 数组。使用 `*`、`?`、`[集合]` 和 `[!集合]` 的 shell 通配语义，`*` 和 `?` 可跨 `/`。 |
| `fixture_suffixes` | 与禁止后缀的语法相同；匹配后要求在来源清单登记。 |

先检查路径、禁止后缀、对象类型及大小，再检查魔数和文本特征。普通文件须为 UTF-8，允许 BOM、制表符和换行；拒绝其余控制字节及符号链接。Parquet、压缩包、图片、数据库和 Git LFS 指针等可通过魔数识别。匹配 fixture 路径、后缀或文本特征时，按来源清单检查 Git blob 的 SHA-256。文本特征包含常见 OHLCV 和买卖报价表头、连续的 CSV/TSV 数值记录，以及带典型字段组合的行情 JSON。即使报价表的前两列是标的与场所，`symbol`、`bid` 和 `ask` 的组合也要求登记。

## 合成 fixture 与例外登记

`sources.json` 的 `fixtures` 对象以确切文件路径为键。每项填写以下四个字段。

| 字段 | 含义 |
| --- | --- |
| `sha256` | 对提交内容的原始字节取 SHA-256，用 64 位小写十六进制表示。 |
| `generator` | 同一 Git 树中可审查的生成器相对路径。手工构造的固定输入可作为生成器的种子，需说明输入用途及校验方式。 |
| `seed` | 随机种子的字符串表示。固定常量的确定性生成填写 `fixed` 并在来源中说明。 |
| `source` | 非空来源说明，记录合成方法、生成入口所需参数和覆盖场景。 |

1. 先实现可离线运行、可复现的生成器。样本可模拟供应商文件结构，数值和事件用固定输入或种子生成。
2. 在工作树之外生成并审查样本，按内容计算 SHA-256。一起暂存生成器、样本和来源条目，复测同一生成步骤可以产生相同字节。协议样本继续使用 [协议导出流程](market-protocol.md#schema-导出与兼容规则)。现有 Python 合成样本可用 `python3 scripts/generate_synthetic_fixtures.py /tmp/finance-synthetic-fixtures` 重建。
3. PR 中说明样本目的和复现方式，请审阅者核对生成器和摘要。格式误报且有合规用途时，也走同样的单文件登记流程。登记只适用于需来源凭证的文本，路径、禁止后缀、魔数、对象类型和大小限制照常执行。需调整硬规则时提交单独的规则变更供评审。

独立运行 fixture 复现测试时也会先按 v1 规则校验清单。测试只读取各自根目录下的普通文件，工作树路径与临时生成路径上的符号链接均会被拒绝。

该规则约定供 fertile、web、frontend 对照复用，由各自仓库维护策略和检查入口。总仓规则协调见 fertile TG#41。
