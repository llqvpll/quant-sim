# -*- coding: utf-8 -*-
"""A 股交易规则：板块、手数、涨跌停、T+1。

这些规则在模拟器和回测里都必须生效，否则回测收益没有意义。
规则按 2026 年常见口径实现：
- 主板：100 股整数倍，涨跌停 ±10%，ST ±5%
- 创业板/科创板：100/200 股起，涨跌停 ±20%（ST 同样 ±20%）
- 北交所（4/8/920）：100 股起、1 股递增，涨跌停 ±30%
- T+1：买入当日不可卖出
"""
from __future__ import annotations

import re
from datetime import date, datetime
from decimal import Decimal, ROUND_HALF_UP
from typing import Optional, Tuple

from trading_calendar import next_trading_day

ST_RE = re.compile(r"^(?:\*?ST|S\*ST|SST)")
TICK = Decimal("0.01")


class MarketRuleError(ValueError):
    """交易规则不满足。"""


def code_digits(code: str) -> str:
    """从 600519 / sh600519 / 600519.SH 中提取 6 位代码。"""
    text = str(code or "").strip().upper()
    text = text.replace(".SH", "").replace(".SZ", "").replace(".BJ", "")
    text = text.replace("SH", "").replace("SZ", "").replace("BJ", "")
    match = re.search(r"(\d{6})", text)
    return match.group(1) if match else ""


def board(code: str) -> str:
    """返回 main / star / chinext / bse / unknown。"""
    digits = code_digits(code)
    if not digits:
        return "unknown"
    if digits.startswith(("688", "689")):
        return "star"
    if digits.startswith(("300", "301", "302")):
        return "chinext"
    if digits.startswith(("4", "8", "920")):
        return "bse"
    if digits.startswith(("6", "0", "3")):
        return "main"
    return "unknown"


def board_name(code: str) -> str:
    return {
        "main": "主板", "star": "科创板", "chinext": "创业板",
        "bse": "北交所", "unknown": "未知",
    }[board(code)]


def is_st_name(name: str) -> bool:
    return bool(ST_RE.match(str(name or "").strip().upper().replace(" ", "")))


def price_limit_ratio(code: str, name: str = "") -> float:
    """按板块/ST 返回涨跌停比例。"""
    kind = board(code)
    if kind == "bse":
        return 0.30
    if kind in ("star", "chinext"):
        return 0.20
    if is_st_name(name):
        return 0.05
    return 0.10


def round_price(value: float) -> float:
    return float(Decimal(str(value)).quantize(TICK, rounding=ROUND_HALF_UP))


def limit_prices(prev_close: float, code: str, name: str = "") -> Tuple[float, float]:
    """返回 (跌停价, 涨停价)；prev_close 无效时返回 (0, 0)。"""
    try:
        prev = float(prev_close)
    except (TypeError, ValueError):
        return 0.0, 0.0
    if prev <= 0:
        return 0.0, 0.0
    ratio = price_limit_ratio(code, name)
    upper = round_price(prev * (1 + ratio))
    lower = round_price(prev * (1 - ratio))
    return max(lower, 0.01), upper


def check_price_limit(price: float, prev_close: Optional[float], code: str,
                      name: str = "") -> Tuple[bool, str]:
    """检查委托价是否在涨跌停范围内；没有 prev_close 时放行。"""
    if not prev_close or float(prev_close) <= 0:
        return True, ""
    try:
        value = float(price)
    except (TypeError, ValueError):
        return False, "价格不是数字"
    lower, upper = limit_prices(float(prev_close), code, name)
    if upper <= 0:
        return True, ""
    if value > upper + 1e-9:
        return False, f"买入价 {value:.2f} 高于涨停价 {upper:.2f}"
    if value < lower - 1e-9:
        return False, f"卖出价 {value:.2f} 低于跌停价 {lower:.2f}"
    return True, ""


def at_limit_up(price: float, prev_close: Optional[float], code: str, name: str = "") -> bool:
    """是否处于涨停价（涨停通常买不进）。"""
    if not prev_close or float(prev_close) <= 0:
        return False
    _, upper = limit_prices(float(prev_close), code, name)
    return upper > 0 and float(price) >= upper - 1e-9


def at_limit_down(price: float, prev_close: Optional[float], code: str, name: str = "") -> bool:
    """是否处于跌停价（跌停通常卖不出）。"""
    if not prev_close or float(prev_close) <= 0:
        return False
    lower, _ = limit_prices(float(prev_close), code, name)
    return lower > 0 and float(price) <= lower + 1e-9


def normalize_buy_shares(code: str, shares: int) -> int:
    """按板块规则把买入股数规整为合法值；买不起最低单位时返回 0。"""
    try:
        value = int(shares)
    except (TypeError, ValueError):
        return 0
    if value <= 0:
        return 0
    kind = board(code)
    if kind == "star":
        return value if value >= 200 else 0
    if kind == "bse":
        return value if value >= 100 else 0
    # 主板/创业板：100 股整数倍
    return (value // 100) * 100


def min_buy_shares(code: str) -> int:
    return 200 if board(code) == "star" else 100


def is_t1_available(buy_date: Optional[str | date | datetime],
                    sell_date: Optional[str | date | datetime] = None) -> bool:
    """T+1：卖出日必须晚于买入日；旧数据没有买入日期时放行。"""
    if not buy_date:
        return True
    if isinstance(buy_date, datetime):
        buy = buy_date.date()
    elif isinstance(buy_date, str):
        try:
            buy = date.fromisoformat(buy_date[:10])
        except ValueError:
            return True
    else:
        buy = buy_date
    if sell_date is None:
        sell = date.today()
    elif isinstance(sell_date, datetime):
        sell = sell_date.date()
    elif isinstance(sell_date, str):
        try:
            sell = date.fromisoformat(sell_date[:10])
        except ValueError:
            sell = date.today()
    else:
        sell = sell_date
    return sell > buy


def available_date(buy_date: Optional[str | date | datetime]) -> Optional[date]:
    """买入后可卖的最早交易日。"""
    if not buy_date:
        return None
    if isinstance(buy_date, datetime):
        buy = buy_date.date()
    elif isinstance(buy_date, str):
        try:
            buy = date.fromisoformat(buy_date[:10])
        except ValueError:
            return None
    else:
        buy = buy_date
    try:
        return next_trading_day(buy)
    except ValueError:
        return None
