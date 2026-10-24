from config import DEFAULT_CONFIG, merge_config, validate_config


def test_defaults_and_merge():
    cfg = merge_config({"stop_loss_pct": -3, "unknown": 1})
    assert cfg["stop_loss_pct"] == -3
    assert "unknown" not in cfg
    assert cfg["stamp_tax_rate"] == 0.0005


def test_validate_fixes_bad_values():
    cfg = validate_config({
        "max_single_position_ratio": 2,
        "stop_loss_pct": 1,
        "take_profit_pct": -1,
        "initial_capital": -5,
        "confidence_buy_threshold": 2,
    })
    assert cfg["max_single_position_ratio"] == 0.25
    assert cfg["stop_loss_pct"] < 0
    assert cfg["take_profit_pct"] > 0
    assert cfg["initial_capital"] == DEFAULT_CONFIG["initial_capital"]
    assert 0 <= cfg["confidence_buy_threshold"] <= 1
