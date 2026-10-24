# -*- coding: utf-8 -*-
"""技术指标（与 Langflow 节点保持同一口径）。

v2 修复：
- RSI / ATR 改为 Wilder 平滑（旧版用简单平均，和 flow 里的指标对不上）
- MACD 柱统一为 DIF-DEA（旧版 ×2，和 flow 不一致）
- as_of 通过 K 线 end_date 取数，不再“先取最近 120 根再过滤”
- 盘中量比按已走交易时段折算，避免早盘被误判为缩量
"""
from __future__ import annotations

from datetime import datetime
from typing import Optional

from market_data import MarketData
from trading_calendar import is_trading_day, session_progress


class TechnicalAnalyzer:
    """基于日 K 线计算常用指标。"""

    def __init__(self, market: Optional[MarketData] = None):
        self.md = market or MarketData()

    def get_indicators(self, symbol: str, limit: int = 120,
                       as_of: Optional[str] = None, now: Optional[datetime] = None) -> dict:
        klines = self.md.get_kline(symbol, "1d", limit, end_date=as_of)
        if as_of:
            klines = [row for row in klines if row["date"] <= str(as_of)[:10]]
        if len(klines) < 25:
            return {}

        closes = [row["close"] for row in klines]
        highs = [row["high"] for row in klines]
        lows = [row["low"] for row in klines]
        volumes = [row["volume"] for row in klines]
        last = closes[-1]
        today = (now or datetime.now()).date().isoformat()
        is_intraday = (
            not as_of
            and klines[-1]["date"] == today
            and is_trading_day(today)
            and 0 < session_progress(now) < 1
        )

        ma5 = self._ma(closes, 5)
        ma10 = self._ma(closes, 10)
        ma20 = self._ma(closes, 20)
        ma60 = self._ma(closes, 60)
        rsi14 = self._rsi(closes, 14)
        dif, dea, hist = self._macd(closes)
        atr14 = self._atr(highs, lows, closes, 14)
        volume_ratio = self._volume_ratio(volumes, is_intraday, now)
        trend = self._trend(ma5, ma10, ma20, ma60, closes)

        label = "最新价" if is_intraday else "收盘"
        signal_txt = (
            f"{label} {last:.2f} | MA5 {self._fmt(ma5)} MA10 {self._fmt(ma10)} "
            f"MA20 {self._fmt(ma20)} MA60 {self._fmt(ma60)} | "
            f"RSI14 {rsi14:.1f} | MACD DIF {dif:.3f} DEA {dea:.3f} 柱 {hist:.3f} | "
            f"ATR14 {atr14:.2f} | 量比 {volume_ratio:.2f}{'(盘中折算)' if is_intraday else ''} | 趋势: {trend}"
        )
        return {
            "code": symbol,
            "close": round(last, 2),
            "is_intraday": is_intraday,
            "ma5": self._round(ma5), "ma10": self._round(ma10),
            "ma20": self._round(ma20), "ma60": self._round(ma60),
            "rsi14": round(rsi14, 1),
            "macd_dif": round(dif, 3), "macd_dea": round(dea, 3), "macd_hist": round(hist, 3),
            "atr14": round(atr14, 2),
            "volume_ratio": round(volume_ratio, 2),
            "trend": trend,
            "signal_txt": signal_txt,
        }

    # ---------- 指标实现 ----------
    @staticmethod
    def _fmt(value: Optional[float]) -> str:
        return "--" if value is None else f"{value:.2f}"

    @staticmethod
    def _round(value: Optional[float]) -> Optional[float]:
        return None if value is None else round(value, 2)

    @staticmethod
    def _ma(values: list, period: int) -> Optional[float]:
        if len(values) < period:
            return None
        return sum(values[-period:]) / period

    @staticmethod
    def _rsi(values: list, period: int = 14) -> float:
        if len(values) <= period:
            return 50.0
        gains, losses = [], []
        for i in range(1, len(values)):
            diff = values[i] - values[i - 1]
            gains.append(max(diff, 0.0))
            losses.append(max(-diff, 0.0))
        avg_gain = sum(gains[:period]) / period
        avg_loss = sum(losses[:period]) / period
        for i in range(period, len(gains)):
            avg_gain = (avg_gain * (period - 1) + gains[i]) / period
            avg_loss = (avg_loss * (period - 1) + losses[i]) / period
        if avg_loss == 0:
            return 100.0
        rs = avg_gain / avg_loss
        return 100 - 100 / (1 + rs)

    @staticmethod
    def _ema_series(values: list, period: int) -> list:
        if not values:
            return []
        k = 2 / (period + 1)
        ema = values[0]
        out = [ema]
        for value in values[1:]:
            ema = value * k + ema * (1 - k)
            out.append(ema)
        return out

    @classmethod
    def _macd(cls, closes: list, fast: int = 12, slow: int = 26, signal: int = 9):
        if len(closes) < slow + signal:
            return 0.0, 0.0, 0.0
        ema_fast = cls._ema_series(closes, fast)
        ema_slow = cls._ema_series(closes, slow)
        difs = [a - b for a, b in zip(ema_fast, ema_slow)]
        deas = cls._ema_series(difs, signal)
        dif, dea = difs[-1], deas[-1]
        return round(dif, 3), round(dea, 3), round(dif - dea, 3)

    @staticmethod
    def _atr(highs: list, lows: list, closes: list, period: int = 14) -> float:
        if len(closes) <= period:
            return 0.0
        trs = []
        for i in range(1, len(closes)):
            trs.append(max(highs[i] - lows[i],
                           abs(highs[i] - closes[i - 1]),
                           abs(lows[i] - closes[i - 1])))
        atr = sum(trs[:period]) / period
        for i in range(period, len(trs)):
            atr = (atr * (period - 1) + trs[i]) / period
        return atr

    @staticmethod
    def _volume_ratio(volumes: list, is_intraday: bool, now: Optional[datetime]) -> float:
        if len(volumes) < 6:
            return 1.0
        base = sum(volumes[-6:-1]) / 5
        if base <= 0:
            return 1.0
        current = volumes[-1]
        if is_intraday:
            progress = session_progress(now)
            current = current / max(progress, 0.05)
        return max(0.01, current / base)

    @staticmethod
    def _trend(ma5: Optional[float], ma10: Optional[float],
               ma20: Optional[float], ma60: Optional[float], closes: list) -> str:
        last = closes[-1]
        if None not in (ma5, ma10, ma20, ma60):
            if ma5 > ma10 > ma20 > ma60 and last > ma5:
                return "多头排列(强势)"
            if ma5 < ma10 < ma20 < ma60 and last < ma5:
                return "空头排列(弱势)"
        if None not in (ma5, ma10, ma20):
            if ma5 > ma10 > ma20 and last > ma5:
                return "短期多头"
            if ma5 < ma10 < ma20 and last < ma5:
                return "短期空头"
        if ma5 is not None and ma10 is not None:
            if ma5 > ma10:
                return "短期偏多"
            if ma5 < ma10:
                return "短期偏空"
        return "震荡"

    # ---------- 规则打分（省 AI 调用） ----------
    def rule_score(self, symbol: str, change_pct: Optional[float] = None,
                   as_of: Optional[str] = None) -> dict:
        ind = self.get_indicators(symbol, as_of=as_of)
        if not ind:
            return {"score": 0, "grade": "数据不足", "reasons": ["K线数据不足"], "ind": {}}
        score = 0
        reasons = []

        trend = ind.get("trend", "震荡")
        if "多头" in trend:
            score += 25
            reasons.append(f"趋势{trend}+25")
        elif "偏多" in trend:
            score += 15
            reasons.append("趋势偏多+15")
        elif "空头" in trend:
            score -= 25
            reasons.append(f"趋势{trend}-25")
        elif "偏空" in trend:
            score -= 10
            reasons.append("趋势偏空-10")
        else:
            reasons.append("趋势震荡0")

        rsi = float(ind.get("rsi14", 50))
        if 40 <= rsi <= 65:
            score += 20
            reasons.append(f"RSI{rsi:.0f}健康+20")
        elif 65 < rsi <= 75:
            score += 10
            reasons.append(f"RSI{rsi:.0f}强势+10")
        elif 75 < rsi <= 80:
            score += 5
            reasons.append(f"RSI{rsi:.0f}偏热+5")
        elif rsi > 80:
            score -= 15
            reasons.append(f"RSI{rsi:.0f}超买-15")
        elif rsi < 30:
            score += 5
            reasons.append(f"RSI{rsi:.0f}超卖+5")
        else:
            score += 5
            reasons.append(f"RSI{rsi:.0f}偏弱+5")

        hist = float(ind.get("macd_hist", 0))
        if hist > 0:
            score += 15
            reasons.append("MACD红柱+15")
        else:
            score -= 10
            reasons.append("MACD绿柱-10")

        vr = float(ind.get("volume_ratio", 1))
        if 1 <= vr <= 3:
            score += 15
            reasons.append(f"量比{vr:.2f}放量+15")
        elif 0.8 <= vr < 1:
            score += 5
            reasons.append(f"量比{vr:.2f}平量+5")
        elif vr < 0.8:
            score -= 5
            reasons.append(f"量比{vr:.2f}缩量-5")
        else:
            score -= 10
            reasons.append(f"量比{vr:.2f}异常放量-10")

        chg = float(change_pct or 0)
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

        close = float(ind.get("close", 0))
        ma20 = float(ind.get("ma20") or 0)
        if ma20 > 0 and close > ma20:
            score += 10
            reasons.append("价>MA20 +10")
        elif ma20 > 0:
            score -= 10
            reasons.append("价<MA20 -10")

        if score >= 60:
            grade = "A(强)"
        elif score >= 30:
            grade = "B(中)"
        elif score >= 10:
            grade = "C(弱)"
        else:
            grade = "D(差)"
        return {"score": score, "grade": grade, "reasons": reasons, "ind": ind, "trend": trend}


if __name__ == "__main__":
    print(TechnicalAnalyzer().get_indicators("600519").get("signal_txt", "无数据"))
