# -*- coding: utf-8 -*-
"""生成桌面一键启动脚本 (纯ASCII内容, 免疫编码闪退)"""
import os

# 内容全部用英文, 避免任何编码问题
BAT = r'''@echo off
title AI Quant Trading
cd /d "C:\Users\10716\Desktop\agent\quant-sim"
"C:\Users\10716\.workbuddy\binaries\python\envs\langflow310b\Scripts\python.exe" quant_launcher.py
pause
'''

path = r"C:\Users\10716\Desktop\启动量化交易.bat"
with open(path, "w", encoding="ascii", newline="\r\n") as f:
    f.write(BAT)
print("已生成:", path, os.path.getsize(path), "bytes")

# 同时清理旧的复杂脚本(避免混淆)
old = r"C:\Users\10716\Desktop\quant_trading.bat"
if os.path.exists(old):
    os.remove(old)
    print("已移除旧脚本: quant_trading.bat")
