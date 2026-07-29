@echo off
chcp 65001 >nul
REM ============================================================
REM  X-Term 构建脚本（release 打包）
REM
REM  用法：双击 scripts\build.bat 或在命令行执行
REM  产物：src-tauri\target\release\bundle\nsis\ 下的 NSIS 安装包
REM
REM  版本号：手动修改 src-tauri\tauri.conf.json 和 package.json 中的 version 字段
REM
REM  前置：已安装 Node.js / pnpm / Rust / Visual Studio Build Tools
REM ============================================================

echo [build] === X-Term Release 构建 ===
echo.

REM 1. 加载 MSVC 编译环境（避免 cc 用到 MinGW gcc）。
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if errorlevel 1 (
    echo [build] [错误] 加载 VS 环境失败，请确认 Visual Studio 2022 已安装
    pause
    exit /b 1
)

REM 2. 强制 cc/cmake 用 MSVC cl.exe（避免 MinGW 产物与 MSVC 链接器冲突）。
set CC_x86_64_pc_windows_msvc=cl.exe
set CXX_x86_64_pc_windows_msvc=cl.exe
set CC=cl.exe
set CXX=cl.exe

echo [build] cl.exe:
where cl
echo.

REM 3. 安装前端依赖（如果 node_modules 不存在）。
cd /d D:\code\tanghan-yunwei\x-term
if not exist node_modules (
    echo [build] 安装前端依赖...
    call pnpm install
    if errorlevel 1 (
        echo [build] [错误] pnpm install 失败
        pause
        exit /b 1
    )
)

REM 4. 执行 Tauri 构建（release + 打包 nsis 安装包）。
echo [build] 开始构建（可能需要几分钟，首次更长）...
echo.
call npx tauri build --bundles nsis
if errorlevel 1 (
    echo.
    echo [build] [错误] 构建失败
    pause
    exit /b 1
)

REM 5. 完成。
echo.
echo [build] === 构建成功！ ===
echo [build] 安装包位置：
echo   D:\code\tanghan-yunwei\x-term\src-tauri\target\release\bundle\nsis\
echo.
echo [build] 可执行文件：
echo   D:\code\tanghan-yunwei\x-term\src-tauri\target\release\x-term.exe
echo.
pause
