# run_single_thread.ps1 — WebP 单线程对账：kimg(OURS) vs libwebp DLL vs C# 表。
# KIMG_WEBP_THREADS=0 强制单线程（kimg vp8 滤波/动画帧并行门控识别 0=串行）。
# 全程前台阻塞到结束（K86：禁止后台化）；测前静置 60s（散热/变频稳定）。
$ErrorActionPreference = 'Continue'   # 保持默认，避免原生命令 stderr 被 2>&1 抬成异常
$SuiteRoot = Split-Path -Parent $PSScriptRoot

$ts = Get-Date -Format 'yyyyMMdd_HHmmss'
$log = Join-Path $SuiteRoot "logs\dll_single_$ts.log"
$cmpLog = Join-Path $SuiteRoot "logs\compare_single_$ts.log"
foreach ($f in @($log, $cmpLog)) { New-Item -ItemType Directory -Force -Path (Split-Path -Parent $f) | Out-Null }

$env:KIMG_WEBP_THREADS = '0'

Write-Output "[run-single] 静置 60s（散热/变频稳定）…"
Start-Sleep -Seconds 60

Push-Location $SuiteRoot
try {
    Write-Output "[run-single] 探针（KIMG_WEBP_THREADS=0）：cargo run --release …（首跑全量编译 kimg，几分钟）"
    # 2>&1 把原生 stderr 变成 ErrorRecord，直喂 Tee/控制台会渲成 NativeCommandError 噪音 →
    # 每项 toString（"$_"）摊平后再进管道（5.1/7 通用）。
    & cargo run --release --manifest-path (Join-Path $SuiteRoot 'webp_compare\Cargo.toml') 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log
    # 主机 Tee-Object（尤其 Windows PowerShell 5.1）会把日志写 UTF-16 → 统重写为 UTF-8（无 BOM），
    # 否则 compare_refs.py 按 UTF-8 读不到 perfile 行（= no matched files）。
    [System.IO.File]::WriteAllText($log, [System.IO.File]::ReadAllText($log), [System.Text.UTF8Encoding]::new($false))
    $code = if ($null -ne $PIPESTATUS -and $PIPESTATUS.Count -gt 0) { $PIPESTATUS[0] } else { $LASTEXITCODE }
    if ($code -ne 0) { Write-Error "探针退出码 $code，详见 $log"; exit $code }

    Write-Output "[run-single] 对账：python compare_refs.py …"
    & python (Join-Path $SuiteRoot 'compare_refs.py') $log (Join-Path $SuiteRoot 'corpus\csharp\csharp_table.log') *> $cmpLog
    $code = $LASTEXITCODE
    if ($code -ne 0) { Write-Error "compare_refs 退出码 $code，详见 $cmpLog"; exit $code }
    Get-Content -LiteralPath $cmpLog
}
finally {
    Pop-Location
}

# 结论行（供一眼看）：从 static-lossy 块抓 geomean kimg/C#
$m = Select-String -LiteralPath $cmpLog -Pattern 'geomean kimg/C# = ([\d.]+)' | Select-Object -First 1
if ($m) {
    $val = $m.Matches[0].Groups[1].Value
    Write-Output "kimg÷C# geomean=$val (single-thread static-lossy；log=$log)"
} else {
    Write-Warning "compare_refs 输出里没找到 geomean kimg/C# 行；对账结果见上（$cmpLog）。"
}
