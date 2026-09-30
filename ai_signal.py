# -*- coding: utf-8 -*-
"""
AI 信号集成层
=============
调用 Langflow「A股量化交易分析」流程 API，把 DeepSeek 输出的分析报告
解析成结构化的买卖信号，交给模拟交易引擎执行。

用法:
    from ai_signal import AIAnalyzer

    analyzer = AIAnalyzer()
    # 分析单只股票
    signal = analyzer.analyze_stock("600519", question="现在适合买入吗？")
    # signal: {"signal": "buy|sell|hold|wait", "confidence": 0.8, "reason": "...", "raw": "..."}
"""

import json
import re
import sys
import time
import urllib.request
import gzip

LANGFLOW_BASE = "http://127.0.0.1:7860"
FLOW_ID = "c25e4d72-5593-48fe-945e-5e3bdde16220"  # A股量化交易分析

# ===================== 防交易幻觉(grounding)配置 =====================
# 交易幻觉: AI 凭空编造标的/价格/理由, 系统却据此下单或记成事实。
# 下面这组开关用于在信号落地前做"真实性校验", 把幻觉信号降级/拦截。
GROUND_SYMBOL = True                  # 校验 AI 是否分析了正确的标的(代码/名称)
PRICE_TOLERANCE_PCT = 8.0             # AI 陈述的"现价"与真实行情偏差容忍(%)，超此视为编造
LOW_TRUST_KEYWORD_CONF_CAP = 0.40     # 关键词兜底解析的置信度上限(非结构化, 低可信)
REQUIRE_EXPLICIT_DECISION = True      # 关键词兜底下, 信号不明确则强制降级为观望
GROUND_STOP_TAKE_LOGIC = True         # 校验 止损/止盈 与信号方向是否自相矛盾(逻辑幻觉)


