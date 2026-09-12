# -*- coding: utf-8 -*-
"""
A股行情数据源
=============
腾讯财经接口(国内直连, requests 实测可通)：
- 实时行情(单只) / 批量行情(多只)
- 日K线 / 周K线(用于 AI 分析前补数据, 计算技术指标)
- 沪深两市全部 A 股列表(用于全市场选股)

用法:
    from market_data import MarketData

    md = MarketData()
    quote = md.get_realtime("600519")     # 单只实时
    quotes = md.get_realtime_batch(["600519","000001"])  # 批量
    kline = md.get_kline("600519", "1d", 120)  # 日K线
    all_stocks = md.get_all_a_stocks()    # 全市场列表
"""

import time

import requests

# 腾讯行情接口
QT_URL = "https://qt.gtimg.cn/q={}"
# 腾讯K线接口
KLINE_URL = "https://web.ifzq.gtimg.cn/appstock/app/fqkline/get"
# 新浪全市场列表(requests 实测可通, 替代东财)
STOCK_LIST_URL = "https://vip.stock.finance.sina.com.cn/quotes_service/api/json_v2.php/Market_Center.getHQNodeData"

HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    "Referer": "https://gu.qq.com/",
}


class MarketData:
    """A股行情数据源(腾讯财经, 国内直连)"""

    # ---------- 实时行情 ----------
    @staticmethod
    def get_realtime(symbol: str) -> dict:
        """获取单只股票实时行情(腾讯)"""
        code = MarketData._to_tencent_code(symbol)
        if not code:
            return {}
        try:
            r = requests.get(QT_URL.format(code), headers=HEADERS, timeout=10)
            r.encoding = "gbk"
            text = r.text.strip()
            if '="' not in text:
                return {}
            payload = text.split('="', 1)[1].rstrip('";')
            fields = payload.split("~")
            if len(fields) < 40:
                return {}
            # 腾讯字段: 1名称 2代码 3当前价 4昨收 5今开 6成交量(手) 30时间 ...
            return {
                "code": symbol,
                "name": fields[1],
                "price": float(fields[3] or 0),
                "prev_close": float(fields[4] or 0),
                "open": float(fields[5] or 0),
                "volume": float(fields[6] or 0),  # 手
                "high": float(fields[33] or 0),
                "low": float(fields[34] or 0),
                "change_pct": float(fields[32] or 0),  # 涨跌幅%
                "time": fields[30],
            }
        except Exception:
            return {}

    @staticmethod
    def get_realtime_batch(symbols: list) -> dict:
        """批量获取实时行情, 返回 {code: quote}"""
        if not symbols:
            return {}
        codes = [MarketData._to_tencent_code(s) for s in symbols]
        codes = [c for c in codes if c]
        result = {}
        # 每 50 个一批, 避免 URL 过长
        for i in range(0, len(codes), 50):
            batch = codes[i:i + 50]
            try:
                r = requests.get(QT_URL.format(",".join(batch)), headers=HEADERS, timeout=10)
                r.encoding = "gbk"
                for line in r.text.strip().splitlines():
                    if '="' not in line:
                        continue
                    key = line.split("=")[0].strip().split("_")[-1]  # v_sh600519
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
            except Exception:
                continue
        return result

    # ---------- K线 ----------
    @staticmethod
    def get_kline(symbol: str, period: str = "1d", limit: int = 120) -> list:
        """获取K线(腾讯), 返回 [{date, open, close, high, low, volume}]"""
        code = MarketData._to_tencent_code(symbol)
        if not code:
            return []
        period_key = "day" if period == "1d" else "week"
        try:
            r = requests.get(KLINE_URL,
                             params={"param": f"{code},{period_key},,,{limit},qfq"},
                             headers=HEADERS, timeout=15)
            data = r.json().get("data", {}).get(code, {})
            key = f"qfq{period_key}" if f"qfq{period_key}" in data else period_key
            klines = data.get(key) or []
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
            return rows
        except Exception:
            return []

    # ---------- 全市场股票列表 ----------
    @staticmethod
    def get_all_a_stocks() -> list:
        """获取沪深 A 股列表(新浪, requests 实测可通), 返回 [{code, name, price, change_pct}]"""
        stocks = []
        for page in range(1, 6):  # 前5页, 每页100, 按涨跌幅排序
            try:
                params = {
                    "page": str(page), "num": "100",
                    "sort": "changepercent", "asc": "0", "node": "hs_a",
                }
                r = requests.get(STOCK_LIST_URL, params=params,
                                 headers={"User-Agent": "Mozilla/5.0", "Referer": "https://finance.sina.com.cn/"},
                                 timeout=15)
                r.encoding = "utf-8"
                text = r.text.strip()
                if not text or text == "null":
                    break
                # 新浪返回 JSON 数组: [{"symbol":"sh688835","code":"688835","name":"...","trade":"...","changepercent":...},...]
                try:
                    items = r.json()
                except Exception:
                    # 兼容非标准 JSON (键无引号等)
                    text_clean = text.replace("null", "None")
                    items = eval(text_clean, {"__builtins__": {}}) if text_clean.startswith("[") else []
                for item in (items or []):
                    code = str(item.get("code", ""))
                    name = item.get("name", "")
                    if code and name:
                        try:
                            price = float(item.get("trade") or 0)
                            change_pct = float(item.get("changepercent") or 0)
                        except (TypeError, ValueError):
                            price, change_pct = 0.0, 0.0
                        stocks.append({"code": code, "name": name, "price": price, "change_pct": change_pct})
            except Exception:
                continue
        return stocks

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