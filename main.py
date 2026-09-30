# -*- coding: utf-8 -*-
"""
AI 模拟交易主循环
=================
把 AI 信号 + 模拟交易引擎 + 行情数据源串起来：
1. 盘中多次扫描全市场候选股票
2. 对候选股票调用 AI 分析，获取买卖信号
3. 按信号 + 风控规则执行模拟买卖
4. 记录净值、信号、持仓，生成成绩单

用法:
    python main.py run --once          # 单次扫描(立即执行)
    python main.py run --loop          # 盘中循环(按设定间隔)
    python main.py run --scan N        # 扫描前 N 只涨幅居前股票
    python main.py summary             # 查看当前成绩单
"""

import argparse
import json
import os
import random
import subprocess
import sys
import time
from datetime import datetime

from simulator import Simulator
from ai_signal import AIAnalyzer
from market_data import MarketData
from technical import TechnicalAnalyzer
from news import NewsAnalyzer
from learning import TradingLearner

# 心跳文件: 每次扫描后写入当前时间, 看板据此判断机器人是否在运行
BASE_DIR = os.path.dirname(os.path.abspath(__file__))
HEARTBEAT_FILE = os.path.join(BASE_DIR, "data", "heartbeat.json")

# 扫描参数
SCAN_TOP_N = 10          # 每次扫描前 N 只候选股票
AI_ANALYZE_N = 5         # 规则预筛选后, 真正调 AI 分析的前 N 只(省 API 成本)
RULE_SCORE_MIN = 20      # 规则打分低于此值的候选不调 AI(明显弱势/超买直接跳过)
SCAN_INTERVAL = 300      # 盘中扫描间隔(秒), 默认5分钟一次
MIN_PRICE = 5.0          # 最低股价(排除仙股)
MAX_PRICE = 500.0        # 最高股价
EXCLUDE_ST = True        # 排除 ST 股
EXCLUDE_NEW = True       # 排除新股(名字含 N/C)
SIGNAL_COOLDOWN_MIN = 30 # 同一只股票两次AI分析的最小间隔(分钟), 省API调用
MIN_TRADE_AMOUNT = 3000  # 单笔买入最低金额(元): 低于此金额, 手续费占比过高不划算
MAX_ROUND_TRIP_COST_PCT = 1.5  # 单笔往返手续费占比上限(%): 高于此值(低价股)过滤
CONFIDENCE_BUY_THRESHOLD = 0.65  # 置信度低于此值不买入(只观望)
CONFIDENCE_BASE_RATIO = 0.15     # 基准仓位: 置信度0.65对应15%
CONFIDENCE_MAX_RATIO = 0.25      # 满置信仓位上限25%

# ---- 小资金可买性过滤 ----
MAX_ONE_LOT_RATIO = 0.30   # 一手(100股)金额占总资产比上限: 高于此值(买一手就超单票仓位)剔除
MIN_ONE_LOT_AMOUNT = 2000  # 一手金额下限(元): 低于此金额手续费占比过高(已被 MAX_ROUND_TRIP_COST_PCT 兜底)


