from datetime import datetime, timedelta

import backtest as backtest_module
from backtest import Backtest
from technical import TechnicalAnalyzer


def make_rows(count=90, start=10.0):
    rows = []
    day = datetime(2026, 1, 5)
    price = start
    for index in range(count):
        while day.weekday() >= 5:
            day += timedelta(days=1)
        open_price = price + 0.1
        close = open_price + 0.1
        rows.append({
            "date": day.date().isoformat(), "open": open_price, "close": close,
            "high": close + 0.2, "low": open_price - 0.2, "volume": 1000 + index,
        })
        price = close
        day += timedelta(days=1)
    return rows


class FakeMarket:
    def __init__(self, rows):
        self.rows = rows

    def get_kline(self, symbol, period="1d", limit=120, end_date=None):
        rows = self.rows
        if end_date:
            rows = [row for row in rows if row["date"] <= end_date]
        return rows[-limit:]


class FakeAI:
    def __init__(self):
        self.calls = 0

    def analyze_stock(self, *args, **kwargs):
        self.calls += 1
        return {"signal": "buy", "confidence": 0.9, "reason": "test",
                "stop_loss": None, "take_profit": None, "position_pct": 10}


def test_backtest_executes_next_open_not_signal_close(tmp_path, monkeypatch):
    monkeypatch.setattr(backtest_module, "DATA_DIR", str(tmp_path))
    rows = make_rows()
    market = FakeMarket(rows)
    bt = Backtest(["600000", "000001"], days=20, decision_every=5, capital=50000,
                  ai=FakeAI(), market=market, technical=TechnicalAnalyzer(market),
                  slippage_bps=0)
    result = bt.run()
    assert "error" not in result
    buys = [trade for trade in result["trades"] if trade["action"] == "BUY"]
    assert buys
    first = buys[0]
    # 成交价必须是执行日开盘价，而不是决策日收盘价
    row_map = {row["date"]: row for row in rows}
    assert first["date"] > result["start"]
    assert first["price"] == row_map[first["date"]]["open"]
    assert result["ai_calls"] > 0
    assert "benchmark_return_pct" in result
    assert result["net_values"]
