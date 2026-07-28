@echo off
chcp 65001 >nul
REM Load MSVC (VS 2022) env so cc crate uses cl.exe (not MinGW gcc) for libsqlite3-sys.
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
echo [run-dev] cl.exe path:
where cl
echo [run-dev] VCINSTALLDIR=%VCINSTALLDIR%
REM Force cc/cmake crate to use MSVC cl.exe (not MinGW gcc) for windows-msvc target.
REM Prevents libsqlite3-sys producing MinGW-style .o that MSVC link.exe can't link.
set CC_x86_64_pc_windows_msvc=cl.exe
set CXX_x86_64_pc_windows_msvc=cl.exe
set CC=cl.exe
set CXX=cl.exe
echo [run-dev] CC=%CC%
echo [run-dev] starting tauri dev ...
cd /d D:\code\tanghan-yunwei\x-term
pnpm tauri:dev
