@echo off
setlocal

rem ============================================================
rem  YunXi Bot portable launcher
rem
rem  IMPORTANT: this file is ASCII-only on purpose.
rem  cmd.exe reads .cmd files using the OEM codepage (GBK on a
rem  Chinese Windows), NOT utf-8. Chinese text here gets mangled
rem  and the lines are then run as commands. Keep it ASCII.
rem  The Chinese docs live in 说明.md, which editors read as utf-8.
rem
rem  Never hardcode a drive letter: this disk may be F: or H: on
rem  another machine. Everything is derived from %~dp0.
rem ============================================================

set "ROOT=%~dp0"
set "YUNXI_BOT_HOME=%ROOT%data"

set "EXE=%ROOT%target\release\yunxi-bot.exe"
if not exist "%EXE%" set "EXE=%ROOT%target\debug\yunxi-bot.exe"

if not exist "%EXE%" (
  echo.
  echo   yunxi-bot.exe not found.
  echo   Build it once from %ROOT% with:
  echo       cargo build --release --bin yunxi-bot
  echo.
  exit /b 2
)

rem Probe the decision model. Report only; never block.
rem Without it, steps that need a call get escalated to a human instead.
set "VERDICT_UP=0"
for /f %%i in ('powershell -NoProfile -Command "try{(Invoke-WebRequest -Uri http://127.0.0.1:17870/health -TimeoutSec 2 -UseBasicParsing).StatusCode}catch{0}"') do set "VERDICT_UP=%%i"
if not "%VERDICT_UP%"=="200" (
  echo   [note] decision model is not running. To start it, open a
  echo          separate window and run: %ROOT%启动决策模型.cmd
  echo.
)

"%EXE%" %*
endlocal
