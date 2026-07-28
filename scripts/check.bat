@echo off
chcp 65001 >nul
REM ============================================================
REM  X-Term Rust 类型检查脚本（仅 cargo check，不打包）
REM
REM  用法：双击 scripts\check.bat 或在命令行执行
REM  前置：已安装 Rust / Visual Studio Build Tools
REM ============================================================

echo [check] === X-Term cargo check ===
echo.

REM 1. 加载 MSVC 编译环境（避免 cc 用到 MinGW gcc）。
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if errorlevel 1 (
    echo [check] [错误] 加载 VS 环境失败，请确认 Visual Studio 2022 已安装
    pause
    exit /b 1
)

REM 2. 强制 cc 用 MSVC cl.exe（避免 MinGW 产物与 MSVC 链接器冲突）。
set CC_x86_64_pc_windows_msvc=cl.exe
set CXX_x86_64_pc_windows_msvc=cl.exe
set CC=cl.exe
set CXX=cl.exe

REM 3. 仅做类型检查（不生成产物，速度快）。
cd /d D:\code\tanghan-yunwei\x-term\src-tauri
cargo check --message-format=short
if errorlevel 1 (
    echo.
    echo [check] [错误] cargo check 失败
    pause
    exit /b 1
)

echo.
echo [check] === 检查通过 ===
pause
