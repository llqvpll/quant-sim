@echo off
chcp 65001 >nul
set "PY=C:\Users\10716\AppData\Local\Programs\Python\Python310\python.exe"
if not exist "%PY%" set "PY=python"
"%PY%" "%~dp0apply_flow_patch.py"
echo.
pause
