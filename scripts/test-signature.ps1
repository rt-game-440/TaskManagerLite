# 自动验收“签名”详细信息列（#76）。
# 做法：用 TML_SETTINGS_DIR 指向临时目录，settings.json 只显示“签名”列；
#   --allow-multiple 启动实例，环境变量 TML_SIGNATURE_DUMP 让每次签名结果刷新把
#   “PID<TAB>结果<TAB>签名者”写进文件，再核对：
#   1. 系统自带进程（svchost.exe，目录签名）为已验证且签名者含 Microsoft；
#   2. 自己编译的本程序（没有 Authenticode 签名）复制一份运行，显示为未签名；
#   3. 整体校验在若干秒内完成，且应用响应（实例仍在运行）。
# 只结束本脚本自己启动的实例与子进程。
# 用法：pwsh scripts/test-signature.ps1 [-Exe target/release/TaskManagerLite.exe]
param(
    [string]$Exe = "$PSScriptRoot/../target/release/TaskManagerLite.exe"
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path $Exe).Path
$id = Get-Random -Maximum 99999999
$dir = Join-Path ([System.IO.Path]::GetTempPath()) "tml-sig-$id"
New-Item -ItemType Directory -Force $dir | Out-Null
$env:TML_SETTINGS_DIR = Join-Path $dir 'settings'
$env:TML_LOG_DIR = Join-Path $dir 'logs'
New-Item -ItemType Directory -Force $env:TML_SETTINGS_DIR | Out-Null
'{"detail_columns":[{"id":"signature"}]}' | Set-Content (Join-Path $env:TML_SETTINGS_DIR 'settings.json') -Encoding utf8

$results = @()
function Check($label, [bool]$ok, $detail = '') {
    $script:results += [pscustomobject]@{ Name = $label; Ok = $ok; Detail = $detail }
    "{0} {1} {2}" -f ($(if ($ok) { '通过' } else { '失败' })), $label, $detail
}

# 未签名目标：把自己编译的 exe 复制一份（本项目产物没有 Authenticode 签名）。
$unsigned = Join-Path $dir "tml-unsigned-$id.exe"
Copy-Item $exePath $unsigned
$env:TML_SIGNATURE_DUMP = Join-Path $dir 'sig.txt'
$kid = $null
$app = $null
try {
    # 用作未签名目标的子进程：运行复制出来的本程序（带 --allow-multiple，只为常驻）。
    $kid = Start-Process $unsigned -ArgumentList '--allow-multiple' -PassThru -WindowStyle Minimized
    Start-Sleep -Milliseconds 800
    $started = Get-Date
    $app = Start-Process $exePath -ArgumentList '--allow-multiple' -PassThru
    $svchosts = @(Get-Process svchost -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })

    $lines = @()
    $end = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $end) {
        if (Test-Path $env:TML_SIGNATURE_DUMP) {
            $lines = Get-Content $env:TML_SIGNATURE_DUMP -Encoding utf8
            $a = $lines | Where-Object { $_ -match "^(\d+)`tsigned`t" -and $svchosts -contains [int]$Matches[1] }
            $b = $lines | Where-Object { $_ -like "$($kid.Id)`t*" }
            if ($a -and $b) { break }
        }
        Start-Sleep -Milliseconds 500
    }
    $elapsed = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)

    $a = $lines | Where-Object { $_ -match "^(\d+)`tsigned`t" -and $svchosts -contains [int]$Matches[1] } | Select-Object -First 1
    $b = $lines | Where-Object { $_ -like "$($kid.Id)`t*" } | Select-Object -First 1
    $pa = @(if ($a) { $a -split "`t" })
    Check 'svchost.exe 已验证' ($pa.Count -ge 2 -and $pa[1] -eq 'signed') "结果=$($pa[1])"
    Check 'svchost.exe 签名者含 Microsoft' ($pa.Count -ge 3 -and $pa[2] -match 'Microsoft') "签名者=$($pa[2])"
    $pb = @(if ($b) { $b -split "`t" })
    Check '自己编译的 exe 显示未签名' ($pb.Count -ge 2 -and $pb[1] -eq 'unsigned') "结果=$($pb[1])"
    $signed = ($lines | Where-Object { $_ -match "`tsigned`t" }).Count
    Check '多数进程已有校验结果' ($lines.Count -ge 10) "共 $($lines.Count) 个，已验证 $signed 个，用时 $elapsed 秒"
    Check '校验期间应用没有退出' (-not $app.HasExited)
}
finally {
    foreach ($p in @($app, $kid)) { if ($p -and -not $p.HasExited) { $p.Kill(); $p.WaitForExit() } }
    Remove-Item Env:TML_SIGNATURE_DUMP -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
$bad = @($results | Where-Object { -not $_.Ok }).Count
"共 $($results.Count) 项，失败 $bad 项"
if ($bad -gt 0) { exit 1 }
