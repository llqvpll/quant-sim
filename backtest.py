# -*- coding: utf-8 -*-
"""
策略回测引擎
============
用历史K线数据回测 AI 交易策略，快速验证能否稳定盈利。
每天(每个交易日)用当时的K线让 AI 分析 -> 按信号模拟买卖 -> 统计净值曲线。

关键: 回测用的AI分析必须"只看当时已有数据"，避免未来函数。
实现方式: 逐日滚动，通过 tweaks 的 as_of 参数把流程内 MarketData 截断到当天，只用当日及之前的K线喂给AI。

用法:
    python backtest.py --symbol 600519 --days 60
    python backtest.py --symbol 600519 --days 60 --symbols 000001,300750
"""

import argparse
import json
import os
import sys
import time
from datetime import datetime

from simulator import Simulator
from ai_signal import AIAnalyzer
from market_data import MarketData

# 回测参数
DEFAULT_DAYS = 60        # 回测多少个交易日
INITIAL_CAPITAL = None   # None = 用模拟器默认资金(5万, 与实盘一致); 需要时传具体数字
START_BUDGET_RATIO = 0.15  # 每次买入用总资产15%


class Backtest:
    """历史回测"""

    def __init__(self, symbols: list, days: int = DEFAULT_DAYS):
        self.symbols = symbols
        self.days = days
        self.md = MarketData()
        self.ai = AIAnalyzer()
        self.results = {}

    def run(self) -> dict:
        """对每只股票单独回测(模拟账户独立)"""
        for symbol in self.symbols:
            print(f"\n===== 回测 {symbol} | {self.days} 个交易日 =====")
            try:
                result = self._backtest_one(symbol)
                self.results[symbol] = result
                self._print_result(symbol, result)
            except Exception as e:
                print(f"  回测失败: {type(e).__name__}: {e}")
        return self.results

    def _backtest_one(self, symbol: str) -> dict:
        """回测单只股票"""
        # 拉历史K线(天数+缓冲)
        klines = self.md.get_kline(symbol, "1d", self.days + 30)
        if len(klines) < 30:
            return {"error": f"K线不足: {len(klines)} 根", "trades": [], "net_values": []}

        sim = Simulator(state_file=os.path.join(
            r"C:\Users\10716\Desktop\agent\quant-sim\data", f"backtest_{symbol}.json"),
            capital=INITIAL_CAPITAL)
        # 重置
        if os.path.exists(sim.state_file):
            os.remove(sim.state_file)
            sim = Simulator(state_file=sim.state_file, capital=INITIAL_CAPITAL)
        initial_capital = sim.state["initial_capital"]   # 口径以此为准, 避免和模拟器默认资金打架

        # 逐日回测: 从第 30 根K线开始(保证指标有足够数据)
        trades = []
        net_values = []
        start_idx = 30
        for i in range(start_idx, len(klines)):
            k = klines[i]
            date = k["date"]
            price = k["close"]
            # 当天收盘价 = 可用价格

            # 让 AI 只用"截止当天"的数据分析: tweaks 里的 as_of 会让流程内 MarketData
            # 丢弃该日期之后的K线, 因此不会再有未来函数(之前那版回测是偷看今天的行情)。
            # 简化方案: 每隔 5 天调用一次 AI 决策(减少 API 调用), 用当天收盘价判断。
            if i % 5 == 0 or i == start_idx:
                question = f"今天是{date}，现在适合买入吗？请给出明确建议（买入/持有/观望/卖出）。"
                result = self.ai.analyze_stock(symbol, question, as_of=date)
                signal = result.get("signal", "wait")
                conf = result.get("confidence", 0)
            else:
                signal = "wait"

            # 执行信号
            if signal == "buy" and symbol not in sim.state["positions"]:
                budget = sim.total_assets * START_BUDGET_RATIO
                shares = int(budget / price / 100) * 100
                if shares >= 100:
                    r = sim.buy(symbol, symbol, price, shares, reason=f"回测买入 {date}")
                    if r["success"]:
                        trades.append({"date": date, "action": "BUY", "price": price,
                                       "shares": shares, "signal": signal, "conf": conf})
            elif signal == "sell" and symbol in sim.state["positions"]:
                r = sim.sell(symbol, price, -1, reason=f"回测卖出 {date}")
                if r["success"]:
                    trades.append({"date": date, "action": "SELL", "price": price,
                                   "shares": "全部", "signal": signal, "conf": conf})

            # 更新价格
            sim.update_prices({symbol: price})
            # 止损止盈
            stop = sim.check_stop_loss_take_profit({symbol: price})
            for s in stop:
                r = sim.sell(symbol, price, -1, reason=f"回测{s['reason']} {date}")
                if r["success"]:
                    trades.append({"date": date, "action": "SELL", "price": price,
                                   "shares": "全部", "signal": "stop", "conf": 1.0})

            # 净值快照
            net_values.append({"date": date, "total_assets": round(sim.total_assets, 2),
                               "cash": round(sim.state["cash"], 2),
                               "market_value": round(sim.market_value, 2)})

        # 最终清仓
        if sim.state["positions"]:
            price = klines[-1]["close"]
            sim.sell(symbol, price, -1, reason="回测结束清仓")

        summary = sim.summary()
        # 计算回测指标
        returns = [nv["total_assets"] for nv in net_values]
        total_return = (returns[-1] - initial_capital) / initial_capital * 100 if returns else 0
        # 最大回撤
        max_drawdown = 0
        peak = initial_capital
        for v in returns:
            peak = max(peak, v)
            dd = (peak - v) / peak * 100 if peak else 0
            max_drawdown = max(max_drawdown, dd)

        return {
            "total_return_pct": round(total_return, 2),
            "max_drawdown_pct": round(max_drawdown, 2),
            "trades": trades,
            "net_values": net_values,
            "final_assets": returns[-1] if returns else initial_capital,
            "initial": initial_capital,
        }

    def _print_result(self, symbol: str, r: dict):
        if "error" in r:
            print(f"  {symbol}: {r['error']}")
            return
        print(f"  {symbol}: 收益率 {r['total_return_pct']}% | 最大回撤 {r['max_drawdown_pct']}% | "
              f"交易 {len(r['trades'])} 笔 | 期末资产 {r['final_assets']:.0f}")
        for t in r["trades"][-5:]:
            print(f"    [{t['date']}] {t['action']} @ {t['price']:.2f} ({t['signal']})")


def main():
    parser = argparse.ArgumentParser(description="AI策略回测")
    parser.add_argument("--symbol", default="600519", help="回测股票代码")
    parser.add_argument("--symbols", default="", help="多个代码用逗号分隔")
    parser.add_argument("--days", type=int, default=DEFAULT_DAYS, help="回测交易日数")
    args = parser.parse_args()

    symbols = [s.strip() for s in args.symbols.split(",") if s.strip()] if args.symbols else [args.symbol]
    bt = Backtest(symbols, args.days)
    results = bt.run()

    # 汇总
    print("\n===== 回测汇总 =====")
    for sym, r in results.items():
        if "error" not in r:
            print(f"{sym}: 收益 {r['total_return_pct']}% | 回撤 {r['max_drawdown_pct']}% | 交易 {len(r['trades'])} 笔")


if __name__ == "__main__":
    main()
