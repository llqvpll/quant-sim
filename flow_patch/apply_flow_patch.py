# -*- coding: utf-8 -*-
"""一键恢复「A股量化交易分析」Langflow flow 的优化版。

会先把当前 flow 备份到 flow_patch/flow_backup_*.json（含 API Key，已 gitignore），
再写入：
  1. MarketData 节点（as_of / 实时快照 / ATR / 数据新鲜度 / fail-fast）
  2. JSON 输出提示词
  3. DeepSeek json_mode / max_tokens / temperature
"""
from __future__ import annotations

import argparse
import gzip
import json
import sys
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_FLOW_ID = "c25e4d72-5593-48fe-945e-5e3bdde16220"

SYSTEM_MESSAGE = (
    "你是严谨、保守的A股量化交易分析师，只依据给定的市场数据分析，绝不编造数据。"
    "你必须只输出一个合法的 JSON 对象，不要输出任何解释文字，也不要用 markdown 代码块包裹。"
    "JSON 字段固定为：symbol 为本次分析的6位代码；signal 取 buy/sell/hold/wait 之一；"
    "confidence 为 0-100 整数；trend 取 up/down/sideways 之一；stop_loss 为数字或 null；"
    "take_profit 为数字或 null；position_pct 为 0-100 整数；reason 为不超过120字的中文说明；"
    "risk 为不超过60字的风险提示。"
)

MD_FIELD_SPECS = {
    "as_of": {
        "type": "str",
        "_input_type": "MessageTextInput",
        "value": "",
        "display_name": "截止日期(回测用)",
        "info": "格式 YYYY-MM-DD。留空=最新数据；填写后只使用该日期及之前的K线，避免回测未来函数。",
        "advanced": True,
    },
    "include_realtime": {
        "type": "bool",
        "_input_type": "BoolInput",
        "value": True,
        "display_name": "附加实时快照",
        "info": "补充现价/涨跌幅/换手率/量比/涨跌停价。回测模式(as_of)下自动忽略。",
        "advanced": True,
    },
}


def fetch(base: str, url: str, token=None, method="GET", body=None, timeout=180):
    req = urllib.request.Request(base.rstrip("/") + url, method=method)
    if token:
        req.add_header("Authorization", "Bearer " + token)
    req.add_header("Accept-Encoding", "identity")
    req.add_header("Content-Type", "application/json")
    data = json.dumps(body).encode() if body is not None else None
    with urllib.request.urlopen(req, data=data, timeout=timeout) as resp:
        raw = resp.read()
        if resp.headers.get("Content-Encoding") == "gzip":
            raw = gzip.decompress(raw)
        return raw


def ensure_fields(template, specs):
    added = []
    for name, spec in specs.items():
        if name in template:
            continue
        base_key = "symbol" if spec["type"] == "str" else "demo_mode"
        base = dict(template[base_key])
        base.update({
            "name": name, "value": spec["value"], "display_name": spec["display_name"],
            "info": spec["info"], "advanced": spec["advanced"], "type": spec["type"],
            "_input_type": spec["_input_type"],
        })
        template[name] = base
        added.append(name)
    return added


def main(argv=None):
    parser = argparse.ArgumentParser(description="恢复优化版 Langflow flow")
    parser.add_argument("--base", default="http://127.0.0.1:7860")
    parser.add_argument("--flow", default=DEFAULT_FLOW_ID)
    parser.add_argument("--dry-run", action="store_true", help="只打印将做的修改，不写入")
    args = parser.parse_args(argv)

    new_code = (HERE / "marketdata_node.py").read_text(encoding="utf-8")
    new_prompt = (HERE / "prompt_template.txt").read_text(encoding="utf-8").strip()

    token = json.loads(fetch(args.base, "/api/v1/auto_login"))["access_token"]
    flow = json.loads(fetch(args.base, f"/api/v1/flows/{args.flow}", token))
    nodes = {node["id"]: node["data"]["node"] for node in flow["data"]["nodes"]}
    required = ("MarketData-aF2Ci", "Prompt-GO1Cu", "DeepSeek-tG2Vx")
    missing = [node for node in required if node not in nodes]
    if missing:
        print(f"flow 缺少节点: {missing}；请确认导入的是仓库自带 flow_current.json")
        return 1
    if args.dry_run:
        print("dry-run: 将更新", ", ".join(required))
        return 0

    backup = HERE / f"flow_backup_{time.strftime('%Y%m%d_%H%M%S')}.json"
    backup.write_text(json.dumps(flow, ensure_ascii=False, indent=2), encoding="utf-8")
    print("已备份当前流 ->", backup.name)

    md = nodes["MarketData-aF2Ci"]["template"]
    md["code"]["value"] = new_code
    print("新增节点输入:", ensure_fields(md, MD_FIELD_SPECS) or "已存在，无需新增")
    nodes["Prompt-GO1Cu"]["template"]["template"]["value"] = new_prompt

    ds = nodes["DeepSeek-tG2Vx"]["template"]
    ds["json_mode"]["value"] = True
    ds["max_tokens"]["value"] = 800
    ds["temperature"]["value"] = 0.2
    ds["seed"]["value"] = 1
    ds["stream"]["value"] = False
    ds["system_message"]["value"] = SYSTEM_MESSAGE

    body = {"data": flow["data"], "name": flow.get("name"), "description": flow.get("description")}
    resp = json.loads(fetch(args.base, f"/api/v1/flows/{args.flow}", token, method="PATCH", body=body))
    print("写入流:", "OK" if resp.get("id") == args.flow else "FAIL")

    after = json.loads(fetch(args.base, f"/api/v1/flows/{args.flow}", token))
    after_nodes = {node["id"]: node["data"]["node"] for node in after["data"]["nodes"]}
    a_md = after_nodes["MarketData-aF2Ci"]["template"]
    a_ds = after_nodes["DeepSeek-tG2Vx"]["template"]
    checks = {
        "MarketData 代码一致": a_md["code"]["value"] == new_code,
        "as_of 输入存在": "as_of" in a_md,
        "Prompt 一致": after_nodes["Prompt-GO1Cu"]["template"]["template"]["value"] == new_prompt,
        "json_mode": a_ds["json_mode"]["value"] is True,
        "max_tokens": a_ds["max_tokens"]["value"],
        "temperature": a_ds["temperature"]["value"],
    }
    for key, value in checks.items():
        print(f"  {key}: {value}")
    ok = checks["MarketData 代码一致"] and checks["as_of 输入存在"] and checks["json_mode"]
    print("\n结果:", "成功，优化版已恢复" if ok else "异常，请检查输出")
    return 0 if ok else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:  # noqa: BLE001
        print(f"执行失败: {type(exc).__name__}: {exc}")
        print("请确认 Langflow 已在 127.0.0.1:7860 运行。")
        sys.exit(1)
