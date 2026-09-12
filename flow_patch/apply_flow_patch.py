# -*- coding: utf-8 -*-
"""One-click restore of the optimized "A股量化交易分析" Langflow flow.

Keeps the current flow as a timestamped backup, then applies:
  1. new MarketData node code  (as_of / realtime snapshot / ATR / freshness / fail-fast)
  2. new prompt template       (structured JSON output)
  3. DeepSeek settings         (json_mode, max_tokens=600, temperature=0.2)

Usage:  python apply_flow_patch.py     (Langflow must be running on 127.0.0.1:7860)
"""
import gzip
import json
import sys
import time
import urllib.request
from pathlib import Path

BASE = "http://127.0.0.1:7860"
FLOW_ID = "c25e4d72-5593-48fe-945e-5e3bdde16220"
HERE = Path(__file__).resolve().parent

SYSTEM_MESSAGE = (
    "你是严谨、保守的A股量化交易分析师，只依据给定的市场数据分析，绝不编造数据。"
    "你必须只输出一个合法的 JSON 对象，不要输出任何解释文字，也不要用 markdown 代码块包裹。"
    "JSON 字段固定为：signal 取 buy/sell/hold/wait 之一；confidence 为 0-100 整数；"
    "trend 取 up/down/sideways 之一；stop_loss 为数字或 null；take_profit 为数字或 null；"
    "position_pct 为 0-100 整数；reason 为不超过120字的中文说明；risk 为不超过60字的风险提示。"
)

MD_FIELD_SPECS = {
    "as_of": {
        "type": "str",
        "_input_type": "MessageTextInput",
        "value": "",
        "display_name": "截止日期(回测用)",
        "info": "格式 YYYY-MM-DD。留空=使用最新数据；填写后只使用该日期及之前的K线，避免回测未来函数。",
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


def fetch(url, token=None, method="GET", body=None):
    req = urllib.request.Request(BASE + url, method=method)
    if token:
        req.add_header("Authorization", "Bearer " + token)
    req.add_header("Accept-Encoding", "identity")
    req.add_header("Content-Type", "application/json")
    data = json.dumps(body).encode() if body is not None else None
    resp = urllib.request.urlopen(req, data=data, timeout=180)
    raw = resp.read()
    if resp.headers.get("Content-Encoding") == "gzip":
        raw = gzip.decompress(raw)
    return raw


def ensure_fields(template, specs):
    added = []
    for name, spec in specs.items():
        if name in template:
            continue
        base = dict(template["symbol"] if spec["type"] == "str" else template["demo_mode"])
        base.update(
            {
                "name": name,
                "value": spec["value"],
                "display_name": spec["display_name"],
                "info": spec["info"],
                "advanced": spec["advanced"],
                "type": spec["type"],
                "_input_type": spec["_input_type"],
            }
        )
        template[name] = base
        added.append(name)
    return added


def main():
    new_code = (HERE / "marketdata_node.py").read_text(encoding="utf-8")
    new_prompt = (HERE / "prompt_template.txt").read_text(encoding="utf-8").strip()

    token = json.loads(fetch("/api/v1/auto_login"))["access_token"]
    flow = json.loads(fetch(f"/api/v1/flows/{FLOW_ID}", token))

    backup = HERE / f"flow_backup_{time.strftime('%Y%m%d_%H%M%S')}.json"
    backup.write_text(json.dumps(flow, ensure_ascii=False, indent=2), encoding="utf-8")
    print("已备份当前流 ->", backup.name)

    nodes = {n["id"]: n["data"]["node"] for n in flow["data"]["nodes"]}
    md = nodes["MarketData-aF2Ci"]["template"]
    md["code"]["value"] = new_code
    print("新增节点输入:", ensure_fields(md, MD_FIELD_SPECS) or "已存在，无需新增")

    nodes["Prompt-GO1Cu"]["template"]["template"]["value"] = new_prompt
    ds = nodes["DeepSeek-tG2Vx"]["template"]
    ds["json_mode"]["value"] = True
    ds["max_tokens"]["value"] = 600
    ds["temperature"]["value"] = 0.2
    ds["seed"]["value"] = 1
    ds["stream"]["value"] = False
    ds["system_message"]["value"] = SYSTEM_MESSAGE

    body = {"data": flow["data"], "name": flow.get("name"), "description": flow.get("description")}
    resp = json.loads(fetch(f"/api/v1/flows/{FLOW_ID}", token, method="PATCH", body=body))
    print("写入流:", "OK" if resp.get("id") == FLOW_ID else "FAIL")

    after = json.loads(fetch(f"/api/v1/flows/{FLOW_ID}", token))
    a_nodes = {n["id"]: n["data"]["node"] for n in after["data"]["nodes"]}
    a_md = a_nodes["MarketData-aF2Ci"]["template"]
    a_ds = a_nodes["DeepSeek-tG2Vx"]["template"]
    print("MarketData 代码一致 :", a_md["code"]["value"] == new_code)
    print("as_of 输入存在      :", "as_of" in a_md)
    print("提示词一致          :", a_nodes["Prompt-GO1Cu"]["template"]["template"]["value"] == new_prompt)
    print("json_mode           :", a_ds["json_mode"]["value"])
    print("max_tokens          :", a_ds["max_tokens"]["value"])
    print("temperature         :", a_ds["temperature"]["value"])
    ok = (a_md["code"]["value"] == new_code and "as_of" in a_md and a_ds["json_mode"]["value"] is True)
    print("\n结果:", "成功，优化版已恢复" if ok else "异常，请把上面的输出发给我")
    return 0 if ok else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:  # noqa: BLE001
        print(f"执行失败: {type(exc).__name__}: {exc}")
        print("请确认 Langflow 已在 127.0.0.1:7860 运行。")
        sys.exit(1)
