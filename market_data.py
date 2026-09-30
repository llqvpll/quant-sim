# -*- coding: utf-8 -*-
"""
A股行情数据源
=============
腾讯财经接口(国内直连, 标准库 urllib 实现, 零第三方依赖)：
- 实时行情(单只) / 批量行情(多只)
- 日K线 / 周K线(用于 AI 分析前补数据, 计算技术指标)
- 沪深两市全部 A 股列表(用于全市场选股)

性能: K线/行情/列表均有 TTL 缓存, 列表分页与批量行情并发抓取, 降低扫描延迟。

用法:
    from market_data import MarketData

    md = MarketData()
    quote = md.get_realtime("600519")     # 单只实时
    quotes = md.get_realtime_batch(["600519","000001"])  # 批量
    kline = md.get_kline("600519", "1d", 120)  # 日K线
    all_stocks = md.get_all_a_stocks()    # 全市场列表
"""

import time

import http_util

# 腾讯行情接口
QT_URL = "https://qt.gtimg.cn/q={}"
# 腾讯K线接口
KLINE_URL = "https://web.ifzq.gtimg.cn/appstock/app/fqkline/get"
# 新浪全市场列表
STOCK_LIST_URL = "https://vip.stock.finance.sina.com.cn/quotes_service/api/json_v2.php/Market_Center.getHQNodeData"
# 新浪列表备用域名(提高可用性)
STOCK_LIST_URL_BAK = "https://quotes.sina.cn/cn/api/json_v2.php/CN_MarketDataService.getHQNodeData"

STOCK_LIST_HEADERS = {"User-Agent": "Mozilla/5.0", "Referer": "https://finance.sina.com.cn/"}
QT_HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    "Referer": "https://gu.qq.com/",
}

# ---------- 进程内缓存(TTL): 避免一轮扫描内反复抓同一只票, 显著降延迟 ----------
KLINE_TTL = 60.0
QUOTE_TTL = 15.0
LIST_TTL = 300.0
_KLINE_CACHE = {}
_QUOTE_CACHE = {}
_LIST_CACHE = {"ts": 0.0, "data": []}


