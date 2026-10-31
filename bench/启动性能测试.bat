@echo off
chcp 65001 >nul
title LittleDB 性能测试台
cd /d %~dp0
where python >nul 2>nul
if errorlevel 1 (
    echo 未找到 python，请先安装 Python 3.8+ 并加入 PATH
    pause
    exit /b 1
)
if not exist "..\dist\littledb.exe" (
    echo 未找到 ..\dist\littledb.exe，请先跑 littledb-rust-src\tools\verify.ps1 构建
    pause
    exit /b 1
)
python app.py
pause
