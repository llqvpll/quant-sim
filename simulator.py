# -*- coding: utf-8 -*-
"""
AI 模拟交易引擎核心
====================
虚拟账户 + 订单执行 + 持仓管理 + 每日净值快照 + 交易记录持久化
全程不碰真钱，用于验证 AI 策略是否能在 A 股稳定盈利。

用法:
    from simulator import Simulator

    sim = Simulator()
    sim.buy("600519", price=1300.0, shares=100, reason="AI信号买入")
    sim.sell("600519", price=1350.0, shares=100, reason="AI信号卖出")
    sim.snapshot()  # 每日净值快照
    sim.save()
"""

import json
import os
import time
from datetime import datetime, date

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
STATE_FILE = os.path.join(DATA_DIR, "state.json")

# 风控参数
INITIAL_CAPITAL = 50_000.0     # 初始虚拟资金 5 万(小资金盘)
MAX_POSITIONS = 3               # 小资金: 最多同时持有 3 只
MAX_SINGLE_POSITION_RATIO = 0.25  # 单只股票仓位不超过总资产 25%
STOP_LOSS_PCT = -5.0            # 单只股票亏损 -5% 止损
TAKE_PROFIT_PCT = 15.0          # 单只股票盈利 +15% 止盈
COMMISSION_RATE = 0.0003        # 佣金万三
MIN_COMMISSION = 5.0            # 最低佣金 5 元(小资金下影响显著)
STAMP_TAX_RATE = 0.001          # 印花税千一(卖出收)
DRAWDOWN_HALT_PCT = -10.0       # 总资产回撤 -10% 熔断(暂停新买入)
MIN_HOLD_MINUTES = 30           # 最短持有时间(分钟), 防止频繁换手吃手续费


