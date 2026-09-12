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
import time
import urllib.request
import gzip

LANGFLOW_BASE = "http://127.0.0.1:7860"
FLOW_ID = "c25e4d72-5593-48fe-945e-5e3bdde16220"  # A股量化交易分析


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
                      as_of: str = None) -> dict:
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
                        "symbol": symbol, "timestamp": time.strftime("%Y-%m-%d %H:%M:%S")}
            return self._parse_signal(text, symbol)
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
            "parsed_from": "json",
        }

    @classmethod
    def _parse_signal(cls, text: str, symbol: str) -> dict:
        """从分析报告中解析买卖信号"""
        parsed = cls._parse_json_signal(text)
        if parsed:
            parsed.update({"symbol": symbol, "raw": text,
                           "timestamp": time.strftime("%Y-%m-%d %H:%M:%S")})
            return parsed

        # 兼容旧版: 结构化输出失败时退回关键词匹配
        text_lower = text.lower()
        reason_lines = [l for l in text.splitlines() if l.strip()]

        # 1. 找明确的建议行
        signal = "hold"
        confidence = 0.5

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
            stop_loss = re.findall(r"[\d.]+", m.group())[-1]

        return {
            "signal": signal,
            "confidence": round(confidence, 2),
            "stop_loss": stop_loss,
            "reason": text[:2000],
            "raw": text,
            "symbol": symbol,
            "timestamp": time.strftime("%Y-%m-%d %H:%M:%S"),
            "parsed_from": "keyword",
        }


if __name__ == "__main__":
    # 自测: 分析贵州茅台
    analyzer = AIAnalyzer()
    print("正在调用 AI 分析 600519 ...")
    result = analyzer.analyze_stock("600519")
    print(f"信号: {result['signal']} | 置信度: {result['confidence']} | 止损: {result.get('stop_loss')}")
    print(f"解析方式: {result.get('parsed_from')} | 趋势: {result.get('trend')} | "
          f"仓位: {result.get('position_pct')} | 止盈: {result.get('take_profit')}")
    print(f"原因摘要: {result['reason'][:200]}")
    print(f"风险提示: {str(result.get('risk'))[:120]}")
