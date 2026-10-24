# -*- coding: utf-8 -*-
"""AI 模拟交易主循环（v2）。

流程：候选股 -> 规则预筛 -> AI 分析 -> 风控执行 -> 净值/心跳。
关键修复：冷却崩溃、错误缓存、交易时段、T+1/涨跌停、配置单一来源、
AI 止损/仓位字段落地、心跳线程、扫描报告持久化。
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import threading
import time
import traceback
from datetime import datetime
from typing import Optional

from ai_signal import AIAnalyzer
from learning import TradingLearner
from market_data import MarketData
from market_rules import at_limit_up, is_st_name, min_buy_shares, normalize_buy_shares
from news import NewsAnalyzer
from simulator import Simulator
from technical import TechnicalAnalyzer

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
HEARTBEAT_FILE = os.path.join(DATA_DIR, "heartbeat.json")
LOCK_FILE = os.path.join(DATA_DIR, "bot.lock")


def log(message: str):
    print(f"[{datetime.now():%H:%M:%S}] {message}", flush=True)


class QuantBot:
    """AI 量化交易机器人；依赖可注入，方便测试。"""

    def __init__(self, capital: Optional[float] = None, sim: Optional[Simulator] = None,
                 ai: Optional[AIAnalyzer] = None, market: Optional[MarketData] = None,
                 technical: Optional[TechnicalAnalyzer] = None,
                 news: Optional[NewsAnalyzer] = None,
                 learner: Optional[TradingLearner] = None):
        self.sim = sim or Simulator(capital=capital)
        self.md = market or MarketData()
        self.ta = technical or TechnicalAnalyzer(self.md)
        self.na = news or NewsAnalyzer()
        self.learner = learner or TradingLearner()
        self.ai = ai or AIAnalyzer(retries=int(self.cfg["ai_max_retries"]),
                                   timeout=int(self.cfg["ai_timeout_seconds"]))
        self._signal_cache: dict = {}
        self._experience_txt = ""
        self._stop_event = threading.Event()
        self._heartbeat_thread: Optional[threading.Thread] = None
        self.force = False

    @property
    def cfg(self):
        return self.sim.cfg

    # ---------- 心跳 ----------
    def _beat(self):
        try:
            os.makedirs(DATA_DIR, exist_ok=True)
            payload = {"last_beat": datetime.now().isoformat(timespec="seconds"), "pid": os.getpid()}
            tmp = HEARTBEAT_FILE + ".tmp"
            with open(tmp, "w", encoding="utf-8") as fh:
                json.dump(payload, fh)
            os.replace(tmp, HEARTBEAT_FILE)
        except OSError:
            pass

    def _heartbeat_loop(self, every: int = 30):
        while not self._stop_event.wait(every):
            self._beat()

    def _start_heartbeat(self):
        if self._heartbeat_thread and self._heartbeat_thread.is_alive():
            return
        self._stop_event.clear()
        self._heartbeat_thread = threading.Thread(target=self._heartbeat_loop, daemon=True)
        self._heartbeat_thread.start()

    def _stop_heartbeat(self):
        self._stop_event.set()
        if self._heartbeat_thread:
            self._heartbeat_thread.join(timeout=2)
        self._heartbeat_thread = None

    # ---------- 信号冷却 ----------
    def _cache_signal(self, code: str, question_type: str, signal: str,
                      confidence: float, reason: str):
        if signal in ("error", "unknown"):
            return
        self._signal_cache[(code, question_type)] = {
            "signal": signal, "confidence": confidence, "reason": reason, "ts": time.time(),
        }

    def _last_signal(self, code: str, question_type: str):
        item = self._signal_cache.get((code, question_type))
        if not item:
            return None
        if time.time() - item["ts"] >= float(self.cfg["signal_cooldown_min"]) * 60:
            self._signal_cache.pop((code, question_type), None)
            return None
        return item

    # ---------- 候选 ----------
    def get_candidates(self, top_n: int = 10) -> list:
        stocks = self.md.get_all_a_stocks()
        if not stocks:
            return []
        total = self.sim.total_assets
        if total <= 0:
            return []
        prefiltered = []
        for stock in stocks:
            code = str(stock.get("code", ""))
            name = str(stock.get("name", ""))
            try:
                price = float(stock.get("price") or 0)
                change_pct = float(stock.get("change_pct") or 0)
            except (TypeError, ValueError):
                continue
            if not code or not name or price <= 0:
                continue
            if price < float(self.cfg["min_price"]) or price > float(self.cfg["max_price"]):
                continue
            if is_st_name(name) or name.startswith(("N", "C")) or "退" in name:
                continue
            lot = min_buy_shares(code)
            one_lot = price * lot
            if one_lot > total or one_lot / total > float(self.cfg["max_one_lot_ratio"]):
                continue
            if self.sim._round_trip_cost_pct(price, code) > float(self.cfg["max_round_trip_cost_pct"]):
                continue
            prefiltered.append({
                "code": code, "name": name, "price": price, "change_pct": change_pct,
                "one_lot": round(one_lot, 2),
            })
        prefiltered.sort(key=lambda item: item["change_pct"], reverse=True)
        shortlist = prefiltered[:max(top_n * 5, 30)]
        if not shortlist:
            return []
        quotes = self.md.get_realtime_batch([item["code"] for item in shortlist])
        candidates = []
        held_codes = set(self.sim.state["positions"])
        for item in shortlist:
            quote = quotes.get(item["code"]) or {}
            price = float(quote.get("price") or item["price"])
            change_pct = float(quote.get("change_pct") or item["change_pct"])
            if price <= 0:
                continue
            # 涨停价通常买不进，别再浪费一次 AI 调用
            if item["code"] not in held_codes and at_limit_up(
                    price, quote.get("prev_close"), item["code"], quote.get("name") or item["name"]):
                continue
            lot = min_buy_shares(item["code"])
            one_lot = price * lot
            if one_lot > total or one_lot / total > float(self.cfg["max_one_lot_ratio"]):
                continue
            if self.sim._round_trip_cost_pct(price, item["code"]) > float(self.cfg["max_round_trip_cost_pct"]):
                continue
            candidates.append({
                "code": item["code"], "name": quote.get("name") or item["name"],
                "price": price, "change_pct": change_pct,
                "prev_close": quote.get("prev_close"),
                "one_lot": round(one_lot, 2),
            })
        candidates.sort(key=lambda item: item["change_pct"], reverse=True)
        return candidates[:top_n]

    # ---------- 上下文 ----------
    def _account_context(self) -> str:
        cfg = self.cfg
        return (
            f"模拟账户总资产 {self.sim.total_assets:.0f} 元（小资金），"
            f"佣金{cfg['commission_rate'] * 10000:.2f}‱、最低{cfg['min_commission']:.0f}元，"
            f"卖出印花税{cfg['stamp_tax_rate'] * 100:.2f}%，过户费{cfg['transfer_fee_rate'] * 100:.3f}%；"
            f"持仓上限 {cfg['max_positions']} 只，单票仓位上限 {cfg['max_single_position_ratio'] * 100:.0f}%，"
            f"止损 {cfg['stop_loss_pct']}%、止盈 {cfg['take_profit_pct']}%。"
        )

    def _confidence_ratio(self, confidence: float) -> float:
        threshold = float(self.cfg["confidence_buy_threshold"])
        base = float(self.cfg["confidence_base_ratio"])
        maximum = float(self.cfg["confidence_max_ratio"])
        if confidence < threshold:
            return 0.0
        ratio = base + (confidence - threshold) * (maximum - base) / max(1.0 - threshold, 1e-9)
        return max(0.0, min(ratio, maximum))

    # ---------- 扫描一轮 ----------
    def scan_once(self, top_n: Optional[int] = None, force: Optional[bool] = None) -> dict:
        top_n = max(1, int(top_n or self.cfg["scan_top_n"]))
        force = self.force if force is None else force
        report = {
            "time": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
            "scanned": 0, "buy": 0, "sell": 0, "wait": 0, "error": 0, "cached": 0,
            "ai_calls": 0, "rule_skipped": 0, "actions": [], "skipped": [],
        }
        if not force and hasattr(self.md, "market_is_open") and not self.md.market_is_open():
            report["market_closed"] = True
            log("当前非交易时段/休市，跳过扫描（如需强制运行请加 --force）")
            self._beat()
            self.sim.state["last_scan"] = report
            self.sim.save()
            return report

        candidates = self.get_candidates(top_n)
        try:
            self.learner.learn_from_trades(self.sim.state)
            self._experience_txt = self.learner.experience_summary()
        except Exception as exc:  # noqa: BLE001
            self._experience_txt = ""
            log(f"经验学习失败: {type(exc).__name__}: {exc}")

        scan_list = list(candidates)
        held = self.sim.state["positions"]
        held_codes = set(held)
        missing = [code for code in held_codes if code not in {item["code"] for item in scan_list}]
        if missing:
            def pnl(code):
                pos = held[code]
                avg = float(pos.get("avg_cost") or 0)
                return ((float(pos.get("last_price", avg)) - avg) / avg * 100) if avg else 0.0
            weakest = sorted(missing, key=pnl)[0]
            quote = self.md.get_realtime(weakest)
            if quote.get("price", 0) > 0:
                scan_list.append({
                    "code": weakest, "name": quote.get("name", held[weakest].get("name", weakest)),
                    "price": quote["price"], "change_pct": quote.get("change_pct", 0),
                    "prev_close": quote.get("prev_close"), "one_lot": 0,
                })
        report["scanned"] = len(scan_list)
        log(f"=== 扫描开始 {report['time']} | 候选 {len(scan_list)} 只 ===")

        held_codes = set(self.sim.state["positions"])
        scored, skipped_rules = [], []
        for candidate in scan_list:
            if candidate["code"] in held_codes:
                candidate["_rule_score"] = 999
                candidate["_rule_grade"] = "持仓"
                candidate["_rule_reasons"] = ""
                scored.append(candidate)
                continue
            try:
                result = self.ta.rule_score(candidate["code"], candidate.get("change_pct"))
                candidate["_rule_score"] = result["score"]
                candidate["_rule_grade"] = result["grade"]
                candidate["_rule_reasons"] = " ".join(result["reasons"])[:80]
                if result["score"] >= int(self.cfg["rule_score_min"]):
                    scored.append(candidate)
                else:
                    skipped_rules.append(candidate)
                    report["skipped"].append({
                        "code": candidate["code"], "name": candidate["name"],
                        "reason": f"规则筛选{result['grade']}({result['score']}分)未达阈值",
                    })
            except Exception as exc:  # noqa: BLE001
                candidate["_rule_score"] = 0
                candidate["_rule_grade"] = "打分失败"
                candidate["_rule_reasons"] = str(exc)[:80]
                scored.append(candidate)
        non_held = [item for item in scored if item["code"] not in held_codes]
        non_held.sort(key=lambda item: item["_rule_score"], reverse=True)
        held_only = [item for item in scored if item["code"] in held_codes]
        ai_list = held_only + non_held[:int(self.cfg["ai_analyze_n"])]
        report["rule_skipped"] = len(skipped_rules) + max(0, len(non_held) - int(self.cfg["ai_analyze_n"]))
        if skipped_rules:
            log(f"规则预筛选: {len(scan_list)} -> {len(ai_list)} 只进 AI（免费跳过 {report['rule_skipped']} 只）")

        for candidate in ai_list:
            self._analyze_and_trade(candidate, report)

        # 更新持仓价格并执行止损/止盈
        positions = list(self.sim.state["positions"].keys())
        if positions:
            quotes = self.md.get_realtime_batch(positions)
            prices = {code: quote["price"] for code, quote in quotes.items() if quote.get("price")}
            self.sim.update_prices(prices)
            for signal in self.sim.check_stop_loss_take_profit(prices):
                quote = quotes.get(signal["code"]) or {}
                price = float(quote.get("price") or signal.get("price") or 0)
                if price <= 0:
                    report["skipped"].append({"code": signal["code"], "name": signal["code"], "reason": "止损/止盈无有效行情"})
                    continue
                result = self.sim.sell(signal["code"], price, -1, reason=signal["reason"],
                                       prev_close=quote.get("prev_close"))
                self.sim.log_signal(signal["code"], quote.get("name", signal["code"]), "sell", 1.0,
                                    signal["reason"], result["success"], price=price,
                                    note="风控卖出", skip_reason="" if result["success"] else result["msg"])
                report["actions"].append({
                    "code": signal["code"], "name": quote.get("name", signal["code"]),
                    "action": "SELL", "shares": "全部", "price": price, "msg": result["msg"],
                })
                if result["success"]:
                    report["sell"] += 1
                else:
                    report["skipped"].append({"code": signal["code"], "name": quote.get("name", signal["code"]),
                                              "reason": result["msg"]})

        record = self.sim.snapshot()
        self.sim.add_ai_usage(calls=report["ai_calls"], errors=report["error"],
                              rule_skipped=report["rule_skipped"])
        report["halted"] = bool(self.sim.state.get("halted"))
        report["net_value"] = record
        self.sim.state["last_scan"] = report
        self.sim.save()
        self._beat()
        self._print_report(report)
        return report

    def _analyze_and_trade(self, candidate: dict, report: dict):
        code = candidate["code"]
        name = candidate["name"]
        price = float(candidate["price"])
        held = code in self.sim.state["positions"]
        question_type = "hold" if held else "buy"
        question = ("我已持有该股票，请分析是继续持有还是卖出？" if held
                    else "现在适合买入吗？请给出明确的操作建议（买入/持有/观望/卖出）。")
        tech_txt = ""
        news_txt = ""
        rule_ctx = ""
        action = None
        skip_reason = ""
        result = {}

        cached = self._last_signal(code, question_type)
        if cached:
            result = {"signal": cached["signal"], "confidence": cached["confidence"],
                      "reason": cached["reason"]}
            report["cached"] += 1
            log(f"[冷却] {name}({code}) 沿用 {question_type} 信号: {cached['signal']}")
        else:
            try:
                indicators = self.ta.get_indicators(code)
                tech_txt = indicators.get("signal_txt", "") if indicators else ""
            except Exception as exc:  # noqa: BLE001
                log(f"[技术面] {code} 失败: {type(exc).__name__}: {exc}")
            try:
                news_txt = self.na.get_news_summary(code, name).get("txt", "")
            except Exception as exc:  # noqa: BLE001
                log(f"[消息面] {code} 失败: {type(exc).__name__}: {exc}")
            if candidate.get("_rule_score", 0) < 999:
                rule_ctx = (f"【规则预筛】技术规则打分 {candidate.get('_rule_score', 0)} 分"
                            f"（{candidate.get('_rule_grade', '?')}），依据: {candidate.get('_rule_reasons', '')}")
            question_full = question + (("\n" + rule_ctx) if rule_ctx else "")
            log(f"[AI分析] {name}({code}) @ {price:.2f} ...")
            result = self.ai.analyze_stock(
                code, question_full, technical=tech_txt, extra=self._account_context(),
                news=news_txt, experience=self._experience_txt,
                verify_price=price, expected_name=name,
            )
            report["ai_calls"] += 1
            signal = result.get("signal", "error")
            self._cache_signal(code, question_type, signal,
                               float(result.get("confidence") or 0), result.get("reason", ""))
            if result.get("hallucination_risk"):
                log(f"  ⚠️ 幻觉风险 [{name}({code})]: {result.get('verify_notes', '')}")

        signal = result.get("signal", "error")
        confidence = float(result.get("confidence") or 0)
        if signal in ("error", "unknown"):
            report["error"] += 1
            skip_reason = result.get("reason", "AI错误")
            self.sim.log_signal(code, name, signal, confidence, skip_reason, False,
                                price=price, skip_reason=skip_reason)
            report["skipped"].append({"code": code, "name": name, "reason": skip_reason})
            return

        try:
            if signal == "buy" and not held:
                threshold = float(self.cfg["confidence_buy_threshold"])
                if confidence < threshold:
                    skip_reason = f"置信度 {confidence:.0%} 低于买入门槛 {threshold:.0%}"
                else:
                    ratio = self._confidence_ratio(confidence)
                    ai_pct = result.get("position_pct")
                    if ai_pct is not None:
                        if float(ai_pct) <= 0:
                            skip_reason = "AI 建议 0 仓位"
                        else:
                            ratio = min(ratio, float(ai_pct) / 100.0)
                    ratio = min(ratio, float(self.cfg["confidence_max_ratio"]))
                    budget = self.sim.total_assets * ratio
                    shares = normalize_buy_shares(code, int(budget / price))
                    if shares <= 0:
                        skip_reason = f"预算 {budget:.0f} 元买不起最低 {min_buy_shares(code)} 股"
                    elif shares * price < float(self.cfg["min_trade_amount"]):
                        skip_reason = f"单笔金额 {shares * price:.0f} 元低于 {self.cfg['min_trade_amount']:.0f} 元"
                    else:
                        action = self.sim.buy(
                            code, name, price, shares,
                            reason=f"AI信号: 买入(置信{confidence:.0%})",
                            prev_close=candidate.get("prev_close"),
                            stop_loss=result.get("stop_loss"),
                            take_profit=result.get("take_profit"),
                        )
                        if not action["success"] and "仓位超限" in action["msg"]:
                            max_budget = self.sim.total_assets * float(self.cfg["max_single_position_ratio"])
                            shares2 = normalize_buy_shares(code, int(max_budget / price))
                            if shares2 > 0:
                                action = self.sim.buy(
                                    code, name, price, shares2,
                                    reason=f"AI信号: 买入-缩量(置信{confidence:.0%})",
                                    prev_close=candidate.get("prev_close"),
                                    stop_loss=result.get("stop_loss"),
                                    take_profit=result.get("take_profit"),
                                )
                                shares = shares2
                        if action["success"]:
                            report["buy"] += 1
                        else:
                            skip_reason = action["msg"]
                        report["actions"].append({
                            "code": code, "name": name, "action": "BUY", "shares": shares,
                            "price": price, "msg": action["msg"],
                        })
            elif signal == "sell" and held:
                action = self.sim.sell(code, price, -1,
                                       reason=f"AI信号: 卖出(置信{confidence:.0%})",
                                       prev_close=candidate.get("prev_close"))
                if action["success"]:
                    report["sell"] += 1
                else:
                    skip_reason = action["msg"]
                report["actions"].append({
                    "code": code, "name": name, "action": "SELL", "shares": "全部",
                    "price": price, "msg": action["msg"],
                })
            elif signal == "sell" and not held:
                skip_reason = "AI建议卖出但未持仓"
            else:
                skip_reason = f"AI建议{signal}"
            if skip_reason:
                report["wait"] += 1
                report["skipped"].append({"code": code, "name": name, "reason": skip_reason})
        except Exception as exc:  # noqa: BLE001
            report["error"] += 1
            skip_reason = f"执行失败: {type(exc).__name__}: {exc}"
            log(f"[执行] {code} 失败: {skip_reason}")
            report["skipped"].append({"code": code, "name": name, "reason": skip_reason})

        note = f"价格{price:.2f}" + (f" | 消息面:{news_txt.replace(chr(10), ' ')[:60]}" if news_txt else "")
        self.sim.log_signal(code, name, signal, confidence, result.get("reason", "")[:200],
                            executed=bool(action and action.get("success")),
                            hallucination_risk=bool(result.get("hallucination_risk")),
                            note=note, skip_reason=skip_reason, price=price)

    # ---------- 输出 ----------
    def _print_report(self, report: dict):
        log(f"=== 扫描完成 | 扫描 {report['scanned']} | 买 {report['buy']} | 卖 {report['sell']} | "
            f"观望 {report['wait']} | 错误 {report['error']} | AI {report['ai_calls']} 次 ===")
        summary = self.sim.summary()
        log(f"总资产 {summary['当前总资产']:.2f} | 收益率 {summary['总收益率%']}% | "
            f"持仓 {summary['持仓数量']} 只 | {summary['熔断状态']}")
        for item in report["actions"]:
            log(f"  [{item['action']}] {item['name']}({item['code']}) {item.get('shares', '')}股 "
                f"@ {item['price']:.2f} | {item['msg']}")
        for item in report["skipped"][:8]:
            log(f"  [跳过] {item['name']}({item['code']}): {item['reason']}")
        if report.get("halted"):
            log(f"⚠️ 熔断中: {self.sim.state.get('halt_reason', '')}")

    def show_summary(self):
        summary = self.sim.summary()
        print("=" * 56)
        print("AI 模拟交易成绩单")
        print("=" * 56)
        for key, value in summary.items():
            if key == "持仓列表":
                if value:
                    print("\n持仓:")
                    for pos in value:
                        print(f"  {pos['name']}({pos['code']}) {pos['shares']}股 成本{pos['avg_cost']:.3f} "
                              f"现价{pos['last_price']:.2f} 盈亏{pos['盈亏%']}% 可卖{pos['available_date']}")
                else:
                    print("持仓: 空仓")
            else:
                print(f"{key}: {value}")
        print("=" * 56)

    # ---------- 循环 ----------
    def _sleep(self, seconds: float):
        end = time.time() + max(0.0, seconds)
        while time.time() < end and not self._stop_event.is_set():
            self._beat()
            time.sleep(min(30.0, max(0.0, end - time.time())))

    def run_loop(self, interval: Optional[int] = None, top_n: Optional[int] = None):
        interval = max(30, int(interval or self.cfg["scan_interval"]))
        top_n = max(1, int(top_n or self.cfg["scan_top_n"]))
        cfg = self.cfg
        log(f"AI 量化交易机器人启动 | 间隔 {interval}s | 每轮候选 {top_n} 只 | 持仓≤{cfg['max_positions']} | "
            f"单票≤{cfg['max_single_position_ratio'] * 100:.0f}% | 止损{cfg['stop_loss_pct']}% "
            f"止盈{cfg['take_profit_pct']}% | 熔断{cfg['drawdown_halt_pct']}%")
        self._start_heartbeat()
        try:
            while not self._stop_event.is_set():
                try:
                    if not self.force and hasattr(self.md, "market_is_open") and not self.md.market_is_open():
                        log("非交易时段，等待开盘（Ctrl+C 停止）")
                        self._sleep(60)
                        continue
                    self.scan_once(top_n)
                    self._sleep(interval)
                except KeyboardInterrupt:
                    log("收到 Ctrl+C，停止")
                    break
                except Exception as exc:  # noqa: BLE001
                    log(f"[错误] {type(exc).__name__}: {exc}")
                    traceback.print_exc()
                    self._sleep(60)
        finally:
            self._stop_heartbeat()
            log("机器人已停止")

    def stop(self):
        self._stop_event.set()


def _pid_alive(pid: int) -> bool:
    if os.name == "nt":
        try:
            result = subprocess.run(
                ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
                capture_output=True, text=True, timeout=10, errors="ignore",
            )
            return f'"{pid}"' in result.stdout
        except Exception:  # noqa: BLE001
            return False
    try:
        os.kill(pid, 0)
        return True
    except PermissionError:
        return True
    except OSError:
        return False


def _acquire_lock() -> bool:
    os.makedirs(DATA_DIR, exist_ok=True)
    for _ in range(2):
        try:
            fd = os.open(LOCK_FILE, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            with os.fdopen(fd, "w", encoding="ascii") as fh:
                fh.write(str(os.getpid()))
            return True
        except FileExistsError:
            try:
                with open(LOCK_FILE, "r", encoding="ascii") as fh:
                    old_pid = int(fh.read().strip())
            except (OSError, ValueError):
                old_pid = 0
            if old_pid and _pid_alive(old_pid):
                log(f"已有机器人实例在运行 (PID {old_pid})，本次退出")
                return False
            try:
                os.remove(LOCK_FILE)
            except OSError:
                return False
    return False


def _release_lock():
    try:
        if not os.path.exists(LOCK_FILE):
            return
        with open(LOCK_FILE, "r", encoding="ascii") as fh:
            content = fh.read().strip()
        # Windows 下必须先关闭文件句柄，再删除
        if content == str(os.getpid()):
            os.remove(LOCK_FILE)
    except OSError:
        pass


def main(argv: Optional[list] = None):
    parser = argparse.ArgumentParser(description="AI 模拟量化交易")
    parser.add_argument("mode", choices=["run", "summary"], default="run", nargs="?")
    parser.add_argument("--once", action="store_true", help="只扫描一次")
    parser.add_argument("--loop", action="store_true", help="盘中循环扫描")
    parser.add_argument("--scan", type=int, default=None, help="每轮候选数")
    parser.add_argument("--interval", type=int, default=None, help="循环间隔秒数")
    parser.add_argument("--capital", type=float, default=None, help="初始资金（仅新账户生效）")
    parser.add_argument("--reset", action="store_true", help="重置模拟账户并备份旧数据")
    parser.add_argument("--force", action="store_true", help="非交易时段也强制扫描（默认禁止）")
    args = parser.parse_args(argv)
    if args.capital is not None and args.capital <= 0:
        parser.error("--capital 必须为正数")
    if args.scan is not None and args.scan < 1:
        parser.error("--scan 必须 >= 1")
    if args.interval is not None and args.interval < 30:
        parser.error("--interval 必须 >= 30 秒")

    os.makedirs(DATA_DIR, exist_ok=True)
    if args.mode == "summary":
        if args.reset:
            Simulator.reset(capital=args.capital)
        QuantBot(capital=args.capital).show_summary()
        return

    if not _acquire_lock():
        return
    try:
        if args.reset:
            Simulator.reset(capital=args.capital)
        bot = QuantBot(capital=args.capital)
        bot.force = bool(args.force)
        if args.once:
            bot.scan_once(args.scan, force=args.force)
        elif args.loop:
            bot.run_loop(args.interval, args.scan)
        else:
            if args.force or bot.md.market_is_open():
                log("当前为交易时段，进入盘中循环模式")
                bot.run_loop(args.interval, args.scan)
            else:
                log("当前非交易时段；单次扫描已禁用，如需强制请加 --force")
    finally:
        _release_lock()


if __name__ == "__main__":
    main()
