# 自动验收进程行变化高亮（#79）：新启动的进程出现“新增”事件，退出后出现“退出”事件，关闭开关后不再有事件。
# 做法：用 --allow-multiple 与临时设置目录启动应用，启动/结束自己的 ping 子进程，读日志“行变化 新增=N 退出=M”。
# 日志只记数量；不依赖 SendInput。需要先 cargo build -p tml-app --release。
# 用法：pwsh scripts/test-row-changes.ps1 [-Exe target/release/TaskManagerLite.exe]
param(
    [string]$Exe = "$PSScriptRoot/../target/release/TaskManagerLite.exe"
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path $Exe).Path
$id = Get-Random -Maximum 99999999
$dir = Join-Path ([System.IO.Path]::GetTempPath()) "tml-rows-$id"
$results = @()
function Check($label, [bool]$ok, $detail = '') {
    $script:results += [pscustomobject]@{ Name = $label; Ok = $ok; Detail = $detail }
    "{0} {1} {2}" -f ($(if ($ok) { '通过' } else { '失败' })), $label, $detail
}
function Run-Case($tag, $settingsJson) {
    $case = Join-Path $dir $tag
    New-Item -ItemType Directory -Force "$case/settings" | Out-Null
    if ($settingsJson) { Set-Content "$case/settings/settings.json" $settingsJson -Encoding utf8 }
    $env:TML_SETTINGS_DIR = "$case/settings"
    $env:TML_LOG_DIR = "$case/logs"
    $app = Start-Process $exePath -ArgumentList '--allow-multiple' -PassThru
    Start-Sleep -Seconds 4
    $ping = Start-Process "$env:WINDIR\System32\PING.EXE" -ArgumentList '-t', '127.0.0.1' -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 4
    $ping.Kill(); $ping.WaitForExit()
    Start-Sleep -Seconds 4
    if (-not $app.HasExited) { $app.Kill(); $app.WaitForExit() }
    $t = ''
    Get-ChildItem "$case/logs" -Filter *.log -Recurse -ErrorAction SilentlyContinue | ForEach-Object { $t += (Get-Content $_.FullName -Raw -Encoding utf8) }
    $t
}

try {
    $on = Run-Case 'on' $null
    Check '开启时记录到新增事件' ($on -match '行变化 新增=[1-9]')
    Check '开启时记录到退出事件' ($on -match '行变化 新增=\d+ 退出=[1-9]')
    $off = Run-Case 'off' '{"highlight_changes": false}'
    Check '关闭开关后没有行变化事件' ($off -notmatch '行变化')
} finally {
    Remove-Item Env:TML_SETTINGS_DIR, Env:TML_LOG_DIR -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
$bad = @($results | Where-Object { -not $_.Ok }).Count
"`n共 $($results.Count) 项，失败 $bad 项"
exit ($(if ($bad) { 1 } else { 0 }))
