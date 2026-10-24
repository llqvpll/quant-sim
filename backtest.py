# -*- coding: utf-8 -*-
"""组合回测（v2）。

相对旧版：
- 收盘决策、次日开盘成交，杜绝“用当天收盘价决策再按收盘价成交”
- 多标的共享一个账户，遵守持仓数/单票仓位/现金约束
- 通过 Simulator 强制执行 T+1、涨跌停、板块手数、费用
- 加等权买入持有基准、年化、最大回撤、夏普、胜率
- 结果写入 data/backtest_result.json
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import statistics
from typing import Dict, List, Optional

from ai_signal import AIAnalyzer
from config import DEFAULT_CONFIG
from market_data import MarketData
from market_rules import normalize_buy_shares
from simulator import Simulator
from technical import TechnicalAnalyzer

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")


class Backtest:
    def __init__(self, symbols: List[str], days: int = 60, decision_every: int = 5,
                 capital: Optional[float] = None, ai: Optional[AIAnalyzer] = None,
                 market: Optional[MarketData] = None,
                 technical: Optional[TechnicalAnalyzer] = None,
                 slippage_bps: float = 5.0, max_volume_ratio: float = 0.05):
        self.symbols = [str(symbol).strip() for symbol in symbols if str(symbol).strip()]
        self.days = max(10, int(days))
        self.decision_every = max(1, int(decision_every))
        self.capital = capital
        self.slippage_bps = max(0.0, float(slippage_bps))
        self.max_volume_ratio = max(0.0, min(float(max_volume_ratio), 1.0))
        self.md = market or MarketData()
        self.ta = technical or TechnicalAnalyzer(self.md)
        self.ai = ai or AIAnalyzer()
        self.bars: Dict[str, Dict[str, dict]] = {}
        self.prev_close: Dict[str, Dict[str, float]] = {}
        self.dates: List[str] = []
        self.pending: List[dict] = []
        self.trades: List[dict] = []
        self.ai_calls = 0
        self.ai_errors = 0

    # ---------- 数据 ----------
    def _load(self) -> bool:
        limit = self.days + 120
        all_dates = set()
        for symbol in self.symbols:
            rows = self.md.get_kline(symbol, "1d", limit)
            if len(rows) < 40:
                print(f"[回测] {symbol} K线不足({len(rows)}根)，跳过")
                continue
            mapping = {row["date"]: row for row in rows}
            self.bars[symbol] = mapping
            all_dates.update(mapping)
            previous = {}
            last_close = None
            for row in rows:
                if last_close is not None:
                    previous[row["date"]] = last_close
                last_close = row["close"]
            self.prev_close[symbol] = previous
        self.dates = sorted(all_dates)
        if len(self.dates) < 45 or not self.bars:
            return False
        return True

    def _start_index(self) -> Optional[int]:
        counts = {symbol: 0 for symbol in self.bars}
        for index, day in enumerate(self.dates):
            for symbol in self.bars:
                if day in self.bars[symbol]:
                    counts[symbol] += 1
            if all(count >= 30 for count in counts.values()):
                return index
        return None

    # ---------- 交易 ----------
    def _confidence_ratio(self, confidence: float, cfg: dict) -> float:
        threshold = float(cfg["confidence_buy_threshold"])
        if confidence < threshold:
            return 0.0
        base = float(cfg["confidence_base_ratio"])
        maximum = float(cfg["confidence_max_ratio"])
        ratio = base + (confidence - threshold) * (maximum - base) / max(1.0 - threshold, 1e-9)
        return max(0.0, min(ratio, maximum))

    def _account_context(self, sim: Simulator) -> str:
        cfg = sim.cfg
        return (
            f"回测账户总资产 {sim.total_assets:.0f} 元，持仓上限 {cfg['max_positions']} 只，"
            f"单票上限 {cfg['max_single_position_ratio'] * 100:.0f}%，"
            f"佣金{cfg['commission_rate'] * 10000:.2f}‱（最低{cfg['min_commission']:.0f}元），"
            f"印花税{cfg['stamp_tax_rate'] * 100:.2f}%，T+1，涨跌停不可成交。"
        )

    def _queue(self, order: dict):
        if any(item["code"] == order["code"] for item in self.pending):
            return
        self.pending.append(order)

    def _execute_pending(self, day: str, sim: Simulator):
        for order in self.pending:
            symbol = order["code"]
            bar = self.bars.get(symbol, {}).get(day)
            if not bar:
                # 当天停牌/无数据：撤单，避免用过期价格成交
                continue
            price = float(bar["open"])
            previous = self.prev_close.get(symbol, {}).get(day)
            if order["action"] == "BUY":
                price = price * (1 + self.slippage_bps / 10_000.0)
                ratio = float(order.get("ratio") or 0)
                budget = sim.total_assets * ratio
                shares = normalize_buy_shares(symbol, int(budget / price)) if price > 0 else 0
                # 成交量约束：单笔不超过当日成交量的 max_volume_ratio（腾讯 K 线量单位为手）
                if self.max_volume_ratio > 0 and bar.get("volume"):
                    volume_cap = int(float(bar["volume"]) * 100 * self.max_volume_ratio)
                    shares = min(shares, volume_cap // 100 * 100)
                if shares <= 0 or shares * price < float(sim.cfg["min_trade_amount"]):
                    continue
                result = sim.buy(symbol, order.get("name", symbol), price, shares,
                                 reason=order["reason"], trade_date=day, prev_close=previous,
                                 stop_loss=order.get("stop_loss"), take_profit=order.get("take_profit"))
            else:
                price = price * (1 - self.slippage_bps / 10_000.0)
                result = sim.sell(symbol, price, -1, reason=order["reason"],
                                  trade_date=day, prev_close=previous)
            if result["success"]:
                self.trades.append({
                    "date": day, "action": order["action"], "code": symbol,
                    "name": order.get("name", symbol), "price": price,
                    "shares": result.get("order", {}).get("shares"), "reason": order["reason"],
                })
            else:
                print(f"  [回测] {day} {order['action']} {symbol} 未成交: {result['msg']}")
            # 无论成功失败都撤单；失败（涨停/T+1/现金不足）不在未来偷偷补单
        self.pending = []

    def _mark(self, day: str, sim: Simulator):
        prices = {}
        for symbol in sim.state["positions"]:
            bar = self.bars.get(symbol, {}).get(day)
            if bar:
                prices[symbol] = bar["close"]
        if prices:
            sim.update_prices(prices, save=False)
            for signal in sim.check_stop_loss_take_profit(prices):
                self._queue({
                    "code": signal["code"], "action": "SELL",
                    "reason": f"回测{signal['reason']}", "name": signal["code"],
                })

    def _decide(self, day: str, sim: Simulator):
        for symbol in self.bars:
            bar = self.bars[symbol].get(day)
            if not bar:
                continue
            if any(item["code"] == symbol for item in self.pending):
                continue
            held = symbol in sim.state["positions"]
            question = ("我已持有该股票，请分析是继续持有还是卖出？" if held
                        else "现在适合买入吗？请给出明确的操作建议（买入/持有/观望/卖出）。")
            technical = ""
            try:
                indicators = self.ta.get_indicators(symbol, as_of=day)
                technical = indicators.get("signal_txt", "")
            except Exception:
                technical = ""
            result = self.ai.analyze_stock(
                symbol, question, technical=technical, extra=self._account_context(sim),
                as_of=day, verify_price=float(bar["close"]),
            )
            self.ai_calls += 1
            signal = result.get("signal", "error")
            if signal in ("error", "unknown"):
                self.ai_errors += 1
                continue
            confidence = float(result.get("confidence") or 0)
            if signal == "buy" and not held:
                if confidence < float(sim.cfg["confidence_buy_threshold"]):
                    continue
                ratio = self._confidence_ratio(confidence, sim.cfg)
                ai_pct = result.get("position_pct")
                if ai_pct is not None:
                    if float(ai_pct) <= 0:
                        continue
                    ratio = min(ratio, float(ai_pct) / 100.0)
                self._queue({
                    "code": symbol, "action": "BUY", "name": symbol,
                    "ratio": min(ratio, float(sim.cfg["confidence_max_ratio"])),
                    "reason": f"AI买入(置信{confidence:.0%})",
                    "stop_loss": result.get("stop_loss"), "take_profit": result.get("take_profit"),
                })
            elif signal == "sell" and held:
                self._queue({
                    "code": symbol, "action": "SELL", "name": symbol,
                    "reason": f"AI卖出(置信{confidence:.0%})",
                })

    # ---------- 指标 ----------
    @staticmethod
    def _max_drawdown(values: List[float]) -> float:
        peak = values[0] if values else 0
        worst = 0.0
        for value in values:
            peak = max(peak, value)
            if peak > 0:
                worst = max(worst, (peak - value) / peak * 100)
        return worst

    @staticmethod
    def _sharpe(values: List[float]) -> float:
        if len(values) < 3:
            return 0.0
        returns = [(values[i] / values[i - 1] - 1) for i in range(1, len(values)) if values[i - 1] > 0]
        if len(returns) < 2:
            return 0.0
        std = statistics.pstdev(returns)
        if std == 0:
            return 0.0
        return statistics.mean(returns) / std * math.sqrt(252)

    def _benchmark(self, start_day: str, end_day: str) -> float:
        returns = []
        for symbol, mapping in self.bars.items():
            start = mapping.get(start_day)
            if not start:
                start = next((mapping[d] for d in self.dates if d >= start_day and d in mapping), None)
                if start is None:
                    continue
            end = mapping.get(end_day)
            if not end:
                candidates = [mapping[d] for d in self.dates if d <= end_day and d in mapping]
                if not candidates:
                    continue
                end = candidates[-1]
            if start["close"] > 0:
                returns.append(end["close"] / start["close"] - 1)
        return statistics.mean(returns) * 100 if returns else 0.0

    def _closed_stats(self, orders: List[dict]) -> dict:
        lots: Dict[str, List[List[float]]] = {}
        pnls = []
        for order in orders:
            code = order.get("code")
            shares = int(order.get("shares") or 0)
            amount = float(order.get("amount") or 0)
            if not code or shares <= 0:
                continue
            if order.get("action") == "BUY":
                fee = float(order.get("commission") or 0) + float(order.get("transfer_fee") or 0)
                lots.setdefault(code, []).append([shares, (amount + fee) / shares])
            elif order.get("action") == "SELL":
                fee = (float(order.get("commission") or 0) + float(order.get("stamp_tax") or 0)
                       + float(order.get("transfer_fee") or 0))
                net = (amount - fee) / shares
                queue = lots.get(code, [])
                remaining = shares
                while remaining > 0 and queue:
                    lot = queue[0]
                    take = min(int(lot[0]), remaining)
                    if lot[1]:
                        pnls.append((net - lot[1]) / lot[1] * 100)
                    lot[0] -= take
                    remaining -= take
                    if lot[0] <= 0:
                        queue.pop(0)
        wins = [pnl for pnl in pnls if pnl > 0]
        return {
            "closed_trades": len(pnls),
            "win_rate": round(len(wins) / len(pnls) * 100, 1) if pnls else 0.0,
            "avg_trade_pct": round(statistics.mean(pnls), 2) if pnls else 0.0,
        }

    # ---------- 主流程 ----------
    def run(self) -> dict:
        if not self._load():
            return {"error": "K线数据不足或标的为空"}
        start = self._start_index()
        if start is None:
            return {"error": "无法找到所有标的同时具备 30 根 K 线的起点"}
        end = min(start + self.days, len(self.dates) - 1)
        days = self.dates[start:end + 1]
        if len(days) < 10:
            return {"error": "回测区间太短"}

        os.makedirs(DATA_DIR, exist_ok=True)
        slug = hashlib.md5(",".join(self.symbols).encode("utf-8")).hexdigest()[:8]
        state_file = os.path.join(DATA_DIR, f"backtest_{slug}.json")
        if os.path.exists(state_file):
            os.remove(state_file)
        sim = Simulator(state_file=state_file, capital=self.capital)

        print(f"[回测] {', '.join(self.symbols)} | {days[0]} ~ {days[-1]} | {len(days)} 个交易日 | "
              f"每 {self.decision_every} 日决策 | 初始 {sim.state['initial_capital']:.0f}")
        for index, day in enumerate(days):
            self._execute_pending(day, sim)
            self._mark(day, sim)
            if index % self.decision_every == 0:
                self._decide(day, sim)
            sim.snapshot(day)

        # 期末清仓（用最后一天收盘；T+1 不允许时保留市值）
        last_day = days[-1]
        for symbol in list(sim.state["positions"]):
            bar = self.bars.get(symbol, {}).get(last_day)
            if not bar:
                continue
            result = sim.sell(symbol, float(bar["close"]), -1, reason="回测结束清仓",
                              trade_date=last_day, prev_close=self.prev_close.get(symbol, {}).get(last_day))
            if result["success"]:
                self.trades.append({"date": last_day, "action": "SELL", "code": symbol,
                                    "name": symbol, "price": float(bar["close"]),
                                    "shares": result.get("order", {}).get("shares"), "reason": "期末清仓"})
        sim.snapshot(last_day)

        net_values = [item["total_assets"] for item in sim.state["daily_net_values"]]
        initial = float(sim.state["initial_capital"])
        final = float(net_values[-1]) if net_values else sim.total_assets
        total_return = (final - initial) / initial * 100 if initial else 0.0
        benchmark = self._benchmark(days[0], last_day)
        trading_days = max(len(net_values) - 1, 1)
        annualized = ((final / initial) ** (252 / trading_days) - 1) * 100 if initial > 0 and final > 0 else 0.0
        stats = self._closed_stats(sim.state["orders"])
        result = {
            "symbols": self.symbols,
            "start": days[0],
            "end": last_day,
            "trading_days": len(days),
            "initial": round(initial, 2),
            "final": round(final, 2),
            "total_return_pct": round(total_return, 2),
            "annualized_pct": round(annualized, 2),
            "benchmark_return_pct": round(benchmark, 2),
            "excess_return_pct": round(total_return - benchmark, 2),
            "max_drawdown_pct": round(self._max_drawdown(net_values), 2),
            "sharpe": round(self._sharpe(net_values), 2),
            "ai_calls": self.ai_calls,
            "ai_errors": self.ai_errors,
            "slippage_bps": self.slippage_bps,
            "max_volume_ratio": self.max_volume_ratio,
            "trades": self.trades,
            "net_values": sim.state["daily_net_values"],
            **stats,
            "summary": sim.summary(),
        }
        output = os.path.join(DATA_DIR, "backtest_result.json")
        with open(output, "w", encoding="utf-8") as fh:
            json.dump(result, fh, ensure_ascii=False, indent=2)
        result["output"] = output
        self._print(result)
        return result

    @staticmethod
    def _print(result: dict):
        print("\n===== 回测结果 =====")
        print(f"区间: {result['start']} ~ {result['end']} | 交易日 {result['trading_days']} | "
              f"AI 调用 {result['ai_calls']} 次(失败 {result['ai_errors']})")
        print(f"收益: {result['total_return_pct']}% | 年化 {result['annualized_pct']}% | "
              f"基准 {result['benchmark_return_pct']}% | 超额 {result['excess_return_pct']}%")
        print(f"最大回撤: {result['max_drawdown_pct']}% | 夏普 {result['sharpe']} | "
              f"平仓 {result['closed_trades']} 笔 | 胜率 {result['win_rate']}%")
        print(f"期末资产: {result['final']:.2f} | 结果文件: {result['output']}")


def main(argv: Optional[list] = None):
    parser = argparse.ArgumentParser(description="AI 组合回测")
    parser.add_argument("--symbol", default="600519", help="单只代码")
    parser.add_argument("--symbols", default="", help="多个代码，逗号分隔")
    parser.add_argument("--days", type=int, default=DEFAULT_CONFIG["backtest_days"], help="回测交易日数")
    parser.add_argument("--decision-every", type=int, default=DEFAULT_CONFIG["backtest_decision_every"],
                        help="每 N 个交易日让 AI 决策一次")
    parser.add_argument("--capital", type=float, default=None, help="初始资金")
    parser.add_argument("--slippage-bps", type=float, default=5.0, help="滑点（基点，默认 5）")
    parser.add_argument("--max-volume-ratio", type=float, default=0.05,
                        help="单笔最多占当日成交量的比例（默认 5%%）")
    args = parser.parse_args(argv)
    symbols = [item.strip() for item in args.symbols.split(",") if item.strip()] if args.symbols else [args.symbol]
    result = Backtest(symbols, days=args.days, decision_every=args.decision_every,
                      capital=args.capital, slippage_bps=args.slippage_bps,
                      max_volume_ratio=args.max_volume_ratio).run()
    if "error" in result:
        print(f"[回测] 失败: {result['error']}")
        raise SystemExit(1)


if __name__ == "__main__":
    main()
