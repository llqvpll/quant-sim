# quant-sim

A 股 AI 模拟盘。一轮扫描的路径是：全市场列表 → 技术规则预筛 → 大模型给出结构化信号 → 风控校验后模拟下单 → 落库并刷新看板。另带组合回测。

单个 Rust 可执行文件，无运行时依赖、无需预热、不需要额外部署服务。模型走任意 OpenAI 兼容的 `/chat/completions`（默认 DeepSeek）；行情、K 线与新闻取自腾讯、新浪、东财的公开接口。

```bash
cargo build --release
export DEEPSEEK_API_KEY=sk-...
quant-sim run --once       # 扫一轮
quant-sim dashboard        # http://127.0.0.1:8000
quant-sim summary          # 打印成绩单
```

完整参数见「命令」，可调参数与费用口径见「配置」「交易规则与费用」。

> **免责声明**：本项目不接券商、不碰真钱，仅用于研究和个人学习，不构成投资建议。市场有风险，任何决策与损益由使用者自行承担。

## 特性

- **直连大模型**：走任意 OpenAI 兼容的 `/chat/completions` 接口，默认 DeepSeek，也可换 Qwen、Kimi 或本地 Ollama。不需要额外部署编排服务。
- **成本可控**：全市场 5000 多只先按价格、ST、一手金额、往返费用筛掉大半，再用技术规则打分，只有排名靠前的几只真正送进模型。
- **防交易幻觉**：信号落地前校验标的回显、价格真实性、止损止盈逻辑自洽，自由文本解析额外降权。原则是宁可错过，不可错下单。
- **A 股规则内建**：T+1、分板块涨跌停、分板块最小交易单位、停牌无行情不成交、涨跌停不追。
- **FIFO 成本配对**：部分卖出不再重复计入已实现盈亏，浮动盈与已平仓口径一致。
- **组合回测**：收盘决策、次日开盘成交，叠加滑点与成交量占比约束；以沪深300 为基准，附年化、最大回撤、夏普及索提诺、卡玛、信息比率等风险指标。
- **仓位算法单一来源**：置信度 → 仓位比例 → 目标股数只有一份实现，实盘与回测共用；两者只在成交价来源与成交时机上不同，回测额外受当日成交量占比约束。
- **实时看板**：内嵌在二进制里，起一个本地 HTTP 服务即可看净值曲线、持仓、信号与跳过原因。

## 快速开始

### 1. 构建

需要 Rust 1.75+。Linux 下 `native-tls` 依赖 OpenSSL 开发包。

```bash
cargo build --release
# 产物：target/release/quant-sim（Windows 为 quant-sim.exe）
```

### 2. 配置 API Key

```bash
# bash
export DEEPSEEK_API_KEY=sk-xxxx

# PowerShell
$env:DEEPSEEK_API_KEY = "sk-xxxx"
```

换其他服务商时覆盖地址和模型即可（键名兼容 OpenAI 习惯）：

| 变量 | 说明 | 默认 |
|---|---|---|
| `QUANT_SIM_LLM_API_KEY` | 优先级高于 `DEEPSEEK_API_KEY` | 无，必填 |
| `QUANT_SIM_LLM_BASE_URL` | 兼容 `/chat/completions` 的地址 | `https://api.deepseek.com` |
| `QUANT_SIM_LLM_MODEL` | 模型名 | `deepseek-chat` |
| `QUANT_SIM_HOME` | 数据目录，默认取可执行文件同目录或当前目录 | 自动推断 |

Key 只从环境变量读，不写入任何文件，日志里统一显示为 `sk-***`。

### 3. 跑起来

```bash
quant-sim run --once            # 扫描一次（非交易时段默认拒绝）
quant-sim run --once --force    # 非交易时段强制扫描，用最近收盘价，仅调试
quant-sim run --loop            # 盘中循环，休市自动等待
quant-sim summary               # 成绩单
quant-sim dashboard             # 看板，默认 http://127.0.0.1:8000
quant-sim backtest --symbol 600000 --days 60
```

## 命令

