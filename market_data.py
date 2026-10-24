# -*- coding: utf-8 -*-
"""A 股行情数据源（腾讯 + 新浪列表，标准库实现）。

v2 修复：
- 全市场列表翻页到空页为止，不再固定 30 页/3000 只；去重
- 北交所 920xxx 正确映射为 bj920xxx
- 实时行情按代码 TTL 缓存，批量接口只补缺失项
- K 线支持 end_date（回测 as_of），避免“先取最近 120 根再过滤”导致指标为空
- 行情解析异常不再让整轮扫描崩溃
"""
from __future__ import annotations

import json
import math
import time
from datetime import datetime
from typing import Dict, List, Optional

import http_util
from trading_calendar import is_trading_time

QT_URL = "https://qt.gtimg.cn/q={}"
KLINE_URL = "https://web.ifzq.gtimg.cn/appstock/app/fqkline/get"
SINA_KLINE_URL = "https://money.finance.sina.com.cn/quotes_service/api/json_v2.php/CN_MarketData.getKLineData"
STOCK_LIST_URL = "https://vip.stock.finance.sina.com.cn/quotes_service/api/json_v2.php/Market_Center.getHQNodeData"
STOCK_LIST_URL_BAK = "https://quotes.sina.cn/cn/api/json_v2.php/CN_MarketDataService.getHQNodeData"

STOCK_LIST_HEADERS = {"User-Agent": "Mozilla/5.0", "Referer": "https://finance.sina.com.cn/"}
QT_HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    "Referer": "https://gu.qq.com/",
}

KLINE_TTL = 60.0
QUOTE_TTL = 15.0
LIST_TTL = 600.0
MAX_LIST_PAGES = 60
PAGE_SIZE = 100

_KLINE_CACHE: Dict[tuple, tuple] = {}
_QUOTE_CACHE: Dict[str, tuple] = {}
_LIST_CACHE: Dict[str, object] = {"ts": 0.0, "data": []}


