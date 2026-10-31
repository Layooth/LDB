# LittleDB 崩溃恢复验证：写入过程中强杀进程，重启后校验「已 ACK 的提交不丢」
#
# 用法：
#   pwsh -File tools\crash-test.ps1                  # 默认 3 轮，200ms 后强杀
#   pwsh -File tools\crash-test.ps1 -Rounds 5 -KillAfterMs 500
#
# 判定标准（full 同步模式）：HTTP 200 = 已 fsync。
# 因此「任何返回 200 的插入，重启后必须还在」；正在飞的那条（无响应）允许丢。
#
# 退出码：0 = 全部轮次通过；1 = 有数据丢失或服务起不来

[CmdletBinding()]
param(
    [int]$Rounds = 3,
    [int]$KillAfterMs = 200,
    [string]$Exe
)

$ErrorActionPreference = "Stop"
$src = Split-Path -Parent $PSScriptRoot
$repo = Split-Path -Parent $src
if (-not $Exe) { $Exe = Join-Path $repo "littledb.exe" }
if (-not (Test-Path $Exe)) { throw "找不到 $Exe，请先构建" }

function Step($text) { Write-Host "`n== $text ==" -ForegroundColor Cyan }
function Ok($text) { Write-Host "  [OK] $text" -ForegroundColor Green }

function Get-FreePort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    return $port
}

