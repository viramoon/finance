# Ken French 与 FRED fixture

这些文件是按来源格式手工构造的小样本，数值为合成值，属于非真实全量数据。

- `french_daily.csv` 和 `french_momentum_daily.csv` 分别模拟 Ken French Data Library 的美国日频 Fama/French 3 Factors 与 Momentum Factor CSV。CSV 在下载 ZIP 内，包含说明、`YYYYMMDD` 日期、百分比收益和文件尾部版权行。样本包含预 epoch 日期、缺失值标记和只出现在动量表的日期。 [Ken French Data Library](https://mba.tuck.dartmouth.edu/pages/faculty/ken.french/data_library.html)
- `fred_dgs10_daily.csv` 模拟 FRED Graph CSV 的 `DGS10` 日频导出，包含预 epoch 日期和 `.` 缺失标记。 [FRED DGS10](https://fred.stlouisfed.org/series/DGS10)
- `fred_dgs10_observations.json` 模拟 FRED 日频 `series/observations` JSON 的单次 vintage 响应。第一条合成值有意使用较多小数位，用于检验精度。 [FRED observations API](https://fred.stlouisfed.org/docs/api/fred/series_observations.html)
