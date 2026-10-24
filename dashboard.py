# -*- coding: utf-8 -*-
"""看板页面工具。

看板 HTML 只有一份：dashboard.html。dashboard.py 不再内嵌 470 行 HTML
（旧版 dashboard.py 与 dashboard.html 完全重复，改一处就漂移）。
"""
from __future__ import annotations

import os
import sys

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
OUTPUT_FILE = os.path.join(BASE_DIR, "dashboard.html")


def build_dashboard(state=None) -> str:
    """返回看板 HTML（state 参数保留兼容，数据由 /api/state 提供）。"""
    with open(OUTPUT_FILE, "r", encoding="utf-8") as fh:
        return fh.read()


def main():
    if not os.path.exists(OUTPUT_FILE):
        print(f"看板文件不存在: {OUTPUT_FILE}")
        return 1
    html = build_dashboard()
    print(f"看板文件: {OUTPUT_FILE} ({len(html)} chars)")
    print("由 dashboard_server.py 提供 HTTP 服务；页面每 10 秒从 /api/state 拉取数据。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
