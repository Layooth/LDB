# LittleDB 发布门禁：测试 → 发布构建 → 部署 → 冒烟（HTTP / 会话 / 并发 / 畸形请求）
#
# 用法（在仓库任意位置）：
#   pwsh -File tools\verify.ps1              # 全流程
#   pwsh -File tools\verify.ps1 -SkipDeploy   # 不覆盖根目录 littledb.exe
#   pwsh -File tools\verify.ps1 -SmokeOnly    # 只跑冒烟（用现有 littledb.exe）
#
# 退出码：0 = 全部通过；1 = 有失败（脚本会打印失败项）

[CmdletBinding()]
param(
    [switch]$SkipDeploy,
    [switch]$SmokeOnly
)

$ErrorActionPreference = "Stop"
$src = Split-Path -Parent $PSScriptRoot      # littledb-rust-src
$repo = Split-Path -Parent $src              # 仓库根（含 littledb.exe / web / data）
$exe = Join-Path $repo "littledb.exe"

function Step($text) { Write-Host "`n== $text ==" -ForegroundColor Cyan }
function Ok($text) { Write-Host "  [OK] $text" -ForegroundColor Green }
function Fail($text) { throw $text }

function Get-FreePort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    return $port
}

function Wait-Port($port, $seconds = 10) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $client = [System.Net.Sockets.TcpClient]::new("127.0.0.1", $port)
            $client.Close()
            return $true
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    return $false
}

function Invoke-Api($method, $url, $body, $sid, $token) {
    $headers = @{}
    if ($token) { $headers["Authorization"] = "Bearer $token" }
    if ($sid) { $headers["X-Session-Id"] = $sid }
    $params = @{
        Uri                = $url
        Method             = $method
        Headers            = $headers
        UseBasicParsing    = $true
        SkipHttpErrorCheck = $true
        TimeoutSec         = 30
    }
    if ($body) {
        $params["Body"] = $body
        $params["ContentType"] = "application/json"
    }
    Invoke-WebRequest @params
}

function Sql($base, $token, $sql, $sid) {
    $payload = @{ sql = $sql } | ConvertTo-Json -Compress
    Invoke-Api "POST" "$base/api/sql" $payload $sid $token
}

function Raw-Http($port, $request) {
    $client = [System.Net.Sockets.TcpClient]::new("127.0.0.1", $port)
    $client.ReceiveTimeout = 5000
    $stream = $client.GetStream()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($request)
    $stream.Write($bytes, 0, $bytes.Length)
    $buffer = New-Object byte[] 8192
    $builder = New-Object System.Text.StringBuilder
    try {
        while ($true) {
            $n = $stream.Read($buffer, 0, $buffer.Length)
            if ($n -le 0) { break }
            [void]$builder.Append([System.Text.Encoding]::UTF8.GetString($buffer, 0, $n))
        }
    } catch { }
    $client.Close()
    return $builder.ToString()
}

# ---------------------------------------------------------------- 构建阶段

if (-not $SmokeOnly) {
    Step "1/4 单元 + 集成测试"
    & (Join-Path $src "build.cmd") test --quiet
    if ($LASTEXITCODE -ne 0) { Fail "测试失败" }
    Ok "测试通过"

    Step "2/4 发布构建"
    & (Join-Path $src "build.cmd") build --release --quiet
    if ($LASTEXITCODE -ne 0) { Fail "发布构建失败" }
    Ok "发布构建通过"

    if (-not $SkipDeploy) {
        Step "3/4 部署"
        Copy-Item (Join-Path $src "target\release\littledb.exe") $exe -Force
        Ok "已更新 $exe"
    }
}

if (-not (Test-Path $exe)) { Fail "找不到 $exe，请先构建或去掉 -SmokeOnly" }

# ---------------------------------------------------------------- 冒烟阶段

Step "4/4 冒烟"

