# Yahoo chart 小样本

`yahoo_ibm.json` 和 `yahoo_gspc.json` 按 Yahoo Finance chart v8 JSON 的
`chart.result[].meta`、`timestamp`、`indicators.quote[].{open,high,low,close,volume}`、
`indicators.adjclose[].adjclose` 和 `events.{dividends,splits}` 结构手工构造。
时间戳采用该格式的 Unix 秒，样本使用纽约本地开盘时间。

两个文件均为**非真实全量数据**。样本价格、公司行为和区间是合成的，
仅用于覆盖长小数、两笔分红、拆股、bar 区间以外的事件、半日市和预 1970 时间戳。
指数样本还覆盖源行情字段的零值与空值。
