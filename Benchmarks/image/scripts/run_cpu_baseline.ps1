# run_cpu_baseline.ps1 — gpui-editor 图片查看器「滚轮逐张切图」真机 CPU 探针
#
# 用法:
#   pwsh -File Benchmarks\image\scripts\run_cpu_baseline.ps1
#     [-Image <绝对路径>]     默认 compext: Benchmarks\corpus\PNG\03.png
#     [-Rate      <Hz>]       滚轮发射速率，默认 30（30 格/秒 ≈ 高速滚动）
#     [-Duration  <s>]        滚动采集时长，默认 12
#     [-Cooldown  <s>]        停滚后观察掉空（解码队列 drain）时长，默认 6
#     [-Cores]                逻辑核数，默认 [Environment]::ProcessorCount
#
# 输出: 进程 CPU% = 采样区间 (ΔtotalCPU_s / Δwall_s) / Cores × 100，口径同任务管理器。
#       每行 [IMG-CPU] 前缀，末尾一行 machine-readable 摘要。
#
# 注: 这是「程序发送滚轮 + 采样」的性能探针（用户已授权真机测量）；
#     只用于性能测量/迭代对表，不做 GUI 正确性代验。

param(
    [string]$Image = "",
    [double]$Rate = 30,
    [double]$Duration = 12,
    [double]$Cooldown = 6,
    [int]$Cores = 0,
    [string]$Exe = ""
)

$ErrorActionPreference = "Stop"

if (-not $Image) {
    $Image = Join-Path $PSScriptRoot "..\..\corpus\PNG\03.png"
}
$Image = (Resolve-Path -LiteralPath $Image).Path
if (-not (Test-Path -LiteralPath $Image)) { throw "图片不存在: $Image" }
if ($Cores -le 0) { $Cores = [Environment]::ProcessorCount }

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Win32 {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    public struct RECT { public int Left, Top, Right, Bottom; }
    public const uint MOUSEEVENTF_WHEEL = 0x0800;
}
"@

if (-not $Exe) {
    $Exe = "E:\Rust\gpui-editor\target\debug\gpui-editor.exe"
}
if (-not (Test-Path -LiteralPath $Exe)) { throw "exe 不存在（先 cargo build --bin gpui-editor）: $Exe" }

$p = Start-Process -FilePath $exe -ArgumentList @("`"$Image`"") -PassThru
try {
    # 等主窗口出现
    $hwnd = [IntPtr]::Zero
    for ($i = 0; $i -lt 100 -and $hwnd -eq [IntPtr]::Zero; $i++) {
        Start-Sleep -Milliseconds 100
        $p.Refresh()
        $hwnd = $p.MainWindowHandle
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw "未找到主窗口" }
    [Win32]::SetForegroundWindow($hwnd) | Out-Null
    [Win32]::ShowWindow($hwnd, 9) | Out-Null   # SW_RESTORE
    Start-Sleep -Milliseconds 300

    # 取窗口中心，光标移过去（wheel 发给光标下窗口）
    $rect = New-Object Win32+RECT
    [Win32]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
    $cx = [int](($rect.Left + $rect.Right) / 2)
    $cy = [int](($rect.Top + $rect.Bottom) / 2)
    [Win32]::SetCursorPos($cx, $cy) | Out-Null

    # 预热：首图解码 + 级联预取铺开（不采）
    Start-Sleep -Seconds 4

    # 采样放独立 job（发射循环不被采样 sleep 拖慢），目标进程 CPU 写入临时文件
    $tmp = Join-Path $env:TEMP ("imgcpu_{0}_{1}.csv" -f $p.Id, [guid]::NewGuid().ToString("N").Substring(0, 8))
    $sampleJob = Start-Job -ArgumentList $p.Id, $tmp, $Cores -ScriptBlock {
        param($procId, $out, $cores)
        $rows = @()
        $p1 = Get-Process -Id $procId -ErrorAction SilentlyContinue
        if (-not $p1) { return }
        $c1 = $p1.CPU; $w1 = [DateTime]::UtcNow
        while ($true) {
            Start-Sleep -Milliseconds 400
            if (-not (Get-Process -Id $procId -ErrorAction SilentlyContinue)) { break }
            $p2 = Get-Process -Id $procId -ErrorAction SilentlyContinue
            if (-not $p2) { break }
            $wall = ([DateTime]::UtcNow - $w1).TotalSeconds
            $dcpu = $p2.CPU - $c1
            $pct = if ($wall -gt 0) { [math]::Max(0.0, $dcpu / $wall / $cores * 100.0) } else { 0 }
            [System.IO.File]::AppendAllText($out, ("{0:0.0}`n" -f $pct))
            $c1 = $p2.CPU; $w1 = [DateTime]::UtcNow
        }
    }

    # —— 滚动阶段：连发 wheel（采样在 job 里并行）——
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $notches = 0
    while ($sw.Elapsed.TotalSeconds -lt $Duration) {
        [Win32]::mouse_event([Win32]::MOUSEEVENTF_WHEEL, 0, 0, 120, [UIntPtr]::Zero)
        $notches++
        Start-Sleep -Milliseconds (1000.0 / $Rate)
    }
    $scrolledSec = $sw.Elapsed.TotalSeconds

    # —— 冷却阶段：停滚后观察解码队列 drain（继续采样）——
    $sw2 = [System.Diagnostics.Stopwatch]::StartNew()
    $drainedAt = -1.0
    while ($sw2.Elapsed.TotalSeconds -lt $Cooldown) {
        $c = (Get-Process -Id $p.Id -ErrorAction SilentlyContinue).CPU
        Start-Sleep -Milliseconds 300
        if ($drainedAt -lt 0) {
            $pK = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
            $pA = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
            if ($pK -and $pA) {
                $cNow = $pA.CPU
                $wNow = [DateTime]::UtcNow
                $wallD = 1.0
                $delta = $cNow - $c
                if ($delta / $wallD / $Cores * 100.0 -lt 10.0) { $drainedAt = $sw2.Elapsed.TotalSeconds }
                $c = $cNow
            }
        }
    }
    $coolend = $sw2.Elapsed.TotalSeconds
    Start-Sleep -Milliseconds 300
    Stop-Job $sampleJob -ErrorAction SilentlyContinue
    Wait-Job $sampleJob -ErrorAction SilentlyContinue | Out-Null
    $rows = @()
    if (Test-Path -LiteralPath $tmp) { $rows = [System.IO.File]::ReadAllLines($tmp) | ForEach-Object { [double]$_ } }
    Remove-Item -LiteralPath $tmp -ErrorAction SilentlyContinue
    $samples = $rows | Where-Object { $_ -gt 0.0 }

    $avg = if ($samples.Count) { ($samples | Measure-Object -Average).Average } else { 0 }
    $max = if ($samples.Count) { ($samples | Measure-Object -Maximum).Maximum } else { 0 }

    Write-Output "[IMG-CPU] ${Image} rate=$([math]::Round(($notches / $sw.Elapsed.TotalSeconds),0))hz notches=$notches cores=$Cores"
    foreach ($s in $samples) { Write-Output "[IMG-CPU]   scroll cpu%=$([math]::Round($s,1))" }
    Write-Output "[IMG-CPU]   SUMMARY avg=$([math]::Round($avg,1)) max=$([math]::Round($max,1)) drainedAt=$([math]::Round($drainedAt,2))s fullCores=$([math]::Round($avg/100*$Cores,2))"
}
finally {
    if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
}
