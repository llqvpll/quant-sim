import json
import os

import pytest

from simulator import Simulator


@pytest.fixture()
def sim(tmp_path):
    return Simulator(state_file=str(tmp_path / "state.json"), capital=50000)


def test_buy_sell_t1(sim):
    result = sim.buy("600519", "贵州茅台", 100.0, 100, reason="AI信号: 买入(置信80%)",
                     trade_date="2026-10-23")
    assert result["success"]
    # 同日不可卖
    blocked = sim.sell("600519", 101.0, -1, reason="AI信号: 卖出(置信80%)",
                       trade_date="2026-10-23")
    assert not blocked["success"]
    assert "T+1" in blocked["msg"]
    # 下一交易日可卖
    sold = sim.sell("600519", 101.0, -1, reason="AI信号: 卖出(置信80%)",
                    trade_date="2026-10-26")
    assert sold["success"]
    assert "600519" not in sim.state["positions"]


def test_lot_and_price_validation(sim):
    assert not sim.buy("688001", "华兴源创", 10.0, 100)["success"]
    assert sim.buy("688001", "华兴源创", 10.0, 200)["success"]
    assert not sim.buy("600519", "贵州茅台", 0, 100)["success"]
    assert sim.summary()["持仓数量"] == 1


def test_price_limit(sim):
    # 10 元前收，涨停 11；等于涨停不可买
    assert not sim.buy("600000", "浦发银行", 11.0, 100, prev_close=10.0)["success"]
    assert sim.buy("600000", "浦发银行", 10.5, 100, prev_close=10.0)["success"]


def test_config_is_single_source(sim):
    sim.cfg["stop_loss_pct"] = -1.0
    sim.buy("600000", "浦发银行", 10.0, 100)
    avg = sim.state["positions"]["600000"]["avg_cost"]
    price = avg * 0.985  # 约 -1.5%，旧代码要 -5% 才触发
    signals = sim.check_stop_loss_take_profit({"600000": price})
    assert signals and "止损" in signals[0]["reason"]


def test_avg_cost_includes_fee(sim):
    sim.buy("600000", "浦发银行", 10.0, 100)
    pos = sim.state["positions"]["600000"]
    assert pos["avg_cost"] > 10.0
    summary = sim.summary()
    assert summary["浮动盈亏"] == pytest.approx((10.0 - pos["avg_cost"]) * 100, abs=0.01)


def test_fifo_partial_sell(sim):
    sim.buy("600000", "浦发银行", 10.0, 200, trade_date="2026-10-23")
    sim.sell("600000", 11.0, 100, trade_date="2026-10-26")
    summary = sim.summary()
    assert summary["持仓数量"] == 1
    assert summary["持仓列表"][0]["shares"] == 100
    assert summary["已平仓盈亏"] > 0


def test_halt_recovery(sim):
    sim.state["peak_assets"] = 60000
    sim.update_halt(today="2026-10-20")
    assert sim.state["halted"]
    # 冷却 5 天后自动解除
    sim.update_halt(today="2026-10-27")
    assert not sim.state["halted"]


def test_atomic_state_and_backup(sim, tmp_path):
    sim.save()
    sim.save()
    assert os.path.exists(str(tmp_path / "state.json"))
    assert os.path.exists(str(tmp_path / "state.json.bak"))
    with open(str(tmp_path / "state.json"), "r", encoding="utf-8") as fh:
        json.load(fh)


def test_corrupt_state_recovers_from_backup(sim, tmp_path):
    sim.save()
    sim.save()
    with open(str(tmp_path / "state.json"), "w", encoding="utf-8") as fh:
        fh.write("{broken")
    recovered = Simulator(state_file=str(tmp_path / "state.json"), capital=50000)
    assert recovered.state["cash"] == 50000.0
    assert recovered.state["version"] == 2
