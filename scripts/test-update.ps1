# 检查更新（#99 一期）端到端验收（普通权限即可，不访问真实网络，不用 SendInput）。
# 脚本在 localhost 上开一个模拟 GitHub Releases 接口的 HttpListener，用环境变量 TML_UPDATE_URL 指向它，
# 以 `--allow-multiple --check-update` 启动自己的实例（开发模式，不读写设置；查询一次、结果写日志后退出），
# 逐个场景核对日志里的“检查更新 结果=…”：
#   新版本 / 同版本 / 旧版本 / 404 无发布 / 403 限流 / 500 / 非 JSON / 非法标签 / 不可信发布页地址 / 连不上 / 超时。
# 用法：pwsh scripts/test-update.ps1 [-Exe target/release/TaskManagerLite.exe] [-SkipTimeout]
param(
    [string]$Exe = "$PSScriptRoot/../target/release/TaskManagerLite.exe",
    # 超时场景要等约 10 秒。
    [switch]$SkipTimeout
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path $Exe).Path
$results = @()
function Check($label, [bool]$ok, $detail = '') {
    $script:results += [pscustomobject]@{ Name = $label; Ok = $ok; Detail = $detail }
    "{0} {1} {2}" -f ($(if ($ok) { '通过' } else { '失败' })), $label, $detail
}

$cargo = Get-Content "$PSScriptRoot/../Cargo.toml" -Raw
$current = [regex]::Match($cargo, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
$page = 'https://github.com/rt-game-440/TaskManagerLite/releases'

function Free-Port {
    $l = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0); $l.Start()
    $port = $l.LocalEndpoint.Port; $l.Stop(); $port
}
$port = Free-Port
$deadPort = Free-Port

# 模拟服务：路径 → 状态码与正文（正文为 $null 时用默认 JSON 模板）。
$routes = @{
    '/newer'   = @(200, (@{ tag_name = 'v99.0.0'; name = 'TaskManagerLite 99.0.0'; body = "## 新功能`n- 模拟发布说明"; html_url = "$page/tag/v99.0.0" } | ConvertTo-Json))
    '/same'    = @(200, (@{ tag_name = "v$current"; name = ''; body = ''; html_url = "$page/tag/v$current" } | ConvertTo-Json))
    '/older'   = @(200, (@{ tag_name = 'v0.0.1'; name = ''; body = ''; html_url = "$page/tag/v0.0.1" } | ConvertTo-Json))
    '/evil'    = @(200, (@{ tag_name = 'v99.0.0'; name = ''; body = ''; html_url = 'https://evil.example/download' } | ConvertTo-Json))
    '/missing' = @(404, '{"message":"Not Found"}')
    '/limited' = @(403, '{"message":"API rate limit exceeded"}')
    '/broken'  = @(500, 'oops')
    '/garbage' = @(200, '<html>not json</html>')
    '/badtag'  = @(200, (@{ tag_name = 'nightly'; html_url = $page } | ConvertTo-Json))
}

$server = Start-ThreadJob -ArgumentList $port, $routes -ScriptBlock {
    param($port, $routes)
    $l = [System.Net.HttpListener]::new()
    $l.Prefixes.Add("http://localhost:$port/")
    $l.Start()
    try {
        while ($true) {
            $ctx = $l.GetContext()
            $path = $ctx.Request.Url.AbsolutePath
            # 关闭响应后请求对象随之释放，先取出 UA。
            $ua = $ctx.Request.Headers['User-Agent']
            if ($path -eq '/quit') { $ctx.Response.Close(); break }
            if ($path -eq '/slow') { Start-Sleep -Seconds 15 }
            $r = $routes[$path]
            if (-not $r) { $r = @(404, '{}') }
            $bytes = [Text.Encoding]::UTF8.GetBytes($r[1])
            $ctx.Response.StatusCode = $r[0]
            $ctx.Response.ContentType = 'application/json; charset=utf-8'
            try { $ctx.Response.OutputStream.Write($bytes, 0, $bytes.Length); $ctx.Response.Close() } catch {}
            "请求 $path UA=$ua"
        }
    } finally { $l.Stop() }
}
Start-Sleep -Milliseconds 500

function Run-Check($url) {
    $dir = Join-Path ([IO.Path]::GetTempPath()) ("tml-update-" + [guid]::NewGuid())
    New-Item -ItemType Directory $dir | Out-Null
    $env:TML_LOG_DIR = $dir
    $env:TML_UPDATE_URL = $url
    try {
        $p = Start-Process $exePath -ArgumentList '--allow-multiple', '--check-update' -PassThru
        if (-not $p.WaitForExit(30000)) { $p.Kill(); return '（超时未退出）' }
        $log = Get-ChildItem $dir -Filter *.log | Select-Object -First 1
        if (-not $log) { return '（没有日志）' }
        ((Get-Content $log.FullName -Encoding utf8) | Where-Object { $_ -match '检查更新 结果=' }) -join "`n"
    } finally {
        Remove-Item Env:TML_UPDATE_URL, Env:TML_LOG_DIR -ErrorAction SilentlyContinue
        Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$base = "http://localhost:$port"
$cases = @(
    @('发现新版本', "$base/newer", "结果=有新版本 最新版=99\.0\.0 发布页=$([regex]::Escape("$page/tag/v99.0.0"))"),
    @('同版本为最新', "$base/same", "结果=最新 最新版=$([regex]::Escape($current))"),
    @('旧版本不提示降级', "$base/older", '结果=最新 最新版=0\.0\.1'),
    @('不可信发布页退回 Releases 列表', "$base/evil", "结果=有新版本 最新版=99\.0\.0 发布页=$([regex]::Escape($page)) "),
    @('404 视为没有正式发布', "$base/missing", '结果=失败 原因=NoRelease'),
    @('403 视为限流', "$base/limited", '结果=失败 原因=RateLimited'),
    @('其他 HTTP 状态', "$base/broken", '结果=失败 原因=Http\(500\) 错误码=Some\(500\)'),
    @('非 JSON 响应', "$base/garbage", '结果=失败 原因=BadResponse'),
    @('非法标签', "$base/badtag", '结果=失败 原因=BadTag\("nightly"\)'),
    @('连不上服务器', "http://localhost:$deadPort/x", '结果=失败 原因=Unreachable\(12029\)')
)
if (-not $SkipTimeout) { $cases += , @('超时', "$base/slow", '结果=失败 原因=Timeout') }

try {
    foreach ($c in $cases) {
        $line = Run-Check $c[1]
        Check $c[0] ($line -match $c[2]) $line
    }
    # 日志不记发布说明正文。
    $line = Run-Check "$base/newer"
    Check '日志不含发布说明正文' ($line -notmatch '模拟发布说明') $line
} finally {
    try { Invoke-WebRequest "$base/quit" -TimeoutSec 20 | Out-Null } catch {}
    $requests = Receive-Job $server -Wait -AutoRemoveJob
}
$ua = $requests | Where-Object { $_ -match '/newer' } | Select-Object -First 1
Check '请求带 User-Agent' ($ua -match "UA=TaskManagerLite/$([regex]::Escape($current))") $ua

$failed = @($results | Where-Object { -not $_.Ok })
""
"合计 {0} 项，失败 {1} 项" -f $results.Count, $failed.Count
if ($failed.Count) { exit 1 }
