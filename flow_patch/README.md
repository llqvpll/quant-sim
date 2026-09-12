# flow_patch —— Langflow 流优化补丁

作用：把本机 Langflow 里的「A股量化交易分析」流恢复成优化版。
如果哪天你在界面上把它改回去了、或者发现 MarketData 节点上找不到「截止日期(回测用)」这个输入，跑一次这个脚本即可。

## 这个补丁包含什么

1. **MarketData 节点代码**（整段替换）
   - 新增 `as_of`（截止日期，回测用）和 `include_realtime`（实时快照）两个输入
   - 补齐 ATR14、量比、换手率、涨跌停价、均线排列、MACD 金叉死叉状态
   - 数据新鲜度校验：标注盘中未完成 / 已收盘 / 距今多少天，长期停牌直接终止
   - 抓不到行情时**直接报错终止**（不再把"获取失败"喂给模型让它编分析）
   - 腾讯 3 次重试 + 新浪K线兜底；60 秒K线缓存 / 15 秒快照缓存
2. **提示词**：只输出结构化 JSON（signal / confidence / trend / stop_loss / take_profit / position_pct / reason / risk）
3. **DeepSeek 节点**：json_mode=True、max_tokens=600、temperature=0.2，system message 固定字段规范

## 怎么用

1. 先确认 Langflow 在运行（本机 127.0.0.1:7860）。
2. 双击 `apply_flow_patch.bat`；或者在这个目录下执行：

       python apply_flow_patch.py

3. 脚本会先把**当前**的流备份成 `flow_backup_日期时间.json`（放在本目录），再写入优化版，
   最后回读一遍做校验并打印结果。看到 `结果: 成功，优化版已恢复` 就是好了。

## 注意

- 备份文件里包含 DeepSeek 的 API Key，别把本目录整体发给别人或传到网上。
- 想彻底回到最原始版本，可以用上一层目录的 `flow_backup_before_optimize_20260911.json`（优化前的原始流）。
