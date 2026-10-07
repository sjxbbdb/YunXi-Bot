@echo off
setlocal

rem ============================================================
rem  Install the Python env for the decision model.
rem
rem  ASCII-only on purpose -- see the note in 启动.cmd.
rem
rem  RUN THIS ON THE TARGET MACHINE, not here.
rem  A venv bakes the absolute path of its base interpreter into
rem  pyvenv.cfg and Scripts\. Installing it on this machine and
rem  carrying it over leaves a path that does not exist there,
rem  and the venv simply will not work.
rem
rem  Needs: Python 3.12 on the target machine, and internet.
rem  Result: about 4 GB (torch's CUDA build is the bulk of it),
rem  kept entirely on this disk.
rem ============================================================

set "ROOT=%~dp0"
set "VENV=%ROOT%python"

echo.
echo   target : %VENV%
echo   deps   : %ROOT%sidecar\requirements.txt
echo.

where python >nul 2>nul
if errorlevel 1 (
  echo   python not found. Install Python 3.12 on this machine first:
  echo       https://www.python.org/downloads/
  echo   Tick "Add python.exe to PATH" during setup.
  echo.
  exit /b 2
)

echo   creating venv ...
python -m venv "%VENV%"
if errorlevel 1 (
  echo   failed to create the venv.
  exit /b 2
)

echo   installing deps (about 1 GB, pulls torch, slow) ...
"%VENV%\Scripts\python.exe" -m pip install --upgrade pip
"%VENV%\Scripts\python.exe" -m pip install -r "%ROOT%sidecar\requirements.txt"
if errorlevel 1 (
  echo.
  echo   install failed. Usually the network -- verdictml comes
  echo   from GitHub. Just run this script again; pip resumes.
  exit /b 2
)

rem ============================================================
rem  CUDA build of torch -- NOT optional for the local model.
rem
rem  requirements.txt pulls plain "torch" from PyPI, and on Windows
rem  that wheel is the CPU-only build. The decision model
rem  (verdict-small) is happy on CPU, so the decision side works and
rem  nothing looks broken -- but the local model (Qwen3-4B) is
rem  useless there: roughly 1-2 tokens/second. And
rem  local_llm_server.py is started with --device cuda, so it would
rem  simply fail.
rem
rem  Uninstall first. Installing over the CPU build makes pip answer
rem  "Requirement already satisfied" and download nothing at all --
rem  which looks like success and changes nothing.
rem
rem  About 2.5-3 GB more. Pick the wheel index matching the GPU;
rem  cu128 is what the development machine uses.
rem ============================================================
echo   installing the CUDA build of torch (about 3 GB) ...
"%VENV%\Scripts\python.exe" -m pip uninstall -y torch
"%VENV%\Scripts\python.exe" -m pip install --index-url https://download.pytorch.org/whl/cu128 torch
if errorlevel 1 (
  echo.
  echo   CUDA torch install failed. Everything else is installed,
  echo   so the decision model will run; the local model will not.
  echo   Re-run this script when the network is back.
  exit /b 2
)

echo   verifying ...
"%VENV%\Scripts\python.exe" -c "import torch;print('  torch',torch.__version__,'cuda',torch.cuda.is_available())"
echo   (if cuda says False, the local model will not run here)

echo.
echo   done. Now run: %ROOT%启动决策模型.cmd
echo.
endlocal
