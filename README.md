# quant-sim — A股 AI 模拟盘量化交易系统

用 AI（DeepSeek）驱动一套**纯模拟盘**的量化交易流程：盘中自动扫描候选股 → 规则预筛 → AI 生成买卖信号 → 风控执行模拟买卖 → 实时看板展示净值与持仓。

> ⚠️ **免责声明**：本项目全程不碰真钱，仅用于研究和个人学习。AI 产出的信号不构成任何投资建议；市场有风险，任何交易决策与损益由使用者自行承担。

## 功能特性

- **AI 信号引擎**：通过 Langflow 编排「A股量化交易分析」流程，调用 DeepSeek 输出结构化买卖信号（buy / sell / hold / wait + 置信度 + 理由）
- **模拟交易引擎**：虚拟账户、订单执行、持仓管理、每日净值快照、交易记录持久化
- **策略回测**：逐日滚动回测历史 K 线，通过 `as_of` 参数把行情数据截断到"当天为止"，**严格避免未来函数**
- **实时看板**：浏览器每 10 秒自动刷新，展示账户净值、持仓、信号记录，并通过心跳文件判断机器人是否在线
- **成本控制**：全市场扫描 → 规则预筛选 → 只把少量候选股送去 AI 分析，省 API 费用
- **零第三方 Python 依赖**：全部使用 Python 标准库（urllib / http.server / json），克隆即跑

## 项目结构

```
quant-sim/
├── main.py               # 主循环：扫描 → AI 分析 → 模拟交易 → 成绩单
├── ai_signal.py          # AI 信号集成层（调用 Langflow API，解析信号）
├── simulator.py          # 模拟交易引擎（虚拟账户/订单/持仓/净值）
├── backtest.py           # 回测引擎（逐日滚动，防未来函数）
├── market_data.py        # 行情数据源
├── technical.py          # 技术指标计算
├── news.py               # 新闻面分析
├── learning.py           # 交易经验学习器
├── dashboard_server.py   # 看板 HTTP 服务（GET /api/state）
├── dashboard.html        # 实时看板页面（ECharts）
├── quant_launcher.py     # 一键启动器
├── check_flow.py         # Langflow 流程连通性检查
├── flow_current.json     # Langflow「A股量化交易分析」流程配置
└── flow_patch/           # 流程补丁工具（marketdata 节点等）
```

## 快速开始

### 前置条件

- Python 3.10+
- [Langflow](https://github.com/langflow-ai/langflow)（用于编排 AI 分析流程）
- 一个 DeepSeek API Key

### 步骤

1. **安装并启动 Langflow**

   ```bash
   pip install langflow
   langflow run        # 默认监听 http://127.0.0.1:7860
   ```

2. **导入 AI 分析流程**

   在 Langflow 界面中导入 `flow_current.json`，并在 DeepSeek 节点里填入你自己的 API Key（仓库中已替换为占位符 `sk-REPLACE_WITH_YOUR_DEEPSEEK_API_KEY`，**请勿提交真实密钥**）。

3. **跑一次单轮扫描（模拟盘）**

   ```bash
   python main.py run --once          # 立即执行一次：扫描 → AI 分析 → 模拟买卖
   python main.py run --loop          # 盘中循环模式（按设定间隔反复扫描）
   python main.py run --scan 20       # 扫描涨幅榜前 20 只
   python main.py summary             # 查看当前成绩单
   ```

4. **启动实时看板**

   ```bash
   python dashboard_server.py         # 默认 http://127.0.0.1:8000
   ```

   浏览器打开看板地址，每 10 秒自动刷新账户状态。

5. **回测（可选）**

   ```bash
   python backtest.py --symbol 600519 --days 60
   python backtest.py --symbol 600519 --days 60 --symbols 000001,300750
   ```

## 工作原理

```
┌─────────────┐    ┌──────────────┐    ┌───────────────┐
│ market_data │───▶│ technical.py │───▶│  规则预筛选    │
│  行情数据    │    │  技术指标     │    │ (控制AI调用成本)│
└─────────────┘    └──────────────┘    └───────┬───────┘
                                               │
                                               ▼
┌─────────────┐    ┌──────────────┐    ┌───────────────┐
│ simulator.py│◀───│ ai_signal.py │◀───│   Langflow    │
│  模拟交易    │    │  信号解析     │    │ + DeepSeek AI │
└──────┬──────┘    └──────────────┘    └───────────────┘
       │
       ▼
┌─────────────┐    ┌──────────────┐
│ data/state  │───▶│  dashboard   │
│  净值/持仓   │    │  实时看板     │
└─────────────┘    └──────────────┘
```

- **防未来函数**：回测时逐日滚动，把流程内行情数据截断到当天（`as_of`），确保 AI 只"看到"当时已知的信息
- **心跳机制**：主循环每次扫描后写 `data/heartbeat.json`，看板据此显示机器人在线/离线
- **经验学习**：`learning.py` 把每笔交易的结果沉淀到 `data/experience.json`，供后续分析参考

## 防交易幻觉（重要）

本项目用 LLM 生成交易信号，而大模型在交易场景下容易**凭空编造**标的、价格、理由（即"交易幻觉"），并据此误下单或把假信息记成事实。为此 `ai_signal.py` 在信号落地前做了一层**真实性校验（grounding）**，命中幻觉的信号会被拦截/降级为观望：

- **标的真实性**：要求 AI 在 JSON 中回显 `symbol` 并与请求代码比对；自由文本模式下若把决策张冠李戴到别的代码，直接作废。
- **价格真实性**：AI 陈述的"现价/最新收盘"若与真实行情偏差超过 8%，判定为编造。
- **逻辑自洽**：买入信号的止损不应高于现价、卖出信号的止盈不应低于现价，矛盾即判幻觉。
- **低可信兜底**：非结构化（关键词）解析一律把置信度封顶到 40%，且只有出现在"明确建议区"的买卖才被采纳，全文偶然扫到的买卖词直接作废。
- **任何被判为幻觉风险的买卖信号，一律不执行**，并在信号日志与看板打 `⚠幻觉` 标记。

> 让 JSON 模式下的"代码一致性"校验开箱生效：在 Langflow 的提示词里**增加 `symbol` 字段**（参考 `flow_patch/prompt_template.txt`），要求 AI 回显与输入一致的 6 位代码。价格/止损逻辑校验不依赖该字段也能工作。
>
> 离线验证（无需 API）：`python ai_signal.py selftest`。

## 配置

主要参数集中在 `main.py` 顶部：

| 参数 | 默认值 | 说明 |
|---|---|---|
| `SCAN_TOP_N` | 10 | 每次扫描涨幅榜前 N 只候选 |
| `AI_ANALYZE_N` | 5 | 规则预筛后真正送 AI 分析的数量 |

Langflow 服务地址与流程 ID 在 `ai_signal.py` 顶部：`LANGFLOW_BASE`（默认 `http://127.0.0.1:7860`）与 `FLOW_ID`。

## License

[MIT](LICENSE)
