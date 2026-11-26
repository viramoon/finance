# 离线因子评价与研究登记

F8 提供 Python 研究接口。调用方显式传入固定快照或合成数据，由同一组输入复算报告和回测选用状态。研究报告只评估历史假设，运行时不接行情源或交易账户，也不写入研究 catalog 目录。复测命令见 [运行手册](../RUNBOOK.md#离线因子研究验收)。

## 因子定义与真实可见时间

`fertile_finance.research_factors.FactorDefinition` 固定 `factor_id`、`version`、计算定义、参数、单位、资产范围、`data_version`、可见时间规则、缺值策略、预热会话数与许可。参数使用字符串、布尔值、整数或有限 `Decimal`。改变定义、规则或数据版本时登记新版本。调用方为输入记录保留来源和许可证明，并使用源数据确认时间。

- `FactorObservation` 区分 `observation_ts` 与 `first_available_ts`。财报用实际披露时间，OI 用当时可获得的业务日时间。修订保存为同一观测时点的新发布版本。
- `UniverseMembership` 区分 `effective_ts` 与 `first_available_ts`。历史成分进出、行业和规模信息均依此按时点读取。代码使用统一标的 ID，规模在同一比较口径下传入正数 `Decimal`。
- `PointInTimeFactor(definition, observations, membership).snapshot(decision_ts)` 从生效且可见的成分中选择每个标的最新可见的观测期和修订。时间必须携带时区，内部归一为 UTC。恰好在决策时点生效且可见的记录可用。事后修订不会改写先前决策。
- `validate_factor_snapshot(definition, decision_ts, observations, membership)` 校验提交的单个截面。超出决策时点的披露或成分抛出 `FactorLookaheadError`。完整历史序列则通过 `PointInTimeFactor` 回放，各决策只看当时可见的部分。
- `missing_policy="exclude"` 将缺观测、已失效（达到 `valid_until_ts`）、缺值或未达预热的标的留在覆盖分母中，其得分为空。`history_sessions` 由调用方按该版得分实际使用的已完成会话填报。历史 Ken French 和 FRED 读取接口继续保留原始日期和版本字段。可见时间未知时须先取得独立的发布时间证据才能构造 PIT 记录。

## 报告与复算

`fertile_finance.factor_evaluation.evaluate_factor(factor, sessions, windows, *, cost_bps, quantiles=5, peers=())` 生成 `FactorEvaluation`。每个 `EvaluationSession(session, decision_ts, closes)` 含交易会话日期、决策时间和截至该时刻可用的收盘价映射。会话日期与决策时间的 UTC 日期一致。价格使用正数 `Decimal`，采用固定且一致的复权口径。按升序提供研究日历的完整会话序列，缺行情的会话保留日期和缺口。

`EvaluationWindow(name, phase, start, end)` 指定含首尾的会话范围。依次提供 `train`、`validation` 和一个或多个 `out_of_sample` 窗，各窗不重叠，未来收益的价格端点限在同一窗内。调用者在样本外验证前冻结定义、参数和研究协议。此接口使用给定得分计算指标。

| 报告字段 | 计算口径 |
| --- | --- |
| `ic`、`ic_pairs` | 在 PIT 截面上配对有得分及两端价格的标的，以平均秩处理并列，计算得分与未来收益的 Spearman 相关。期限为 1、5、20 个输入会话。窗口汇总包含均值、样本标准差和 ICIR（均值除以样本标准差）。 |
| `quantiles`、`benchmark` | 按可用得分升序切分等权分位组，同分按标的 ID 排序，不能均分的余数依次分给靠前组。得分和当期定价均可用的标的数少于组数时，各分位组保留现金。基准是决策时有定价的合格成分等权组合。 |
| `gross_information`、`net_information` | 最高分组相对基准的逐期收益差均值除以样本标准差，再乘以 √252。分别比较两边的费前和费后收益。 |
| `factor_coverage`、`scored_coverage` | 将可用得分数、可用得分且有当期定价的标的数分别除以全部合格成分数，在窗内先合计分子与分母。逐期提供缺值原因、得分分布与期限配对数。 |
| `redundancy` | 对同一时点两因子均有得分的标的计算截面秩相关，按期保留结果并汇总。 |
| `industry_weights`、`weighted_mean_size` | 每组的行业目标权重和有规模值标的的加权平均规模，分别附可用信息覆盖权重。 |

分位组和基准以决策收盘价到下一会话收盘价估值，每个窗从现金开始。以下计算使用报告中的 `target_weights`、上期收益漂移后的 `prior_weights` 与 `asset_returns`。

```text
gross_return = Σ(target_weight × asset_return)
turnover     = Σ|target_weight - prior_weight|
fee_fraction = turnover × cost_bps / 10000
net_return   = (1 + gross_return) × (1 - fee_fraction) - 1
累计收益      = Π(1 + 逐期收益) - 1
```

换手计入初始建仓和调仓两边，窗末不补造平仓。`total_fee_fraction` 是各期费用比例之和。缺少持有标的的下一期定价时，该期组合收益及依赖它的上期权重留空。窗口累计值要求完整链路，样本数也随结果输出。无法计算的相关或比率以 `null` 表示。该成本模型用于研究，收盘估值不含委托、成交和整股处理。

复算采用项目 34 位、半偶舍入的 Decimal 上下文。`FactorEvaluation.as_dict()` 将价格相关计算结果和其他 Decimal 值保留为十进制字符串，日期和时间为 ISO 文本，可用 `json.dumps` 导出。保留各窗报告和固定输入，就能比较覆盖、得分分布漂移、滚动 IC、换手和费用。

## 因子版本与状态

`FactorResearchCatalog` 位于 `fertile_finance.research_catalog`。`register(definition)` 建立版本记录，同一 ID 和版本登记同一份定义可重试，定义发生变化时拒绝覆盖。调用 `transition(factor_id, version, state, reason=..., evidence_ref=..., report=...)` 按相邻状态迁移。`get(factor_id, version)` 可读取任一登记版本及其迁移记录。

| 迁移 | 所需证据 |
| --- | --- |
| `research` → `out_of_sample_validation` | 理由和冻结的研究协议引用。 |
| `out_of_sample_validation` → `observe_only` | 理由、报告引用、同一因子和数据版本的评价报告，含至少一期可用的样本外 1 会话 IC。 |
| `observe_only` → `backtest_selectable` | 理由、审核报告引用和同版本样本外评价，至少一期有效截面须超过九个配对资产。该最低范围条件由人工审核补足，IC 曲线不会自动触发迁移。 |
| 退回前一级 | 记录退回理由。退回 `observe_only` 立即停止新选用，历史引用和迁移记录保留。 |

F7 的新选用入口可调用 `select_for_backtest(factor_id, version)`，只有 `backtest_selectable` 返回定义。目录随调用实例保存在内存中。需要跨进程保存记录的调用方持久化其版本、证据和迁移记录。历史复现继续读取原定义和报告引用。

## 发布 API 读快照

离线调用方用 `publish_factor_read_snapshot` 把已审核的版本和评价摘要发布到显式指定的 `data_root`。每次发布替换完整快照，API 读取 `finance-state/factor-research/read-v1.json`。首次发布前 API 返回空列表。快照上限 4 MiB，发布临时文件以 `0600` 创建并原子替换，API 进程需要可读权限。部署按产品许可安排文件可见范围。[REST 契约](api.md#首批读契约)

```python
from fertile_finance.factor_read import (
    FactorReadEvaluation,
    FactorReadPermission,
    publish_factor_read_snapshot,
)

key = (definition.factor_id, definition.version)
publish_factor_read_snapshot(
    data_root,
    as_of=publication_time,
    permissions={key: FactorReadPermission(definition.license, True, True)},
    factors=[catalog.get(*key)],
    evaluations=[FactorReadEvaluation("fit-1", report)],
)
```

调用方提供已有绝对路径、带时区的发布时间及其实际审核过的权限。每个因子版本和评价中的关联因子都需匹配许可，允许本地记录和非展示派生，项目禁止再分发。快照只包含因子定义、状态迁移、可见的证据引用与窗口汇总。逐标的研究输入和详细账本留在原研究环境中。使用 `historical_repo_root` 可纳入下方的已验证历史登记，权限表另需列入该登记的 `(id, version)`。

## 月频动量

`fertile_finance.historical_research.load_factor_momentum_registration(repo_root)` 从指定仓库读取 `research.factor-momentum@1.0.0`，校验登记的原报告、来源说明和审计账本哈希，返回 `research_only`。登记不进入因子状态流转。Python 查询的 `selectable_for_backtest` 和 `tradable` 均为 `False`。F2 也保留该策略 ID，拒绝其进入现金权益编译与版本绑定。[历史登记](../strategies/registry/factor-momentum-v1.json)

原门槛要求主规格在同一 25 bp 成本下，相对静态等权常多的月 RF Sharpe 差在最长样本、主发表后窗口和辅助发表后窗口均严格为正。最长窗显示值为 **0.4024 对 0.7680**，原结论为**不通过**。登记从原报告读取完整精度、规则和结论。原研究沿用有符号的因子名义、现金抵押和净结算账本。[原研究](../strategies/2026-11-25-factor-momentum/README.md) · [原始报告](../strategies/2026-11-25-factor-momentum/results/summary.json)
