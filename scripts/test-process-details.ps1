# 自动验收进程详情窗口的“模块”页签（#74）。
# 做法：复制 ping.exe 到临时目录并启动（自己的子进程），用 --menu-action details:<pid> 打开详情窗口，
# 等描述 / 公司读完后由钩子把模块行数、有描述的行数写到标准错误，并保存窗口截图（TML_DETAILS_SHOT），再核对：
#   1. 模块行数与 .NET 的 Process.Modules 一致（含 ping.exe 自身与 ntdll.dll）；
#   2. 至少有一部分模块读到了描述（后台线程读文件版本）；
#   3. System 进程（PID 4）显示原因而不是空表（错误非空，模块 0 行）。
# 只结束本脚本自己启动的子进程与应用实例。截图写到 -Out（默认临时目录）。
# 用法：pwsh scripts/test-process-details.ps1 [-Exe target/release/TaskManagerLite.exe] [-Out <目录>]
param(
    [string]$Exe = "$PSScriptRoot/../target/release/TaskManagerLite.exe",
    [string]$Out = ''
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path $Exe).Path
$id = Get-Random -Maximum 99999999
$dir = Join-Path ([System.IO.Path]::GetTempPath()) "tml-details-$id"
New-Item -ItemType Directory -Force $dir | Out-Null
if (-not $Out) { $Out = $dir }
New-Item -ItemType Directory -Force $Out | Out-Null
$env:TML_LOG_DIR = Join-Path $dir 'logs'

$results = @()
function Check($label, [bool]$ok, $detail = '') {
    $script:results += [pscustomobject]@{ Name = $label; Ok = $ok; Detail = $detail }
    "{0} {1} {2}" -f ($(if ($ok) { '通过' } else { '失败' })), $label, $detail
}

# 运行钩子并返回标准错误文本。
function Run-Details($pid_, $shot) {
    $env:TML_DETAILS_SHOT = $shot
    $errFile = Join-Path $dir "err-$pid_.txt"
    $app = Start-Process $exePath -ArgumentList '--allow-multiple', '--menu-action', "details:$pid_" `
        -PassThru -RedirectStandardError $errFile
    if (-not $app.WaitForExit(30000)) { $app.Kill(); $app.WaitForExit() }
    Remove-Item Env:TML_DETAILS_SHOT
    if (Test-Path $errFile) { Get-Content $errFile -Raw -Encoding utf8 } else { '' }
}

$kid = $null
try {
    $t = Join-Path $dir "tml-details-$id.exe"
    Copy-Item (Join-Path $env:WINDIR 'System32\PING.EXE') $t
    $kid = Start-Process $t -ArgumentList '-t', '-w', '7777', '127.0.0.1' -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 800
    if ($kid.HasExited) { throw '子进程没有启动' }

    $expected = (Get-Process -Id $kid.Id).Modules.Count
    $text = Run-Details $kid.Id (Join-Path $Out 'details-modules.bmp')
    $m = [regex]::Match($text, '详情窗口 模块行数=(\d+) 有描述=(\d+) 错误=(.*)')
    $rows = if ($m.Success) { [int]$m.Groups[1].Value } else { -1 }
    $desc = if ($m.Success) { [int]$m.Groups[2].Value } else { -1 }
    Check '模块行数与 Process.Modules 接近' ([math]::Abs($rows - $expected) -le 3 -and $rows -gt 0) "界面=$rows .NET=$expected"
    Check '读到了描述' ($desc -gt 0) "有描述=$desc"
    Check '保存了截图' (Test-Path (Join-Path $Out 'details-modules.bmp'))

    $text4 = Run-Details 4 (Join-Path $Out 'details-system.bmp')
    $m4 = [regex]::Match($text4, '详情窗口 模块行数=(\d+) 有描述=\d+ 错误=(.*)')
    Check 'System 进程显示原因' ($m4.Success -and $m4.Groups[1].Value -eq '0' -and $m4.Groups[2].Value.Trim() -ne '') $m4.Groups[2].Value
}
finally {
    if ($kid -and -not $kid.HasExited) { $kid.Kill() }
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue -Exclude '*.bmp'
}

$bad = @($results | Where-Object { -not $_.Ok }).Count
"`n通过 $($results.Count - $bad) / $($results.Count)"
exit $bad
