# 自动验收进程右键菜单“创建转储文件”（#75）。
# 做法：复制 ping.exe 到临时目录并启动（自己的子进程），用
#   --menu-action dump-mini:<pid> / dump-full:<pid>   （等价于点菜单项；完整转储自动点确认框）
# 并用环境变量 TML_DUMP_PATH 指定输出路径（不弹保存对话框），再核对：
#   1. 最小 / 完整转储都写出了文件，且文件头是 MDMP；完整转储明显大于最小转储；
#   2. 受保护的 System 进程（PID 4）被拒绝，不留下文件；
#   3. 32 位进程（SysWOW64\ping.exe）被拒绝（位数不匹配），不留下文件；
#   4. 目标目录不存在时失败且不留下文件；日志只记 PID、类型、结果与错误码。
# 只结束本脚本自己启动的子进程与应用实例；文件写在临时目录。
# 用法：pwsh scripts/test-dump.ps1 [-Exe target/release/TaskManagerLite.exe]
param(
    [string]$Exe = "$PSScriptRoot/../target/release/TaskManagerLite.exe"
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path $Exe).Path
$id = Get-Random -Maximum 99999999
$dir = Join-Path ([System.IO.Path]::GetTempPath()) "tml-dump-$id"
New-Item -ItemType Directory -Force $dir | Out-Null
$logDir = Join-Path $dir 'logs'
$env:TML_LOG_DIR = $logDir

$results = @()
function Check($label, [bool]$ok, $detail = '') {
    $script:results += [pscustomobject]@{ Name = $label; Ok = $ok; Detail = $detail }
    "{0} {1} {2}" -f ($(if ($ok) { '通过' } else { '失败' })), $label, $detail
}
function Log-Text($root = $logDir) {
    $t = ''
    Get-ChildItem $root -Filter *.log -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
        $t += (Get-Content $_.FullName -Raw -Encoding utf8)
    }
    $t
}
function Start-Target($src, $tag) {
    $t = Join-Path $dir "tml-dump-$tag-$id.exe"
    Copy-Item $src $t
    $p = Start-Process $t -ArgumentList '-t', '-w', '7777', '127.0.0.1' -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 800
    if ($p.HasExited) { throw "子进程 $tag 没有启动" }
    $p
}
function Run-Dump($action, $pid_, $out, [int]$seconds = 20) {
    $env:TML_DUMP_PATH = $out
    # 每次运行用独立的日志子目录，等待时只看本次运行的日志。
    $runLog = Join-Path $logDir ([guid]::NewGuid().ToString('N'))
    $env:TML_LOG_DIR = $runLog
    $app = Start-Process $exePath -ArgumentList '--menu-action', "${action}:${pid_}" -PassThru
    $end = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $end) {
        $t = Log-Text $runLog
        if ($t -match "转储 (最小|完整) (成功|失败|已取消) pid=$pid_" -or $t -match "转储 拒绝 pid=$pid_") { break }
        Start-Sleep -Milliseconds 300
    }
    Start-Sleep -Milliseconds 500
    if (-not $app.HasExited) { $app.Kill(); $app.WaitForExit() }
    Remove-Item Env:TML_DUMP_PATH
    $env:TML_LOG_DIR = $logDir
}

$kids = @()
try {
    $x64 = Start-Target (Join-Path $env:WINDIR 'System32\PING.EXE') 'x64'
    $kids += $x64

    $mini = Join-Path $dir 'mini.dmp'
    Run-Dump 'dump-mini' $x64.Id $mini
    $okMini = Test-Path $mini
    $head = if ($okMini) { [System.Text.Encoding]::ASCII.GetString((Get-Content $mini -AsByteStream -TotalCount 4)) } else { '' }
    Check '最小转储写出文件' $okMini
    Check '最小转储文件头为 MDMP' ($head -eq 'MDMP') "头=$head"

    $full = Join-Path $dir 'full.dmp'
    Run-Dump 'dump-full' $x64.Id $full 60
    $okFull = Test-Path $full
    Check '完整转储写出文件' $okFull
    if ($okMini -and $okFull) {
        $sm = (Get-Item $mini).Length; $sf = (Get-Item $full).Length
        Check '完整转储大于最小转储' ($sf -gt $sm) "最小=$sm 完整=$sf"
    }

    $sys = Join-Path $dir 'system.dmp'
    Run-Dump 'dump-mini' 4 $sys 15
    Check '受保护进程被拒绝且不留文件' (-not (Test-Path $sys))

    $wow = Join-Path $env:WINDIR 'SysWOW64\PING.EXE'
    if (Test-Path $wow) {
        $x86 = Start-Target $wow 'x86'
        $kids += $x86
        $out86 = Join-Path $dir 'x86.dmp'
        Run-Dump 'dump-mini' $x86.Id $out86
        Check '32 位进程被拒绝且不留文件' (-not (Test-Path $out86))
    }

    $bad = Join-Path $dir 'no-such-dir\bad.dmp'
    Run-Dump 'dump-mini' $x64.Id $bad
    Check '目标目录不存在时失败且不留文件' (-not (Test-Path $bad) -and -not (Test-Path (Split-Path $bad)))

    $log = Log-Text
    Check '日志记录了成功与失败结果' (($log -match '转储 最小 成功') -and ($log -match '转储 最小 失败'))
    Check '日志不含临时目录路径' (-not $log.Contains($dir))
}
finally {
    foreach ($k in $kids) { if ($k -and -not $k.HasExited) { $k.Kill() } }
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}

$failed = @($results | Where-Object { -not $_.Ok })
"`n共 $($results.Count) 项，失败 $($failed.Count) 项"
if ($failed.Count) { exit 1 }
