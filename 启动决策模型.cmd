@echo off
setlocal

rem ============================================================
rem  Start the decision-model sidecar (Verdict).
rem
rem  ASCII-only on purpose -- see the note in 启动.cmd.
rem  No hardcoded drive letter; everything comes from %~dp0.
rem
rem  verdict_server.py resolves the model directory from
rem  YUNXI_BOT_HOME (verdict_server.py:119), so pointing that at
rem  the on-disk data\ makes the weights travel with the drive.
rem ============================================================

set "ROOT=%~dp0"
set "YUNXI_BOT_HOME=%ROOT%data"

set "PY=%ROOT%python\Scripts\python.exe"
if not exist "%PY%" set "PY=python"

if not exist "%YUNXI_BOT_HOME%\models\verdict-small\model.safetensors" (
  echo.
  echo   Model weights missing:
  echo       %YUNXI_BOT_HOME%\models\verdict-small\model.safetensors
  echo   Copy them from the original machine, or fetch once online:
  echo       %PY% scripts\fetch_model.py --home "%YUNXI_BOT_HOME%"
  echo.
  exit /b 2
)

echo   model  : %YUNXI_BOT_HOME%\models\verdict-small
echo   listen : http://127.0.0.1:17870
echo   Ctrl+C to stop.
echo.

"%PY%" "%ROOT%sidecar\verdict_server.py" --port 17870 --model verdict-small
endlocal