class QuantBot:
    """AI 量化交易机器人"""

    def __init__(self, capital: float = None):
        self.sim = Simulator(capital=capital)
        self.ai = AIAnalyzer()
        self.md = MarketData()
        self.ta = TechnicalAnalyzer()
        self.na = NewsAnalyzer()
        self.learner = TradingLearner()
        self._signal_cache = {}   # {code: {"signal", "confidence", "reason", "ts"}}
        self._experience_txt = ""  # 历史经验摘要(每轮扫描前更新)

    # ---------- 信号记忆(冷却) ----------
    def _cache_signal(self, code, signal, confidence, reason):
        self._signal_cache[code] = {
            "signal": signal, "confidence": confidence, "reason": reason,
            "ts": time.time(),
        }

    def _last_signal(self, code):
        return self._signal_cache.get(code)

    # ---------- 候选股票 ----------
    def get_candidates(self, top_n: int = SCAN_TOP_N) -> list:
        """获取候选股票: 全市场涨幅居前 + 过滤条件(含小资金可买性过滤)

        小资金核心过滤: 只保留"买得起一手"的股票, 避免 AI 分析买不起的标的:
        1. 一手金额 > 总资产(现金都不够)  -> 剔除(买不起)
        2. 一手金额 / 总资产 > MAX_ONE_LOT_RATIO  -> 剔除(买一手就超单票仓位, 无法配仓)
        3. 一手往返手续费占比 > MAX_ROUND_TRIP_COST_PCT  -> 剔除(手续费不划算)
        """
        all_stocks = self.md.get_all_a_stocks()
        if not all_stocks:
            return []
        # 批量获取行情, 过滤
        codes = [s["code"] for s in all_stocks]
        quotes = self.md.get_realtime_batch(codes)
        total = self.sim.total_assets
        candidates = []
        for s in all_stocks:
            q = quotes.get(s["code"])
            if not q or q.get("price", 0) <= 0:
                continue
            price = q["price"]
            if price < MIN_PRICE or price > MAX_PRICE:
                continue
            if EXCLUDE_ST and ("ST" in s["name"].upper() or "st" in s["name"]):
                continue
            if EXCLUDE_NEW and s["name"].startswith(("N", "C")):
                continue
            # ---- 小资金可买性过滤 ----
            one_lot = price * 100                       # 一手金额
            if one_lot > total:                          # 现金都不够买一手
                continue
            if one_lot / total > MAX_ONE_LOT_RATIO:      # 买一手就超单票仓位上限
                continue
            rt_cost = self.sim._round_trip_cost_pct(price)   # 往返手续费占比
            if rt_cost > MAX_ROUND_TRIP_COST_PCT:        # 手续费不划算(低价股)
                continue
            candidates.append({
                "code": s["code"], "name": s["name"],
                "price": price, "change_pct": q.get("change_pct", 0),
                "round_trip_cost_pct": round(rt_cost, 2),
                "one_lot": round(one_lot, 0),
            })
        # 按涨幅排序(也可改为按成交量/换手等)
        candidates.sort(key=lambda x: x["change_pct"], reverse=True)
        return candidates[:top_n]

    # ---------- 扫描一轮 ----------
    def scan_once(self, top_n: int = SCAN_TOP_N) -> dict:
        """执行一轮扫描: 候选 -> AI分析 -> 执行信号"""
        report = {
            "time": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
            "scanned": 0, "buy": 0, "sell": 0, "wait": 0, "error": 0,
            "actions": [], "skipped": [],
            "ai_calls": 0, "rule_skipped": 0,   # AI 调用数 / 规则筛掉的数(成本统计)
        }

        candidates = self.get_candidates(top_n)
        # 每轮扫描前: 学习已平仓交易, 更新经验摘要(随着交易次数增多, AI 越来越了解自己的判断准不准)
        try:
            self.learner.learn_from_trades(self.sim.state)
            self._experience_txt = self.learner.experience_summary()
        except Exception:
            self._experience_txt = ""
        # 补齐存量持仓: 不在涨幅榜的持仓股, 取浮亏最弱 1 只让 AI 审视(否则永远不被分析)
        scan_list = list(candidates)
        held = self.sim.state["positions"]
        held_codes = set(held.keys())
        missing = [c for c in held_codes if c not in {x["code"] for x in scan_list}]
        if missing:
            # 按浮亏排序, 最弱优先
            def pnl(code):
                p = held[code]
                return (p.get("last_price", p["avg_cost"]) - p["avg_cost"]) / p["avg_cost"] * 100
            missing.sort(key=pnl)
            weakest = missing[0]
            q = self.md.get_realtime(weakest)
            if q.get("price", 0) > 0:
                scan_list.append({
                    "code": weakest, "name": q.get("name", held[weakest].get("name", weakest)),
                    "price": q["price"], "change_pct": q.get("change_pct", 0),
                    "round_trip_cost_pct": 0, "one_lot": 0, "_held": True,
                })
        report["scanned"] = len(scan_list)
        print(f"\n=== 扫描开始 {report['time']} | 候选 {len(scan_list)} 只 ===")

        # ========== 免费规则预筛选(省 AI 成本) ==========
        # 用本地技术指标打分(零成本), 只让得分高/持仓股进入 AI 分析
        held_codes2 = set(self.sim.state["positions"].keys())
        scored, skipped_rules = [], []
        for c in scan_list:
            if c["code"] in held_codes2:
                # 持仓股必须 AI 审视(决定买卖), 不筛选
                c["_rule_score"] = 999
                c["_rule_grade"] = "持仓"
                scored.append(c)
                continue
            try:
                rs = self.ta.rule_score(c["code"], c.get("change_pct"))
                c["_rule_score"] = rs["score"]
                c["_rule_grade"] = rs["grade"]
                c["_rule_reasons"] = " ".join(rs["reasons"])[:80]
                if rs["score"] >= RULE_SCORE_MIN:
                    scored.append(c)
                else:
                    skipped_rules.append(c)
                    report["skipped"].append({"code": c["code"], "name": c["name"],
                                              "reason": f"规则筛选{rs['grade']}({rs['score']}分)未达阈值"})
            except Exception:
                scored.append(c)  # 打分失败不阻塞, 交给 AI
        # 非持仓按规则分排序, 只保留前 AI_ANALYZE_N 只
        non_held = [c for c in scored if c["code"] not in held_codes2]
        non_held.sort(key=lambda x: x["_rule_score"], reverse=True)
        held_only = [c for c in scored if c["code"] in held_codes2]
        ai_list = held_only + non_held[:AI_ANALYZE_N]
        rule_skipped = len(scan_list) - len(ai_list) - len(skipped_rules)
        report["rule_skipped"] = len(skipped_rules) + rule_skipped
        if skipped_rules or rule_skipped:
            print(f"  规则预筛选: {len(scan_list)} -> {len(ai_list)} 只进 AI (省 {report['rule_skipped']} 次调用)")
        for c in ai_list:
            print(f"  [规则{c.get('_rule_grade','?')} {c.get('_rule_score','?')}分] {c['name']}({c['code']})")
        scan_list = ai_list

        for c in scan_list:
            code, name, price = c["code"], c["name"], c["price"]
            # 1. 已持有的股票: 分析是否要卖/持有
            if code in self.sim.state["positions"]:
                question = "我已持有该股票，请分析是继续持有还是卖出？"
            else:
                question = "现在适合买入吗？请给出明确的操作建议（买入/持有/观望/卖出）。"

            # 技术指标输入(供 AI 技术面判断; 持仓股也计算, 帮助判断卖出时机)
            tech_txt = ""
            ind = self.ta.get_indicators(code)
            tech_txt = ind.get("signal_txt", "") if ind else ""

            # 账户上下文(小资金 + 手续费)
            sim_cfg = self.sim.state["config"]
            account_ctx = (
                f"模拟账户总资产 {self.sim.total_assets:.0f} 元(小资金), 佣金万三最低5元, 卖出印花税千一, "
                f"持仓上限 {sim_cfg['max_positions']} 只, 单票仓位上限 {sim_cfg['max_single_position_ratio']*100:.0f}%, "
                f"止损 {sim_cfg['stop_loss_pct']}% 止盈 {sim_cfg['take_profit_pct']}%。建议优先考虑低价股(一手成本低)与低换手标的。"
            )

            # 信号记忆: 同一股票冷却期内不重复调用 AI, 沿用上次信号
            last_sig = self._last_signal(code)
            if last_sig and (time.time() - last_sig["ts"]) < SIGNAL_COOLDOWN_MIN * 60:
                signal = last_sig["signal"]
                conf = last_sig["confidence"]
                result = {"signal": signal, "confidence": conf, "reason": last_sig["reason"]}
                print(f"[冷却跳过AI] {name}({code}) 沿用上次信号: {signal} ({(time.time()-last_sig['ts'])/60:.0f}分钟前)")
            else:
                print(f"\n[AI分析] {name}({code}) @ {price:.2f} ...")
                # 规则预筛选结果也带给 AI 参考(低成本前置判断)
                rule_ctx = ""
                if c.get("_rule_score", 0) < 999:
                    rule_ctx = f"【规则预筛】技术规则打分 {c.get('_rule_score', 0)} 分({c.get('_rule_grade', '?')}), 依据: {c.get('_rule_reasons', '')}"
                # 抓取消息面(最新新闻 + 情感打分)
                news_txt = ""
                try:
                    news_info = self.na.get_news_summary(code)
                    news_txt = news_info.get("txt", "")
                except Exception:
                    news_txt = ""
                question_full = question
                if rule_ctx:
                    question_full += "\n" + rule_ctx
                result = self.ai.analyze_stock(code, question_full, technical=tech_txt, extra=account_ctx,
                                               news=news_txt, experience=self._experience_txt,
                                               verify_price=price, expected_name=name)
                report["ai_calls"] += 1
                signal = result.get("signal", "error")
                conf = result.get("confidence", 0)
                self._cache_signal(code, signal, conf, result.get("reason", ""))
                # 防交易幻觉: 若信号被判定为幻觉风险, 打印醒目警告(信号已被降级/拦截)
                if result.get("hallucination_risk"):
                    print(f"  ⚠️ 幻觉风险 [{name}({code})]: {result.get('verify_notes', '')}")

            if signal == "error":
                report["error"] += 1
                report["skipped"].append({"code": code, "name": name, "reason": result.get("reason", "AI错误")})
                continue

            # 2. 执行信号
            action = None
            if signal == "buy" and code not in self.sim.state["positions"]:
                # 置信度门槛: 低于阈值只观望(宁可错过不追高)
                if conf < CONFIDENCE_BUY_THRESHOLD:
                    report["wait"] += 1
                    report["skipped"].append({"code": code, "name": name, "reason": f"置信度 {conf:.0%} 低于买入门槛 {CONFIDENCE_BUY_THRESHOLD:.0%}"})
                else:
                    # 动态仓位: 置信度越高仓位越大 (0.65→15%, 0.9→~22%, 1.0→25%)
                    ratio = CONFIDENCE_BASE_RATIO + (conf - CONFIDENCE_BUY_THRESHOLD) * \
                            (CONFIDENCE_MAX_RATIO - CONFIDENCE_BASE_RATIO) / (1.0 - CONFIDENCE_BUY_THRESHOLD)
                    ratio = min(ratio, CONFIDENCE_MAX_RATIO)
                    budget = self.sim.total_assets * ratio
                    shares = int(budget / price / 100) * 100
                    # 单笔最低金额: 过滤"买不起一手/手续费占比过高"的标的
                    if shares < 100:
                        report["skipped"].append({"code": code, "name": name,
                                                  "reason": f"预算 {budget:.0f} 元买不起一手({price:.2f}×100)"})
                        report["wait"] += 1
                    elif shares * price < MIN_TRADE_AMOUNT:
                        report["skipped"].append({"code": code, "name": name,
                                                  "reason": f"单笔金额 {shares*price:.0f} 元低于 {MIN_TRADE_AMOUNT} 元, 手续费不划算"})
                        report["wait"] += 1
                    else:
                        # 若超风控上限则自动缩量到上限(engine 返回建议)
                        r = self.sim.buy(code, name, price, shares, reason=f"AI信号: 买入(置信{conf:.0%})")
                        if not r["success"] and "超限" in r["msg"]:
                            # 缩量重试: 按单票上限
                            max_budget = self.sim.total_assets * CONFIDENCE_MAX_RATIO
                            shares2 = int(max_budget / price / 100) * 100
                            if shares2 >= 100:
                                r = self.sim.buy(code, name, price, shares2, reason=f"AI信号: 买入-缩量(置信{conf:.0%})")
                        action = r
                        report["actions"].append({"code": code, "name": name, "action": "BUY",
                                                  "shares": shares, "price": price, "msg": r["msg"]})
                        if r["success"]:
                            report["buy"] += 1
                        else:
                            report["skipped"].append({"code": code, "name": name, "reason": r["msg"]})
            elif signal == "sell" and code in self.sim.state["positions"]:
                r = self.sim.sell(code, price, -1, reason=f"AI信号: 卖出(置信{conf:.0%})")
                action = r
                report["actions"].append({"code": code, "name": name, "action": "SELL",
                                          "shares": "全部", "price": price, "msg": r["msg"]})
                if r["success"]:
                    report["sell"] += 1
            elif signal == "sell" and code not in self.sim.state["positions"]:
                report["skipped"].append({"code": code, "name": name, "reason": "AI建议卖出但未持仓"})
                report["wait"] += 1
            else:
                report["wait"] += 1
                report["skipped"].append({"code": code, "name": name, "reason": f"AI建议{signal}"})

            # 记录信号
            self.sim.log_signal(code, name, signal, conf,
                                result.get("reason", "")[:200],
                                executed=bool(action and action.get("success")),
                                hallucination_risk=result.get("hallucination_risk", False),
                                note=f"价格{price:.2f}" + (f"| 消息面:{news_txt.replace(chr(10), ' ')[:60]}" if news_txt else ""))

        # 3. 更新持仓价格 + 止损止盈
        pos_codes = list(self.sim.state["positions"].keys())
        if pos_codes:
            quotes = self.md.get_realtime_batch(pos_codes)
            prices = {c: q["price"] for c, q in quotes.items() if q.get("price")}
            self.sim.update_prices(prices)
            stop_signals = self.sim.check_stop_loss_take_profit(prices)
            for s in stop_signals:
                q = quotes.get(s["code"])
                r = self.sim.sell(s["code"], q["price"] if q else 0, -1, reason=s["reason"])
                report["actions"].append({"code": s["code"], "name": s["code"], "action": "SELL",
                                          "shares": "全部", "price": q["price"] if q else 0, "msg": r["msg"]})
                report["sell"] += 1

        # 4. 净值快照 + 心跳 (snapshot 内含熔断更新)
        self.sim.snapshot()
        self.sim.add_ai_usage(calls=report.get("ai_calls", 0),
                              rule_skipped=report.get("rule_skipped", 0))
        self._beat()
        if self.sim.state.get("halted"):
            report["halted"] = True
            print(f"⚠️ 熔断中: {self.sim.state.get('halt_reason', '')}")
        self._print_report(report)
        return report

    # ---------- 心跳 ----------
    def _beat(self):
        """写心跳文件, 看板据此显示机器人运行状态"""
        try:
            os.makedirs(os.path.dirname(HEARTBEAT_FILE), exist_ok=True)
            with open(HEARTBEAT_FILE, "w", encoding="utf-8") as f:
                json.dump({"last_beat": datetime.now().isoformat()}, f)
        except Exception:
            pass

    # ---------- 循环 ----------
    def run_loop(self, interval: int = SCAN_INTERVAL, top_n: int = SCAN_TOP_N):
        """盘中循环扫描"""
        cfg = self.sim.state["config"]
        print(f"AI 量化交易机器人启动 | 扫描间隔 {interval}s | 每次扫描 {top_n} 只 | 冷却 {SIGNAL_COOLDOWN_MIN}分钟")
        print(f"初始资金: {self.sim.state['initial_capital']:.0f} | 当前: {self.sim.total_assets:.2f}")
        print(f"小资金模式: 持仓≤{cfg['max_positions']}只 | 单票≤{cfg['max_single_position_ratio']*100:.0f}% | "
              f"止损{cfg['stop_loss_pct']}% 止盈{cfg['take_profit_pct']}% | 熔断{cfg['drawdown_halt_pct']}% | "
              f"最低单笔 {MIN_TRADE_AMOUNT}元 | 置信门槛 {CONFIDENCE_BUY_THRESHOLD:.0%}")
        print("按 Ctrl+C 停止")
        # 启动即写心跳: 看板立刻显示"运行中", 不用等首轮扫描(需1-2分钟)
        self._beat()
        while True:
            try:
                self.scan_once(top_n)
                print(f"\n--- 等待 {interval/60:.1f} 分钟后再次扫描 ---")
                # 扫描间隔可能长于心跳超时(120s), 等待期间定期写心跳, 避免看板误报"已停止"
                waited = 0
                while waited < interval:
                    time.sleep(min(60, interval - waited))
                    waited += min(60, interval - waited)
                    self._beat()
            except KeyboardInterrupt:
                print("\n已停止")
                break
            except Exception as e:
                print(f"\n[错误] {type(e).__name__}: {e}")
                time.sleep(60)

    # ---------- 输出 ----------
    def _print_report(self, report: dict):
        print(f"\n=== 扫描完成 | 扫描 {report['scanned']} | 买 {report['buy']} | 卖 {report['sell']} | 观望 {report['wait']} ===")
        print(f"  成本: AI 调用 {report.get('ai_calls', 0)} 次 | 规则预筛跳过 {report.get('rule_skipped', 0)} 只(免费)")
        s = self.sim.summary()
        print(f"总资产: {s['当前总资产']:.2f} | 收益率: {s['总收益率%']}% | 持仓 {s['持仓数量']} 只")
        for a in report["actions"]:
            print(f"  [{a['action']}] {a['name']}({a['code']}) {a.get('shares', '')}股 @ {a['price']:.2f} | {a['msg']}")

    def show_summary(self):
        """显示当前成绩单"""
        s = self.sim.summary()
        print("=" * 50)
        print("AI 模拟交易成绩单")
        print("=" * 50)
        for k, v in s.items():
            if k == "持仓列表":
                if v:
                    print(f"\n持仓:")
                    for p in v:
                        print(f"  {p['name']}({p['code']}) {p['shares']}股 成本{p['avg_cost']:.2f} 现价{p['last_price']:.2f} 盈亏{p['盈亏%']}%")
                else:
                    print("持仓: 空仓")
            else:
                print(f"{k}: {v}")
        print("=" * 50)