$tempDir = Join-Path $env:TEMP ("ldb-verify-" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $tempDir | Out-Null
$httpPort = Get-FreePort
$protoPort = Get-FreePort
$token = "verify-" + [Guid]::NewGuid().ToString("N").Substring(0, 8)
$base = "http://127.0.0.1:$httpPort"
$proc = $null
$checks = 0

# 令牌走环境变量（不落命令行）：顺带验证 LDB_TOKEN 这条路径
$env:LDB_TOKEN = $token

try {
    $proc = Start-Process -PassThru -FilePath $exe -ArgumentList @(
        "serve", "`"$tempDir`"", "--port", "$protoPort", "--http-port", "$httpPort",
        "--no-open"
    ) -RedirectStandardOutput (Join-Path $tempDir "server.log") `
      -RedirectStandardError (Join-Path $tempDir "server.err")

    if (-not (Wait-Port $httpPort)) { Fail "服务未在 10 秒内监听 $httpPort" }
    Ok "服务已启动 (HTTP $httpPort / LDBP $protoPort)"

    # 1. 认证
    $r = Invoke-Api "GET" "$base/api/health" $null $null $null
    if ([int]$r.StatusCode -ne 401) { Fail "无 token 应返回 401，实际 $($r.StatusCode)" }
    $checks++; Ok "无 token → 401"

    $r = Invoke-Api "GET" "$base/api/health" $null $null $token
    if ([int]$r.StatusCode -ne 200) { Fail "health 应 200，实际 $($r.StatusCode)" }
    # 默认不放行跨源（安全默认值；要放行必须显式 --cors-origin）
    if ($r.Headers["Access-Control-Allow-Origin"]) {
        Fail "默认不应返回 Access-Control-Allow-Origin，实际 $($r.Headers["Access-Control-Allow-Origin"])"
    }
    $checks++; Ok "health → 200（默认不放行跨源）"

    # 2. 建表 / 写入 / 查询
    $r = Sql $base $token "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, price DECIMAL(10,2))"
    if ([int]$r.StatusCode -ne 200) { Fail "建表失败: $($r.Content)" }
    $sid = [string]$r.Headers["X-Session-Id"]
    if (-not $sid) { Fail "响应缺少 X-Session-Id" }

    $r = Sql $base $token "INSERT INTO t VALUES (1,'a','19.99'),(2,'b','5.00'),(3,'c','1.00')" $sid
    if ([int]$r.StatusCode -ne 200) { Fail "插入失败: $($r.Content)" }
    $r = Sql $base $token "SELECT COUNT(*) FROM t" $sid
    if ($r.Content -notmatch '\[\[3\]\]') { Fail "行数应为 3: $($r.Content)" }
    $checks++; Ok "建表 + 插入 3 行"

    # 3. 事务跨请求 + 回滚
    [void](Sql $base $token "BEGIN" $sid)
    [void](Sql $base $token "INSERT INTO t VALUES (4,'d','2.50')" $sid)
    $r = Sql $base $token "SELECT COUNT(*) FROM t" $sid
    if ($r.Content -notmatch '\[\[4\]\]') { Fail "事务内应看到自己的未提交行: $($r.Content)" }
    [void](Sql $base $token "ROLLBACK" $sid)
    $r = Sql $base $token "SELECT COUNT(*) FROM t" $sid
    if ($r.Content -notmatch '\[\[3\]\]') { Fail "回滚后应回到 3 行: $($r.Content)" }
    $checks++; Ok "事务跨请求 + 回滚"

    # 4. 并发写（4 会话 × 20 行）
    $parallel = 1..4 | ForEach-Object -Parallel {
        $port = $using:httpPort
        $token = $using:token
        $sid = $using:sid
        $ok = 0
        foreach ($i in 1..20) {
            $id = 100 + ($_ * 100) + $i
            $body = @{ sql = "INSERT INTO t VALUES ($id,'p$id','1.00')" } | ConvertTo-Json -Compress
            $resp = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sql" -Method Post `
                -Headers @{ Authorization = "Bearer $token"; "X-Session-Id" = $sid } `
                -Body $body -ContentType "application/json" -UseBasicParsing -SkipHttpErrorCheck
            if ([int]$resp.StatusCode -eq 200) { $ok++ }
        }
        $ok
    } -ThrottleLimit 4
    $written = ($parallel | Measure-Object -Sum).Sum
    $r = Sql $base $token "SELECT COUNT(*) FROM t" $sid
    if ($written -ne 80) { Fail "并发写应成功 80 行，实际 $written" }
    if ($r.Content -notmatch '\[\[83\]\]') { Fail "最终应为 83 行: $($r.Content)" }
    $checks++; Ok "4 会话并发写 80 行"

    # 5. 错误语义
    $r = Sql $base $token "SELECT * FROM t WHERE id = '1'" $sid
    if ([int]$r.StatusCode -ne 400 -or $r.Content -notmatch 'TYPE_ERROR') {
        Fail "跨类型比较应 400 TYPE_ERROR: $($r.Content)"
    }
    $checks++; Ok "跨类型比较 → 400 TYPE_ERROR"

    $r = Invoke-Api "GET" "$base/api/tables/t?limit=2&offset=1" $null $sid $token
    if ($r.Content -notmatch '"limit":2') { Fail "分页参数未生效: $($r.Content)" }
    $checks++; Ok "分页 limit/offset"

    # 6. 协议硬化
    $resp = Raw-Http $httpPort "POST /api/sql HTTP/1.1`r`nHost: t`r`nConnection: close`r`nTransfer-Encoding: chunked`r`n`r`n0`r`n`r`n"
    if ($resp -notmatch "^HTTP/1\.1 411") { Fail "chunked 应返回 411，实际: $resp" }
    $checks++; Ok "chunked → 411"

    $resp = Raw-Http $httpPort "GET /missing.js HTTP/1.1`r`nHost: t`r`nConnection: close`r`n`r`n"
    if ($resp -notmatch "^HTTP/1\.1 404") { Fail "缺失静态文件应 404，实际: $resp" }
    $checks++; Ok "缺失静态文件 → 404"

    $r = Invoke-Api "GET" "$base/" $null $null $null
    if ([int]$r.StatusCode -ne 200) { Fail "首页应 200，实际 $($r.StatusCode)" }
    $checks++; Ok "控制台首页 → 200"

    Write-Host ("`n冒烟通过：$checks 项检查全部 OK") -ForegroundColor Green
    exit 0
} catch {
    Write-Host ("`n冒烟失败：" + $_.Exception.Message) -ForegroundColor Red
    if (Test-Path (Join-Path $tempDir "server.err")) {
        Write-Host "--- server.err ---"
        Get-Content (Join-Path $tempDir "server.err")
    }
    exit 1
} finally {
    if ($proc -and -not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $proc.WaitForExit(5000) | Out-Null
    }
    Remove-Item Env:LDB_TOKEN -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
}