class MarketData:
    """A股行情数据源(腾讯财经, 国内直连, 零第三方依赖)"""

    # ---------- 实时行情 ----------
    @staticmethod
    def get_realtime(symbol: str) -> dict:
        """获取单只股票实时行情(腾讯)"""
        code = MarketData._to_tencent_code(symbol)
        if not code:
            return {}
        try:
            text = http_util.get_text(QT_URL.format(code), headers=QT_HEADERS,
                                      encoding="gbk", timeout=10)
            if '="' not in text:
                return {}
            payload = text.split('="', 1)[1].rstrip('";')
            fields = payload.split("~")
            if len(fields) < 40:
                return {}
            return {
                "code": symbol,
                "name": fields[1],
                "price": float(fields[3] or 0),
                "prev_close": float(fields[4] or 0),
                "open": float(fields[5] or 0),
                "volume": float(fields[6] or 0),
                "high": float(fields[33] or 0),
                "low": float(fields[34] or 0),
                "change_pct": float(fields[32] or 0),
                "time": fields[30],
            }
        except Exception:
            return {}

    @staticmethod
    def get_realtime_batch(symbols: list) -> dict:
        """批量获取实时行情(并发抓取, 返回 {code: quote})"""
        if not symbols:
            return {}
        codes = [MarketData._to_tencent_code(s) for s in symbols]
        codes = [c for c in codes if c]
        result = {}
        # 每 50 个一批, 各批并发抓取(IO 密集型, 提速明显)
        batches = [codes[i:i + 50] for i in range(0, len(codes), 50)]
        tasks = [{"url": QT_URL.format(",".join(b)), "headers": QT_HEADERS,
                  "encoding": "gbk", "timeout": 10} for b in batches]
        texts = http_util.fetch_many(tasks, max_workers=min(8, len(tasks))) if tasks else []
        for text in texts:
            if not text:
                continue
            for line in text.strip().splitlines():
                if '="' not in line:
                    continue
                key = line.split("=")[0].strip().split("_")[-1]
                code = key[2:] if key.startswith(("sh", "sz", "bj")) else key
                payload = line.split('="', 1)[1].rstrip('";')
                fields = payload.split("~")
                if len(fields) < 40:
                    continue
                result[code] = {
                    "code": code,
                    "name": fields[1],
                    "price": float(fields[3] or 0),
                    "prev_close": float(fields[4] or 0),
                    "open": float(fields[5] or 0),
                    "volume": float(fields[6] or 0),
                    "high": float(fields[33] or 0),
                    "low": float(fields[34] or 0),
                    "change_pct": float(fields[32] or 0),
                    "time": fields[30],
                }
        return result

    # ---------- K线 ----------
    @staticmethod
    def get_kline(symbol: str, period: str = "1d", limit: int = 120) -> list:
        """获取K线(腾讯, 带 TTL 缓存), 返回 [{date, open, close, high, low, volume}]"""
        code = MarketData._to_tencent_code(symbol)
        if not code:
            return []
        period_key = "day" if period == "1d" else "week"
        cache_key = (code, period_key, limit)
        now = time.time()
        hit = _KLINE_CACHE.get(cache_key)
        if hit and now - hit[0] < KLINE_TTL:
            return hit[1]
        try:
            data = http_util.get_json(KLINE_URL,
                                     params={"param": f"{code},{period_key},,,{limit},qfq"},
                                     headers=QT_HEADERS, timeout=15)
            stock = (data.get("data") or {}).get(code) or {}
            klines = stock.get(f"qfq{period_key}") or stock.get(period_key) or []
            rows = []
            for line in klines:
                rows.append({
                    "date": line[0],
                    "open": float(line[1]),
                    "close": float(line[2]),
                    "high": float(line[3]),
                    "low": float(line[4]),
                    "volume": float(line[5]),
                })
            _KLINE_CACHE[cache_key] = (now, rows)
            return rows
        except Exception:
            return []

    # ---------- 全市场股票列表 ----------
    @staticmethod
    def get_all_a_stocks() -> list:
        """获取沪深 A 股列表(新浪, 并发分页, 失败自动切备用域名), 返回 [{code,name,price,change_pct}]"""
        # 缓存: 列表变化慢, 5 分钟有效, 避免每轮扫描都抓全市场
        if _LIST_CACHE["data"] and (time.time() - _LIST_CACHE["ts"]) < LIST_TTL:
            return _LIST_CACHE["data"]
        stocks = []
        pages = list(range(1, 31))  # 最多 30 页(约 3000 只), 提前取空则停止
        tasks = [{"url": STOCK_LIST_URL,
                  "params": {"page": str(p), "num": "100", "sort": "changepercent",
                             "asc": "0", "node": "hs_a"},
                  "headers": STOCK_LIST_HEADERS, "timeout": 15} for p in pages]
        results = http_util.fetch_many(tasks, max_workers=8)
        for text in results:
            if not text or text.strip() in ("", "null"):
                continue
            try:
                items = MarketData._parse_sina_list(text)
            except Exception:
                items = None
            if not items:
                continue
            if len(items) < 50:  # 末页, 标记停止信号
                MarketData._append_list_items(stocks, items)
                break
            MarketData._append_list_items(stocks, items)
        # 若新浪主域名全失败, 尝试备用域名补充(最多再 10 页)
        if not stocks:
            for p in range(1, 11):
                try:
                    text = http_util.get_text(STOCK_LIST_URL_BAK,
                                              params={"page": str(p), "num": "100",
                                                      "sort": "changepercent", "asc": "0",
                                                      "node": "hs_a"},
                                              headers=STOCK_LIST_HEADERS, timeout=15)
                    items = MarketData._parse_sina_list(text) if text and text.strip() not in ("", "null") else None
                except Exception:
                    items = None
                if not items:
                    break
                MarketData._append_list_items(stocks, items)
                if len(items) < 50:
                    break
        _LIST_CACHE["ts"] = time.time()
        _LIST_CACHE["data"] = stocks
        return stocks

    @staticmethod
    def _parse_sina_list(text: str):
        """解析新浪列表 JSON(容错: 非标准 JSON 也尽量解析)"""
        text = (text or "").strip()
        try:
            return json_safe_loads(text)
        except Exception:
            return None

    @staticmethod
    def _append_list_items(stocks: list, items: list):
        for item in (items or []):
            code = str(item.get("code", ""))
            name = item.get("name", "")
            if code and name:
                try:
                    price = float(item.get("trade") or 0)
                    change_pct = float(item.get("changepercent") or 0)
                except (TypeError, ValueError):
                    price, change_pct = 0.0, 0.0
                stocks.append({"code": code, "name": name, "price": price,
                               "change_pct": change_pct})

    # ---------- 工具 ----------
    @staticmethod
    def _to_tencent_code(symbol: str) -> str:
        """600519 -> sh600519, 000001 -> sz000001"""
        s = str(symbol or "").strip().upper()
        s = s.replace(".SH", "").replace(".SZ", "").replace(".BJ", "")
        s = s.replace("SH", "").replace("SZ", "").replace("BJ", "").strip()
        if not s.isdigit() or len(s) != 6:
            return ""
        if s.startswith(("6", "9")):
            return "sh" + s
        if s.startswith(("0", "3")):
            return "sz" + s
        if s.startswith(("4", "8")):
            return "bj" + s
        return ""


def json_safe_loads(text: str):
    """安全解析 JSON: 先标准解析, 失败再处理 'null'/单引号等非标准情况"""
    import json
    try:
        return json.loads(text)
    except Exception:
        cleaned = text.replace("null", "None").replace("'", '"')
        try:
            return json.loads(cleaned)
        except Exception:
            return None


if __name__ == "__main__":
    md = MarketData()
    q = md.get_realtime("600519")
    print("600519 实时:", q.get("name"), q.get("price"), f"{q.get('change_pct')}%", q.get("time"))

    batch = md.get_realtime_batch(["600519", "000001", "300750"])
    print("批量行情:", {k: v.get("price") for k, v in batch.items()})

    k = md.get_kline("600519", "1d", 5)
    print("600519 最近K线:", [(r["date"], r["close"]) for r in k])

    stocks = md.get_all_a_stocks()
    print("全市场股票数:", len(stocks), "前5:", stocks[:5])
