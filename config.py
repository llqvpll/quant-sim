# -*- coding: utf-8 -*-
"""集中配置与默认值。

所有风控/费用/策略参数只在这里定义一次，Simulator 从 state["config"] 读取，
state 文件里缺失的键用 DEFAULT_CONFIG 补齐。这样 CLI、看板、AI prompt 和
真实下单逻辑看到的是同一套数值。
"""
from __future__ import annotations

import copy
import math
from typing import Any, Dict

STATE_VERSION = 2

DEFAULT_CONFIG: Dict[str, Any] = {
    # 账户
    "initial_capital": 50_000.0,
    # 仓位与风控
    "max_positions": 3,
    "max_single_position_ratio": 0.25,
    "stop_loss_pct": -5.0,
    "take_profit_pct": 15.0,
    "drawdown_halt_pct": -10.0,
    "halt_recover_pct": -5.0,
    "halt_cooldown_days": 5,
    "min_hold_minutes": 30,
    # 费用（A 股：佣金双边、印花税卖出、过户费双边）
    "commission_rate": 0.0003,
    "min_commission": 5.0,
    "stamp_tax_rate": 0.0005,
    "transfer_fee_rate": 0.00001,
    # 交易约束
    "min_trade_amount": 3_000.0,
    "max_one_lot_ratio": 0.25,
    "max_round_trip_cost_pct": 1.5,
    "min_price": 5.0,
    "max_price": 500.0,
    # 策略
    "scan_top_n": 10,
    "ai_analyze_n": 5,
    "rule_score_min": 20,
    "scan_interval": 300,
    "signal_cooldown_min": 30,
    "confidence_buy_threshold": 0.65,
    "confidence_base_ratio": 0.15,
    "confidence_max_ratio": 0.25,
    # 回测
    "backtest_days": 60,
    "backtest_decision_every": 5,
    # AI
    "ai_max_retries": 3,
    "ai_timeout_seconds": 180,
    "price_tolerance_pct": 3.0,
}

# 这些键必须是数值，用于启动时校验配置文件/状态文件
_NUMERIC_KEYS = {
    "initial_capital", "max_positions", "max_single_position_ratio",
    "stop_loss_pct", "take_profit_pct", "drawdown_halt_pct",
    "halt_recover_pct", "halt_cooldown_days", "min_hold_minutes",
    "commission_rate", "min_commission", "stamp_tax_rate",
    "transfer_fee_rate", "min_trade_amount", "max_one_lot_ratio",
    "max_round_trip_cost_pct", "min_price", "max_price", "scan_top_n", "ai_analyze_n",
    "rule_score_min", "scan_interval", "signal_cooldown_min",
    "confidence_buy_threshold", "confidence_base_ratio",
    "confidence_max_ratio", "backtest_days", "backtest_decision_every",
    "ai_max_retries", "ai_timeout_seconds", "price_tolerance_pct",
}


def merge_config(raw: Dict[str, Any] | None) -> Dict[str, Any]:
    """把旧 state 里的配置合并到默认配置上，并丢弃未知键。"""
    cfg = copy.deepcopy(DEFAULT_CONFIG)
    if not raw:
        return cfg
    for key, value in raw.items():
        if key in cfg:
            cfg[key] = value
    return cfg


def validate_config(cfg: Dict[str, Any]) -> Dict[str, Any]:
    """校验并修正明显不合法的配置，返回修正后的副本。

    不抛异常：状态文件是用户可编辑的，启动时尽量修复并打印警告。
    """
    fixed = merge_config(cfg)
    issues = []
    for key in _NUMERIC_KEYS:
        value = fixed.get(key)
        try:
            number = float(value)
        except (TypeError, ValueError):
            issues.append(f"{key}={value!r} 不是数字，使用默认值")
            fixed[key] = DEFAULT_CONFIG[key]
            continue
        if not math.isfinite(number):
            issues.append(f"{key}={value!r} 不是有限数，使用默认值")
            fixed[key] = DEFAULT_CONFIG[key]
            continue
        fixed[key] = int(number) if key in {
            "max_positions", "scan_top_n", "ai_analyze_n", "rule_score_min",
            "scan_interval", "signal_cooldown_min", "backtest_days",
            "backtest_decision_every", "ai_max_retries", "ai_timeout_seconds",
            "halt_cooldown_days",
        } else number

    if fixed["initial_capital"] <= 0:
        issues.append("initial_capital 必须为正，使用默认值")
        fixed["initial_capital"] = DEFAULT_CONFIG["initial_capital"]
    if not 0 < fixed["max_single_position_ratio"] <= 1:
        issues.append("max_single_position_ratio 必须在 (0,1]，修正为 0.25")
        fixed["max_single_position_ratio"] = 0.25
    if fixed["max_positions"] < 1:
        issues.append("max_positions 至少为 1")
        fixed["max_positions"] = 1
    if fixed["stop_loss_pct"] >= 0:
        issues.append("stop_loss_pct 必须为负，修正为 -5")
        fixed["stop_loss_pct"] = -5.0
    if fixed["take_profit_pct"] <= 0:
        issues.append("take_profit_pct 必须为正，修正为 15")
        fixed["take_profit_pct"] = 15.0
    if fixed["drawdown_halt_pct"] >= 0:
        issues.append("drawdown_halt_pct 必须为负，修正为 -10")
        fixed["drawdown_halt_pct"] = -10.0
    if fixed["halt_recover_pct"] <= fixed["drawdown_halt_pct"]:
        issues.append("halt_recover_pct 应高于 drawdown_halt_pct，修正为 -5")
        fixed["halt_recover_pct"] = -5.0
    if not 0 <= fixed["confidence_buy_threshold"] <= 1:
        issues.append("confidence_buy_threshold 必须在 [0,1]，修正为 0.65")
        fixed["confidence_buy_threshold"] = 0.65
    if fixed["confidence_base_ratio"] > fixed["confidence_max_ratio"]:
        issues.append("confidence_base_ratio 不能大于 max，修正")
        fixed["confidence_base_ratio"] = 0.15
        fixed["confidence_max_ratio"] = 0.25
    if fixed["commission_rate"] < 0 or fixed["stamp_tax_rate"] < 0:
        issues.append("费率不能为负，修正为默认值")
        fixed["commission_rate"] = DEFAULT_CONFIG["commission_rate"]
        fixed["stamp_tax_rate"] = DEFAULT_CONFIG["stamp_tax_rate"]
    if issues:
        print("[配置校验] " + "；".join(issues))
    return fixed
