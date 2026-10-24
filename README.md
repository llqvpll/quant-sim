# quant-sim — A 股 AI 模拟盘量化交易系统（v2）

用 Langflow + DeepSeek 驱动一套**纯模拟盘**的 A 股交易流程：盘中扫描候选股 → 规则预筛 → AI 生成结构化信号 → 风控执行模拟买卖 → 实时看板。

> ⚠️ **免责声明**：本项目全程不碰真钱，仅用于研究和个人学习。AI 信号不构成投资建议；市场有风险，任何交易决策与损益由使用者自行承担。

## v2 修复了什么

v1 更像“能跑通的 demo”；v2 重点修了会让结论失真或直接跑不起来的问题：

- **flow 与客户端对齐**：仓库自带 `flow_current.json` 已更新为 JSON 输出 + `as_of` 截断 + `symbol` 回显；客户端启动/`check_flow.py` 可检查契约。
- **冷却崩溃**：修复 `news_txt` 未初始化导致的 `UnboundLocalError`；错误信号不再缓存 30 分钟。
- **A 股规则**：T+1、涨跌停（主板 10%/创业板科创板 20%/北交所 30%/主板 ST 5%）、板块手数（科创板 200 股起、北交所 100 股起）、停牌无数据不成交。
- **回测口径**：收盘决策、**次日开盘**成交；滑点、成交量占比约束；多标的共享一个账户；等权买入持有基准；年化/最大回撤/夏普/胜率。
- **费用**：卖出印花税改为 0.05%，补过户费；`avg_cost` 含买入费用，已平仓与浮动盈亏口径一致。
- **配置单一来源**：所有风控/费用参数只从 `state["config"]` 读取，不再出现“看板写 -1%、实际按 -5%”。
- **状态安全**：原子写 + `.bak` 备份；价格/股数校验；损坏状态自动另存并告警。
- **熔断恢复**：回撤恢复或冷却期满自动解除，不再空仓永久锁死。
- **看板**：本地时区日期、转义、显示执行/跳过原因、心跳线程、移除未使用的 ECharts 文件。
- **启动器**：首次运行自动建 `data/`，不再 `FileNotFoundError`；健康检查端口；非 Windows 不再使用专属 flag。
- **安全**：`check_flow.py` 不再覆盖受版本控制的 flow；导出写到忽略的 `flow_current.local.json` 并脱敏 API Key。
- **工程**：补 pytest + CI、删除个人路径脚本、去重看板 HTML、修复未使用代码。

## 功能

- **AI 信号**：Langflow 流程调用 DeepSeek，输出 `symbol/signal/confidence/trend/stop_loss/take_profit/position_pct/reason/risk`。
- **模拟交易**：虚拟账户、订单、持仓、每日净值、FIFO 已平仓盈亏、交易记录持久化。
- **组合回测**：多标的、次日开盘成交、T+1/涨跌停/手数/费用、基准与风险指标。
- **实时看板**：10 秒刷新，显示净值、持仓、交易、信号与跳过原因；心跳判断在线。
- **成本控制**：全市场列表 → 规则预筛 → 只把少量候选送 AI。
- **零第三方 Python 依赖**：客户端只用标准库；Langflow 服务端另装。

## 项目结构

```
quant-sim/
├── main.py               # 主循环：扫描 → AI 分析 → 模拟交易 → 成绩单
├── ai_signal.py          # AI 信号集成（Langflow、解析、防幻觉）
├── simulator.py          # 模拟交易引擎（账户/订单/持仓/净值）
├── backtest.py           # 组合回测（次日开盘、基准、指标）
├── market_rules.py       # 板块/手数/涨跌停/T+1
├── trading_calendar.py   # 交易时段、节假日文件、下一交易日
├── config.py             # 唯一默认配置与校验
├── market_data.py        # 腾讯行情/K线 + 新浪全市场列表
├── technical.py          # MA/RSI/MACD/ATR/量比/规则打分
├── news.py               # 个股新闻相关性过滤 + 情感
├── learning.py           # 已平仓交易统计（供 prompt 参考）
├── dashboard_server.py   # 看板 HTTP 服务
├── dashboard.html        # 看板页面（唯一来源）
├── dashboard.py          # 看板文件检查工具
├── quant_launcher.py     # 一体化启动器
├── check_flow.py         # flow 契约检查（只读/脱敏导出）
├── flow_current.json     # Langflow「A股量化交易分析」优化版
├── flow_patch/           # 恢复优化版 flow 的工具
└── tests/                # pytest
```

## 快速开始

### 前置条件

