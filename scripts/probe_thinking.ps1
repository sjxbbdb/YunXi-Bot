# 实测 DeepSeek 思考模式的真实行为——不靠猜。
#
# 为什么需要这个脚本：把思考模式关掉能省 token，但"省了多少"和"关掉之后
# 工具调用还正不正常"这两件事都会直接改变路由设计。猜错了要么白花钱，
# 要么在复杂任务上悄悄退化。
#
# 用法：pwsh scripts/probe_thinking.ps1
$ErrorActionPreference = 'Stop'

$keyPath = Join-Path $env:LOCALAPPDATA 'YunXiBot\secrets\deepseek.key'
if (-not (Test-Path $keyPath)) { throw "找不到密钥: $keyPath" }
$key = (Get-Content $keyPath -Raw).Trim()

# 绕开代理：本机 Clash 的 NO_PROXY 传播不可靠，直连更可控
$env:HTTP_PROXY = ''
$env:HTTPS_PROXY = ''
$env:ALL_PROXY = ''
$env:NO_PROXY = '*'

$endpoint = 'https://api.deepseek.com/v1/chat/completions'
$headers = @{ 'Authorization' = "Bearer $key"; 'Content-Type' = 'application/json' }

# 一个需要真正推理的问题——简单问题测不出思考模式的价值
$question = '一个仓库有 3 个货架，每个货架 4 层，每层放 6 箱。每天出库 17 箱，' +
            '第几天会不足 10 箱？给出推理过程和最终数字。'

function Invoke-Probe {
    param([string]$Name, [hashtable]$Extra, [string]$Prompt)
    $body = @{
        model    = 'deepseek-flash'
        messages = @(@{ role = 'user'; content = $Prompt })
        max_tokens = 2048
    }
    foreach ($k in $Extra.Keys) { $body[$k] = $Extra[$k] }
    $json = $body | ConvertTo-Json -Depth 10 -Compress

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $r = Invoke-RestMethod -Uri $endpoint -Method Post -Headers $headers -Body $json -TimeoutSec 180
    } catch {
        Write-Host ("[{0}] 失败: {1}" -f $Name, $_.Exception.Message) -ForegroundColor Red
        if ($_.ErrorDetails) { Write-Host $_.ErrorDetails.Message }
        return
    }
    $sw.Stop()

    $u = $r.usage
    $msg = $r.choices[0].message
    $reasoning = if ($msg.reasoning_content) { $msg.reasoning_content.Length } else { 0 }
    $content = if ($msg.content) { $msg.content.Length } else { 0 }
    $calls = if ($msg.tool_calls) { $msg.tool_calls.Count } else { 0 }

    [pscustomobject]@{
        模式           = $Name
        耗时ms         = $sw.ElapsedMilliseconds
        prompt_tokens  = $u.prompt_tokens
        completion     = $u.completion_tokens
        缓存命中tok    = $u.prompt_cache_hit_tokens
        缓存未命中tok  = $u.prompt_cache_miss_tokens
        思考字符数     = $reasoning
        正文字符数     = $content
        工具调用数     = $calls
        finish         = $r.choices[0].finish_reason
    }
}

Write-Host '=== 1. 默认（不传 thinking 字段）===' -ForegroundColor Cyan
Invoke-Probe -Name '默认' -Extra @{} -Prompt $question | Format-List

Write-Host '=== 2. thinking 显式关 ===' -ForegroundColor Cyan
Invoke-Probe -Name '关' -Extra @{ thinking = @{ type = 'disabled' } } -Prompt $question | Format-List

Write-Host '=== 3. thinking 显式开 ===' -ForegroundColor Cyan
Invoke-Probe -Name '开' -Extra @{ thinking = @{ type = 'enabled' } } -Prompt $question | Format-List

Write-Host '=== 4. 开思考 + 工具调用（验证是否 400）===' -ForegroundColor Cyan
$tools = @(@{
    type = 'function'
    function = @{
        name = 'get_weather'
        description = '查询某地天气'
        parameters = @{
            type = 'object'
            properties = @{ city = @{ type = 'string'; description = '城市名' } }
            required = @('city')
        }
    }
})
Invoke-Probe -Name '开+工具' -Extra @{ thinking = @{ type = 'enabled' }; tools = $tools } `
    -Prompt '北京现在天气怎么样？用工具查。' | Format-List

Write-Host '=== 5. 关思考 + 工具调用（对照）===' -ForegroundColor Cyan
Invoke-Probe -Name '关+工具' -Extra @{ thinking = @{ type = 'disabled' }; tools = $tools } `
    -Prompt '北京现在天气怎么样？用工具查。' | Format-List
