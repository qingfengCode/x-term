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

REM 2. 从 PATH 中移除 MinGW/TDM-GCC，防止 cc crate 误用 gcc。
set "PATH=%PATH:C:\mypc\TDM-GCC-64\bin;=%"
set "PATH=%PATH:C:\mypc\TDM-GCC-64\bin=%"
set "PATH=%PATH:C:\MinGW\bin;=%"
set "PATH=%PATH:C:\MinGW\bin=%"
set "PATH=%PATH:C:\msys64\mingw64\bin;=%"
set "PATH=%PATH:C:\msys64\mingw64\bin=%"

REM 3. 强制 cc/cmake 用 MSVC cl.exe（覆盖系统级 CC 环境变量）。
set CC=cl.exe
set CXX=cl.exe
set CC_x86_64_pc_windows_msvc=cl.exe
set CXX_x86_64_pc_windows_msvc=cl.exe

echo [build] cl.exe:
where cl
echo.

REM 4. 清除可能被 gcc 污染的构建缓存。
echo [build] 清除 libsqlite3-sys 旧缓存（避免 GNU ABI 残留）...
cd /d D:\code\tanghan-yunwei\x-term\src-tauri
cargo clean -p libsqlite3-sys 2>nul
cd /d D:\code\tanghan-yunwei\x-term

REM 5. 安装前端依赖（如果 node_modules 不存在）。
if not exist node_modules (
    echo [build] 安装前端依赖...
    call pnpm install
    if errorlevel 1 (
        echo [build] [错误] pnpm install 失败
        pause
        exit /b 1
    )
)

REM 6. 执行 Tauri 构建（release + 打包 nsis 安装包）。
echo [build] 开始构建（可能需要几分钟，首次更长）...
echo.
call npx tauri build --bundles nsis
if errorlevel 1 (
    echo.
    echo [build] [错误] 构建失败
    pause
    exit /b 1
)

REM 7. 完成。
echo.
echo [build] === 构建成功！ ===
echo [build] 安装包位置：
echo   D:\code\tanghan-yunwei\x-term\src-tauri\target\release\bundle\nsis\
echo.
echo [build] 可执行文件：
echo   D:\code\tanghan-yunwei\x-term\src-tauri\target\release\x-term.exe
echo.
pause