class AIAnalyzer:
    """调用 Langflow 流程获取 AI 分析信号"""

    def __init__(self, base=LANGFLOW_BASE, flow_id=FLOW_ID):
        self.base = base
        self.flow_id = flow_id
        self._token = None

    # ---------- Langflow API ----------
    def _fetch(self, url, token=None, method="GET", body=None, timeout=180):
        r = urllib.request.Request(self.base + url, method=method)
        if token:
            r.add_header("Authorization", "Bearer " + token)
        r.add_header("Accept-Encoding", "identity")
        r.add_header("Content-Type", "application/json")
        data = json.dumps(body).encode() if body is not None else None
        resp = urllib.request.urlopen(r, data=data, timeout=timeout)
        raw = resp.read()
        if resp.headers.get("Content-Encoding") == "gzip":
            raw = gzip.decompress(raw)
        return raw

    def _get_token(self):
        if not self._token:
            self._token = json.loads(self._fetch("/api/v1/auto_login"))["access_token"]
        return self._token

    # ---------- 分析单只股票 ----------
    def analyze_stock(self, symbol: str, question: str = "现在适合买入吗？请给出明确的操作建议（买入/持有/观望/卖出）。",
                      technical: str = None, extra: str = None,
                      news: str = None, experience: str = None,
                      as_of: str = None,
                      verify_price: float = None, expected_name: str = None) -> dict:
        """分析一只股票，返回结构化信号 —— 多重预测: 技术面 + 消息面 + 历史经验
        technical:   技术指标摘要(如 "MA5 1295 MA10 1307 ... 趋势: 短期偏空")
        extra:       附加上下文(账户资金/手续费等)
        news:        消息面摘要(最新新闻 + 利好/利空情感打分)
        experience:  历史交易经验(胜率/盈亏比/置信度胜率)
        as_of:       回测用截止日期 "YYYY-MM-DD"，只让AI看到该日及之前的K线，避免未来函数
        """
        token = self._get_token()
        # 把技术面与账户上下文拼进问题, 让 DeepSeek 基于技术指标判断
        if technical:
            question = f"{question}\n【技术面】{technical}\n请基于技术指标判断。"
        if news:
            question = f"{question}\n【消息面】{news}\n请参考最新消息面, 判断利好利空。"
        if experience:
            question = f"{question}\n【历史经验】{experience}\n请参考你自己过去的交易统计, 修正本次判断。"
        if extra:
            question = f"{question}\n【账户情况】{extra}\n请结合小资金与手续费成本考虑。"
        question += (
            "\n\n请综合【技术面】【消息面】【历史经验】三个维度, 输出: "
            "1) 方向判断(买入/卖出/观望) 2) 置信度(0-100%) 3) 预测理由(简述每个维度支持还是反对当前判断) 4) 止损位(如有)"
        )
        run_body = {
            "input_value": question,
            "output_type": "chat",
            "input_type": "chat",
            "session_id": f"sim-{symbol}-{int(time.time())}",
            # 通过 tweaks 显式传入股票代码，覆盖流程节点的默认值，这样分析任何股票都无需改流程
            "tweaks": {
                "MarketData-aF2Ci": {"symbol": symbol},
                "TextInput-5pgMC": {"input_value": symbol},
            },
        }
        # 回测: 只喂"截止当天"的K线，杜绝未来函数
        if as_of:
            run_body["tweaks"]["MarketData-aF2Ci"]["as_of"] = as_of
        try:
            resp_raw = self._fetch(f"/api/v1/run/session/{self.flow_id}", token,
                                   method="POST", body=run_body, timeout=240)
            resp = json.loads(resp_raw)
            text = self._extract_text(resp)
            if not text:
                return {"signal": "unknown", "confidence": 0.0, "reason": "AI 无输出", "raw": "",
                        "symbol": symbol, "timestamp": time.strftime("%Y-%m-%d %H:%M:%S"),
                        "hallucination_risk": True, "ungrounded": True,
                        "verify_notes": "AI 无输出(无法落地真实信号)"}
            return self._parse_signal(text, symbol, verify_price=verify_price, expected_name=expected_name)
        except Exception as e:
            return {"signal": "error", "confidence": 0.0,
                    "reason": f"AI 调用失败: {type(e).__name__}: {str(e)[:200]}", "raw": "",
                    "symbol": symbol, "timestamp": time.strftime("%Y-%m-%d %H:%M:%S")}

    @staticmethod
    def _extract_text(resp: dict) -> str:
        """从 Langflow 响应中提取 AI 输出的文本"""
        try:
            outputs = resp.get("outputs", [])
            for o in outputs:
                for r_ in o.get("outputs", []):
                    msg = r_.get("results", {}).get("message", {})
                    text = msg.get("text", "")
                    if text:
                        return text
        except Exception:
            pass
        return ""

    # ---------- 信号解析 ----------
    @staticmethod
    def _parse_json_signal(text: str):
        """解析流里 DeepSeek(JSON模式) 返回的结构化信号; 解析失败返回 None。"""
        if not text:
            return None
        raw = text.strip()
        if raw.startswith("```"):
            raw = re.sub(r"^```[a-zA-Z]*\s*", "", raw)
            raw = re.sub(r"\s*```$", "", raw)
        start, end = raw.find("{"), raw.rfind("}")
        if start == -1 or end <= start:
            return None
        try:
            data = json.loads(raw[start:end + 1])
        except Exception:
            return None
        if not isinstance(data, dict):
            return None
        signal = str(data.get("signal", "")).strip().lower()
        if signal not in ("buy", "sell", "hold", "wait"):
            return None
        try:
            confidence = float(data.get("confidence", 50))
        except (TypeError, ValueError):
            confidence = 50.0
        confidence = max(0.0, min(confidence, 100.0)) / 100.0
        # 防幻觉: 要求 AI 在 JSON 中回显分析对象的代码, 用于后续校验是否分析错标的
        ai_symbol = str(data.get("symbol", "")).strip() or None
        if ai_symbol is not None and not re.fullmatch(r"\d{6}", ai_symbol):
            ai_symbol = None

        def _num(key):
            value = data.get(key)
            if value in (None, "", "null", "None"):
                return None
            try:
                return float(value)
            except (TypeError, ValueError):
                return None

        try:
            position_pct = int(float(data.get("position_pct")))
            position_pct = max(0, min(position_pct, 100))
        except (TypeError, ValueError):
            position_pct = None
        return {
            "signal": signal,
            "confidence": round(confidence, 2),
            "trend": str(data.get("trend", "")).strip().lower() or None,
            "stop_loss": _num("stop_loss"),
            "take_profit": _num("take_profit"),
            "position_pct": position_pct,
            "reason": str(data.get("reason", ""))[:500],
            "risk": str(data.get("risk", ""))[:300],
            "symbol_echo": ai_symbol,
            "parsed_from": "json",
        }

    @classmethod
    def _parse_signal(cls, text: str, symbol: str, verify_price: float = None,
                      expected_name: str = None) -> dict:
        """从分析报告中解析买卖信号，并做防幻觉校验"""
        parsed = cls._parse_json_signal(text)
        if parsed:
            parsed.update({"symbol": symbol, "raw": text,
                           "timestamp": time.strftime("%Y-%m-%d %H:%M:%S")})
            return cls._ground_signal(parsed, text, symbol, verify_price, expected_name)

        # 兼容旧版: 结构化输出失败时退回关键词匹配
        text_lower = text.lower()
        reason_lines = [l for l in text.splitlines() if l.strip()]

        # 1. 找明确的建议行
        signal = "hold"
        confidence = 0.5
        advice_hit = False  # 信号是否来自"明确建议区"(而非全文偶然扫到的词)

        # 交易建议块
        buy_kw = ["买入", "建议买入", "转为买入", "轻仓买入", "可买入", "buy"]
        sell_kw = ["卖出", "建议卖出", "转为卖出", "减仓", "离场", "清仓", "sell"]
        wait_kw = ["观望", "等待", "持有", "hold", "wait", "watch", "不追高", "不建议追高"]
        neg_kw = ["不", "无", "不应", "不宜", "暂不", "不建议", "别"]

        # 先找"交易建议"区块(结论区), 区块内的信号最可信
        advice_zone = ""
        in_advice = False
        for line in reason_lines:
            if "交易建议" in line or "操作建议" in line or "建议总结" in line:
                in_advice = True
            if in_advice:
                advice_zone += line + "\n"
                if "风险提示" in line or "免责" in line:
                    in_advice = False  # 建议区结束
        zone_lines = [l for l in advice_zone.splitlines() if l.strip()] or reason_lines

        # 从建议区找信号(带否定词过滤)
        for line in zone_lines:
            has_neg = any(k in line for k in neg_kw)
            if any(k in line for k in wait_kw) and not has_neg:
                signal = "wait"
                confidence = 0.65
                break
            if any(k in line for k in buy_kw) and not has_neg:
                signal = "buy"
                confidence = 0.75
                break
            if any(k in line for k in sell_kw) and not has_neg:
                signal = "sell"
                confidence = 0.75
                break

        # 兜底: 若建议区未命中, 全文找强信号词
        advice_hit = (signal != "hold")  # 建议区是否命中明确信号
        if signal == "hold":
            for line in reason_lines:
                has_neg = any(k in line for k in neg_kw)
                if any(k in line for k in buy_kw) and not has_neg:
                    signal = "buy"
                    confidence = 0.7
                    break
                if any(k in line for k in sell_kw) and not has_neg:
                    signal = "sell"
                    confidence = 0.7
                    break

        # 置信度提升: 出现"触发条件""止损位""仓位比例"说明分析完整
        if "触发" in text and ("止损" in text or "仓位" in text):
            confidence = min(confidence + 0.15, 0.95)

        # 提取止损位
        stop_loss = None
        m = re.search(r"止损(?:位|价|点)?[：:]\s*[\d.]+", text)
        if m:
            _nums = re.findall(r"[\d.]+", m.group())
            if _nums:
                try:
                    stop_loss = float(_nums[-1])
                except ValueError:
                    stop_loss = None

        result = {
            "signal": signal,
            "confidence": round(confidence, 2),
            "stop_loss": stop_loss,
            "reason": text[:2000],
            "raw": text,
            "symbol": symbol,
            "timestamp": time.strftime("%Y-%m-%d %H:%M:%S"),
            "parsed_from": "keyword",
            "_advice_hit": advice_hit,
        }
        return cls._ground_signal(result, text, symbol, verify_price, expected_name)


    # ===================== 防交易幻觉(grounding) =====================
    @staticmethod
    def _extract_codes(text: str):
        """从文本中提取所有 6 位股票代码"""
        return set(re.findall(r"\b\d{6}\b", text or ""))

    @classmethod
    def _ground_signal(cls, result: dict, text: str, symbol: str,
                       verify_price: float = None, expected_name: str = None) -> dict:
        """防交易幻觉：校验 AI 输出是否基于真实数据，而非凭空编造。
        返回带 hallucination_risk / ungrounded / verify_notes 的 result。

        校验维度：
          1) 标的真实性：AI 回显的代码(symbol_echo)是否等于请求代码；或在自由文本里
             是否把决策对象张冠李戴到别的代码上。
          2) 价格真实性：AI 陈述的"现价/最新收盘"是否与真实行情吻合（偏差过大＝编造）。
          3) 逻辑自洽：买入信号的止损是否低于现价、卖出信号的止盈是否高于现价。
          4) 低可信兜底：非结构化(关键词)解析一律降权封顶，信号不明确强制观望。
        """
        notes = []
        risk = False
        sig = result.get("signal")

        # 1) 标的真实性
        if GROUND_SYMBOL:
            ai_symbol = result.get("symbol_echo")
            if ai_symbol and ai_symbol != symbol:
                risk = True
                notes.append(f"AI 回显代码 {ai_symbol} ≠ 请求 {symbol}（可能分析错标的）")
                result["signal"] = "wait"
                result["confidence"] = min(result.get("confidence", 0.0), 0.3)
            elif result.get("parsed_from") == "keyword":
                # 自由文本模式：若文中出现别的 6 位代码却完全没提到请求代码，疑似张冠李戴
                codes = cls._extract_codes(text)
                if codes and symbol not in codes:
                    risk = True
                    notes.append(f"AI 文本未提及请求标的 {symbol}，反而提到 {sorted(codes)}（疑似分析错标的）")
                    result["signal"] = "wait"
                    result["confidence"] = min(result.get("confidence", 0.0), 0.3)
            # 名称校验（可选）：AI 把另一只股票名称当成本标的讨论
            if expected_name and expected_name not in ("", None):
                # 仅当文本里出现了明显是"另一家公司"的知名代码/名称且无请求代码时才提示（不强制，避免误杀）
                pass

        # 2) 价格真实性 + 3) 逻辑自洽
        if verify_price and verify_price > 0:
            for label in ("最新收盘", "收盘价", "现价", "当前价", "最新价", "当前价格"):
                m = re.search(label + r"[：: ]*\s*([0-9]+(?:\.[0-9]+)?)", text or "")
                if m:
                    try:
                        cited = float(m.group(1))
                    except ValueError:
                        cited = None
                    if cited:
                        dev = abs(cited - verify_price) / verify_price * 100
                        if dev > PRICE_TOLERANCE_PCT:
                            risk = True
                            notes.append(f"AI 陈述{label}{cited} 与真实行情 {verify_price:.2f} 偏差 {dev:.0f}%（疑似编造）")
                    break
            if GROUND_STOP_TAKE_LOGIC:
                sl = result.get("stop_loss")
                tp = result.get("take_profit")
                if sig == "buy" and isinstance(sl, (int, float)) and sl >= verify_price:
                    risk = True
                    notes.append(f"买入信号但止损 {sl} 不低于现价 {verify_price:.2f}（逻辑矛盾，疑似编造）")
                if sig == "sell" and isinstance(tp, (int, float)) and tp <= verify_price:
                    risk = True
                    notes.append(f"卖出信号但止盈 {tp} 不高于现价 {verify_price:.2f}（逻辑矛盾，疑似编造）")

        # 4) 低可信兜底：非结构化(关键词)解析一律降权封顶，且只有"明确建议区"里
        #    出现的买卖才算数；全文偶然扫到的买卖词(典型幻觉/歧义)直接作废。
        if result.get("parsed_from") == "keyword":
            result["low_confidence"] = True
            capped = min(result.get("confidence", 0.0), LOW_TRUST_KEYWORD_CONF_CAP)
            if capped < result.get("confidence", 0.0):
                notes.append(f"非结构化(关键词兜底)解析，置信度封顶 {LOW_TRUST_KEYWORD_CONF_CAP:.0%}")
            result["confidence"] = round(capped, 2)
            if sig in ("buy", "sell") and not result.get("_advice_hit"):
                notes.append("买卖词仅出现在全文叙述中(非明确建议区)，疑似幻觉/歧义，已作废")
                result["signal"] = "wait"

        # 5) 任何被判定为幻觉风险的买卖信号，一律拦截执行(降级观望)，不拿假信号下单
        if risk and result.get("signal") in ("buy", "sell"):
            notes.append("存在幻觉风险，已拦截执行并降级为观望")
            result["signal"] = "wait"
            result["confidence"] = min(result.get("confidence", 0.0), 0.3)

        result["hallucination_risk"] = risk
        result["ungrounded"] = bool(result.get("low_confidence", False) or risk)
        result["verify_notes"] = "; ".join(notes)
        if notes:
            result["reason"] = (result.get("reason", "") or "") + " 【校验：" + "; ".join(notes) + "】"
        return result

    # ===================== 离线自检(无需 API) =====================
    @classmethod
    def selftest_grounding(cls):
        """不调用 Langflow/DeepSeek，直接对若干构造样本跑解析+校验，验证护栏是否生效。"""
        print("=== 防幻觉护栏离线自检 ===")
        samples = [
            ("规范JSON-正常", '{"signal":"buy","confidence":80,"symbol":"600519","stop_loss":1250,"take_profit":1400,"reason":"多头排列，现价1295可介入"}', 1295.0),
            ("规范JSON-错标的", '{"signal":"buy","confidence":80,"symbol":"000001","stop_loss":10,"take_profit":12,"reason":"买入平安银行"}', 1295.0),
            ("自由文本-明确买入", "综合来看，建议买入，止损位1300，仓位15%。", 1295.0),
            ("自由文本-编造现价", "当前价格 9999.00，建议买入。", 1295.0),
            ("自由文本-无明确信号", "本报告仅供参考，市场有风险，投资需谨慎。", 1295.0),
            ("规范JSON-止损矛盾", '{"signal":"buy","confidence":70,"symbol":"600519","stop_loss":1350,"take_profit":1400,"reason":"买入"}', 1295.0),
        ]
        for name, txt, price in samples:
            r = cls._parse_signal(txt, "600519", verify_price=price, expected_name="贵州茅台")
            print(f"[{name}] signal={r['signal']} conf={r['confidence']} "
                  f"risk={r.get('hallucination_risk')} from={r.get('parsed_from')} | {r.get('verify_notes','')}")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "selftest":
        AIAnalyzer.selftest_grounding()
    else:
        # 自测: 分析贵州茅台(需本地 Langflow + DeepSeek)
        analyzer = AIAnalyzer()
        print("正在调用 AI 分析 600519 ...")
        result = analyzer.analyze_stock("600519")
        print(f"信号: {result['signal']} | 置信度: {result['confidence']} | 止损: {result.get('stop_loss')}")
        print(f"解析方式: {result.get('parsed_from')} | 趋势: {result.get('trend')} | "
              f"仓位: {result.get('position_pct')} | 止盈: {result.get('take_profit')}")
        print(f"幻觉风险: {result.get('hallucination_risk')} | 校验: {result.get('verify_notes','')}")
        print(f"原因摘要: {result['reason'][:200]}")
        print(f"风险提示: {str(result.get('risk'))[:120]}")
