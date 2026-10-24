from datetime import datetime, timedelta

from technical import TechnicalAnalyzer


class FakeMarket:
    def __init__(self, rows):
        self.rows = rows
        self.calls = []

    def get_kline(self, symbol, period="1d", limit=120, end_date=None):
        self.calls.append(end_date)
        if end_date:
            return [row for row in self.rows if row["date"] <= end_date][-limit:]
        return self.rows[-limit:]


def make_rows(count=120, start=10.0):
    rows = []
    day = datetime(2026, 1, 5)
    price = start
    for index in range(count):
        while day.weekday() >= 5:
            day += timedelta(days=1)
        close = price + (0.1 if index % 3 else -0.05)
        rows.append({
            "date": day.date().isoformat(), "open": price, "close": close,
            "high": close + 0.2, "low": price - 0.2, "volume": 1000 + index * 10,
        })
        price = close
        day += timedelta(days=1)
    return rows


def test_indicators_shape_and_as_of():
    rows = make_rows()
    market = FakeMarket(rows)
    ta = TechnicalAnalyzer(market)
    ind = ta.get_indicators("600519", as_of=rows[-10]["date"])
    assert ind
    assert 0 <= ind["rsi14"] <= 100
    assert ind["atr14"] > 0
    assert ind["macd_hist"] == round(ind["macd_dif"] - ind["macd_dea"], 3)
    assert market.calls[-1] == rows[-10]["date"]


def test_short_history_returns_empty():
    ta = TechnicalAnalyzer(FakeMarket(make_rows(20)))
    assert ta.get_indicators("600519") == {}


def test_intraday_volume_ratio_is_annualized():
    rows = make_rows()
    # 最后一天成交量只有平时的 1/4，模拟早盘
    rows[-1]["volume"] = sum(row["volume"] for row in rows[-6:-1]) / 5 / 4
    ta = TechnicalAnalyzer(FakeMarket(rows))
    now = datetime.fromisoformat(rows[-1]["date"] + "T10:30:00")
    ind = ta.get_indicators("600519", now=now)
    assert ind["is_intraday"] is True
    assert ind["volume_ratio"] > 0.5


def test_rule_score_ranges():
    ta = TechnicalAnalyzer(FakeMarket(make_rows()))
    result = ta.rule_score("600519", change_pct=2.0)
    assert "score" in result
    assert result["grade"]
