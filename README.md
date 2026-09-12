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

## 配置

主要参数集中在 `main.py` 顶部：

| 参数 | 默认值 | 说明 |
|---|---|---|
| `SCAN_TOP_N` | 10 | 每次扫描涨幅榜前 N 只候选 |
| `AI_ANALYZE_N` | 5 | 规则预筛后真正送 AI 分析的数量 |

Langflow 服务地址与流程 ID 在 `ai_signal.py` 顶部：`LANGFLOW_BASE`（默认 `http://127.0.0.1:7860`）与 `FLOW_ID`。

## License

[MIT](LICENSE)
