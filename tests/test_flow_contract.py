import json
from pathlib import Path

from check_flow import sanitize

BASE = Path(__file__).resolve().parents[1]


def test_committed_flow_matches_client():
    flow = json.loads((BASE / "flow_current.json").read_text(encoding="utf-8"))
    nodes = {node["id"]: node["data"]["node"] for node in flow["data"]["nodes"]}
    md = nodes["MarketData-aF2Ci"]["template"]
    deepseek = nodes["DeepSeek-tG2Vx"]["template"]
    prompt = nodes["Prompt-GO1Cu"]["template"]["template"]["value"]

    assert "as_of" in md
    assert "as_of" in md["code"]["value"]
    assert deepseek["json_mode"]["value"] is True
    assert "symbol" in prompt
    assert "JSON" in prompt
    assert deepseek["api_key"]["value"].startswith("sk-REPLACE_WITH")


def test_sanitize_api_key():
    node = {"template": {"api_key": {"value": "sk-real-secret"}, "other": 1}}
    clean = sanitize(node)
    assert clean["template"]["api_key"]["value"].startswith("sk-REPLACE_WITH")
    assert clean["template"]["other"] == 1