class Simulator:
    """AI 模拟交易引擎: 虚拟账户、下单、持仓、统计"""

    def __init__(self, state_file: str = STATE_FILE, capital: float = None):
        self.state_file = state_file
        self._capital_override = capital  # 可选: 指定初始资金(默认用 INITIAL_CAPITAL)
        self.state = self._load_or_init()

    # ---------- 状态持久化 ----------
    def _load_or_init(self):
        if os.path.exists(self.state_file):
            with open(self.state_file, "r", encoding="utf-8") as f:
                state = json.load(f)
            # 旧状态文件补新字段
            state.setdefault("peak_assets", state.get("initial_capital", INITIAL_CAPITAL))
            state.setdefault("halted", False)
            state.setdefault("halt_reason", "")
            state.setdefault("ai_usage", {"calls": 0, "rule_skipped": 0})
            state["config"].setdefault("min_commission", MIN_COMMISSION)
            state["config"].setdefault("drawdown_halt_pct", DRAWDOWN_HALT_PCT)
            state["config"].setdefault("min_hold_minutes", MIN_HOLD_MINUTES)
            return state
        return self._init_state()

    def _init_state(self):
        # 支持 --capital 指定初始资金; 旧状态文件若无新字段则补默认值
        init_cap = float(self._capital_override) if self._capital_override else INITIAL_CAPITAL
        return {
            "created_at": datetime.now().isoformat(),
            "initial_capital": init_cap,
            "cash": init_cap,                 # 可用现金
            "positions": {},                   # {code: {"shares": int, "avg_cost": float, "last_price": float}}
            "orders": [],                      # 所有成交记录
            "daily_net_values": [],            # [{date, total_assets, cash, market_value}]
            "signal_log": [],                  # AI 信号日志
            "peak_assets": init_cap,           # 历史峰值资产(熔断用)
            "halted": False,                   # 是否熔断中
            "halt_reason": "",                 # 熔断原因
            "ai_usage": {"calls": 0, "rule_skipped": 0},  # AI 调用成本统计
            "config": {
                "initial_capital": init_cap,
                "max_positions": MAX_POSITIONS,
                "max_single_position_ratio": MAX_SINGLE_POSITION_RATIO,
                "stop_loss_pct": STOP_LOSS_PCT,
                "take_profit_pct": TAKE_PROFIT_PCT,
                "commission_rate": COMMISSION_RATE,
                "stamp_tax_rate": STAMP_TAX_RATE,
                "min_commission": MIN_COMMISSION,
                "drawdown_halt_pct": DRAWDOWN_HALT_PCT,
                "min_hold_minutes": MIN_HOLD_MINUTES,
            },
        }

    def save(self):
        os.makedirs(DATA_DIR, exist_ok=True)
        with open(self.state_file, "w", encoding="utf-8") as f:
            json.dump(self.state, f, ensure_ascii=False, indent=2)

    @classmethod
    def reset(cls, state_file: str = STATE_FILE, capital: float = None):
        """重置模拟账户(覆盖为新状态, 用指定资金重新开始)。旧数据先备份"""
        if os.path.exists(state_file):
            bak = state_file.replace(".json", f"_bak_{datetime.now().strftime('%Y%m%d_%H%M%S')}.json")
            try:
                os.replace(state_file, bak)  # 先移走旧文件作为备份
            except Exception:
                pass
        tmp = cls(state_file=state_file, capital=capital)
        tmp.save()
        print(f"模拟账户已重置: 初始资金 {tmp.state['initial_capital']:.0f} 元"
              + (f" (旧状态已备份为 {os.path.basename(bak)})" if 'bak' in locals() else ""))
        return tmp

    # ---------- 资产计算 ----------
    @property
    def total_assets(self) -> float:
        """总资产 = 现金 + 持仓市值"""
        return self.state["cash"] + self.market_value

    @property
    def market_value(self) -> float:
        mv = 0.0
        for code, pos in self.state["positions"].items():
            mv += pos["shares"] * pos.get("last_price", pos["avg_cost"])
        return mv

    def get_total_return_pct(self) -> float:
        """总收益率"""
        init = self.state["initial_capital"]
        return (self.total_assets - init) / init * 100 if init else 0.0

    # ---------- 风控检查 ----------
    def _check_buy_risk(self, code: str, price: float, shares: int, reason: str) -> tuple[bool, str]:
        """买入前风控检查，返回 (是否允许, 拒绝原因)"""
        if code in self.state["positions"]:
            return False, f"已持有 {code}，不重复买入"
        if len(self.state["positions"]) >= MAX_POSITIONS:
            return False, f"持仓已满 {MAX_POSITIONS} 只"
        cost = price * shares
        if cost > self.state["cash"]:
            return False, f"现金不足: 需 {cost:.2f}, 可用 {self.state['cash']:.2f}"
        # 单票仓位限制(按含佣总成本)
        after_cost = cost + self._commission(cost)
        if after_cost / self.total_assets > MAX_SINGLE_POSITION_RATIO:
            max_shares = int(self.total_assets * MAX_SINGLE_POSITION_RATIO / price / 100) * 100
            return False, f"单票仓位超限({MAX_SINGLE_POSITION_RATIO*100:.0f}%), 最多可买 {max_shares} 股"
        # 熔断: 总资产回撤超阈值时暂停新买入
        if self.state.get("halted"):
            return False, f"熔断中: {self.state.get('halt_reason', '')}"
        return True, ""

    # ---------- 熔断 ----------
    def update_halt(self):
        """根据总资产回撤更新熔断状态。总资产含浮盈亏, 回撤超阈值即暂停新买入"""
        peak = self.state.get("peak_assets", self.state["initial_capital"])
        total = self.total_assets
        if total > peak:
            self.state["peak_assets"] = round(total, 2)
            # 净值创新高自动解除熔断
            if self.state.get("halted"):
                self.state["halted"] = False
                self.state["halt_reason"] = ""
        else:
            dd = (total - peak) / peak * 100
            if dd <= DRAWDOWN_HALT_PCT:
                if not self.state.get("halted"):
                    self.state["halted"] = True
                    self.state["halt_reason"] = f"总资产回撤 {dd:.1f}% 触发熔断, 暂停新买入, 待净值回升"
        self.save()

    def check_min_hold(self, code: str, force: bool = False) -> bool:
        """最短持有检查: 买入不足 MIN_HOLD_MINUTES 不允许卖出(防频繁交易)
        force=True 时豁免(如 AI 明确止损信号/强卖出), 但仅限持有超 5 分钟"""
        if force:
            return self._held_minutes(code) >= 5
        for o in reversed(self.state["orders"]):
            if o["code"] == code and o["action"] == "BUY":
                try:
                    t = datetime.fromisoformat(o["time"])
                except Exception:
                    return True
                held = (datetime.now() - t).total_seconds() / 60
                return held >= MIN_HOLD_MINUTES
        return True

    def _held_minutes(self, code: str) -> float:
        """返回股票已持有的分钟数"""
        for o in reversed(self.state["orders"]):
            if o["code"] == code and o["action"] == "BUY":
                try:
                    return (datetime.now() - datetime.fromisoformat(o["time"])).total_seconds() / 60
                except Exception:
                    return 999
        return 999

    @staticmethod
    def _commission(amount: float) -> float:
        """佣金计算，最低 5 元"""
        return max(amount * COMMISSION_RATE, MIN_COMMISSION)

    def _round_trip_cost_pct(self, price: float) -> float:
        """估算一次完整买卖(一手100股)的手续费占比, 用于过滤小资金下不划算的标的"""
        amount = price * 100
        return (self._commission(amount) * 2 + amount * STAMP_TAX_RATE) / amount * 100

    # ---------- 下单 ----------
    def buy(self, code: str, name: str, price: float, shares: int, reason: str = "") -> dict:
        """买入。shares 必须是 100 的整数倍(A股一手)。返回订单结果"""
        # 风控
        ok, msg = self._check_buy_risk(code, price, shares, reason)
        if not ok:
            return {"success": False, "msg": msg, "code": code}
        # A股一手=100股
        if shares % 100 != 0:
            shares = int(shares / 100) * 100
        if shares <= 0:
            return {"success": False, "msg": "股数必须为正", "code": code}

        amount = price * shares
        commission = self._commission(amount)
        total_cost = amount + commission
        if total_cost > self.state["cash"]:
            return {"success": False, "msg": f"现金不足: 需 {total_cost:.2f}(含佣), 可用 {self.state['cash']:.2f}", "code": code}

        # 成交
        self.state["cash"] -= total_cost
        self.state["positions"][code] = {
            "name": name,
            "shares": shares,
            "avg_cost": price,
            "last_price": price,
            "open_time": datetime.now().isoformat(),
        }
        order = {
            "id": len(self.state["orders"]) + 1,
            "time": datetime.now().isoformat(),
            "action": "BUY",
            "code": code,
            "name": name,
            "price": price,
            "shares": shares,
            "amount": amount,
            "commission": commission,
            "stamp_tax": 0.0,
            "reason": reason,
        }
        self.state["orders"].append(order)
        self.save()
        return {"success": True, "msg": f"已买入 {name}({code}) {shares}股 @ {price:.2f} (费用 {commission:.2f})", "order": order}

    def sell(self, code: str, price: float, shares: int, reason: str = "") -> dict:
        """卖出。shares 可传 -1 表示全部卖出。止损/止盈/熔断清仓不受最短持有限制"""
        pos = self.state["positions"].get(code)
        if not pos:
            return {"success": False, "msg": f"未持有 {code}", "code": code}
        # 最短持有: 普通AI卖出需持有足够时长, 止损止盈等风控卖出豁免
        if reason and "AI信号" in reason:
            # AI 明确"止损卖出"信号且持有超 5 分钟 → 豁免最短持有(尊重 AI 止损判断)
            force = "止损" in reason or "止盈" in reason
            if not self.check_min_hold(code, force=force):
                held = self._held_minutes(code)
                return {"success": False, "msg": f"持有仅 {held:.0f} 分钟, 未到最短持有 {MIN_HOLD_MINUTES} 分钟, 暂缓卖出(避免频繁交易)", "code": code}
        if shares == -1 or shares > pos["shares"]:
            shares = pos["shares"]
        if shares <= 0:
            return {"success": False, "msg": "股数必须为正", "code": code}

        amount = price * shares
        commission = self._commission(amount)
        stamp_tax = amount * STAMP_TAX_RATE
        net = amount - commission - stamp_tax

        # 成交
        self.state["cash"] += net
        if shares >= pos["shares"]:
            del self.state["positions"][code]
        else:
            pos["shares"] -= shares
            pos["last_price"] = price
        order = {
            "id": len(self.state["orders"]) + 1,
            "time": datetime.now().isoformat(),
            "action": "SELL",
            "code": code,
            "name": pos.get("name", code),
            "price": price,
            "shares": shares,
            "amount": amount,
            "commission": commission,
            "stamp_tax": stamp_tax,
            "reason": reason,
        }
        self.state["orders"].append(order)
        self.save()
        return {"success": True, "msg": f"已卖出 {code} {shares}股 @ {price:.2f}", "order": order}

    # ---------- 持仓管理 ----------
    def update_prices(self, prices: dict):
        """用最新行情更新持仓价格，用于计算市值和触发止损止盈"""
        for code, pos in self.state["positions"].items():
            if code in prices:
                pos["last_price"] = prices[code]
        self.save()

    def check_stop_loss_take_profit(self, prices: dict) -> list[dict]:
        """检查止损止盈，返回需要卖出的指令列表"""
        signals = []
        for code, pos in list(self.state["positions"].items()):
            price = prices.get(code)
            if not price:
                continue
            pnl_pct = (price - pos["avg_cost"]) / pos["avg_cost"] * 100
            if pnl_pct <= STOP_LOSS_PCT:
                signals.append({"code": code, "action": "SELL", "shares": -1,
                                "reason": f"止损触发: 盈亏 {pnl_pct:+.1f}% (阈值 {STOP_LOSS_PCT}%)"})
            elif pnl_pct >= TAKE_PROFIT_PCT:
                signals.append({"code": code, "action": "SELL", "shares": -1,
                                "reason": f"止盈触发: 盈亏 {pnl_pct:+.1f}% (阈值 {TAKE_PROFIT_PCT}%)"})
        return signals

    # ---------- 统计与快照 ----------
    def snapshot(self, date_str: str = None):
        """记录每日净值快照"""
        date_str = date_str or date.today().isoformat()
        # 净值快照前先更新熔断状态(用含浮盈亏的总资产)
        self.update_halt()
        # 避免同一天重复快照(更新而非追加)
        for i, snap in enumerate(self.state["daily_net_values"]):
            if snap["date"] == date_str:
                self.state["daily_net_values"][i] = {
                    "date": date_str,
                    "total_assets": round(self.total_assets, 2),
                    "cash": round(self.state["cash"], 2),
                    "market_value": round(self.market_value, 2),
                }
                self.save()
                return self.state["daily_net_values"][i]
        snap = {
            "date": date_str,
            "total_assets": round(self.total_assets, 2),
            "cash": round(self.state["cash"], 2),
            "market_value": round(self.market_value, 2),
        }
        self.state["daily_net_values"].append(snap)
        self.save()
        return snap

    def log_signal(self, code: str, name: str, signal: str, confidence: float, reason: str, executed: bool, note: str = ""):
        """记录 AI 信号"""
        self.state["signal_log"].append({
            "time": datetime.now().isoformat(),
            "code": code,
            "name": name,
            "signal": signal,
            "confidence": confidence,
            "reason": reason,
            "executed": executed,
            "note": note,
        })
        self.save()

    def add_ai_usage(self, calls: int = 1, rule_skipped: int = 0):
        """累计 AI 调用成本统计(看板展示用)"""
        usage = self.state.setdefault("ai_usage", {"calls": 0, "rule_skipped": 0})
        usage["calls"] = usage.get("calls", 0) + calls
        usage["rule_skipped"] = usage.get("rule_skipped", 0) + rule_skipped
        self.save()

    # ---------- 业绩摘要 ----------
    def summary(self) -> dict:
        """生成业绩摘要(成绩单)"""
        orders = self.state["orders"]
        buys = [o for o in orders if o["action"] == "BUY"]
        sells = [o for o in orders if o["action"] == "SELL"]
        # 已平仓盈亏(简单法: 按卖单)
        realized = 0.0
        for s in sells:
            realized += s["amount"] - s["commission"] - s["stamp_tax"]
        # 买入成本(含佣金)
        buy_cost = sum(b["amount"] + b["commission"] for b in buys)
        realized_pnl = realized - buy_cost
        # 未平仓浮动盈亏
        unrealized = 0.0
        for code, pos in self.state["positions"].items():
            unrealized += (pos["last_price"] - pos["avg_cost"]) * pos["shares"]
        # 累计手续费
        total_fee = sum(o.get("commission", 0) + o.get("stamp_tax", 0) for o in orders)

        return {
            "初始资金": self.state["initial_capital"],
            "当前总资产": round(self.total_assets, 2),
            "总收益率%": round(self.get_total_return_pct(), 2),
            "现金": round(self.state["cash"], 2),
            "持仓市值": round(self.market_value, 2),
            "持仓数量": len(self.state["positions"]),
            "持仓列表": [{"code": c, "name": p.get("name", c), "shares": p["shares"],
                          "avg_cost": p["avg_cost"], "last_price": p.get("last_price", p["avg_cost"]),
                          "盈亏%": round((p.get("last_price", p["avg_cost"]) - p["avg_cost"]) / p["avg_cost"] * 100, 2)}
                         for c, p in self.state["positions"].items()],
            "总成交笔数": len(orders),
            "已平仓盈亏": round(realized_pnl, 2),
            "浮动盈亏": round(unrealized, 2),
            "累计手续费": round(total_fee, 2),
            "熔断状态": "熔断中" if self.state.get("halted") else "正常",
        }


if __name__ == "__main__":
    # 自测
    sim = Simulator()
    print("=== 自测 ===")
    r1 = sim.buy("600519", "贵州茅台", 1300.0, 100, "自测买入")
    print(r1["msg"])
    r2 = sim.buy("000001", "平安银行", 11.0, 10000, "自测买入")
    print(r2["msg"])
    r3 = sim.buy("600519", "贵州茅台", 1300.0, 2000, "超仓位买入")
    print(r3["msg"])
    sim.update_prices({"600519": 1350.0, "000001": 11.5})
    print(f"总资产: {sim.total_assets:.2f}, 总收益率: {sim.get_total_return_pct():.2f}%")
    sim.snapshot()
    r4 = sim.sell("600519", 1350.0, -1, "止盈测试")
    print(r4["msg"])
    print("\n=== 摘要 ===")
    for k, v in sim.summary().items():
        print(f"{k}: {v}")
