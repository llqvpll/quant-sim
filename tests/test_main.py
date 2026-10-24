from main import QuantBot
from simulator import Simulator


class FakeMarket:
    def market_is_open(self):
        return True

    def get_all_a_stocks(self):
        return [{"code": "600000", "name": "浦发银行", "price": 10.0, "change_pct": 1.0}]

    def get_realtime_batch(self, codes):
        return {code: {"code": code, "name": "浦发银行", "price": 10.0, "change_pct": 1.0,
                       "prev_close": 9.9} for code in codes}

    def get_realtime(self, code):
        return {"code": code, "name": "浦发银行", "price": 10.0, "change_pct": 1.0}


class FakeTechnical:
    def rule_score(self, code, change_pct=None):
        return {"score": 80, "grade": "A", "reasons": ["test"]}

    def get_indicators(self, code, as_of=None):
        return {"signal_txt": "MA5 10.0"}


class FakeNews:
    def get_news_summary(self, code, name=None):
        return {"txt": "近1条相关新闻, 消息面中性"}


class FakeLearner:
    def learn_from_trades(self, state):
        return {}

    def experience_summary(self):
        return ""


class FakeAI:
    def __init__(self, result=None):
        self.result = result or {"signal": "buy", "confidence": 0.9, "reason": "test",
                                 "stop_loss": 9.0, "take_profit": 12.0, "position_pct": 10}
        self.calls = 0

    def analyze_stock(self, *args, **kwargs):
        self.calls += 1
        result = dict(self.result)
        if result.get("signal") == "buy" and kwargs.get("verify_price"):
            result["symbol"] = kwargs.get("expected_name") or "600000"
        return result


def make_bot(tmp_path, ai=None):
    sim = Simulator(state_file=str(tmp_path / "state.json"), capital=50000)
    return QuantBot(sim=sim, ai=ai or FakeAI(), market=FakeMarket(),
                    technical=FakeTechnical(), news=FakeNews(), learner=FakeLearner())


def test_scan_buys_once(tmp_path):
    bot = make_bot(tmp_path)
    report = bot.scan_once(1, force=True)
    assert report["buy"] == 1
    assert "600000" in bot.sim.state["positions"]
    # 不应重复买入
    report2 = bot.scan_once(1, force=True)
    assert report2["buy"] == 0


def test_cooldown_path_does_not_crash(tmp_path):
    bot = make_bot(tmp_path)
    bot._cache_signal("600000", "buy", "buy", 0.9, "cached")
    report = bot.scan_once(1, force=True)
    assert report["cached"] == 1
    assert report["buy"] == 1


def test_error_not_cached(tmp_path):
    class ErrAI(FakeAI):
        def analyze_stock(self, *args, **kwargs):
            self.calls += 1
            return {"signal": "error", "confidence": 0, "reason": "timeout"}

    ai = ErrAI()
    bot = make_bot(tmp_path, ai=ai)
    bot.scan_once(1, force=True)
    assert ("600000", "buy") not in bot._signal_cache
    bot.scan_once(1, force=True)
    assert ai.calls == 2


def test_scan_closed_market_skips(tmp_path):
    class ClosedMarket(FakeMarket):
        def market_is_open(self):
            return False

    sim = Simulator(state_file=str(tmp_path / "state.json"), capital=50000)
    bot = QuantBot(sim=sim, ai=FakeAI(), market=ClosedMarket(),
                   technical=FakeTechnical(), news=FakeNews(), learner=FakeLearner())
    report = bot.scan_once(1, force=False)
    assert report.get("market_closed") is True
    assert report["buy"] == 0
