# -*- coding: utf-8 -*-
"""A 股模拟交易引擎（v2）。

相对旧版的关键修复：
- 只使用 state["config"] 一套配置，杜绝“看板显示 -1%、实际按 -5% 执行”
- 买入前按板块规整手数（主板/创业板 100 股，科创板 200 股起，北交所 100 股起）
- 卖出执行 T+1；涨跌停价校验（调用方传入 prev_close 时）
- avg_cost 含买入佣金/过户费，已平仓与浮动盈亏口径一致
- 状态文件原子写 + .bak 备份；损坏时尝试备份并告警
- 熔断有恢复条件（回撤恢复或冷却天数），不会空仓永久锁死
"""
from __future__ import annotations

import json
import math
import os
import shutil
from datetime import date, datetime
from typing import Any, Dict, List, Optional

from config import DEFAULT_CONFIG, STATE_VERSION, merge_config, validate_config
from market_rules import (
    at_limit_down,
    at_limit_up,
    available_date,
    check_price_limit,
    is_t1_available,
    min_buy_shares,
    normalize_buy_shares,
)
from trading_calendar import is_trading_day

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
STATE_FILE = os.path.join(DATA_DIR, "state.json")
EXPERIENCE_FILE = os.path.join(DATA_DIR, "experience.json")
MAX_SIGNAL_LOG = 5_000


def _now_iso() -> str:
    return datetime.now().isoformat(timespec="seconds")


def _parse_date(value: Any) -> Optional[date]:
    if value is None or value == "":
        return None
    if isinstance(value, datetime):
        return value.date()
    if isinstance(value, date):
        return value
    try:
        return date.fromisoformat(str(value)[:10])
    except ValueError:
        return None


