# -*- coding: utf-8 -*-
"""交易经验学习（v2）。

旧版用卖出净价对比“不含买入佣金”的入场价，收益被高估；统计也不区分
“平均盈亏比”和“总盈亏比”。v2 改为：
- 买入成本含佣金/过户费，卖出净额扣佣金/印花税/过户费
- FIFO 配对，部分卖出按批次拆记录
- 胜率、平均收益、总盈亏比、按置信度分段分开统计
- 样本少于 10 笔时不给“可更信任/反向参考”这类建议，避免噪声指导
"""
from __future__ import annotations

import json
import os
import re
from datetime import datetime
from typing import Dict, List, Optional

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
EXPERIENCE_FILE = os.path.join(DATA_DIR, "experience.json")


class TradingLearner:
    def __init__(self, state_file: Optional[str] = None):
        self.state_file = state_file or os.path.join(DATA_DIR, "state.json")
        self.experience_file = EXPERIENCE_FILE
        self.experience = self._load_experience()

    def _load_experience(self) -> dict:
        if os.path.exists(self.experience_file):
            try:
                with open(self.experience_file, "r", encoding="utf-8") as fh:
                    data = json.load(fh)
                if isinstance(data, dict):
                    data.setdefault("closed_trades", [])
                    data.setdefault("stats", {})
                    return data
            except (OSError, ValueError):
                pass
        return {"version": 2, "closed_trades": [], "stats": {}, "updated_at": ""}

    def save(self):
        os.makedirs(DATA_DIR, exist_ok=True)
        tmp = self.experience_file + ".tmp"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(self.experience, fh, ensure_ascii=False, indent=2)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, self.experience_file)

    # ---------- 从订单学习 ----------
    def learn_from_trades(self, state: Optional[dict] = None) -> dict:
        state = state or self._load_state()
        orders = state.get("orders", []) if isinstance(state, dict) else []
        lots: Dict[str, List[dict]] = {}
        closed: List[dict] = []

        for order in orders:
            code = order.get("code")
            action = order.get("action")
            shares = int(order.get("shares") or 0)
            if not code or shares <= 0:
                continue
            amount = float(order.get("amount") or 0)
            if action == "BUY":
                buy_fee = float(order.get("commission") or 0) + float(order.get("transfer_fee") or 0)
                lots.setdefault(code, []).append({
                    "shares": shares,
                    "cost_per_share": (amount + buy_fee) / shares,
                    "time": order.get("time", ""),
                    "reason": order.get("reason", ""),
                })
            elif action == "SELL":
                sell_fee = (float(order.get("commission") or 0) + float(order.get("stamp_tax") or 0)
                            + float(order.get("transfer_fee") or 0))
                net_per_share = (amount - sell_fee) / shares if shares else 0.0
                queue = lots.get(code, [])
                remaining = shares
                while remaining > 0 and queue:
                    lot = queue[0]
                    take = min(int(lot["shares"]), remaining)
                    cost = float(lot["cost_per_share"])
                    pnl_pct = (net_per_share - cost) / cost * 100 if cost else 0.0
                    closed.append({
                        "code": code,
                        "name": order.get("name", code),
                        "entry_time": lot["time"],
                        "exit_time": order.get("time", ""),
                        "entry_price": round(cost, 4),
                        "exit_price": round(net_per_share, 4),
                        "pnl_pct": round(pnl_pct, 2),
                        "pnl_amount": round((net_per_share - cost) * take, 2),
                        "shares": take,
                        "confidence": self._extract_confidence(lot["reason"]),
                        "hold_minutes": round(self._hold_minutes(lot["time"], order.get("time", "")), 1),
                        "reason": str(lot["reason"])[:100],
                    })
                    lot["shares"] -= take
                    remaining -= take
                    if lot["shares"] <= 0:
                        queue.pop(0)

        seen = set()
        unique = []
        for trade in closed:
            key = (trade["code"], trade["entry_time"], trade["exit_time"])
            if key in seen:
                continue
            seen.add(key)
            unique.append(trade)
        self.experience["version"] = 2
        self.experience["closed_trades"] = unique
        self.experience["stats"] = self._compute_stats(unique)
        self.experience["updated_at"] = datetime.now().isoformat(timespec="seconds")
        self.save()
        return self.experience["stats"]

    def _load_state(self) -> dict:
        if not os.path.exists(self.state_file):
            return {}
        try:
            with open(self.state_file, "r", encoding="utf-8") as fh:
                return json.load(fh)
        except (OSError, ValueError):
            return {}

    # ---------- 统计 ----------
    @staticmethod
    def _compute_stats(trades: List[dict]) -> dict:
        if not trades:
            return {
                "total_trades": 0, "win_rate": 0, "avg_pnl": 0,
                "profit_factor": 0, "avg_win": 0, "avg_loss": 0,
                "best": 0, "worst": 0, "confidence_stats": {},
                "message": "暂无已平仓交易，完成几笔后自动生成经验",
            }
        wins = [trade for trade in trades if trade["pnl_pct"] > 0]
        losses = [trade for trade in trades if trade["pnl_pct"] <= 0]
        gross_win = sum(trade["pnl_amount"] for trade in wins)
        gross_loss = abs(sum(trade["pnl_amount"] for trade in losses))
        avg_win = sum(trade["pnl_pct"] for trade in wins) / len(wins) if wins else 0.0
        avg_loss = sum(trade["pnl_pct"] for trade in losses) / len(losses) if losses else 0.0

        buckets: Dict[str, dict] = {}
        for trade in trades:
            conf = float(trade.get("confidence") or 0.5)
            bucket = "高置信(≥0.85)" if conf >= 0.85 else ("中置信(0.70-0.85)" if conf >= 0.70 else "低置信(<0.70)")
            item = buckets.setdefault(bucket, {"count": 0, "wins": 0, "avg_pnl": 0.0})
            item["count"] += 1
            item["wins"] += 1 if trade["pnl_pct"] > 0 else 0
            item["avg_pnl"] += trade["pnl_pct"]
        for item in buckets.values():
            item["win_rate"] = round(item["wins"] / item["count"] * 100, 1)
            item["avg_pnl"] = round(item["avg_pnl"] / item["count"], 2)

        return {
            "total_trades": len(trades),
            "win_rate": round(len(wins) / len(trades) * 100, 1),
            "avg_pnl": round(sum(trade["pnl_pct"] for trade in trades) / len(trades), 2),
            "profit_factor": round(gross_win / gross_loss, 2) if gross_loss > 0 else (999 if gross_win > 0 else 0),
            "avg_win": round(avg_win, 2),
            "avg_loss": round(avg_loss, 2),
            "best": round(max(trade["pnl_pct"] for trade in trades), 2),
            "worst": round(min(trade["pnl_pct"] for trade in trades), 2),
            "confidence_stats": buckets,
            "recent": [
                {"code": trade["code"], "name": trade["name"], "pnl_pct": trade["pnl_pct"],
                 "exit_time": str(trade["exit_time"])[:16]}
                for trade in trades[-5:]
            ],
        }

    # ---------- 经验摘要 ----------
    def experience_summary(self) -> str:
        stats = self.experience.get("stats") or {}
        total = int(stats.get("total_trades") or 0)
        if total == 0:
            return "暂无历史交易经验（系统刚启动，完成几笔交易后自动生成）。请基于技术面与消息面谨慎决策。"
        lines = [
            f"【历史交易经验】已平仓 {total} 笔，胜率 {stats['win_rate']}%，平均每笔 {stats['avg_pnl']:+.2f}%，"
            f"总盈亏比 {stats['profit_factor']}，平均盈利 {stats['avg_win']:+.2f}%，平均亏损 {stats['avg_loss']:+.2f}%，"
            f"最好 {stats['best']:+.2f}% 最差 {stats['worst']:+.2f}%",
        ]
        for bucket, item in (stats.get("confidence_stats") or {}).items():
            lines.append(f"  {bucket}: {item['count']}笔，胜率 {item['win_rate']}%，均收益 {item['avg_pnl']:+.2f}%")
        advice = []
        for bucket, item in (stats.get("confidence_stats") or {}).items():
            if item["count"] >= 10 and item["win_rate"] >= 60:
                advice.append(f"{bucket} 样本 {item['count']} 笔、胜率 {item['win_rate']}%，可维持当前权重")
            elif item["count"] >= 10 and item["win_rate"] <= 40:
                advice.append(f"{bucket} 样本 {item['count']} 笔、胜率 {item['win_rate']}%，建议降低仓位或提高门槛")
        if total < 10:
            advice.append("样本不足 10 笔，统计仅供参考，不要据此重仓")
        if advice:
            lines.append("  " + "；".join(advice))
        return "\n".join(lines)

    # ---------- 工具 ----------
    @staticmethod
    def _extract_confidence(reason: str) -> float:
        text = str(reason or "")
        match = re.search(r"置信\s*(\d+(?:\.\d+)?)\s*%", text)
        if match:
            return min(float(match.group(1)) / 100.0, 1.0)
        match = re.search(r"confidence\s*[=:]\s*(0?\.\d+|1(?:\.0+)?)", text, re.I)
        if match:
            return float(match.group(1))
        return 0.5

    @staticmethod
    def _hold_minutes(start: str, end: str) -> float:
        try:
            return (datetime.fromisoformat(end) - datetime.fromisoformat(start)).total_seconds() / 60
        except (TypeError, ValueError):
            return 0.0
