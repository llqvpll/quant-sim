# quant-sim

A 股 AI 模拟盘。盘中扫描候选股，技术指标做规则预筛，再把少量候选交给大模型出结构化买卖信号，由内置风控执行模拟下单，附带实时看板与组合回测。不接券商、不碰真钱。

Rust 单二进制实现，无运行时依赖、无本地服务、无需预热。行情来自腾讯/新浪/东财的公开接口。

> **免责声明**：本项目仅用于研究和个人学习，不构成投资建议。市场有风险，任何决策与损益由使用者自行承担。

## 特性

- **直连大模型**：走任意 OpenAI 兼容的 `/chat/completions` 接口，默认 DeepSeek，也可换 Qwen、Kimi 或本地 Ollama。不需要额外部署编排服务。
- **成本可控**：全市场 5000 多只先按价格、ST、一手金额、往返费用筛掉大半，再用技术规则打分，只有排名靠前的几只真正送进模型。
- **防交易幻觉**：信号落地前校验标的回显、价格真实性、止损止盈逻辑自洽，自由文本解析额外降权。原则是宁可错过，不可错下单。
- **A 股规则内建**：T+1、分板块涨跌停、分板块最小交易单位、停牌无行情不成交、涨跌停不追。
- **FIFO 成本配对**：部分卖出不再重复计入已实现盈亏，浮动盈与已平仓口径一致。
- **组合回测**：收盘决策、次日开盘成交，叠加滑点与成交量占比约束，自带等权基准、年化、最大回撤、夏普。
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

`summary`、`dashboard`、`reset` 不需要 API Key。

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
- 大模型输出不可复现，同样数据可能给出不同信号，回测结果依赖模型版本与温度。
- 节假日默认只排除周末，需要按年份维护 `data/holidays.json`。

## License

[MIT](LICENSE)
