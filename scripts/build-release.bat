@echo off
REM ============================================================
REM  X-Term release build (ASCII-only to avoid cmd.exe UTF-8 bug)
REM
REM  Output:
REM    src-tauri\target\release\x-term.exe
REM    src-tauri\target\release\bundle\nsis\*_setup.exe
REM
REM  Prereq: Node.js / pnpm / Rust (MSVC) / Visual Studio 2022
REM ============================================================

setlocal enabledelayedexpansion

echo [build] === X-Term Release Build ===

REM 1. Load MSVC env (avoid cc falling back to MinGW gcc).
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if errorlevel 1 (
    echo [build] [ERROR] Failed to load VS env. Check Visual Studio 2022.
    exit /b 1
)

REM 2. Strip MinGW/TDM-GCC from PATH so cc crate uses MSVC.
set "PATH=%PATH:C:\mypc\TDM-GCC-64\bin;=%"
set "PATH=%PATH:C:\mypc\TDM-GCC-64\bin=%"
set "PATH=%PATH:C:\MinGW\bin;=%"
set "PATH=%PATH:C:\MinGW\bin=%"
set "PATH=%PATH:C:\msys64\mingw64\bin;=%"
set "PATH=%PATH:C:\msys64\mingw64\bin=%"

REM 3. Force cc/cmake to MSVC cl.exe (override system CC=gcc).
set CC=cl.exe
set CXX=cl.exe
set CC_x86_64_pc_windows_msvc=cl.exe
set CXX_x86_64_pc_windows_msvc=cl.exe

echo [build] cl.exe:
where cl

REM 4. Clean sqlite cache that may be polluted by GNU ABI.
echo [build] Cleaning libsqlite3-sys cache...
pushd "%~dp0..\src-tauri"
cargo clean -p libsqlite3-sys 2>nul
popd

REM 5. Install frontend deps if missing.
if not exist "%~dp0..\node_modules" (
    echo [build] Installing frontend deps...
    pushd "%~dp0.."
    call pnpm install
    if errorlevel 1 (
        echo [build] [ERROR] pnpm install failed
        popd
        exit /b 1
    )
    popd
)

REM 6. Run Tauri build (release + nsis installer).
echo [build] Building (this may take several minutes)...
pushd "%~dp0.."
call npx tauri build --bundles nsis
set BUILD_ERR=%errorlevel%
popd

if not "%BUILD_ERR%"=="0" (
    echo [build] [ERROR] Build failed with code %BUILD_ERR%
    exit /b %BUILD_ERR%
)

echo.
echo [build] === Build succeeded ===
echo [build] Installer: src-tauri\target\release\bundle\nsis\
echo [build] Executable: src-tauri\target\release\x-term.exe
endlocal
exit /b 0
