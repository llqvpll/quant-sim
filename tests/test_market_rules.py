from datetime import date

from market_data import MarketData
from market_rules import (
    at_limit_down,
    at_limit_up,
    available_date,
    board,
    is_st_name,
    is_t1_available,
    limit_prices,
    normalize_buy_shares,
    price_limit_ratio,
)


def test_board_and_st():
    assert board("600519") == "main"
    assert board("300750") == "chinext"
    assert board("688001") == "star"
    assert board("920285") == "bse"
    assert is_st_name("*ST海航")
    assert is_st_name("ST板块")
    assert not is_st_name("贵州茅台")


def test_price_limits():
    assert limit_prices(10, "600000") == (9.0, 11.0)
    assert limit_prices(10, "600000", "*ST测试") == (9.5, 10.5)
    assert limit_prices(10, "300001") == (8.0, 12.0)
    assert limit_prices(10, "688001") == (8.0, 12.0)
    assert limit_prices(10, "920285") == (7.0, 13.0)
    assert price_limit_ratio("300001") == 0.20
    assert at_limit_up(11.0, 10, "600000")
    assert at_limit_down(9.0, 10, "600000")


def test_lot_rules():
    assert normalize_buy_shares("600000", 250) == 200
    assert normalize_buy_shares("600000", 99) == 0
    assert normalize_buy_shares("688001", 100) == 0
    assert normalize_buy_shares("688001", 300) == 300
    assert normalize_buy_shares("920285", 50) == 0
    assert normalize_buy_shares("920285", 150) == 150


def test_t1():
    assert not is_t1_available("2026-10-23", "2026-10-23")
    assert is_t1_available("2026-10-23", "2026-10-26")
    assert available_date("2026-10-23") == date(2026, 10, 26)
    assert is_t1_available(None, "2026-10-26")


def test_920_mapping():
    assert MarketData._to_tencent_code("920285") == "bj920285"
    assert MarketData._to_tencent_code("600519") == "sh600519"
    assert MarketData._to_tencent_code("000001") == "sz000001"
