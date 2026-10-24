# -*- coding: utf-8 -*-
"""Langflow flow 连通性 / 契约检查（只读）。

旧版会把本地 flow（含真实 DeepSeek api_key）直接覆盖到受版本控制的
flow_current.json，容易泄露密钥。v2 默认只读；需要导出时写到
flow_current.local.json（.gitignore 已忽略）并脱敏 api_key。
"""
from __future__ import annotations

import argparse
import json
import os
import sys

from ai_signal import AIAnalyzer

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
LOCAL_FLOW = os.path.join(BASE_DIR, "flow_current.local.json")
PLACEHOLDER = "sk-REPLACE_WITH_YOUR_DEEPSEEK_API_KEY"


def sanitize(node):
    if isinstance(node, dict):
        result = {}
        for key, value in node.items():
            if key == "api_key" and isinstance(value, dict):
                value = dict(value)
                value["value"] = PLACEHOLDER
            elif key == "api_key" and isinstance(value, str):
                value = PLACEHOLDER
            else:
                value = sanitize(value)
            result[key] = value
        return result
    if isinstance(node, list):
        return [sanitize(item) for item in node]
    return node


def main(argv=None):
    parser = argparse.ArgumentParser(description="检查 Langflow flow 契约（不会覆盖仓库里的 flow_current.json）")
    parser.add_argument("--base", default="http://127.0.0.1:7860")
    parser.add_argument("--flow", default="")
    parser.add_argument("--write-local", action="store_true",
                        help="导出脱敏后的 flow 到 flow_current.local.json")
    args = parser.parse_args(argv)

    analyzer = AIAnalyzer(base=args.base, flow_id=args.flow or None)
    contract = analyzer.check_flow_contract()
    print("flow 契约检查:")
    for key in ("ok", "has_as_of", "json_mode", "has_symbol_prompt"):
        print(f"  {key}: {contract.get(key)}")
    for error in contract.get("errors", []):
        print(f"  ✗ {error}")
    if not contract.get("ok"):
        print("提示: 运行 flow_patch/apply_flow_patch.py 可恢复优化版 flow。")

    if args.write_local:
        token = analyzer._get_token()  # noqa: SLF001 - 运维脚本，复用认证逻辑
        raw = analyzer._fetch(f"/api/v1/flows/{analyzer.flow_id}", token, timeout=30)  # noqa: SLF001
        flow = sanitize(json.loads(raw))
        with open(LOCAL_FLOW, "w", encoding="utf-8") as fh:
            json.dump(flow, fh, ensure_ascii=False, indent=2)
        print(f"已写入脱敏 flow: {LOCAL_FLOW}（已在 .gitignore 中）")
    return 0 if contract.get("ok") else 1


if __name__ == "__main__":
    sys.exit(main())