class Simulator:
    """虚拟账户、下单、持仓、统计。"""

    def __init__(self, state_file: str = STATE_FILE, capital: Optional[float] = None):
        self.state_file = state_file
        self._capital_override = capital
        self.state = self._load_or_init()
        self.state["config"] = validate_config(self.state.get("config"))

    # ---------- 状态 ----------
    @property
    def cfg(self) -> Dict[str, Any]:
        return self.state["config"]

    def _new_state(self, capital: Optional[float] = None) -> Dict[str, Any]:
        init_cap = float(capital if capital is not None else DEFAULT_CONFIG["initial_capital"])
        if not math.isfinite(init_cap) or init_cap <= 0:
            raise ValueError("初始资金必须为正数")
        cfg = merge_config(None)
        cfg["initial_capital"] = init_cap
        return {
            "version": STATE_VERSION,
            "created_at": _now_iso(),
            "initial_capital": init_cap,
            "cash": init_cap,
            "positions": {},
            "orders": [],
            "daily_net_values": [],
            "signal_log": [],
            "peak_assets": init_cap,
            "halted": False,
            "halt_reason": "",
            "halted_at": None,
            "ai_usage": {"calls": 0, "errors": 0, "rule_skipped": 0, "cost_cny": 0.0},
            "last_scan": {},
            "next_order_id": 1,
            "config": cfg,
        }

    def _load_or_init(self) -> Dict[str, Any]:
        if not os.path.exists(self.state_file):
            return self._new_state(self._capital_override)
        try:
            with open(self.state_file, "r", encoding="utf-8") as fh:
                state = json.load(fh)
            if not isinstance(state, dict):
                raise ValueError("state 根节点不是对象")
        except Exception as exc:  # noqa: BLE001
            bak = self.state_file + ".bak"
            if os.path.exists(bak):
                try:
                    with open(bak, "r", encoding="utf-8") as fh:
                        state = json.load(fh)
                    if isinstance(state, dict):
                        print(f"[状态] 主状态损坏({exc})，已从 {os.path.basename(bak)} 恢复")
                        return self._migrate(state)
                except Exception:  # noqa: BLE001
                    pass
            backup = self.state_file.replace(".json", f".corrupt_{datetime.now():%Y%m%d_%H%M%S}.json")
            try:
                os.replace(self.state_file, backup)
            except OSError:
                pass
            print(f"[状态] 读取 {self.state_file} 失败({exc})，已另存为 {os.path.basename(backup)}，将新建账户")
            return self._new_state(self._capital_override)

        return self._migrate(state)

    def _migrate(self, state: Dict[str, Any]) -> Dict[str, Any]:
        state.setdefault("version", 1)
        state.setdefault("peak_assets", state.get("initial_capital", DEFAULT_CONFIG["initial_capital"]))
        state.setdefault("halted", False)
        state.setdefault("halt_reason", "")
        state.setdefault("halted_at", None)
        state.setdefault("ai_usage", {"calls": 0, "errors": 0, "rule_skipped": 0, "cost_cny": 0.0})
        state.setdefault("last_scan", {})
        state.setdefault("orders", [])
        state.setdefault("signal_log", [])
        state.setdefault("daily_net_values", [])
        state.setdefault("positions", {})
        state.setdefault("next_order_id", len(state["orders"]) + 1)
        state["config"] = merge_config(state.get("config"))
        for code, pos in state["positions"].items():
            pos.setdefault("name", code)
            pos.setdefault("last_price", pos.get("avg_cost", 0))
            pos.setdefault("buy_date", None)
            pos.setdefault("available_date", available_date(pos.get("buy_date")) if pos.get("buy_date") else None)
            pos.setdefault("stop_loss", None)
            pos.setdefault("take_profit", None)
            pos.setdefault("open_time", _now_iso())
        state["version"] = STATE_VERSION
        return state

    # ---------- 原子持久化 ----------
    def save(self):
        os.makedirs(os.path.dirname(os.path.abspath(self.state_file)) or DATA_DIR, exist_ok=True)
        tmp = self.state_file + ".tmp"
        bak = self.state_file + ".bak"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(self.state, fh, ensure_ascii=False, indent=2)
            fh.flush()
            os.fsync(fh.fileno())
        if os.path.exists(self.state_file):
            try:
                shutil.copy2(self.state_file, bak)
            except OSError:
                pass
        os.replace(tmp, self.state_file)

    @classmethod
    def reset(cls, state_file: str = STATE_FILE, capital: Optional[float] = None,
              clear_experience: bool = True):
        """备份旧状态并新建账户；同时清理经验库（旧账户的经验不再适用）。"""
        if os.path.exists(state_file):
            bak = state_file.replace(".json", f"_bak_{datetime.now():%Y%m%d_%H%M%S}.json")
            try:
                os.replace(state_file, bak)
                print(f"[状态] 旧账户已备份为 {os.path.basename(bak)}")
            except OSError as exc:
                print(f"[状态] 备份旧账户失败: {exc}")
        if clear_experience and os.path.exists(EXPERIENCE_FILE):
            try:
                os.replace(EXPERIENCE_FILE, EXPERIENCE_FILE.replace(".json", f"_bak_{datetime.now():%Y%m%d_%H%M%S}.json"))
            except OSError:
                pass
        fresh = cls(state_file=state_file, capital=capital)
        fresh.save()
        print(f"[状态] 模拟账户已重置: 初始资金 {fresh.state['initial_capital']:.0f} 元")
        return fresh

    # ---------- 资产 ----------
    @property
    def total_assets(self) -> float:
        return float(self.state["cash"]) + self.market_value

    @property
    def market_value(self) -> float:
        total = 0.0
        for pos in self.state["positions"].values():
            total += float(pos["shares"]) * float(pos.get("last_price", pos.get("avg_cost", 0)))
        return total

    def get_total_return_pct(self) -> float:
        init = float(self.state.get("initial_capital") or 0)
        return (self.total_assets - init) / init * 100 if init else 0.0

    # ---------- 费用 ----------
    def _commission(self, amount: float) -> float:
        return max(float(amount) * float(self.cfg["commission_rate"]), float(self.cfg["min_commission"]))

    def _transfer_fee(self, amount: float) -> float:
        return float(amount) * float(self.cfg["transfer_fee_rate"])

    def _stamp_tax(self, amount: float) -> float:
        return float(amount) * float(self.cfg["stamp_tax_rate"])

    def _round_trip_cost_pct(self, price: float, code: str = "600000") -> float:
        """估算“买一手再卖出”的往返费用占比（含最低佣金）。"""
        lot = min_buy_shares(code)
        amount = float(price) * lot
        if amount <= 0:
            return 999.0
        buy_fee = self._commission(amount) + self._transfer_fee(amount)
        sell_fee = self._commission(amount) + self._transfer_fee(amount) + self._stamp_tax(amount)
        return (buy_fee + sell_fee) / amount * 100

    # ---------- 风控 ----------
    def _check_buy_risk(self, code: str, price: float, shares: int) -> tuple[bool, str]:
        if code in self.state["positions"]:
            return False, f"已持有 {code}，不重复买入"
        if len(self.state["positions"]) >= int(self.cfg["max_positions"]):
            return False, f"持仓已满 {self.cfg['max_positions']} 只"
        if self.state.get("halted"):
            return False, f"熔断中: {self.state.get('halt_reason', '')}"
        amount = price * shares
        total_cost = amount + self._commission(amount) + self._transfer_fee(amount)
        if total_cost > self.state["cash"] + 1e-6:
            return False, f"现金不足: 需 {total_cost:.2f}, 可用 {self.state['cash']:.2f}"
        total = self.total_assets
        if total <= 0:
            return False, "总资产为 0，无法下单"
        if total_cost / total > float(self.cfg["max_single_position_ratio"]) + 1e-9:
            max_shares = int(total * float(self.cfg["max_single_position_ratio"]) / price)
            return False, f"单票仓位超限({float(self.cfg['max_single_position_ratio'])*100:.0f}%), 最多可买 {max_shares} 股"
        return True, ""

    def update_halt(self, today: Optional[date | str] = None, save: bool = True):
        """回撤熔断；回撤恢复到 halt_recover_pct 或冷却期满后自动解除。"""
        peak = float(self.state.get("peak_assets") or self.state["initial_capital"])
        total = self.total_assets
        today_date = _parse_date(today) or date.today()
        if total > peak:
            self.state["peak_assets"] = round(total, 2)
            if self.state.get("halted"):
                self.state["halted"] = False
                self.state["halt_reason"] = ""
                self.state["halted_at"] = None
        else:
            dd = (total - peak) / peak * 100 if peak else 0.0
            if not self.state.get("halted") and dd <= float(self.cfg["drawdown_halt_pct"]):
                self.state["halted"] = True
                self.state["halted_at"] = today_date.isoformat()
                self.state["halt_reason"] = (
                    f"总资产回撤 {dd:.1f}% 触发熔断；回撤恢复至 {self.cfg['halt_recover_pct']}% "
                    f"或 {self.cfg['halt_cooldown_days']} 个交易日后自动解除"
                )
            elif self.state.get("halted"):
                recovered = dd >= float(self.cfg["halt_recover_pct"])
                cooled = False
                halted_at = _parse_date(self.state.get("halted_at"))
                if halted_at:
                    cooled = (today_date - halted_at).days >= int(self.cfg["halt_cooldown_days"])
                if recovered or cooled:
                    self.state["halted"] = False
                    self.state["halt_reason"] = ""
                    self.state["halted_at"] = None
        if save:
            self.save()

    def check_min_hold(self, code: str, force: bool = False) -> bool:
        if force:
            return self._held_minutes(code) >= 5
        for order in reversed(self.state["orders"]):
            if order.get("code") == code and order.get("action") == "BUY":
                try:
                    held = (datetime.now() - datetime.fromisoformat(order["time"])).total_seconds() / 60
                except (KeyError, ValueError):
                    return True
                return held >= float(self.cfg["min_hold_minutes"])
        return True

    def _held_minutes(self, code: str) -> float:
        for order in reversed(self.state["orders"]):
            if order.get("code") == code and order.get("action") == "BUY":
                try:
                    return (datetime.now() - datetime.fromisoformat(order["time"])).total_seconds() / 60
                except (KeyError, ValueError):
                    return 10_000.0
        return 10_000.0

    # ---------- 下单 ----------
    def buy(self, code: str, name: str, price: float, shares: int, reason: str = "",
            trade_date: Optional[str | date] = None, prev_close: Optional[float] = None,
            stop_loss: Optional[float] = None, take_profit: Optional[float] = None) -> dict:
        try:
            price = float(price)
        except (TypeError, ValueError):
            return {"success": False, "msg": "价格不是数字", "code": code}
        if not math.isfinite(price) or price <= 0:
            return {"success": False, "msg": "价格必须为正的有限数", "code": code}
        shares = normalize_buy_shares(code, shares)
        if shares <= 0:
            return {"success": False, "msg": f"股数不足最低买入单位({min_buy_shares(code)}股)", "code": code}
        ok, msg = check_price_limit(price, prev_close, code, name)
        if not ok:
            return {"success": False, "msg": msg, "code": code}
        if at_limit_up(price, prev_close, code, name):
            return {"success": False, "msg": f"涨停价 {price:.2f} 通常买不进", "code": code}
        ok, msg = self._check_buy_risk(code, price, shares)
        if not ok:
            return {"success": False, "msg": msg, "code": code}

        amount = price * shares
        commission = self._commission(amount)
        transfer_fee = self._transfer_fee(amount)
        total_cost = amount + commission + transfer_fee
        if total_cost > self.state["cash"] + 1e-6:
            return {"success": False, "msg": f"现金不足: 需 {total_cost:.2f}(含费), 可用 {self.state['cash']:.2f}", "code": code}

        trade_day = _parse_date(trade_date) or date.today()
        if trade_date is not None and not is_trading_day(trade_day):
            return {"success": False, "msg": f"{trade_day} 不是交易日", "code": code}
        stop = None
        take = None
        if stop_loss is not None:
            try:
                candidate = float(stop_loss)
                if 0 < candidate < price:
                    stop = round(candidate, 2)
            except (TypeError, ValueError):
                pass
        if take_profit is not None:
            try:
                candidate = float(take_profit)
                if candidate > price:
                    take = round(candidate, 2)
            except (TypeError, ValueError):
                pass

        self.state["cash"] = round(self.state["cash"] - total_cost, 2)
        self.state["positions"][code] = {
            "name": name,
            "shares": shares,
            "avg_cost": total_cost / shares,
            "last_price": price,
            "buy_date": trade_day.isoformat(),
            "available_date": (available_date(trade_day) or trade_day).isoformat(),
            "stop_loss": stop,
            "take_profit": take,
            "open_time": _now_iso(),
        }
        order = {
            "id": self.state["next_order_id"],
            "time": f"{trade_day.isoformat()}T{datetime.now():%H:%M:%S}",
            "action": "BUY",
            "code": code,
            "name": name,
            "price": price,
            "shares": shares,
            "amount": amount,
            "commission": commission,
            "transfer_fee": transfer_fee,
            "stamp_tax": 0.0,
            "reason": reason,
        }
        self.state["next_order_id"] += 1
        self.state["orders"].append(order)
        self.save()
        return {"success": True, "msg": f"已买入 {name}({code}) {shares}股 @ {price:.2f} (费用 {commission + transfer_fee:.2f})",
                "order": order}

    def sell(self, code: str, price: float, shares: int, reason: str = "",
             trade_date: Optional[str | date] = None, prev_close: Optional[float] = None) -> dict:
        pos = self.state["positions"].get(code)
        if not pos:
            return {"success": False, "msg": f"未持有 {code}", "code": code}
        try:
            price = float(price)
        except (TypeError, ValueError):
            return {"success": False, "msg": "价格不是数字", "code": code}
        if not math.isfinite(price) or price <= 0:
            return {"success": False, "msg": "价格必须为正的有限数", "code": code}

        trade_day = _parse_date(trade_date) or date.today()
        if trade_date is not None and not is_trading_day(trade_day):
            return {"success": False, "msg": f"{trade_day} 不是交易日", "code": code}
        if not is_t1_available(pos.get("buy_date"), trade_day):
            return {"success": False, "msg": f"T+1 限制: {pos.get('buy_date')} 买入，最早 {pos.get('available_date')} 可卖",
                    "code": code}
        ok, msg = check_price_limit(price, prev_close, code, pos.get("name", ""))
        if not ok:
            return {"success": False, "msg": msg, "code": code}
        if at_limit_down(price, prev_close, code, pos.get("name", "")):
            return {"success": False, "msg": f"跌停价 {price:.2f} 通常卖不出", "code": code}

        if reason and "AI信号" in reason and "止损" not in reason and "止盈" not in reason:
            if not self.check_min_hold(code):
                return {"success": False,
                        "msg": f"持有仅 {self._held_minutes(code):.0f} 分钟，未到最短持有 {self.cfg['min_hold_minutes']} 分钟",
                        "code": code}

        try:
            shares = int(shares)
        except (TypeError, ValueError):
            return {"success": False, "msg": "股数不是整数", "code": code}
        if shares == -1 or shares > int(pos["shares"]):
            shares = int(pos["shares"])
        if shares <= 0:
            return {"success": False, "msg": "股数必须为正", "code": code}

        amount = price * shares
        commission = self._commission(amount)
        stamp_tax = self._stamp_tax(amount)
        transfer_fee = self._transfer_fee(amount)
        net = amount - commission - stamp_tax - transfer_fee

        self.state["cash"] = round(self.state["cash"] + net, 2)
        if shares >= int(pos["shares"]):
            del self.state["positions"][code]
        else:
            pos["shares"] = int(pos["shares"]) - shares
            pos["last_price"] = price

        order = {
            "id": self.state["next_order_id"],
            "time": f"{trade_day.isoformat()}T{datetime.now():%H:%M:%S}",
            "action": "SELL",
            "code": code,
            "name": pos.get("name", code),
            "price": price,
            "shares": shares,
            "amount": amount,
            "commission": commission,
            "transfer_fee": transfer_fee,
            "stamp_tax": stamp_tax,
            "reason": reason,
        }
        self.state["next_order_id"] += 1
        self.state["orders"].append(order)
        self.save()
        return {"success": True, "msg": f"已卖出 {code} {shares}股 @ {price:.2f}", "order": order}

    # ---------- 持仓 ----------
    def update_prices(self, prices: dict, save: bool = True):
        for code, pos in self.state["positions"].items():
            if code in prices and prices[code]:
                try:
                    value = float(prices[code])
                except (TypeError, ValueError):
                    continue
                if math.isfinite(value) and value > 0:
                    pos["last_price"] = value
        if save:
            self.save()

    def check_stop_loss_take_profit(self, prices: dict) -> list[dict]:
        signals = []
        for code, pos in list(self.state["positions"].items()):
            try:
                price = float(prices.get(code, 0))
            except (TypeError, ValueError):
                continue
            if not math.isfinite(price) or price <= 0:
                continue
            avg = float(pos.get("avg_cost", 0))
            if avg <= 0:
                continue
            pnl_pct = (price - avg) / avg * 100
            stop = pos.get("stop_loss") or self.cfg["stop_loss_pct"]
            take = pos.get("take_profit") or self.cfg["take_profit_pct"]
            if pnl_pct <= float(stop):
                signals.append({"code": code, "action": "SELL", "shares": -1,
                                "price": price,
                                "reason": f"止损触发: 盈亏 {pnl_pct:+.1f}% (阈值 {float(stop):.1f}%)"})
            elif pnl_pct >= float(take):
                signals.append({"code": code, "action": "SELL", "shares": -1,
                                "price": price,
                                "reason": f"止盈触发: 盈亏 {pnl_pct:+.1f}% (阈值 {float(take):.1f}%)"})
        return signals

    # ---------- 统计 ----------
    def snapshot(self, date_str: Optional[str] = None):
        day = date_str or date.today().isoformat()
        self.update_halt(today=day, save=False)
        record = {
            "date": day,
            "total_assets": round(self.total_assets, 2),
            "cash": round(self.state["cash"], 2),
            "market_value": round(self.market_value, 2),
        }
        for index, snap in enumerate(self.state["daily_net_values"]):
            if snap.get("date") == day:
                self.state["daily_net_values"][index] = record
                break
        else:
            self.state["daily_net_values"].append(record)
        self.state["daily_net_values"].sort(key=lambda item: item.get("date", ""))
        self.save()
        return record

    def log_signal(self, code: str, name: str, signal: str, confidence: float, reason: str,
                   executed: bool, note: str = "", hallucination_risk: bool = False,
                   skip_reason: str = "", price: Optional[float] = None):
        self.state["signal_log"].append({
            "time": _now_iso(),
            "code": code,
            "name": name,
            "signal": signal,
            "confidence": confidence,
            "reason": reason,
            "executed": bool(executed),
            "skip_reason": skip_reason,
            "note": note,
            "price": price,
            "hallucination_risk": bool(hallucination_risk),
        })
        if len(self.state["signal_log"]) > MAX_SIGNAL_LOG:
            self.state["signal_log"] = self.state["signal_log"][-MAX_SIGNAL_LOG:]
        self.save()

    def add_ai_usage(self, calls: int = 0, errors: int = 0, rule_skipped: int = 0,
                     cost_cny: float = 0.0):
        usage = self.state.setdefault("ai_usage", {"calls": 0, "errors": 0, "rule_skipped": 0, "cost_cny": 0.0})
        usage["calls"] = int(usage.get("calls", 0)) + int(calls)
        usage["errors"] = int(usage.get("errors", 0)) + int(errors)
        usage["rule_skipped"] = int(usage.get("rule_skipped", 0)) + int(rule_skipped)
        usage["cost_cny"] = round(float(usage.get("cost_cny", 0.0)) + float(cost_cny), 4)
        self.save()

    @staticmethod
    def _realized_pnl_from_orders(orders: list) -> float:
        """FIFO 配对；未匹配的卖出（如手工导入）不计为利润。"""
        lots: Dict[str, List[List[float]]] = {}
        realized = 0.0
        for order in orders:
            code = order.get("code")
            action = order.get("action")
            shares = int(order.get("shares") or 0)
            if not code or shares <= 0:
                continue
            amount = float(order.get("amount") or 0)
            if action == "BUY":
                fee = float(order.get("commission") or 0) + float(order.get("transfer_fee") or 0)
                cost_per_share = (amount + fee) / shares
                lots.setdefault(code, []).append([shares, cost_per_share])
            elif action == "SELL":
                fee = (float(order.get("commission") or 0) + float(order.get("stamp_tax") or 0)
                       + float(order.get("transfer_fee") or 0))
                net_per_share = (amount - fee) / shares
                queue = lots.get(code, [])
                remaining = shares
                cost_basis = 0.0
                matched = 0
                while remaining > 0 and queue:
                    lot = queue[0]
                    take = min(int(lot[0]), remaining)
                    cost_basis += take * float(lot[1])
                    lot[0] -= take
                    remaining -= take
                    matched += take
                    if lot[0] <= 0:
                        queue.pop(0)
                if matched:
                    realized += net_per_share * matched - cost_basis
        return realized

    def summary(self) -> dict:
        orders = self.state["orders"]
        realized = self._realized_pnl_from_orders(orders)
        unrealized = 0.0
        for pos in self.state["positions"].values():
            last = float(pos.get("last_price", pos.get("avg_cost", 0)))
            avg = float(pos.get("avg_cost", 0))
            unrealized += (last - avg) * int(pos["shares"])
        total_fee = sum(float(o.get("commission") or 0) + float(o.get("stamp_tax") or 0)
                        + float(o.get("transfer_fee") or 0) for o in orders)
        positions = []
        for code, pos in self.state["positions"].items():
            avg = float(pos.get("avg_cost", 0))
            last = float(pos.get("last_price", avg))
            positions.append({
                "code": code, "name": pos.get("name", code), "shares": int(pos["shares"]),
                "avg_cost": round(avg, 3), "last_price": round(last, 3),
                "盈亏%": round((last - avg) / avg * 100, 2) if avg else 0.0,
                "buy_date": pos.get("buy_date"), "available_date": pos.get("available_date"),
                "stop_loss": pos.get("stop_loss"), "take_profit": pos.get("take_profit"),
            })
        return {
            "初始资金": round(float(self.state["initial_capital"]), 2),
            "当前总资产": round(self.total_assets, 2),
            "总收益率%": round(self.get_total_return_pct(), 2),
            "现金": round(float(self.state["cash"]), 2),
            "持仓市值": round(self.market_value, 2),
            "持仓数量": len(self.state["positions"]),
            "持仓列表": positions,
            "总成交笔数": len(orders),
            "已平仓盈亏": round(realized, 2),
            "浮动盈亏": round(unrealized, 2),
            "累计手续费": round(total_fee, 2),
            "熔断状态": "熔断中" if self.state.get("halted") else "正常",
            "熔断原因": self.state.get("halt_reason", ""),
        }


if __name__ == "__main__":
    sim = Simulator()
    print(json.dumps(sim.summary(), ensure_ascii=False, indent=2))
