# -*- coding: utf-8 -*-
"""
交易学习模块
============
让机器人"越跑越聪明": 通过一次次真实模拟交易的结果, 统计历史经验, 回喂给 AI。

核心思路:
1. 每笔交易平仓(卖出)后, 记录"当时的技术指标快照 + 持仓期涨跌结果"
2. 汇总所有已平仓交易, 统计:
   - 买入信号置信度 vs 实际胜率(高置信度真的更准吗?)
   - 各技术形态(多头/空头/震荡)的历史胜率
   - 各消息面(利好/利空/中性)的历史胜率
   - 平均持仓收益 / 盈亏比 / 最佳最差交易
3. 生成"经验摘要", 拼进 AI 的 prompt, 让 AI 参考自己过去的判断修正决策

用法:
    from learning import TradingLearner
    tl = TradingLearner("data/state.json")
    tl.learn_from_trades()            # 扫描已平仓交易, 生成经验库
    summary = tl.experience_summary() # 生成给 AI 看的经验摘要
"""

import json
import os

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
EXPERIENCE_FILE = os.path.join(DATA_DIR, "experience.json")


class TradingLearner:
    """交易学习: 从已平仓交易中提炼历史经验"""

    def __init__(self, state_file: str = None):
        self.state_file = state_file or os.path.join(DATA_DIR, "state.json")
        self.experience_file = EXPERIENCE_FILE
        self.experience = self._load_experience()

    # ---------- 经验库持久化 ----------
    def _load_experience(self) -> dict:
        if os.path.exists(self.experience_file):
            try:
                with open(self.experience_file, "r", encoding="utf-8") as f:
                    return json.load(f)
            except Exception:
                pass
        return {
            "version": 1,
            "closed_trades": [],   # 已平仓交易记录 [{code,name,entry,exit,pnl_pct,confidence,trend,news_sentiment,hold_minutes}]
            "stats": {},           # 统计摘要
            "updated_at": "",
        }

    def save(self):
        os.makedirs(DATA_DIR, exist_ok=True)
        with open(self.experience_file, "w", encoding="utf-8") as f:
            json.dump(self.experience, f, ensure_ascii=False, indent=2)

    # ---------- 从订单学习 ----------
    def learn_from_trades(self, state: dict = None):
        """扫描 state.json 的订单, 把新平仓的交易加入经验库"""
        state = state or self._load_state()
        orders = state.get("orders", [])
        buys = {}   # code -> 最后一笔买入
        # 逐笔处理: 买入记录快照, 卖出结算盈亏
        for o in orders:
            code = o.get("code")
            if o.get("action") == "BUY":
                buys[code] = o
            elif o.get("action") == "SELL" and code in buys:
                buy = buys.pop(code)
                # 计算该笔交易盈亏
                pnl = self._calc_pnl(buy, o)
                # 从买入时的 reason 提取置信度
                conf = self._extract_confidence(buy.get("reason", ""))
                # 持仓分钟数
                hold_minutes = self._hold_minutes(buy.get("time", ""), o.get("time", ""))
                trade = {
                    "code": code,
                    "name": o.get("name", buy.get("name", code)),
                    "entry_time": buy.get("time", ""),
                    "exit_time": o.get("time", ""),
                    "entry_price": buy.get("price", 0),
                    "exit_price": o.get("price", 0),
                    "pnl_pct": round(pnl, 2),
                    "confidence": conf,
                    "hold_minutes": hold_minutes,
                    "reason": buy.get("reason", "")[:100],
                }
                self.experience["closed_trades"].append(trade)

        # 去重: 按 entry_time+code 去重(防止重复学习)
        seen = set()
        unique = []
        for t in self.experience["closed_trades"]:
            key = (t.get("code"), t.get("entry_time"))
            if key not in seen:
                seen.add(key)
                unique.append(t)
        self.experience["closed_trades"] = unique

        # 重新统计
        self.experience["stats"] = self._compute_stats()
        self.experience["updated_at"] = __import__("datetime").datetime.now().isoformat()
        self.save()
        return self.experience["stats"]

    def _load_state(self) -> dict:
        if not os.path.exists(self.state_file):
            return {}
        with open(self.state_file, "r", encoding="utf-8") as f:
            return json.load(f)

    # ---------- 统计 ----------
    def _compute_stats(self) -> dict:
        trades = self.experience["closed_trades"]
        if not trades:
            return {
                "total_trades": 0,
                "win_rate": 0, "avg_pnl": 0, "profit_factor": 0,
                "best": 0, "worst": 0, "confidence_stats": {}, "message": "暂无已平仓交易, 交易几笔后自动生成经验",
            }
        wins = [t for t in trades if t["pnl_pct"] > 0]
        losses = [t for t in trades if t["pnl_pct"] <= 0]
        win_rate = len(wins) / len(trades) * 100
        avg_win = sum(t["pnl_pct"] for t in wins) / len(wins) if wins else 0
        avg_loss = sum(t["pnl_pct"] for t in losses) / len(losses) if losses else 0
        profit_factor = abs(avg_win / avg_loss) if avg_loss != 0 else float("inf")

        # 按置信度分段统计胜率
        conf_stats = {}
        for t in trades:
            conf = t.get("confidence", 0)
            bucket = "高置信(≥0.85)" if conf >= 0.85 else ("中置信(0.7-0.85)" if conf >= 0.7 else "低置信(<0.7)")
            s = conf_stats.setdefault(bucket, {"count": 0, "wins": 0, "avg_pnl": 0})
            s["count"] += 1
            s["wins"] += 1 if t["pnl_pct"] > 0 else 0
            s["avg_pnl"] += t["pnl_pct"]
        for s in conf_stats.values():
            s["win_rate"] = round(s["wins"] / s["count"] * 100, 1) if s["count"] else 0
            s["avg_pnl"] = round(s["avg_pnl"] / s["count"], 2) if s["count"] else 0

        return {
            "total_trades": len(trades),
            "win_rate": round(win_rate, 1),
            "avg_pnl": round(sum(t["pnl_pct"] for t in trades) / len(trades), 2),
            "profit_factor": round(profit_factor, 2) if profit_factor != float("inf") else 999,
            "best": round(max(t["pnl_pct"] for t in trades), 2),
            "worst": round(min(t["pnl_pct"] for t in trades), 2),
            "confidence_stats": conf_stats,
            "recent": [{"code": t.get("code"), "name": t.get("name"), "pnl_pct": t.get("pnl_pct"),
                        "exit_time": t.get("exit_time", "")[:16]} for t in trades[-5:]],
        }

    # ---------- 经验摘要 ----------
    def experience_summary(self) -> str:
        """生成给 AI 看的经验摘要文本"""
        s = self.experience["stats"]
        if not s or not s.get("total_trades"):
            return "暂无历史交易经验(系统刚启动, 完成几笔交易后自动生成)。请基于技术面与消息面谨慎决策。"
        lines = [
            f"【历史交易经验】已平仓 {s['total_trades']} 笔, 胜率 {s['win_rate']}%, 平均每笔 {s['avg_pnl']:+.2f}%, "
            f"盈亏比 {s['profit_factor']}, 最好 {s['best']:+.2f}% 最差 {s['worst']:+.2f}%",
        ]
        # 置信度胜率
        for bucket, st in s.get("confidence_stats", {}).items():
            lines.append(f"  {bucket}: {st['count']}笔, 胜率{st['win_rate']}%, 均收益{st['avg_pnl']:+.2f}%")
        # 建议
        advice = []
        for bucket, st in s.get("confidence_stats", {}).items():
            if st["count"] >= 3 and st["win_rate"] >= 60:
                advice.append(f"你的{bucket}历史表现好(胜率{st['win_rate']}%), 可更信任此类信号")
            elif st["count"] >= 3 and st["win_rate"] <= 40:
                advice.append(f"你的{bucket}历史表现差(胜率{st['win_rate']}%), 此类信号应更谨慎或反向参考")
        if advice:
            lines.append("  " + "; ".join(advice))
        return "\n".join(lines)

    # ---------- 工具 ----------
    @staticmethod
    def _calc_pnl(buy: dict, sell: dict) -> float:
        """计算一笔交易收益率(扣除手续费)"""
        buy_amount = buy.get("amount", 0)
        buy_fee = buy.get("commission", 0)
        sell_amount = sell.get("amount", 0)
        sell_fee = sell.get("commission", 0) + sell.get("stamp_tax", 0)
        cost = buy_amount + buy_fee
        net = sell_amount - sell_fee
        return (net - cost) / cost * 100 if cost else 0

    @staticmethod
    def _extract_confidence(reason: str) -> float:
        """从 reason 提取置信度, 如 'AI信号: 买入(置信80%)' -> 0.8"""
        import re
        m = re.search(r"置信(\d+)%", reason or "")
        return int(m.group(1)) / 100 if m else 0.5

    @staticmethod
    def _hold_minutes(t1: str, t2: str) -> float:
        from datetime import datetime
        try:
            return (datetime.fromisoformat(t2) - datetime.fromisoformat(t1)).total_seconds() / 60
        except Exception:
            return 0


if __name__ == "__main__":
    tl = TradingLearner()
    stats = tl.learn_from_trades()
    print("=== 学习结果 ===")
    print(json.dumps(stats, ensure_ascii=False, indent=2))
    print("\n=== 经验摘要 ===")
    print(tl.experience_summary())
