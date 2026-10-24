# -*- coding: utf-8 -*-
"""A 股交易日历与会话时间。

默认只排除周末；中国法定节假日可通过 data/holidays.json 覆盖
（格式：["2026-10-01", "2026-10-02", ...]）。交易日历不内置易过期的
节假日表，避免写死错误日期；生产使用请每年更新该文件。
"""
from __future__ import annotations

import json
import os
from datetime import date, datetime, time, timedelta
from typing import Iterable, Optional, Set

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
HOLIDAYS_FILE = os.path.join(BASE_DIR, "data", "holidays.json")

MORNING_OPEN = time(9, 30)
MORNING_CLOSE = time(11, 30)
AFTERNOON_OPEN = time(13, 0)
AFTERNOON_CLOSE = time(15, 0)

_holiday_cache: Optional[Set[date]] = None
_holiday_mtime: Optional[float] = None


def _parse_holidays(raw: Iterable[str]) -> Set[date]:
    result: Set[date] = set()
    for item in raw:
        try:
            result.add(date.fromisoformat(str(item)[:10]))
        except (TypeError, ValueError):
            continue
    return result


def load_holidays(path: str = HOLIDAYS_FILE) -> Set[date]:
    """读取可选节假日文件；文件不存在时返回空集合。"""
    global _holiday_cache, _holiday_mtime
    try:
        mtime = os.path.getmtime(path)
    except OSError:
        _holiday_cache = set()
        _holiday_mtime = None
        return _holiday_cache
    if _holiday_cache is not None and _holiday_mtime == mtime:
        return _holiday_cache
    try:
        with open(path, "r", encoding="utf-8") as fh:
            raw = json.load(fh)
        if isinstance(raw, dict):
            raw = raw.get("holidays", [])
        _holiday_cache = _parse_holidays(raw if isinstance(raw, list) else [])
    except (OSError, ValueError, TypeError):
        _holiday_cache = set()
    _holiday_mtime = mtime
    return _holiday_cache


def is_trading_day(day: date | datetime | str, holidays: Optional[Set[date]] = None) -> bool:
    """是否为交易日（周末 + 可选节假日）。"""
    if isinstance(day, datetime):
        day = day.date()
    elif isinstance(day, str):
        day = date.fromisoformat(day[:10])
    if day.weekday() >= 5:
        return False
    holiday_set = load_holidays() if holidays is None else holidays
    return day not in holiday_set


def is_trading_time(now: datetime | None = None) -> bool:
    """是否处于连续竞价时段：9:30-11:30、13:00-15:00（含边界）。"""
    now = now or datetime.now()
    if not is_trading_day(now.date()):
        return False
    clock = now.time()
    return (MORNING_OPEN <= clock <= MORNING_CLOSE) or (AFTERNOON_OPEN <= clock <= AFTERNOON_CLOSE)


def session_progress(now: datetime | None = None) -> float:
    """当前交易时段已走过的比例（0~1），用于盘中量比折算。"""
    now = now or datetime.now()
    if not is_trading_day(now.date()):
        return 1.0
    clock = now.time()
    if clock < MORNING_OPEN:
        return 0.0
    if clock <= MORNING_CLOSE:
        elapsed = (clock.hour * 60 + clock.minute + clock.second / 60) - (9 * 60 + 30)
        return max(0.01, min(elapsed / 240.0, 1.0))
    if clock < AFTERNOON_OPEN:
        return 0.5
    if clock <= AFTERNOON_CLOSE:
        elapsed = 120.0 + ((clock.hour * 60 + clock.minute + clock.second / 60) - 13 * 60)
        return max(0.01, min(elapsed / 240.0, 1.0))
    return 1.0


def next_trading_day(day: date | datetime | str, include_today: bool = False,
                     holidays: Optional[Set[date]] = None) -> date:
    """下一个交易日；include_today=True 时若当天是交易日则返回当天。"""
    if isinstance(day, datetime):
        day = day.date()
    elif isinstance(day, str):
        day = date.fromisoformat(day[:10])
    current = day if include_today else day + timedelta(days=1)
    for _ in range(370):
        if is_trading_day(current, holidays):
            return current
        current += timedelta(days=1)
    raise ValueError("无法找到下一个交易日，请检查节假日文件")


def previous_trading_day(day: date | datetime | str,
                         holidays: Optional[Set[date]] = None) -> date:
    if isinstance(day, datetime):
        day = day.date()
    elif isinstance(day, str):
        day = date.fromisoformat(day[:10])
    current = day - timedelta(days=1)
    for _ in range(370):
        if is_trading_day(current, holidays):
            return current
        current -= timedelta(days=1)
    raise ValueError("无法找到上一个交易日，请检查节假日文件")


def trading_days(start: date, end: date,
                 holidays: Optional[Set[date]] = None) -> list[date]:
    """[start, end] 内的交易日列表（闭区间）。"""
    result = []
    current = start
    while current <= end:
        if is_trading_day(current, holidays):
            result.append(current)
        current += timedelta(days=1)
    return result
