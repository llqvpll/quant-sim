# -*- coding: utf-8 -*-
"""
AI 模拟盘实时看板 HTTP 服务
============================
提供:
- GET /            -> dashboard.html(实时看板页面)
- GET /api/state   -> JSON: 模拟盘账户数据 + 机器人心跳状态

用法:
    python dashboard_server.py [--port 8000]

与交易机器人(main.py)并行运行: 机器人写 data/state.json, 本服务读取并提供给浏览器,
页面每 10 秒自动刷新一次, 实现"界面时刻变化"。
"""

import argparse
import json
import os
import sys
import time
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
STATE_FILE = os.path.join(BASE_DIR, "data", "state.json")
HEARTBEAT_FILE = os.path.join(BASE_DIR, "data", "heartbeat.json")
EXPERIENCE_FILE = os.path.join(BASE_DIR, "data", "experience.json")
DASHBOARD_FILE = os.path.join(BASE_DIR, "dashboard.html")

# 机器人心跳超时: 超过该秒数未心跳则视为已停止
BEAT_TIMEOUT = 120


def load_json(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None


def get_bot_status():
    """根据心跳文件判断机器人是否在运行"""
    beat = load_json(HEARTBEAT_FILE)
    if not beat or not beat.get("last_beat"):
        return {"running": False, "last_beat": None, "last_scan": None}
    try:
        last = datetime.fromisoformat(beat["last_beat"])
    except Exception:
        return {"running": False, "last_beat": beat.get("last_beat"), "last_scan": None}
    return {
        "running": (time.time() - last.timestamp()) < BEAT_TIMEOUT,
        "last_beat": beat["last_beat"],
        "last_scan": beat["last_beat"],
    }


class Handler(BaseHTTPRequestHandler):
    server_version = "QuantDashboard/1.0"

    def log_message(self, fmt, *args):
        sys.stdout.write("[%s] %s\n" % (datetime.now().strftime("%H:%M:%S"), fmt % args))

    def _send(self, code, body, ctype):
        data = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0]

        if path in ("/", "/index.html", "/dashboard.html"):
            html = None
            if os.path.exists(DASHBOARD_FILE):
                with open(DASHBOARD_FILE, "r", encoding="utf-8") as f:
                    html = f.read()
            if not html:
                # 看板文件缺失时兜底, 提示先运行 dashboard.py 或一键启动脚本
                html = """<!DOCTYPE html><html lang="zh-CN"><head><meta charset="UTF-8">
<title>AI 模拟量化交易看板</title></head>
<body style="background:#0f1420;color:#e8eaf0;font-family:Microsoft YaHei;padding:40px">
<h2>看板页面缺失</h2><p>请先运行 <code>dashboard.py</code> 生成 dashboard.html，或直接使用桌面「一键启动」脚本。</p>
</body></html>"""
            self._send(200, html, "text/html; charset=utf-8")
            return

        if path == "/api/state":
            state = load_json(STATE_FILE)
            bot = get_bot_status()
            exp = load_json(EXPERIENCE_FILE)
            payload = json.dumps({"ok": True, "ts": time.time(), "bot": bot,
                                  "state": state, "experience": exp},
                                 ensure_ascii=False).encode("utf-8")
            self._send(200, payload, "application/json; charset=utf-8")
            return

        if path == "/api/health":
            self._send(200, json.dumps({"ok": True, "ts": time.time()}), "application/json")
            return

        self._send(404, "Not Found", "text/plain")

    # 禁用错误日志噪音
    def log_error(self, format, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description="AI 模拟盘实时看板服务")
    parser.add_argument("--port", type=int, default=8000, help="看板端口(默认 8000)")
    args = parser.parse_args()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"看板服务已启动: http://127.0.0.1:{args.port}/")
    print(f"数据源: {STATE_FILE}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n看板服务已停止")
        server.server_close()


if __name__ == "__main__":
    main()