| 命令 | 说明 |
|---|---|
| `run --once` / `--loop` | 单次扫描或循环扫描 |
| `run --scan N` | 每轮候选数 |
| `run --interval S` | 循环间隔秒数，最小 30 |
| `run --capital X` | 初始资金，仅新账户生效 |
| `run --reset` | 重置账户，旧数据会备份 |
| `run --force` | 忽略交易时段限制 |
| `summary` | 打印总资产、收益率、持仓、已平仓盈亏、累计手续费 |
| `reset` | 只重置账户不动看板 |
| `dashboard --port` / `--host` | 本地看板，仅监听 127.0.0.1 |
| `backtest --symbol/--symbols` | 单只或多只，逗号分隔 |
| `backtest --days` / `--decision-every` | 回测长度与决策频率 |
| `backtest --slippage-bps` / `--max-volume-ratio` | 滑点基点与单笔成交量占比上限 |
| `backtest --benchmark` | 基准指数，默认 `000300`（沪深300），也可传 `sh000905` 这类带市场前缀的代码 |
| `backtest --replay` | 重放上次落盘的信号日志，不调用模型，用于复现结果 |
| `backtest --snapshot <id\|latest>` | 只读本地行情快照，不联网，区间固定 |
| `snapshot create --symbols A,B [--days N]` | 抓取并落盘一份行情快照 |
| `snapshot list` | 列出已有快照的 id、创建时间与标的数 |
| `backtest --walk N` | 把区间切成 N 个连续窗口分别回测，做样本外稳健性检验 |

`summary`、`dashboard`、`reset` 不需要 API Key；`backtest --replay`、`backtest --snapshot`、`backtest --walk --replay` 也不需要。

## 回测结果与可复现性

回测以收盘价决策、次日开盘价成交，结果写到 `data/backtest_result.json`。基准默认是沪深300 指数的区间涨幅，`超额收益` 是策略收益减去指数涨幅；只有指数行情取不到时才回落到「回测标的等权买入持有」，此时会打印警告并把 `benchmark_source` 标成 `equal_weight_fallback`。**看到这个标记时不要拿超额收益评价策略**，那不是市场基准。

落盘指标除总收益、年化、最大回撤、夏普、胜率外，还有索提诺（只惩罚下行波动）、卡玛（年化÷最大回撤）、信息比率（相对基准的超额稳定性）、最大连续亏损天数、换手率、分年度收益。

默认每次回测会重新调用模型，同样的行情可能得到不同信号，结果不可复现。要复现就走重放链路：

```bash
quant-sim backtest --symbol 600000 --days 60        # 正常跑，写日志与结果
quant-sim backtest --symbol 600000 --days 60 --replay   # 不连模型，重放同一批信号
```

| 文件 | 内容 |
|---|---|
| `data/backtest_signals.jsonl` | 每次模型调用的日期、标的、问题、技术面与原始回复，正常回测开始时清空重写，重放时只读 |
| `data/backtest_manifest.json` | 本次参数全集：区间、资金、滑点、基准、模型名、prompt 版本、数据源、是否可复现（不含任何密钥） |
| `data/backtest_result.json` | 结果与新增指标，字段说明见上 |

重放时校验同一份 `manifest` 与 `signals.jsonl`，财务与成交字段应逐项一致；`ai_calls` 按设计不同（首次大于 0，重放为 0）。

## 固定行情输入：快照

默认回测直接拉实时 K 线。行情源每多一根新 K 线，相同 `--days` 的回测区间就会右移一天，基准、收益、成交全部跟着变，跨版本对比没有意义。要固定输入就用快照：

```bash
quant-sim snapshot create --symbols 600000,600519 --days 60
quant-sim snapshot list
quant-sim backtest --snapshot latest --symbol 600000 --days 60 --replay
```

一次快照写到 `data/snapshots/<yyyymmdd_HHMMSS>/`：

| 文件 | 内容 |
|---|---|
| `meta.json` | 抓取时刻的全市场列表、实际抓了 K 线的标的、基准代码、幸存者偏差说明 |
| `kline/<code>.json` | 指定标的的日 K；**只抓 `--symbols` 列的那几只**，不会去抓 5000 多只 |
| `benchmark.json` | 沪深300 等基准指数的日 K |
| `../latest.json` | 指向最新快照，`--snapshot latest` 读它 |

快照模式完全不构造行情客户端，只读磁盘；快照缺某个标的会打印警告并返回空，**绝不联网补数据**。`backtest_manifest.json` 里的 `snapshot_id` 记录了用的是哪一份。同一份快照配合 `--replay` 连跑两次，`backtest_result.json` 应逐字节一致，连成交的日期、价格、股数都相同。