def _pid_alive(pid: int) -> bool:
    """跨平台检查进程是否存活。Windows 的 os.kill(pid,0) 会抛 WinError87, 须用 tasklist"""
    if os.name == "nt":
        try:
            r = subprocess.run(["tasklist", "/FI", f"PID eq {pid}"],
                               capture_output=True, timeout=10)
            # 直接判断 bytes, 避免 tasklist 中文输出(GBK)解码问题
            return str(pid).encode() in r.stdout
        except Exception:
            return False
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def main():
    parser = argparse.ArgumentParser(description="AI 模拟量化交易")
    parser.add_argument("mode", choices=["run", "summary"], default="run", nargs="?")
    parser.add_argument("--once", action="store_true", help="只扫描一次")
    parser.add_argument("--loop", action="store_true", help="盘中循环扫描")
    parser.add_argument("--scan", type=int, default=SCAN_TOP_N, help="每次扫描候选数")
    parser.add_argument("--interval", type=int, default=SCAN_INTERVAL, help="循环间隔秒数")
    parser.add_argument("--capital", type=float, default=None, help="初始资金(默认 5 万, 仅新账户生效)")
    parser.add_argument("--reset", action="store_true", help="重置模拟账户(删除现有持仓与记录)")
    args = parser.parse_args()

    if args.reset:
        Simulator.reset()

    # PID 锁: 防止多个机器人实例同时跑(会互相覆盖 state.json, 曾导致100万幽灵账户问题)
    if args.mode == "run" and (args.loop or not args.once):
        lock_file = os.path.join(BASE_DIR, "data", "bot.lock")
        os.makedirs(os.path.dirname(lock_file), exist_ok=True)
        if os.path.exists(lock_file):
            try:
                with open(lock_file) as f:
                    old_pid = int(f.read().strip())
                if _pid_alive(old_pid):
                    print(f"⚠️ 已有机器人实例在运行 (PID {old_pid}), 本次退出。")
                    print("  如需重启, 请先结束旧实例: taskkill /F /PID", old_pid)
                    sys.exit(0)
            except ValueError:
                pass  # 锁文件损坏, 接管
        with open(lock_file, "w") as f:
            f.write(str(os.getpid()))

    bot = QuantBot(capital=args.capital)
    try:
        if args.mode == "summary":
            bot.show_summary()
        elif args.once:
            bot.scan_once(args.scan)
        elif args.loop:
            bot.run_loop(args.interval, args.scan)
        else:
            # 默认: 盘中判断, 交易时间则循环, 否则只跑一次
            now = datetime.now()
            is_trading_time = now.weekday() < 5 and (9 <= now.hour < 15)
            if is_trading_time:
                print("当前为交易时段, 进入盘中循环模式")
                bot.run_loop(args.interval, args.scan)
            else:
                print("当前非交易时段, 执行单次扫描")
                bot.scan_once(args.scan)
    finally:
        # 释放锁
        try:
            lock = os.path.join(BASE_DIR, "data", "bot.lock")
            if os.path.exists(lock):
                with open(lock) as f:
                    if f.read().strip() == str(os.getpid()):
                        os.remove(lock)
        except Exception:
            pass


if __name__ == "__main__":
    main()
