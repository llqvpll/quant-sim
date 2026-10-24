# -*- coding: utf-8 -*-
"""一体化启动器（v2）：修复首次运行因 data/ 不存在而崩溃、端口误判、
Windows 专属 flag 在其它平台报错等问题。
"""
from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import time
import urllib.request
import webbrowser
from datetime import datetime

from config import DEFAULT_CONFIG

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
LOG_FILE = os.path.join(DATA_DIR, "run.log")
DASHBOARD_PORT = 8000
LANGFLOW_PORT = 7860


def log(msg: str):
    os.makedirs(DATA_DIR, exist_ok=True)
    line = f"[{datetime.now():%H:%M:%S}] {msg}"
    print(line, flush=True)
    try:
        with open(LOG_FILE, "a", encoding="utf-8") as fh:
            fh.write(line + "\n")
    except OSError:
        pass


def port_open(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(1)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def dashboard_healthy(port: int) -> bool:
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/health", timeout=2) as response:
            payload = json.loads(response.read().decode("utf-8"))
        return payload.get("service") == "quant-dashboard"
    except Exception:
        return False


def check_langflow(port: int = LANGFLOW_PORT):
    if port_open(port):
        log(f"[OK] Langflow 已在运行（端口 {port}）")
    else:
        log(f"[!!] Langflow 未运行（端口 {port}）")
        log("     请先启动 Langflow，并导入仓库自带的 flow_current.json")


def start_dashboard(port: int) -> bool:
    if port_open(port):
        if dashboard_healthy(port):
            log(f"[OK] 看板服务已在运行: http://127.0.0.1:{port}/")
            return True
        log(f"[!!] 端口 {port} 被其它服务占用，请用 --dashboard-port 换一个端口")
        return False
    os.makedirs(DATA_DIR, exist_ok=True)
    args = [sys.executable, os.path.join(BASE_DIR, "dashboard_server.py"), "--port", str(port)]
    log("[..] 启动看板服务...")
    flags = 0
    if os.name == "nt":
        flags = getattr(subprocess, "CREATE_NO_WINDOW", 0) | getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    try:
        with open(LOG_FILE, "a", encoding="utf-8") as fh:
            subprocess.Popen(args, cwd=BASE_DIR, stdout=fh, stderr=subprocess.STDOUT,
                             creationflags=flags, close_fds=True)
    except OSError as exc:
        log(f"[!!] 看板启动失败: {exc}")
        return False
    for _ in range(30):
        time.sleep(1)
        if dashboard_healthy(port):
            log(f"[OK] 看板就绪: http://127.0.0.1:{port}/")
            return True
    log("[!!] 看板服务启动超时，请查看 data/run.log")
    return False


def run_bot_foreground(scan_n: int, interval: int):
    args = [sys.executable, os.path.join(BASE_DIR, "main.py"), "run", "--loop",
            "--scan", str(scan_n), "--interval", str(interval)]
    log("=" * 50)
    log("  交易机器人启动（前台模式，关窗口 = 停止）")
    log("=" * 50)
    return subprocess.Popen(args, cwd=BASE_DIR)


def main(argv=None):
    parser = argparse.ArgumentParser(description="AI 量化交易系统一体化启动")
    parser.add_argument("--dashboard-port", type=int, default=DASHBOARD_PORT)
    parser.add_argument("--langflow-port", type=int, default=LANGFLOW_PORT)
    parser.add_argument("--scan", type=int, default=DEFAULT_CONFIG["scan_top_n"])
    parser.add_argument("--interval", type=int, default=DEFAULT_CONFIG["scan_interval"])
    parser.add_argument("--no-browser", action="store_true")
    args = parser.parse_args(argv)

    os.makedirs(DATA_DIR, exist_ok=True)
    log("=" * 50)
    log("  AI 量化交易系统 - 一体化启动")
    log("=" * 50)
    check_langflow(args.langflow_port)
    started = start_dashboard(args.dashboard_port)
    if started and not args.no_browser:
        webbrowser.open(f"http://127.0.0.1:{args.dashboard_port}/")
    proc = run_bot_foreground(args.scan, args.interval)
    try:
        proc.wait()
    except KeyboardInterrupt:
        log("收到 Ctrl+C，停止机器人...")
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    log(f"机器人已退出；看板仍在 http://127.0.0.1:{args.dashboard_port}/（如果启动成功）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