function Wait-Port($port, $seconds = 15) {
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

function Start-Node($dir, $httpPort, $protoPort, $token) {
    return Start-Process -PassThru -FilePath $Exe -ArgumentList @(
        "serve", "`"$dir`"", "--port", "$protoPort", "--http-port", "$httpPort",
        "--no-open", "--token", $token
    ) -RedirectStandardOutput (Join-Path $dir "server.log") `
      -RedirectStandardError (Join-Path $dir "server.err")
}

function Sql($base, $token, $sql, $sid) {
    $headers = @{ Authorization = "Bearer $token" }
    if ($sid) { $headers["X-Session-Id"] = $sid }
    $body = @{ sql = $sql } | ConvertTo-Json -Compress
    Invoke-WebRequest -Uri "$base/api/sql" -Method Post -Headers $headers -Body $body `
        -ContentType "application/json" -UseBasicParsing -SkipHttpErrorCheck -TimeoutSec 20
}

$tempDir = Join-Path $env:TEMP ("ldb-crash-" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $tempDir | Out-Null
$ackLog = Join-Path $tempDir "acked.txt"
New-Item -ItemType File -Path $ackLog | Out-Null
$httpPort = Get-FreePort
$protoPort = Get-FreePort
$token = "crash-" + [Guid]::NewGuid().ToString("N").Substring(0, 8)
$base = "http://127.0.0.1:$httpPort"
$proc = $null
$job = $null
$totalAcked = 0

try {
    # ---- 第 0 轮：建表（把 catalog 落到磁盘，后续崩溃只考验数据页与 WAL）
    Step "初始化数据目录"
    $proc = Start-Node $tempDir $httpPort $protoPort $token
    if (-not (Wait-Port $httpPort)) { throw "服务未启动" }
    $r = Sql $base $token "CREATE TABLE crash (id INT PRIMARY KEY, name TEXT)"
    if ([int]$r.StatusCode -ne 200) { throw "建表失败: $($r.Content)" }
    Ok "表 crash 已创建"

    for ($round = 1; $round -le $Rounds; $round++) {
        Step "第 $round/$Rounds 轮：写入 ${KillAfterMs}ms 后强杀"

        $idBase = $round * 1000000
        $job = Start-Job -ScriptBlock {
            param($port, $token, $idBase, $log)
            $headers = @{ Authorization = "Bearer $token" }
            for ($i = 0; $i -lt 100000; $i++) {
                $id = $idBase + $i
                $body = @{ sql = "INSERT INTO crash VALUES ($id, 'c$id')" } | ConvertTo-Json -Compress
                try {
                    $resp = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sql" -Method Post `
                        -Headers $headers -Body $body -ContentType "application/json" `
                        -UseBasicParsing -SkipHttpErrorCheck -TimeoutSec 10
                    if ([int]$resp.StatusCode -eq 200) {
                        Add-Content -Path $log -Value $id
                    } else {
                        break
                    }
                } catch {
                    break   # 服务被杀：在飞的那条不算 ACK
                }
            }
        } -ArgumentList $httpPort, $token, $idBase, $ackLog

        # 先确认写入真的开始了（否则「杀得太早、一行没写」会变成假通过）
        $ready = $false
        $deadline = (Get-Date).AddSeconds(15)
        while ((Get-Date) -lt $deadline) {
            # 必须等到「本轮」的 ACK，而不是只看文件非空（上一轮的行会误判为已就绪）
            $fresh = @(
                Get-Content $ackLog -ErrorAction SilentlyContinue |
                Where-Object { $_ -and ([int]$_ -ge $idBase) }
            )
            if ($fresh.Count -gt 0) { $ready = $true; break }
            Start-Sleep -Milliseconds 50
        }
        if (-not $ready) { throw "写入线程在 15 秒内没有产生任何 ACK，脚本判定无效" }

        Start-Sleep -Milliseconds $KillAfterMs
        if (-not $proc.HasExited) {
            Stop-Process -Id $proc.Id -Force
            $proc.WaitForExit(5000) | Out-Null
        }
        Stop-Job $job -ErrorAction SilentlyContinue
        Remove-Job $job -Force -ErrorAction SilentlyContinue
        $job = $null
        $proc = $null

        $roundAcked = @(
            Get-Content $ackLog -ErrorAction SilentlyContinue |
            Where-Object { $_ -and ([int]$_ -ge $idBase) }
        )
        $ackedSoFar = @(Get-Content $ackLog -ErrorAction SilentlyContinue | Where-Object { $_ })
        Write-Host "  本轮已 ACK $($roundAcked.Count) 行（累计 $($ackedSoFar.Count)）"
        if ($roundAcked.Count -eq 0) { throw "第 $round 轮没有产生任何 ACK" }

        # ---- 重启校验
        $proc = Start-Node $tempDir $httpPort $protoPort $token
        if (-not (Wait-Port $httpPort)) { throw "第 $round 轮重启失败（见 server.err）" }

        $r = Sql $base $token "SELECT COUNT(*) FROM crash WHERE id >= $idBase"
        if ([int]$r.StatusCode -ne 200) { throw "重启后查询失败: $($r.Content)" }
        $inRound = [int]([regex]::Match($r.Content, '\[\[(\d+)\]\]').Groups[1].Value)

        # 下界：已 ACK 的绝不允许丢
        if ($inRound -lt $roundAcked.Count) {
            throw "数据丢失：本轮已 ACK $($roundAcked.Count) 行，重启后只剩 $inRound 行"
        }
        # 上界：允许「已提交但响应没送到」的少量在飞行（不算丢数据，但要防异常膨胀）
        if ($inRound -gt $roundAcked.Count + 64) {
            throw "数据异常膨胀：已 ACK $($roundAcked.Count) 行，重启后 $inRound 行"
        }

        # 逐条抽查首尾（计数对上但内容错位也能抓到）
        foreach ($probe in @($roundAcked[0], $roundAcked[-1])) {
            $r = Sql $base $token "SELECT name FROM crash WHERE id = $probe"
            if ($r.Content -notmatch ('c' + $probe)) {
                throw "ACK 的 id=$probe 在重启后不存在: $($r.Content)"
            }
        }
        $totalAcked = $ackedSoFar.Count
        Ok "重启后本轮 $inRound 行（ACK $($roundAcked.Count) 行零丢失，首尾抽查通过）"
    }

    Write-Host "`n崩溃恢复验证通过：$Rounds 轮强杀，累计 $totalAcked 行已 ACK 数据零丢失" -ForegroundColor Green
    exit 0
} catch {
    Write-Host ("`n崩溃恢复验证失败：" + $_.Exception.Message) -ForegroundColor Red
    if (Test-Path (Join-Path $tempDir "server.err")) {
        Write-Host "--- server.err ---"
        Get-Content (Join-Path $tempDir "server.err")
    }
    exit 1
} finally {
    if ($job) { Stop-Job $job -ErrorAction SilentlyContinue; Remove-Job $job -Force -ErrorAction SilentlyContinue }
    if ($proc -and -not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $proc.WaitForExit(5000) | Out-Null
    }
    Get-Process littledb -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $Exe } |
        ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
}
