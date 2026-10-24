# flow_patch —— Langflow 流优化补丁

作用：把本机 Langflow 里的「A股量化交易分析」流恢复成优化版（仓库的 `flow_current.json` 已经是优化版；这个目录用于本地 flow 被改坏/重新导入后恢复）。

## 包含什么

1. **MarketData 节点代码**（整段替换）
   - `as_of`（回测截止日期）与 `include_realtime`（实时快照）输入
   - 腾讯 K 线按 `as_of` 取数，避免“先取最近 120 根再过滤”导致指标为空
   - ATR14、量比、换手率、涨跌停价、均线排列、MACD 金叉死叉
   - 数据新鲜度校验：盘中未完成 / 已收盘 / 距今多少天 / 长期停牌终止
   - 抓不到行情直接报错，不把“获取失败”喂给模型编分析
   - 腾讯 3 次重试 + 新浪 K 线兜底；60 秒 K 线缓存 / 15 秒快照缓存
2. **提示词**：只输出结构化 JSON（symbol / signal / confidence / trend / stop_loss / take_profit / position_pct / reason / risk）
3. **DeepSeek 节点**：`json_mode=True`、`max_tokens=800`、`temperature=0.2`、固定 system message

## 怎么用

1. 确认 Langflow 在运行（默认 `127.0.0.1:7860`）。
2. 在本目录执行：

   ```bash
   python apply_flow_patch.py
   ```

   或双击 `apply_flow_patch.bat`。

3. 脚本会先把当前流备份成 `flow_backup_日期时间.json`（本目录，已 gitignore），再写入优化版并回读校验。

## 安全

- 备份文件包含 DeepSeek API Key，**不要提交/外发**。
- 检查 flow 用 `python ../check_flow.py`：默认只读；需要导出时用 `--write-local`，会脱敏 API Key 并写到 `flow_current.local.json`（已 gitignore）。
- 不要运行来历不明的 Langflow flow：自定义组件里的 Python 代码会被执行。