**快照锁不住的东西**：`meta.json` 里的股票池是**抓取时刻**的当前全市场列表，无法回溯历史某一天真实上市/退市的全部标的。所以回测历史区间仍存在幸存者偏差，这一条不会因为用了快照而消失（见「已知限制」）。

## 滚动窗口稳健性检验

单段回测给出一个「超额 +15%」，看不出是策略稳定，还是刚好赶上那一段行情。`--walk N` 把区间切成 N 个互不重叠、首尾相接的窗口，每段用独立账户各跑一遍：

```bash
quant-sim backtest --snapshot latest --symbol 600000 --walk 3 --replay   # 用满快照可用区间
quant-sim backtest --snapshot latest --symbol 600000 --days 60 --walk 3 --replay  # 只取 60 天切成 3 段
```

`--days` 在多窗口模式下是**参与检验的总交易日上限**：显式传了才截断，不传则用满快照的全部可用交易日（日志会打印实际用了多少天、快照可用多少天）。每段至少要 10 个交易日，不够会直接报参数错误。

结果写到 `data/backtest_walk.json`，不覆盖 `backtest_result.json`：

| 字段 | 内容 |
|---|---|
| `segments[]` | 每段的起止日期、交易日数、收益、基准、超额、最大回撤、夏普、索提诺、成交笔数、胜率 |
| `summary` | 超额为正的窗口数与占比、超额均值与总体标准差、最差/最好窗口、收益均值、盈利窗口数 |

终端会逐段打印，并给出一句判断：只有所有窗口超额都为正时才提示「各段均为正超额」，否则提示「存在为负的窗口，单段结果不足以说明策略稳定」。标准差用的是总体标准差（除以 N），与夏普的口径一致。

**这个功能的准确名字是「滚动样本外稳健性检验」，不是 walk-forward optimization**：系统没有任何可训练参数，不做参数寻优，也不划分训练/验证/测试三段，因此每个窗口天然都是样本外。它只回答「同一套规则在不同行情段里表现是否一致」，不回答「哪组参数更好」，也不给投资建议。

## 数据源

| 用途 | 来源 | 备忘 |
|---|---|---|
| 实时行情 | 腾讯 `qt.gtimg.cn` | GBK 编码，中文名需转码 |
| 日 K 线 | 腾讯 `web.ifzq.gtimg.cn` | 前复权，支持按截止日截断 |
| 全市场列表 | 新浪 `Market_Center.getHQNodeData` | 分页并发抓取，含备用域名 |
| 个股新闻 | 东方财富搜索接口 | 标题相关性过滤 + 关键词情感 |

都是非官方公开接口，可能限流或变更，没有可用性承诺。进程内做了 TTL 缓存：行情 15 秒、K 线 60 秒、列表与新闻 600 秒。

## 交易规则与费用

| 项目 | 规则 |
|---|---|
| 主板 | 100 股整数倍，涨跌停 ±10%，ST ±5% |
| 创业板 / 科创板 | ±20%，科创板 200 股起 |
| 北交所（4 / 8 / 920） | ±30%，100 股起、1 股递增 |
| T+1 | 买入当日不可卖 |
| 涨停 / 跌停 | 涨停不买、跌停不卖 |
| 佣金 | 默认万三，最低 5 元 |
| 印花税 | 卖出 0.05% |
| 过户费 | 双边 0.001% |
| 交易日 | 周末休市，法定节假日读 `data/holidays.json` |

开市判断不只看时间，还要求上证指数快照的日期是今天，避免节假日误判。

## 配置

所有风控与费用参数都存在 `data/state.json` 的 `config` 字段里，启动时校验一次。手改出非法值时不会让整份状态读失败，会逐项回落默认值并打 `[配置校验]` 日志。

| 参数 | 默认 | 说明 |
|---|---|---|
| `initial_capital` | 50000 | 初始资金 |
| `max_positions` | 3 | 最多持仓只数 |
| `max_single_position_ratio` | 0.25 | 单票仓位上限 |
| `stop_loss_pct` / `take_profit_pct` | -5 / 15 | 固定止损止盈；模型给出有效值时优先用模型的 |
| `drawdown_halt_pct` / `halt_recover_pct` | -10 / -5 | 熔断与恢复 |
| `halt_cooldown_days` | 5 | 熔断冷却天数，到期自动解除 |
| `commission_rate` / `min_commission` | 0.0003 / 5 | 佣金 |
| `stamp_tax_rate` | 0.0005 | 印花税 |
| `scan_top_n` / `ai_analyze_n` | 10 / 5 | 扫描数与送模型的数量 |
| `confidence_buy_threshold` | 0.65 | 买入置信门槛 |
| `price_tolerance_pct` | 3.0 | 价格真实性的容忍偏差 |