class MarketData:
    """行情数据源。"""

    # ---------- 实时行情 ----------
    @staticmethod
    def _parse_quote(symbol: str, payload: str) -> dict:
        fields = payload.split("~")
        if len(fields) < 40:
            return {}
        def num(index: int, default: float = 0.0) -> float:
            try:
                value = float(fields[index])
                return value if math.isfinite(value) else default
            except (IndexError, TypeError, ValueError):
                return default
        price = num(3)
        if price <= 0:
            return {}
        return {
            "code": symbol,
            "name": fields[1],
            "price": price,
            "prev_close": num(4),
            "open": num(5),
            "volume": num(6),
            "high": num(33),
            "low": num(34),
            "change_pct": num(32),
            "time": fields[30] if len(fields) > 30 else "",
            "turnover": num(38),
            "limit_up": num(47),
            "limit_down": num(48),
            "volume_ratio": num(49),
        }

    @staticmethod
    def get_index_quote() -> dict:
        """上证指数快照，用于判断今天是否真的开市（节假日/停市）。"""
        key = "sh000001"
        now = time.time()
        hit = _QUOTE_CACHE.get(key)
        if hit and now - hit[0] < QUOTE_TTL:
            return hit[1]
        try:
            text = http_util.get_text(QT_URL.format(key), headers=QT_HEADERS, encoding="gbk", timeout=10)
            if '="' not in text:
                return {}
            quote = MarketData._parse_quote("000001", text.split('="', 1)[1].rstrip('";'))
        except Exception:
            return {}
        _QUOTE_CACHE[key] = (now, quote)
        return quote

    @staticmethod
    def market_is_open(now: Optional[datetime] = None) -> bool:
        """真实开市判断：交易时段 + 指数快照日期是今天。"""
        now = now or datetime.now()
        if not is_trading_time(now):
            return False
        quote = MarketData.get_index_quote()
        stamp = str(quote.get("time") or "")
        if len(stamp) < 8:
            return False
        quote_day = f"{stamp[0:4]}-{stamp[4:6]}-{stamp[6:8]}"
        return quote_day == now.date().isoformat()

    @staticmethod
    def get_realtime(symbol: str) -> dict:
        code = MarketData._to_tencent_code(symbol)
        if not code:
            return {}
        now = time.time()
        hit = _QUOTE_CACHE.get(code)
        if hit and now - hit[0] < QUOTE_TTL:
            return hit[1]
        try:
            text = http_util.get_text(QT_URL.format(code), headers=QT_HEADERS, encoding="gbk", timeout=10)
            if '="' not in text:
                return {}
            quote = MarketData._parse_quote(str(symbol), text.split('="', 1)[1].rstrip('";'))
        except Exception:
            return {}
        if quote:
            _QUOTE_CACHE[code] = (now, quote)
        return quote

    @staticmethod
    def get_realtime_batch(symbols: list) -> dict:
        """批量行情；先查缓存，只请求缺失/过期的代码。"""
        if not symbols:
            return {}
        wanted = [str(s) for s in symbols]
        result: Dict[str, dict] = {}
        missing: List[str] = []
        now = time.time()
        for symbol in wanted:
            code = MarketData._to_tencent_code(symbol)
            if not code:
                continue
            hit = _QUOTE_CACHE.get(code)
            if hit and now - hit[0] < QUOTE_TTL:
                result[code[2:]] = hit[1]
            else:
                missing.append(code)
        codes = sorted(set(missing))
        if not codes:
            return result

        batches = [codes[i:i + 50] for i in range(0, len(codes), 50)]
        tasks = [{"url": QT_URL.format(",".join(batch)), "headers": QT_HEADERS,
                  "encoding": "gbk", "timeout": 10} for batch in batches]
        texts = http_util.fetch_many(tasks, max_workers=min(8, len(tasks))) if tasks else []
        stamp = time.time()
        for text in texts:
            if not text:
                continue
            for line in text.strip().splitlines():
                if '="' not in line:
                    continue
                key = line.split("=", 1)[0].strip().split("_")[-1]
                code = key[2:] if key.startswith(("sh", "sz", "bj")) else key
                payload = line.split('="', 1)[1].rstrip('";')
                quote = MarketData._parse_quote(code, payload)
                if quote:
                    result[code] = quote
                    _QUOTE_CACHE[key] = (stamp, quote)
        return result

    # ---------- K 线 ----------
    @staticmethod
    def get_kline(symbol: str, period: str = "1d", limit: int = 120,
                  end_date: Optional[str] = None) -> list:
        """获取 K 线；end_date='YYYY-MM-DD' 时只取该日及之前的 K 线。"""
        code = MarketData._to_tencent_code(symbol)
        if not code:
            return []
        period_key = "day" if period in ("1d", "day") else "week"
        try:
            limit = max(30, min(int(limit), 640))
        except (TypeError, ValueError):
            limit = 120
        end = str(end_date or "")[:10]
        cache_key = (code, period_key, limit, end)
        now = time.time()
        hit = _KLINE_CACHE.get(cache_key)
        if hit and now - hit[0] < KLINE_TTL:
            return hit[1]
        try:
            data = http_util.get_json(
                KLINE_URL,
                params={"param": f"{code},{period_key},,{end},{limit},qfq"},
                headers=QT_HEADERS, timeout=15,
            )
            stock = (data.get("data") or {}).get(code) or {}
            klines = stock.get(f"qfq{period_key}") or stock.get(period_key) or []
            rows = []
            for line in klines:
                try:
                    rows.append({
                        "date": str(line[0])[:10],
                        "open": float(line[1]),
                        "close": float(line[2]),
                        "high": float(line[3]),
                        "low": float(line[4]),
                        "volume": float(line[5]),
                    })
                except (IndexError, TypeError, ValueError):
                    continue
            if end:
                rows = [row for row in rows if row["date"] <= end]
        except Exception:
            rows = []
        # 北交所/部分标的腾讯日 K 不足，用新浪日线兜底
        if len(rows) < 30 and period_key == "day":
            rows = MarketData._get_sina_kline(code, limit + (120 if end else 0), end)
        _KLINE_CACHE[cache_key] = (now, rows)
        return rows

    @staticmethod
    def _get_sina_kline(code: str, limit: int, end: str = "") -> list:
        try:
            payload = http_util.get_json(
                SINA_KLINE_URL,
                params={"symbol": code, "scale": 240, "ma": "no",
                        "datalen": max(30, min(int(limit), 1023))},
                headers=STOCK_LIST_HEADERS, timeout=15,
            )
            if not isinstance(payload, list):
                return []
            rows = []
            for item in payload:
                try:
                    rows.append({
                        "date": str(item.get("day", ""))[:10],
                        "open": float(item["open"]),
                        "close": float(item["close"]),
                        "high": float(item["high"]),
                        "low": float(item["low"]),
                        "volume": float(item["volume"]) / 100.0,  # 新浪成交量单位为股 -> 手
                    })
                except (KeyError, TypeError, ValueError):
                    continue
            if end:
                rows = [row for row in rows if row["date"] <= end]
            return rows[-max(30, int(limit)):] if end else rows
        except Exception:
            return []

    # ---------- 全市场列表 ----------
    @staticmethod
    def get_all_a_stocks(force: bool = False) -> list:
        """全部沪深京 A 股；翻页到空页，去重。"""
        if not force and _LIST_CACHE["data"] and (time.time() - _LIST_CACHE["ts"]) < LIST_TTL:
            return list(_LIST_CACHE["data"])

        stocks: List[dict] = []
        seen = set()
        pages = range(1, MAX_LIST_PAGES + 1)
        tasks = [{
            "url": STOCK_LIST_URL,
            "params": {"page": str(page), "num": str(PAGE_SIZE), "sort": "changepercent",
                       "asc": "0", "node": "hs_a"},
            "headers": STOCK_LIST_HEADERS, "timeout": 15,
        } for page in pages]
        results = http_util.fetch_many(tasks, max_workers=8)
        for text in results:
            items = MarketData._parse_sina_list(text)
            if not items:
                continue
            MarketData._append_list_items(stocks, seen, items)
            if len(items) < PAGE_SIZE:
                break

        if not stocks:
            # 主域名整体失败时用备用域名再试前 20 页
            for page in range(1, 21):
                try:
                    text = http_util.get_text(
                        STOCK_LIST_URL_BAK,
                        params={"page": str(page), "num": str(PAGE_SIZE),
                                "sort": "changepercent", "asc": "0", "node": "hs_a"},
                        headers=STOCK_LIST_HEADERS, timeout=15,
                    )
                except Exception:
                    break
                items = MarketData._parse_sina_list(text)
                if not items:
                    break
                MarketData._append_list_items(stocks, seen, items)
                if len(items) < PAGE_SIZE:
                    break

        if len(stocks) < 1000:
            print(f"[行情] 警告：股票列表仅 {len(stocks)} 只，数据源可能不完整")
        _LIST_CACHE["ts"] = time.time()
        _LIST_CACHE["data"] = list(stocks)
        return stocks

    @staticmethod
    def _parse_sina_list(text: str):
        text = (text or "").strip()
        if not text or text in ("null", "[]"):
            return []
        try:
            value = json.loads(text)
        except ValueError:
            return []
        return value if isinstance(value, list) else []

    @staticmethod
    def _append_list_items(stocks: List[dict], seen: set, items: list):
        for item in items or []:
            if not isinstance(item, dict):
                continue
            code = str(item.get("code", ""))
            name = str(item.get("name", ""))
            if not code or not name or code in seen:
                continue
            try:
                price = float(item.get("trade") or 0)
                change_pct = float(item.get("changepercent") or 0)
            except (TypeError, ValueError):
                price, change_pct = 0.0, 0.0
            seen.add(code)
            stocks.append({"code": code, "name": name, "price": price, "change_pct": change_pct})

    # ---------- 工具 ----------
    @staticmethod
    def _to_tencent_code(symbol: str) -> str:
        """600519 -> sh600519, 000001 -> sz000001, 920285 -> bj920285。"""
        text = str(symbol or "").strip().upper()
        text = text.replace(".SH", "").replace(".SZ", "").replace(".BJ", "")
        text = text.replace("SH", "").replace("SZ", "").replace("BJ", "").strip()
        if not text.isdigit() or len(text) != 6:
            return ""
        if text.startswith(("920", "4", "8")):
            return "bj" + text
        if text.startswith(("6", "9")):
            return "sh" + text
        if text.startswith(("0", "3")):
            return "sz" + text
        return ""
