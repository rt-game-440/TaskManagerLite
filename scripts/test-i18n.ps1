# 自动验收本地化（#87）：
#   1. python scripts/i18n.py check：所有 Slint / Rust 文案都有英文译文；
#   2. 中文与英文各启动一次 --screenshot 前的日志不崩溃（--lang en 能启动并采到快照）。
# 用法：pwsh scripts/test-i18n.ps1 [-Exe target/release/TaskManagerLite.exe]
param([string]$Exe = "$PSScriptRoot/../target/release/TaskManagerLite.exe")
$ErrorActionPreference = 'Stop'
$ok = $true
python "$PSScriptRoot/i18n.py" check
if ($LASTEXITCODE -ne 0) { $ok = $false; Write-Host 'FAIL 译文缺失' } else { Write-Host 'PASS 译文完整' }
$exePath = (Resolve-Path $Exe).Path
foreach ($lang in 'zh', 'en') {
    $p = Start-Process $exePath -ArgumentList '--allow-multiple', '--measure-startup', '--lang', $lang -PassThru -Wait -WindowStyle Hidden
    if ($p.ExitCode -ne 0) { $ok = $false; Write-Host "FAIL --lang $lang 退出码 $($p.ExitCode)" } else { Write-Host "PASS --lang $lang 启动" }
}
if (-not $ok) { exit 1 }
