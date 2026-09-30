# -*- coding: utf-8 -*-
"""
AI 量化交易系统 - 一体化启动器 (单进程前台版)
=============================================
一个窗口, 全部搞定, 所见即所得:
- 窗口内直接显示机器人运行日志(实时滚动)
- 自动启动/复用看板服务 (8000)
- 自动打开浏览器看板
- 关窗口 / Ctrl+C = 停止机器人(看板服务继续)

用法:
    python quant_launcher.py
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time
import webbrowser
from datetime import datetime

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
STATE_FILE = os.path.join(DATA_DIR, "state.json")
HEARTBEAT_FILE = os.path.join(DATA_DIR, "heartbeat.json")
LOCK_FILE = os.path.join(DATA_DIR, "bot.lock")
LOG_FILE = os.path.join(DATA_DIR, "run.log")
PORT = 8000
LF_PORT = 7860
SCAN_N = 10


def resolve_python():
    """返回用于启动机器人/看板的 python 解释器。

    本项目运行脚本(机器人/看板)已改为零第三方依赖(仅标准库 + 独立运行的 Langflow 服务端),
    因此直接用当前解释器即可, 不再要求 requests。
    """
    log(f"[OK] 使用解释器: {sys.executable} (标准库, 零第三方依赖)")
    return sys.executable


def log(msg):
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"[{ts}] {msg}", flush=True)
    try:
        with open(LOG_FILE, "a", encoding="utf-8") as f:
            f.write(f"[{ts}] {msg}\n")
    except Exception:
        pass


def port_open(port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(1)
        return s.connect_ex(("127.0.0.1", port)) == 0


def check_langflow():
    if port_open(LF_PORT):
        log(f"[OK] Langflow 已在运行 (端口 {LF_PORT})")
    else:
        log(f"[!!] Langflow 未运行 (端口 {LF_PORT})!")
        log("     请先双击启动: agent-studio\\start_langflow.bat")
        log("     或手动运行:   langflow run --host 127.0.0.1 --port 7860")


def start_dashboard():
    if port_open(PORT):
        log(f"[OK] 看板服务已在运行 (端口 {PORT})")
        return
    py = resolve_python()
    args = [py] if isinstance(py, str) else list(py)
    args += [os.path.join(BASE_DIR, "dashboard_server.py"), "--port", str(PORT)]
    log("[..] 启动看板服务...")
    flags = subprocess.CREATE_NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP
    with open(LOG_FILE, "a", encoding="utf-8") as f:
        subprocess.Popen(args, cwd=BASE_DIR, stdout=f, stderr=subprocess.STDOUT,
                         creationflags=flags, close_fds=True)
    for _ in range(30):
        time.sleep(1)
        if port_open(PORT):
            log(f"[OK] 看板就绪: http://127.0.0.1:{PORT}/")
            return
    log("[!!] 看板服务启动超时, 请检查日志")


def open_browser():
    time.sleep(1)
    webbrowser.open(f"http://127.0.0.1:{PORT}/")
    log("[OK] 浏览器看板已打开")


def run_bot_foreground():
    """前台运行机器人: 日志直接显示在本窗口"""
    py = resolve_python()
    args = [py] if isinstance(py, str) else list(py)
    args += [os.path.join(BASE_DIR, "main.py"), "run", "--loop", "--scan", str(SCAN_N)]
    log("=" * 46)
    log("  交易机器人启动 (前台模式)")
    log("  日志实时显示在本窗口, 关窗口 = 停止")
    log("=" * 46)
    # 直接把机器人 stdout/stderr 接到本窗口
    proc = subprocess.Popen(args, cwd=BASE_DIR)
    return proc


def main():
    log("=" * 46)
    log("  AI 量化交易系统 - 一体化启动")
    log("=" * 46)
    check_langflow()
    start_dashboard()
    open_browser()
    # 前台运行机器人(阻塞, 关窗口/CTRL+C 即停)
    proc = run_bot_foreground()
    try:
        proc.wait()
    except KeyboardInterrupt:
        log("收到 Ctrl+C, 停止机器人...")
        proc.terminate()
        proc.wait(timeout=10)
        log("机器人已停止, 窗口可关闭")
    except Exception as e:
        log(f"机器人异常退出: {e}")
    # 机器人结束后等 2 秒让看板数据定格
    time.sleep(2)
    log("看板服务仍在运行 (http://127.0.0.1:8000/), 可刷新查看最终状态")


if __name__ == "__main__":
    main()
