# YunXi Bot 一键安装
#
#   pwsh scripts/setup.ps1
#
# 做三件事：建 venv、装依赖、拉决策模型权重（校验 SHA256）。
# 跑完就能直接 `cargo run -- agent`。
#
# 为什么权重不放在 git 里：单个 model.safetensors 有 448.8 MB，超过 GitHub 的
# 100 MiB 单文件硬上限，推不上去。所以走 Release 资产 + 本仓库的拉取脚本。

[CmdletBinding()]
param(
    [string]$Python = "py -3.12",
    [switch]$SkipModel,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($n, $text) { Write-Host "`n[$n] $text" -ForegroundColor Cyan }
function Ok($text)        { Write-Host "    ✓ $text" -ForegroundColor Green }
function Warn($text)      { Write-Host "    ! $text" -ForegroundColor Yellow }

# —— 1. venv ——
Step 1 "Python 虚拟环境"
$venvPy = Join-Path $root ".venv\Scripts\python.exe"
if (Test-Path $venvPy) {
    Ok "已存在：.venv"
} else {
    Write-Host "    创建 .venv ..."
    $parts = $Python -split ' '
    & $parts[0] $parts[1..($parts.Count-1)] -m venv .venv
    if ($LASTEXITCODE -ne 0) { throw "建 venv 失败，请确认已安装 Python 3.12" }
    Ok "已创建 .venv"
}

# —— 2. 依赖 ——
Step 2 "安装 Python 依赖（约 1 GB，含 torch CPU 版）"
& $venvPy -m pip install --quiet --upgrade pip
& $venvPy -m pip install -r sidecar\requirements.txt
if ($LASTEXITCODE -ne 0) { throw "依赖安装失败" }
Ok "依赖就绪"

# —— 3. 决策模型权重 ——
if ($SkipModel) {
    Step 3 "跳过模型权重（-SkipModel）"
    Warn "sidecar 会回落到 HuggingFace 下载，首次启动慢，且需要联网"
} else {
    Step 3 "拉取决策模型权重（约 350 MB，校验 SHA256）"
    $fetchArgs = @("scripts\fetch_model.py")
    if ($Force) { $fetchArgs += "--force" }
    & $venvPy @fetchArgs
    if ($LASTEXITCODE -ne 0) {
        throw "权重拉取失败。可直接用 HuggingFace 兜底：删掉 models/manifest.json 里的 sha256_archive 再试"
    }
    Ok "决策模型就绪"
}

Write-Host "`n完成。下一步：" -ForegroundColor Cyan
Write-Host "    cargo build"
Write-Host "    cargo run -- isolation-check          # 验证写入隔离真的生效"
Write-Host "    .venv\Scripts\python.exe sidecar\verdict_server.py --port 17870"
Write-Host "    cargo run -- agent                    # 跑一个 Agent 回合"
Write-Host ""
Write-Host "Agnes 密钥放在（仓库外）：$env:LOCALAPPDATA\YunXiBot\secrets\agnes.key"
Write-Host "或用环境变量 YUNXI_BOT_AGNES_KEY 覆盖。"
