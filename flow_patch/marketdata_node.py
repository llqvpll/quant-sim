import math
import statistics
import time
from datetime import datetime

import requests

from lfx.custom.custom_component.component import Component
from lfx.inputs.inputs import BoolInput, DropdownInput, IntInput, MessageTextInput
from lfx.schema.message import Message
from lfx.template.field.base import Output

TENCENT_KLINE_URL = "https://web.ifzq.gtimg.cn/appstock/app/fqkline/get"
TENCENT_QUOTE_URL = "https://qt.gtimg.cn/q={}"
SINA_KLINE_URL = (
    "https://money.finance.sina.com.cn/quotes_service/api/json_v2.php/CN_MarketData.getKLineData"
)
HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    "Referer": "https://gu.qq.com/",
}

# 进程内缓存: 避免同一轮扫描里反复抓同一只票
KLINE_TTL = 60.0
QUOTE_TTL = 15.0
_KLINE_CACHE = {}
_QUOTE_CACHE = {}


class MarketDataComponent(Component):
    display_name = "市场数据 (A股K线+指标)"
    description = (
        "获取A股K线(腾讯财经，失败自动切新浪)并计算 MA/EMA/MACD/RSI/ATR/布林带/量比，"
        "输出中文市场摘要。支持 as_of 截止日期，回测时只使用当日及之前的数据，消除未来函数。"
    )
    icon = "TrendingUp"
    name = "MarketData"

    inputs = [
        MessageTextInput(
            name="symbol",
            display_name="股票代码",
            value="600519",
            info="例如 600519(贵州茅台)、000001(平安银行)、300750(宁德时代)",
        ),
        DropdownInput(
            name="interval",
            display_name="K线周期",
            options=["1d", "1w"],
            value="1d",
            info="1d=日K线, 1w=周K线",
        ),
        IntInput(
            name="limit",
            display_name="K线数量",
            value=120,
            advanced=True,
            info="拉取多少根K线用于计算指标。",
        ),
        MessageTextInput(
            name="as_of",
            display_name="截止日期(回测用)",
            value="",
            advanced=True,
            info="格式 YYYY-MM-DD。留空=使用最新数据；填写后只使用该日期及之前的K线，避免回测未来函数。",
        ),
        BoolInput(
            name="include_realtime",
            display_name="附加实时快照",
            value=True,
            advanced=True,
            info="补充现价/涨跌幅/换手率/量比/涨跌停价。回测模式(as_of)下自动忽略。",
        ),
        BoolInput(
            name="demo_mode",
            display_name="演示模式（无需联网）",
            value=False,
            advanced=True,
            info="开启后使用模拟行情，便于先测试整个流程。",
        ),
    ]

    outputs = [
        Output(display_name="市场摘要", name="summary", method="build_summary"),
    ]

    # ---------- 基础工具 ----------
    @staticmethod
    def _demo_rows(limit: int):
        rows = []
        base = 1500.0
        for i in range(limit):
            t = i / 14.0
            close = base * (1 + 0.03 * math.sin(t) + 0.008 * math.sin(t * 3.7))
            open_ = rows[-1]["close"] if rows else close * 0.999
            high = max(open_, close) * 1.004
            low = min(open_, close) * 0.996
            rows.append(
                {
                    "time": datetime.now().strftime("%Y-%m-%d"),
                    "open": open_,
                    "high": high,
                    "low": low,
                    "close": close,
                    "volume": 100 + (i % 97),
                }
            )
        return rows

    @staticmethod
    def _normalize_symbol(symbol: str):
        """转成腾讯格式: 600519 -> sh600519, 000001 -> sz000001"""
        s = str(symbol or "").strip().upper()
        s = s.replace(".SH", "").replace(".SZ", "").replace(".BJ", "")
        s = s.replace("SH", "").replace("SZ", "").replace("BJ", "").strip()
        if not s.isdigit() or len(s) != 6:
            return None, f"股票代码格式不正确：{symbol}。请输入6位数字，如 600519 或 000001。"
        if s.startswith(("920", "4", "8")):
            return "bj" + s, None
        if s.startswith(("6", "9")):
            return "sh" + s, None
        if s.startswith(("0", "3")):
            return "sz" + s, None
        return None, f"无法识别的股票代码前缀：{symbol}"

    @staticmethod
    def _to_float(value, default=None):
        try:
            return float(str(value).strip())
        except (TypeError, ValueError):
            return default

    @staticmethod
    def _in_trading_hours(now=None) -> bool:
        now = now or datetime.now()
        if now.weekday() >= 5:
            return False
        minutes = now.hour * 60 + now.minute
        return (9 * 60 + 30) <= minutes <= (11 * 60 + 30) or (13 * 60) <= minutes <= (15 * 60)

    @staticmethod
    def _session_progress(now=None) -> float:
        """交易时段已走比例，用于盘中量比折算。"""
        now = now or datetime.now()
        if now.weekday() >= 5:
            return 1.0
        minutes = now.hour * 60 + now.minute + now.second / 60
        if minutes < 9 * 60 + 30:
            return 0.01
        if minutes <= 11 * 60 + 30:
            return max(0.01, min((minutes - 9 * 60 - 30) / 240.0, 1.0))
        if minutes < 13 * 60:
            return 0.5
        if minutes <= 15 * 60:
            return max(0.01, min((120 + minutes - 13 * 60) / 240.0, 1.0))
        return 1.0

    @staticmethod
    def _limit_ratio(code6: str, name: str = "") -> float:
        """按板块推断涨跌停幅度。"""
        if code6.startswith(("4", "8", "920")):
            return 0.30
        if code6.startswith(("300", "301", "302", "688", "689")):
            return 0.20
        if name and "ST" in str(name).upper():
            return 0.05
        return 0.10

    # ---------- 网络 ----------
    @staticmethod
    def _get(url, params=None, timeout=15, encoding=None):
        """带重试的 GET，失败抛异常。"""
        last_exc = None
        for attempt in range(3):
            try:
                resp = requests.get(url, params=params, headers=HEADERS, timeout=timeout)
                resp.raise_for_status()
                if encoding:
                    resp.encoding = encoding
                return resp
            except Exception as exc:  # noqa: BLE001
                last_exc = exc
                time.sleep(0.5 * (attempt + 1))
        raise RuntimeError(f"{url} 请求失败: {type(last_exc).__name__}: {last_exc}")

    def _fetch_tencent(self, tencent_code: str, period: str, limit: int, as_of: str = ""):
        payload = self._get(
            TENCENT_KLINE_URL,
            params={"param": f"{tencent_code},{period},,{as_of},{limit},qfq"},
        ).json()
        if payload.get("code") != 0:
            raise RuntimeError(f"腾讯K线接口返回错误: {payload.get('msg') or payload.get('code')}")
        stock = (payload.get("data") or {}).get(tencent_code) or {}
        klines = stock.get("qfq" + period) or stock.get(period) or []
        if not klines:
            raise RuntimeError(f"腾讯未返回 {tencent_code} 的K线")
        rows = []
        for line in klines:
            # 腾讯格式: [日期, 开, 收, 高, 低, 成交量(手), ...]
            rows.append(
                {
                    "time": str(line[0])[:10],
                    "open": float(line[1]),
                    "close": float(line[2]),
                    "high": float(line[3]),
                    "low": float(line[4]),
                    "volume": float(line[5]),
                }
            )
        if as_of:
            rows = [row for row in rows if row["time"] <= as_of]
        return rows

    def _fetch_sina(self, tencent_code: str, limit: int):
        payload = self._get(
            SINA_KLINE_URL,
            params={"symbol": tencent_code, "scale": 240, "ma": "no", "datalen": limit},
        ).json()
        if not isinstance(payload, list) or not payload:
            raise RuntimeError("新浪未返回K线")
        rows = []
        for item in payload:
            rows.append(
                {
                    "time": str(item.get("day", ""))[:10],
                    "open": float(item["open"]),
                    "close": float(item["close"]),
                    "high": float(item["high"]),
                    "low": float(item["low"]),
                    # 新浪成交量单位是股，统一换算成手
                    "volume": float(item["volume"]) / 100.0,
                }
            )
        return rows

    def _fetch_kline(self, tencent_code: str, period: str, limit: int, as_of: str = ""):
        """腾讯优先，失败自动切新浪(仅日线)。as_of 时腾讯按截止日取数。"""
        key = (tencent_code, period, limit, as_of)
        now = time.time()
        hit = _KLINE_CACHE.get(key)
        if hit and now - hit[0] < KLINE_TTL:
            return hit[1], hit[2]
        try:
            rows = self._fetch_tencent(tencent_code, period, limit, as_of)
            source = "腾讯财经(前复权)"
        except Exception as exc:  # noqa: BLE001
            errors = [f"腾讯: {exc}"]
            if period != "day":
                raise RuntimeError("；".join(errors)) from exc
            try:
                rows = self._fetch_sina(tencent_code, limit + 120 if as_of else limit)
                if as_of:
                    rows = [row for row in rows if row["time"] <= as_of][-limit:]
                source = "新浪财经(不复权)"
            except Exception as exc2:  # noqa: BLE001
                errors.append(f"新浪: {exc2}")
                raise RuntimeError("；".join(errors)) from exc2
        _KLINE_CACHE[key] = (now, rows, source)
        return rows, source

    def _fetch_quote(self, tencent_code: str):
        """腾讯实时快照，失败返回 None(不影响主流程)。"""
        now = time.time()
        hit = _QUOTE_CACHE.get(tencent_code)
        if hit and now - hit[0] < QUOTE_TTL:
            return hit[1]
        try:
            text = self._get(TENCENT_QUOTE_URL.format(tencent_code), timeout=10, encoding="gbk").text.strip()
            if '="' not in text:
                return None
            fields = text.split('="', 1)[1].rstrip('";').split("~")
            if len(fields) < 50:
                return None
            quote = {
                "name": fields[1],
                "price": self._to_float(fields[3]),
                "prev_close": self._to_float(fields[4]),
                "time": fields[30],
                "change_pct": self._to_float(fields[32]),
                "high": self._to_float(fields[33]),
                "low": self._to_float(fields[34]),
                "volume": self._to_float(fields[36]),
                "amount": self._to_float(fields[37]),
                "turnover": self._to_float(fields[38]),
                "amplitude": self._to_float(fields[43]),
                "float_mv": self._to_float(fields[44]),
                "total_mv": self._to_float(fields[45]),
                "limit_up": self._to_float(fields[47]),
                "limit_down": self._to_float(fields[48]),
                "volume_ratio": self._to_float(fields[49]),
            }
        except Exception:  # noqa: BLE001
            return None
        _QUOTE_CACHE[tencent_code] = (now, quote)
        return quote

    # ---------- 指标 ----------
    @staticmethod
    def _sma(values, period):
        if len(values) < period:
            return None
        return sum(values[-period:]) / period

    @staticmethod
    def _ema_series(values, period):
        k = 2 / (period + 1)
        ema = values[0]
        out = [ema]
        for v in values[1:]:
            ema = v * k + ema * (1 - k)
            out.append(ema)
        return out

    @classmethod
    def _ema(cls, values, period):
        if not values:
            return None
        return cls._ema_series(values, period)[-1]

    @staticmethod
    def _rsi(closes, period=14):
        if len(closes) <= period:
            return None
        gains, losses = [], []
        for i in range(1, len(closes)):
            diff = closes[i] - closes[i - 1]
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
    def _atr(highs, lows, closes, period=14):
        """Wilder ATR，用于给止损位提供依据。"""
        if len(closes) <= period:
            return None
        trs = []
        for i in range(1, len(closes)):
            trs.append(
                max(
                    highs[i] - lows[i],
                    abs(highs[i] - closes[i - 1]),
                    abs(lows[i] - closes[i - 1]),
                )
            )
        atr = sum(trs[:period]) / period
        for i in range(period, len(trs)):
            atr = (atr * (period - 1) + trs[i]) / period
        return atr

    # ---------- 主逻辑 ----------
    def build_summary(self) -> Message:
        self.status = "正在获取市场数据..."
        symbol = str(getattr(self, "symbol", "600519") or "600519").strip()
        interval = str(getattr(self, "interval", "1d") or "1d")
        limit = int(getattr(self, "limit", 120) or 120)
        as_of = str(getattr(self, "as_of", "") or "").strip()
        include_realtime = bool(getattr(self, "include_realtime", True))
        demo = bool(getattr(self, "demo_mode", False))

        if as_of:
            try:
                datetime.strptime(as_of, "%Y-%m-%d")
            except ValueError as exc:
                raise ValueError(f"as_of 格式错误：{as_of}，应为 YYYY-MM-DD。") from exc
            include_realtime = False  # 回测禁止使用实时数据，否则又是未来函数

        if demo:
            rows, source = self._demo_rows(limit), "演示数据(本地生成)"
            tencent_code = "sh600519"
        else:
            tencent_code, err = self._normalize_symbol(symbol)
            if err:
                raise ValueError(err)
            period = "day" if interval == "1d" else "week"
            try:
                rows, source = self._fetch_kline(tencent_code, period, limit, as_of)
            except Exception as exc:  # noqa: BLE001
                raise ValueError(f"行情获取失败（腾讯/新浪都不可用，已终止分析，避免让模型凭空猜测）：{exc}") from exc

        if as_of:
            rows = [r for r in rows if str(r["time"])[:10] <= as_of]
            if not rows:
                raise ValueError(f"截止 {as_of} 没有任何K线数据，请检查日期是否早于上市日期。")

        if len(rows) < 30:
            raise ValueError(f"K线数据不足（{len(rows)} 根，至少需要 30 根），请加大K线数量或调整 as_of。")

        code6 = tencent_code[2:]
        closes = [r["close"] for r in rows]
        highs = [r["high"] for r in rows]
        lows = [r["low"] for r in rows]
        volumes = [r["volume"] for r in rows]
        last = closes[-1]
        prev = closes[-2]
        change_pct = (last - prev) / prev * 100 if prev else 0.0
        period_change = (last - closes[0]) / closes[0] * 100 if closes[0] else 0.0

        # 数据新鲜度 / 停牌校验
        last_date = str(rows[-1]["time"])[:10]
        today = datetime.now().strftime("%Y-%m-%d")
        if as_of:
            fresh = f"回测模式：只用 {as_of} 及之前的K线（实际最后一根 {last_date}），不含未来数据"
        else:
            try:
                gap = (datetime.strptime(today, "%Y-%m-%d") - datetime.strptime(last_date, "%Y-%m-%d")).days
            except ValueError:
                gap = 0
            if gap <= 0:
                if self._in_trading_hours():
                    fresh = "数据日期：今天（盘中，最后一根K线尚未走完，收盘价会变）"
                else:
                    fresh = "数据日期：今天（已收盘）"
            else:
                fresh = f"数据日期：{last_date}（距今 {gap} 个自然日）"
                if gap >= 90:
                    raise ValueError(f"最新K线停留在 {last_date}（{gap} 天前），疑似长期停牌，已终止分析。")
                if gap >= 7:
                    fresh += "  ⚠ 数据明显滞后，可能停牌或数据源未更新"

        # 指标
        ma5 = self._sma(closes, 5)
        ma10 = self._sma(closes, 10)
        ma20 = self._sma(closes, 20)
        ma60 = self._sma(closes, 60)
        ema12 = self._ema(closes, 12)
        ema26 = self._ema(closes, 26)
        dif_series = [
            e12 - e26 for e12, e26 in zip(self._ema_series(closes, 12), self._ema_series(closes, 26))
        ]
        dea_series = self._ema_series(dif_series, 9)
        dif, dea = dif_series[-1], dea_series[-1]
        hist = dif - dea
        prev_hist = (dif_series[-2] - dea_series[-2]) if len(dif_series) >= 2 else None
        if prev_hist is not None and hist > 0 >= prev_hist:
            macd_state = "刚金叉"
        elif prev_hist is not None and hist < 0 <= prev_hist:
            macd_state = "刚死叉"
        elif hist > 0:
            macd_state = "多头（DIF在DEA上方）"
        else:
            macd_state = "空头（DIF在DEA下方）"
        if prev_hist is not None:
            macd_state += "，柱比前一根" + ("放大" if abs(hist) > abs(prev_hist) else "缩小")

        rsi14 = self._rsi(closes, 14)
        atr14 = self._atr(highs, lows, closes, 14)
        mean20 = None
        boll_upper = boll_lower = boll_width = None
        if len(closes) >= 20:
            window = closes[-20:]
            mean20 = sum(window) / 20
            std = statistics.pstdev(window)
            boll_upper = mean20 + 2 * std
            boll_lower = mean20 - 2 * std
            boll_width = (boll_upper - boll_lower) / mean20 * 100 if mean20 else None
        high20 = max(highs[-20:])
        low20 = min(lows[-20:])

        # 均线排列
        if None not in (ma5, ma10, ma20, ma60):
            if ma5 > ma10 > ma20 > ma60:
                arrange = "多头排列"
            elif ma5 < ma10 < ma20 < ma60:
                arrange = "空头排列"
            else:
                arrange = "均线纠缠（无明显趋势）"
        else:
            arrange = "均线数据不足60根"

        vol_ratio_kline = volumes[-1] / max(sum(volumes[-6:-1]) / 5, 1e-9) if len(volumes) >= 6 else None
        if vol_ratio_kline is not None and not as_of and last_date == today:
            # 盘中最后一根 K 线成交量不完整，按已走交易时段折算
            progress = self._session_progress()
            if 0 < progress < 1:
                vol_ratio_kline = vol_ratio_kline / max(progress, 0.05)

        def fmt(v, digits=2):
            return "None" if v is None else f"{v:.{digits}f}"

        def money(v):
            if v is None:
                return "None"
            return f"{v / 10000:.2f}万手" if v >= 10000 else f"{v:.0f}手"

        quote = self._fetch_quote(tencent_code) if include_realtime else None
        name = (quote or {}).get("name") or ""

        lines = [
            f"股票代码：{symbol}{('（' + name + '）') if name else ''}｜数据源：{source}｜周期：{'日K' if interval == '1d' else '周K'}｜共 {len(rows)} 根K线",
            f"数据新鲜度：{fresh}",
            f"最新收盘：{fmt(last)}｜单日涨跌：{change_pct:+.2f}%｜区间涨跌：{period_change:+.2f}%",
            f"区间最高：{fmt(max(highs))}　区间最低：{fmt(min(lows))}　20日最高：{fmt(high20)}　20日最低：{fmt(low20)}",
            f"均线：MA5={fmt(ma5)} MA10={fmt(ma10)} MA20={fmt(ma20)} MA60={fmt(ma60)}　排列：{arrange}",
            f"EMA12={fmt(ema12)}　EMA26={fmt(ema26)}",
            f"MACD：DIF={fmt(dif, 3)} DEA={fmt(dea, 3)} 柱={fmt(hist, 3)}（{macd_state}）",
            f"RSI14={fmt(rsi14, 1)}（>70 超买，<30 超卖）",
            f"ATR14={fmt(atr14)}（约合现价 {fmt(atr14 / last * 100, 2) if last else '0'}%，可作为止损幅度参考）",
            f"布林带：上轨={fmt(boll_upper)} 中轨={fmt(mean20)} 下轨={fmt(boll_lower)}　带宽={fmt(boll_width, 2)}%",
            f"量比(K线口径)={fmt(vol_ratio_kline)}　当日成交量={money(volumes[-1])}",
        ]

        ratio = self._limit_ratio(code6, name)
        if quote and quote.get("prev_close"):
            lines.append(
                "实时快照：现价={} 涨跌={:+.2f}% 换手率={}% 量比={} 振幅={}% 涨停价={} 跌停价={} 流通市值={}亿".format(
                    fmt(quote.get("price")),
                    quote.get("change_pct") or 0.0,
                    fmt(quote.get("turnover")),
                    fmt(quote.get("volume_ratio")),
                    fmt(quote.get("amplitude")),
                    fmt(quote.get("limit_up")),
                    fmt(quote.get("limit_down")),
                    fmt(quote.get("float_mv")),
                )
            )
            lines.append(f"该股涨跌停幅度按板块/名称推断为 ±{ratio * 100:.0f}%（ST股为 ±5%）")
        elif not as_of:
            ref = last
            lines.append(
                f"实时快照不可用（实时接口失败）。按 ±{ratio * 100:.0f}% 估算：涨停约 {ref * (1 + ratio):.2f}，跌停约 {ref * (1 - ratio):.2f}"
            )
        else:
            lines.append(
                f"回测模式：未使用实时数据。按 ±{ratio * 100:.0f}% 估算：下一交易日涨停约 {last * (1 + ratio):.2f}，跌停约 {last * (1 - ratio):.2f}"
            )

        lines.append("最近K线（日期 开 高 低 收 量，单位手）：")
        for r in rows[-6:]:
            lines.append(
                f"  {r['time']}  O{r['open']:.2f} H{r['high']:.2f} L{r['low']:.2f} "
                f"C{r['close']:.2f} V{r['volume']:.0f}"
            )

        text = "\n".join(lines)
        self.status = text[:200] + ("..." if len(text) > 200 else "")
        return Message(text=text)
