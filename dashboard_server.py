# -*- coding: utf-8 -*-
"""AI 模拟盘实时看板 HTTP 服务（v2）。

- /            -> dashboard.html
- /api/state   -> 账户 + 心跳 + 经验
- /api/health  -> 健康检查（launcher 用它判断“是不是我们的看板”）
"""
from __future__ import annotations

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
BEAT_TIMEOUT = 180


def load_json(path):
    try:
        with open(path, "r", encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return None


def get_bot_status(state=None):
    beat = load_json(HEARTBEAT_FILE)
    last_scan = (state or {}).get("last_scan", {}).get("time") if isinstance(state, dict) else None
    if not beat or not beat.get("last_beat"):
        return {"running": False, "last_beat": None, "last_scan": last_scan}
    try:
        last = datetime.fromisoformat(beat["last_beat"])
    except (TypeError, ValueError):
        return {"running": False, "last_beat": beat.get("last_beat"), "last_scan": last_scan}
    return {
        "running": (time.time() - last.timestamp()) < BEAT_TIMEOUT,
        "last_beat": beat["last_beat"],
        "last_scan": last_scan or beat["last_beat"],
    }


class Handler(BaseHTTPRequestHandler):
    server_version = "QuantDashboard/2.0"

    def log_message(self, fmt, *args):
        sys.stdout.write("[%s] %s\n" % (datetime.now().strftime("%H:%M:%S"), fmt % args))

    def _send(self, code, body, ctype):
        data = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path in ("/", "/index.html", "/dashboard.html"):
            html = None
            if os.path.exists(DASHBOARD_FILE):
                with open(DASHBOARD_FILE, "r", encoding="utf-8") as fh:
                    html = fh.read()
            if not html:
                html = ("<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"UTF-8\">"
                        "<title>看板缺失</title></head><body style=\"background:#0f1420;color:#e8eaf0;"
                        "font-family:Microsoft YaHei;padding:40px\"><h2>看板页面缺失</h2>"
                        "<p>请确认 dashboard.html 与 dashboard_server.py 在同一目录。</p></body></html>")
            self._send(200, html, "text/html; charset=utf-8")
            return
        if path == "/api/state":
            state = load_json(STATE_FILE)
            payload = json.dumps({
                "ok": True, "ts": time.time(),
                "bot": get_bot_status(state),
                "state": state,
                "experience": load_json(EXPERIENCE_FILE),
            }, ensure_ascii=False).encode("utf-8")
            self._send(200, payload, "application/json; charset=utf-8")
            return
        if path == "/api/health":
            self._send(200, json.dumps({"ok": True, "service": "quant-dashboard", "ts": time.time()}),
                       "application/json")
            return
        self._send(404, "Not Found", "text/plain")

    def log_error(self, fmt, *args):
        pass


def main(argv=None):
    parser = argparse.ArgumentParser(description="AI 模拟盘实时看板服务")
    parser.add_argument("--port", type=int, default=8000, help="端口（默认 8000）")
    parser.add_argument("--host", default="127.0.0.1", help="监听地址（默认仅本机）")
    args = parser.parse_args(argv)
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"看板服务已启动: http://{args.host}:{args.port}/")
    print(f"数据源: {STATE_FILE}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n看板服务已停止")
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