- Python 3.10+
- [Langflow](https://github.com/langflow-ai/langflow)（用于编排 AI 流程）
- 一个 DeepSeek API Key
- 能访问腾讯/新浪/东财行情接口的网络

### 1. 启动 Langflow 并导入 flow

```bash
pip install langflow
langflow run        # 默认 http://127.0.0.1:7860
```

在 Langflow 界面导入 `flow_current.json`，打开 DeepSeek 节点填入自己的 API Key。
仓库中 `api_key` 是占位符 `sk-REPLACE_WITH_YOUR_DEEPSEEK_API_KEY`，**不要提交真实 Key**。

检查 flow 是否符合客户端要求：

```bash
python check_flow.py
```

应看到 `has_as_of: True`、`json_mode: True`、`has_symbol_prompt: True`。如果失败：

```bash
python flow_patch/apply_flow_patch.py
```

### 2. 跑模拟盘

```bash
python main.py run --once          # 立即扫描一次（非交易时段默认拒绝）
python main.py run --once --force  # 非交易时段强制扫描（用最近收盘价，仅调试）
python main.py run --loop          # 盘中循环，非交易时段自动等待
python main.py run --scan 20       # 候选 20 只（实际送 AI 的数量仍受 ai_analyze_n 限制）
python main.py summary             # 查看成绩单
python main.py run --reset         # 重置账户（会备份旧 state 和经验库）
```

### 3. 看板

```bash
python dashboard_server.py         # http://127.0.0.1:8000
```

或一键启动：

```bash
python quant_launcher.py
```

### 4. 回测

```bash
python backtest.py --symbol 600519 --days 60
python backtest.py --symbols 600519,000001,300750 --days 120 --decision-every 5
python backtest.py --symbols 600519,000001 --slippage-bps 5 --max-volume-ratio 0.05
```

回测结果写入 `data/backtest_result.json`，包含收益、年化、基准、超额、最大回撤、夏普、胜率、交易与净值曲线。

## 交易规则与费用

| 项目 | 规则 |
|---|---|
| 主板 | 100 股整数倍，涨跌停 ±10%，ST ±5% |
| 创业板/科创板 | 100/200 股起，涨跌停 ±20% |
| 北交所（4/8/920） | 100 股起、1 股递增，涨跌停 ±30% |
| T+1 | 买入当日不可卖；`sell` 会拒绝 |
| 涨停/跌停 | 涨停不买、跌停不卖 |
| 佣金 | 默认万三、最低 5 元 |
| 印花税 | 卖出 0.05% |
| 过户费 | 双边 0.001% |
| 交易日 | 周末休市；节假日可用 `data/holidays.json` 补充 |
| 开市判断 | 交易时段 + 上证指数快照日期是今天 |

所有参数可在 `state["config"]` 中调整，启动时会校验并修正非法值。

## 配置（`config.py` 默认值）

| 参数 | 默认 | 说明 |
|---|---|---|
| `initial_capital` | 50000 | 初始资金 |
| `max_positions` | 3 | 最多持仓 |
| `max_single_position_ratio` | 0.25 | 单票上限 |
| `stop_loss_pct` / `take_profit_pct` | -5 / 15 | 固定止损止盈；AI 给出有效止损/止盈时优先用 AI 的 |
| `drawdown_halt_pct` / `halt_recover_pct` | -10 / -5 | 熔断/恢复 |
| `halt_cooldown_days` | 5 | 熔断冷却天数 |
| `commission_rate` / `min_commission` | 0.0003 / 5 | 佣金 |
| `stamp_tax_rate` | 0.0005 | 印花税 |
| `scan_top_n` / `ai_analyze_n` | 10 / 5 | 扫描/送 AI 数量 |
| `confidence_buy_threshold` | 0.65 | 买入置信门槛 |
| `confidence_base_ratio` / `max` | 0.15 / 0.25 | 置信度仓位 |
| `price_tolerance_pct` | 3.0 | AI 价格真实性容忍 |

## 防交易幻觉

`ai_signal.py` 在信号落地前校验：

- **标的**：JSON 回显 `symbol` 必须与请求 6 位代码一致；自由文本提到别的代码但没提目标代码 → 作废。
- **价格**：AI 陈述的“现价/最新价”与真实行情偏差 > 3% → 判编造。
- **逻辑**：买入止损不低于现价、卖出止盈不高于现价 → 判矛盾。
- **低可信**：关键词解析置信度封顶 40%，非“明确建议区”的买卖词作废。
- 任何幻觉风险的买卖信号不执行，并记录 `⚠幻觉`。

离线验证：

```bash
python ai_signal.py selftest
```

## 测试

```bash
python -m pip install pytest
pytest
```

CI：`.github/workflows/ci.yml`，Python 3.10/3.12。

## 已知限制

- 数据源是腾讯/新浪/东财的**非官方公开接口**，可能限流、变更或失效；没有 SLA。
- 没有逐笔/盘口/集合竞价撮合，无法模拟排队、部分成交、冲击成本；回测用开盘价 + 滑点 + 成交量占比近似。
- **未处理除权除息/送转**：长周期持有遇到除权日会失真；回测用前复权 K 线，实盘模拟用原始报价。
- 新闻只做标题相关性 + 关键词情感，不是完整舆情/公告/财报分析。
- LLM 输出不可复现：同样的数据可能给出不同信号；回测结果依赖模型版本与温度。
- 节假日默认只排除周末；请按年份维护 `data/holidays.json`。
- 这是模拟盘，不连接券商、不涉及真实资金。

## License

[MIT](LICENSE)
