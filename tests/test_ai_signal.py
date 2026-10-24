import json

from ai_signal import AIAnalyzer


def test_confidence_normalization():
    assert AIAnalyzer._normalize_confidence(0.8) == 0.8
    assert AIAnalyzer._normalize_confidence(80) == 0.8
    assert AIAnalyzer._normalize_confidence("80%") == 0.8
    assert AIAnalyzer._normalize_confidence(1.0) == 1.0
    assert AIAnalyzer._normalize_confidence(None) == 0.5
    assert AIAnalyzer._normalize_confidence("bad") == 0.5


def test_json_chinese_signal():
    parsed = AIAnalyzer._parse_json_signal('{"signal":"买入","confidence":80,"symbol":"600519"}')
    assert parsed["signal"] == "buy"
    assert parsed["confidence"] == 0.8


def test_json_symbol_mismatch_blocks():
    text = json.dumps({"signal": "buy", "confidence": 80, "symbol": "000001",
                       "stop_loss": 9, "take_profit": 12, "reason": "x"})
    result = AIAnalyzer._parse_signal(text, "600519", verify_price=10.0)
    assert result["signal"] == "wait"
    assert result["hallucination_risk"]


def test_price_tolerance_is_three_percent():
    text = json.dumps({"signal": "buy", "confidence": 80, "symbol": "600519",
                       "reason": "现价 107 元"})
    result = AIAnalyzer._parse_signal(text, "600519", verify_price=100.0)
    assert result["signal"] == "wait"
    assert result["hallucination_risk"]

    text_ok = json.dumps({"signal": "buy", "confidence": 80, "symbol": "600519",
                          "reason": "现价 102 元"})
    result_ok = AIAnalyzer._parse_signal(text_ok, "600519", verify_price=100.0)
    assert result_ok["signal"] == "buy"


def test_keyword_negation_and_advice_zone():
    blocked = AIAnalyzer._parse_signal("交易建议：不建议买入。", "600519", verify_price=10.0)
    assert blocked["signal"] in ("wait", "hold")

    good = AIAnalyzer._parse_signal("交易建议：基本面不错，建议买入，止损位 9.5。", "600519", verify_price=10.0)
    assert good["signal"] == "buy"
    assert good["confidence"] == 0.4

    fallback = AIAnalyzer._parse_signal("综合来看，建议买入。", "600519", verify_price=10.0)
    assert fallback["signal"] == "wait"  # 非“明确建议区”，低可信兜底


def test_stop_take_logic():
    text = json.dumps({"signal": "buy", "confidence": 80, "symbol": "600519",
                       "stop_loss": 11, "take_profit": 13})
    result = AIAnalyzer._parse_signal(text, "600519", verify_price=10.0)
    assert result["signal"] == "wait"
    assert result["hallucination_risk"]


def test_selftest():
    assert AIAnalyzer.selftest_grounding()