## 防幻觉

信号落地前逐条校验，任一条不过就降级为观望并记录原因：

- **标的**：模型回显的代码必须与请求一致；自由文本里提到了别的股票代码却没提目标代码，视为张冠李戴。
- **价格**：模型陈述的现价与真实行情偏差超过 3%，判定为编造。
- **逻辑**：买入信号的止损不低于现价、卖出信号的止盈不高于现价，视为矛盾。
- **可信度**：走关键词兜底解析的信号置信度封顶 40%，且买卖词必须出现在明确的建议区，正文叙述里的买卖词作废。

## 目录结构

```
quant-sim/
├── src/
│   ├── main.rs           命令行入口
│   ├── bot.rs            扫描循环、信号冷却、单实例锁
│   ├── backtest.rs       组合回测
│   ├── simulator.rs      撮合、费用、持仓、净值
│   ├── sizing.rs         仓位计算的唯一入口（实盘与回测共用）
│   ├── snapshot.rs       行情快照的落盘与离线读取
│   ├── walk.rs           滚动窗口切分与汇总统计
│   ├── indicators.rs     MA/RSI/MACD/ATR/量比与规则打分
│   ├── rules.rs          板块、涨跌停、交易单位、T+1
│   ├── market.rs         行情与 K 线
│   ├── news.rs           新闻抓取与情感
│   ├── llm.rs            大模型客户端
│   ├── signal.rs         信号解析与防幻觉
│   ├── store.rs          状态持久化
│   ├── config.rs         配置与校验
│   ├── calendar.rs       交易日历
│   ├── learner.rs        已平仓统计与经验沉淀
│   └── dashboard.rs      看板服务
├── dashboard.html        看板页面，编译时内嵌
└── data/                 运行时生成：state.json、experience.json、heartbeat.json
```

## 已知限制

- 数据源是非官方公开接口，可能限流、变更或失效。
- 回测与实盘模拟都受仓位上限约束：标的一手金额超过「单票仓位上限」时永远买不进。用 5 万本金跑 600519（一手约 14 万）必然是空仓，这类回测只能验证基准，验证不了策略。
- 没有逐笔与盘口撮合，无法模拟排队、部分成交和冲击成本；回测用开盘价加滑点近似。
- 未处理除权除息，长周期持有跨越除权日会失真；回测用前复权 K 线，实盘模拟用原始报价。
- 新闻只做标题相关性与关键词情感，不是完整舆情分析。
- 正常回测依赖模型输出，同样行情可能给出不同信号；需要可复现的结果请用 `--replay` 重放信号日志（见「回测结果与可复现性」）。
- 不加 `--snapshot` 时 K 线是实时拉取的，行情源每多一根新 K 线，相同 `--days` 的回测区间就会右移一天；跨版本结果对比必须先固定行情快照。
- 加载 K 线时取 `--days + 120` 根，因此改 `--days` 可能让回测起点偏移一天（例如 60 天取 180 根、150 天取 270 根）。`--walk` 且不传 `--days` 时是例外：会加载快照全部 K 线（上限 640 根），起点比默认单段早一天。跨版本比较要固定同一 `--days` 与同一快照，不要拿不同 `--days` 的结果逐日对比。
- 股票池取的是抓取时刻的当前全市场列表，回测历史区间时存在幸存者偏差：今天仍在交易的标的才可能入选。快照能锁死行情与区间，但**拿不到历史某一天真实上市/退市的全部标的**，因此这一条偏差无法靠代码消除，需要历史成分股数据源。
- 回测滑点默认 5bp（0.05%），但 A 股最小价位是 0.01 元，价格低于 10 元的标的要推动一个价位需要更大的比例，因此低价股的滑点会被取整吸收，成交价可能正好等于开盘价。这是取整的结果，不是滑点未生效；要观察滑点效果请用 `--slippage-bps 50` 或 10 元以上的标的。
- 节假日默认只排除周末，需要按年份维护 `data/holidays.json`。

## License

[MIT](LICENSE)
