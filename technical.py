# -*- coding: utf-8 -*-
"""
技术指标计算模块
================
基于腾讯日K线数据计算常用技术指标, 供 AI 分析时输入:
- MA5 / MA10 / MA20       均线
- RSI14                   相对强弱
- MACD (DIF/DEA/柱)       趋势动能
- ATR14                   波动率
- 量比                    今日成交量 / 过去5日均量
- 趋势判断                均线多空排列 + 价格位置

用法:
    from technical import TechnicalAnalyzer
    ta = TechnicalAnalyzer()
    summary = ta.get_indicators("600519")
    # {"code","name","close","ma5","ma10","ma20","rsi14","macd_dif","macd_dea","macd_hist","atr14",
    #  "volume_ratio","trend","signal_txt"}
"""

from market_data import MarketData


class TechnicalAnalyzer:
    """基于腾讯K线计算技术指标"""

    def __init__(self):
        self.md = MarketData()

    def get_indicators(self, symbol: str, limit: int = 120) -> dict:
        """计算单只股票的技术指标摘要"""
        klines = self.md.get_kline(symbol, "1d", limit)
        if len(klines) < 25:
            return {}

        closes = [k["close"] for k in klines]
        highs = [k["high"] for k in klines]
        lows = [k["low"] for k in klines]
        volumes = [k["volume"] for k in klines]
        n = len(closes)
        last = closes[-1]

        # MA
        ma5 = self._ma(closes, 5)
        ma10 = self._ma(closes, 10)
        ma20 = self._ma(closes, 20)

        # RSI14 (Wilder 平滑)
        rsi14 = self._rsi(closes, 14)

        # MACD (12,26,9)
        dif, dea, hist = self._macd(closes)

        # ATR14
        atr14 = self._atr(highs, lows, closes, 14)

        # 量比: 今日量 / 前5日均量
        vol_ratio = round(volumes[-1] / max(sum(volumes[-6:-1]) / 5, 1), 2) if n >= 6 else 1.0

        # 趋势判断
        trend = self._trend(ma5, ma10, ma20, closes)

        # 给 AI 的中文摘要
        signal_txt = (
            f"最新收盘 {last:.2f} | MA5 {ma5:.2f} MA10 {ma10:.2f} MA20 {ma20:.2f} | "
            f"RSI14 {rsi14:.1f} | MACD DIF {dif:.3f} DEA {dea:.3f} 柱 {hist:.3f} | "
            f"ATR14 {atr14:.2f}(波动率) | 量比 {vol_ratio} | 趋势: {trend}"
        )

        return {
            "code": symbol,
            "close": round(last, 2),
            "ma5": round(ma5, 2), "ma10": round(ma10, 2), "ma20": round(ma20, 2),
            "rsi14": round(rsi14, 1),
            "macd_dif": round(dif, 3), "macd_dea": round(dea, 3), "macd_hist": round(hist, 3),
            "atr14": round(atr14, 2),
            "volume_ratio": vol_ratio,
            "trend": trend,
            "signal_txt": signal_txt,
        }

    # ---------- 指标实现 ----------
    @staticmethod
    def _ma(values, period):
        if len(values) < period:
            return sum(values) / len(values)
        return sum(values[-period:]) / period

    @staticmethod
    def _rsi(values, period=14):
        if len(values) < period + 1:
            return 50.0
        gains, losses = [], []
        for i in range(len(values) - period, len(values)):
            diff = values[i] - values[i - 1]
            gains.append(max(diff, 0))
            losses.append(max(-diff, 0))
        avg_gain = sum(gains) / period
        avg_loss = sum(losses) / period
        if avg_loss == 0:
            return 100.0
        rs = avg_gain / avg_loss
        return 100 - 100 / (1 + rs)

    @staticmethod
    def _ema(values, period):
        k = 2 / (period + 1)
        ema = values[0]
        for v in values[1:]:
            ema = v * k + ema * (1 - k)
        return ema

    def _macd(self, closes, fast=12, slow=26, signal=9):
        if len(closes) < slow + signal:
            return 0.0, 0.0, 0.0
        ema_fast = self._ema(closes, fast)
        ema_slow = self._ema(closes, slow)
        dif = ema_fast - ema_slow
        # 用足够长的序列算 DEA
        difs = []
        for i in range(len(closes) - 60, len(closes)):
            if i < slow:
                continue
            seg = closes[:i + 1]
            ef = self._ema(seg, fast)
            es = self._ema(seg, slow)
            difs.append(ef - es)
        if len(difs) < signal:
            return round(dif, 3), 0.0, 0.0
        dea = self._ema(difs, signal)
        return round(dif, 3), round(dea, 3), round((dif - dea) * 2, 3)

    @staticmethod
    def _atr(highs, lows, closes, period=14):
        if len(highs) < period + 1:
            return 0.0
        trs = []
        for i in range(1, len(highs)):
            tr = max(highs[i] - lows[i],
                     abs(highs[i] - closes[i - 1]),
                     abs(lows[i] - closes[i - 1]))
            trs.append(tr)
        return sum(trs[-period:]) / period

    @staticmethod
    def _trend(ma5, ma10, ma20, closes):
        """均线多空排列判断"""
        last = closes[-1]
        if ma5 > ma10 > ma20 and last > ma5:
            return "多头排列(强势)"
        if ma5 < ma10 < ma20 and last < ma5:
            return "空头排列(弱势)"
        if ma5 > ma10:
            return "短期偏多"
        if ma5 < ma10:
            return "短期偏空"
        return "震荡"

    # ---------- 免费规则打分(省 AI 调用) ----------
    def rule_score(self, symbol: str, change_pct: float = None) -> dict:
        """纯本地技术规则打分(零成本), 用于 AI 分析前的预筛选。
        打分规则(满分 100):
        - 趋势: 多头排列 +25, 短期偏多 +15, 震荡 0, 短期偏空 -10, 空头排列 -25
        - RSI: 40-65 健康区 +20, 65-75 强势 +10, >80 超买 -15, <30 超卖 +5, 30-40 弱 +5
        - MACD: 柱>0 +15, 柱<0 -10
        - 量比: 1-3 放量 +15, 0.8-1 平量 +5, <0.8 缩量 -5, >5 异常放量 -10
        - 涨幅: 0-5% +15, 5-9% +10, >9%(涨停) -5(追高风险), <0 -10
        - 价格位置: 收盘>MA20 +10, <MA20 -10
        返回: {"score", "grade", "reasons", "ind"}
        """
        ind = self.get_indicators(symbol)
        if not ind:
            return {"score": 0, "grade": "数据不足", "reasons": ["K线数据不足"], "ind": {}}

        score = 0
        reasons = []

        # 1. 趋势(权重最高)
        trend = ind.get("trend", "震荡")
        if "多头" in trend:
            score += 25
            reasons.append(f"趋势多头({trend})+25")
        elif "偏多" in trend:
            score += 15
            reasons.append(f"趋势偏多+15")
        elif "空头" in trend:
            score -= 25
            reasons.append(f"趋势空头({trend})-25")
        elif "偏空" in trend:
            score -= 10
            reasons.append(f"趋势偏空-10")
        else:
            reasons.append("趋势震荡0")

        # 2. RSI
        rsi = ind.get("rsi14", 50)
        if 40 <= rsi <= 65:
            score += 20
            reasons.append(f"RSI{rsi:.0f}健康区+20")
        elif 65 < rsi <= 75:
            score += 10
            reasons.append(f"RSI{rsi:.0f}强势+10")
        elif rsi > 80:
            score -= 15
            reasons.append(f"RSI{rsi:.0f}超买-15")
        elif rsi < 30:
            score += 5
            reasons.append(f"RSI{rsi:.0f}超卖+5")
        else:
            score += 5
            reasons.append(f"RSI{rsi:.0f}偏弱+5")

        # 3. MACD
        hist = ind.get("macd_hist", 0)
        if hist > 0:
            score += 15
            reasons.append("MACD红柱+15")
        else:
            score -= 10
            reasons.append("MACD绿柱-10")

        # 4. 量比
        vr = ind.get("volume_ratio", 1)
        if 1 <= vr <= 3:
            score += 15
            reasons.append(f"量比{vr}放量+15")
        elif 0.8 <= vr < 1:
            score += 5
            reasons.append(f"量比{vr}平量+5")
        elif vr < 0.8:
            score -= 5
            reasons.append(f"量比{vr}缩量-5")
        else:
            score -= 10
            reasons.append(f"量比{vr}异常放量-10")

        # 5. 当日涨幅
        chg = change_pct if change_pct is not None else 0
        if 0 <= chg < 5:
            score += 15
            reasons.append(f"涨幅{chg:.1f}%健康+15")
        elif 5 <= chg < 9:
            score += 10
            reasons.append(f"涨幅{chg:.1f}%强势+10")
        elif chg >= 9:
            score -= 5
            reasons.append(f"涨幅{chg:.1f}%过高-5")
        else:
            score -= 10
            reasons.append(f"涨幅{chg:.1f}%下跌-10")

        # 6. 价格位置
        close = ind.get("close", 0)
        ma20 = ind.get("ma20", 0)
        if close > ma20:
            score += 10
            reasons.append("价>MA20+10")
        elif ma20 > 0:
            score -= 10
            reasons.append("价<MA20-10")

        # 评级
        if score >= 60:
            grade = "A(强)"
        elif score >= 30:
            grade = "B(中)"
        elif score >= 10:
            grade = "C(弱)"
        else:
            grade = "D(差)"

        return {
            "score": score,
            "grade": grade,
            "reasons": reasons,
            "ind": ind,
            "trend": trend,
        }


if __name__ == "__main__":
    ta = TechnicalAnalyzer()
    r = ta.get_indicators("600519")
    print(r.get("signal_txt", "无数据"))
