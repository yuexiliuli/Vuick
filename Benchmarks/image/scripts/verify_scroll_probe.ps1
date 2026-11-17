# verify_scroll_probe.ps1 — 确定性滚轮投递 + 截图自证 + CPU 采样（图片查看器真机测量主干）
# PostMessage(WM_MOUSEWHEEL) 直接投递到主窗口，绕开光标/焦点抖动；截图 diff 自证图片在切换。

param(
    [string]$Image = "",
    [double]$Rate = 30,
    [double]$Duration = 12,
    [int]$Cores = 0
)
$ErrorActionPreference = "Stop"
if (-not $Image) { $Image = Join-Path $PSScriptRoot "..\..\corpus\PNG\03.png" }
$Image = (Resolve-Path -LiteralPath $Image).Path
if ($Cores -le 0) { $Cores = [Environment]::ProcessorCount }

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class WP {
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out R r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    public struct R { public int L,T,Ri,B; }
    public struct POINT { public int X,Y; }
    public const uint WM_MOUSEWHEEL = 0x020A;
    public const uint WM_KEYDOWN = 0x0100;
}
"@

$exe = "E:\Rust\vuick\target\debug\vuick.exe"
$p = Start-Process -FilePath $exe -ArgumentList @("`"$Image`"") -PassThru
try {
    $hwnd = [IntPtr]::Zero
    for ($i = 0; $i -lt 150 -and $hwnd -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 100; $p.Refresh(); $hwnd = $p.MainWindowHandle }
    if ($hwnd -eq [IntPtr]::Zero) { throw "未找到主窗口" }
    [WP]::SetForegroundWindow($hwnd) | Out-Null
    Start-Sleep -Seconds 1

    # 窗口客户区中心（screen 坐标）作为 wheel 命中点
    $cr = New-Object WP+R; [WP]::GetClientRect($hwnd, [ref]$cr) | Out-Null
    $pt = New-Object WP+POINT; $pt.X = [int](($cr.Ri - $cr.L) / 2); $pt.Y = [int](($cr.B - $cr.T) / 2)
    [WP]::ClientToScreen($hwnd, [ref]$pt) | Out-Null
    $wparam = [IntPtr](120 -shl 16)
    $lparam = [IntPtr](($pt.X -band 0xFFFF) -bor ( ($pt.Y -band 0xFFFF) -shl 16 ))

    function Shot([IntPtr]$h) {
        $wr = New-Object WP+R; [WP]::GetWindowRect($h, [ref]$wr) | Out-Null
        $w = $wr.Ri - $wr.L; $ht = $wr.B - $wr.T
        if ($w -le 0 -or $ht -le 0) { return 0 }
        $bmp = New-Object System.Drawing.Bitmap $w, $ht
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($wr.L, $wr.T, 0, 0, (New-Object System.Drawing.Size $w, $ht))
        $g.Dispose()
        $h1 = 0
        $fmt = [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
        $data = $bmp.LockBits((New-Object System.Drawing.Rectangle 0,0,$w,$ht), [System.Drawing.Imaging.ImageLockMode]::ReadOnly, $fmt)
        $bytes = New-Object byte[] $data.Stride
        for ($y = 0; $y -lt $ht; $y += [math]::Max(1, [int]($ht/40))) {
            [Runtime.InteropServices.Marshal]::Copy([IntPtr]::Add($data.Scan0, $y * $data.Stride), $bytes, 0, $data.Stride)
            foreach ($x in 0..([math]::Max(0, [int]($w/40)) * 4 - 1)) { $h1 = ($h1 * 31 + $bytes[$x]) -band 0x7FFFFFFF }
        }
        $bmp.UnlockBits($data); $bmp.Dispose()
        return $h1
    }

    # 预热（首图解码），再截一张
    Start-Sleep -Seconds 4
    $hBefore = Shot $hwnd
    "PRE  shotHash=$hBefore"

    # 滚动阶段：PostMessage + 内联 CPU 采样（采样窗口内不滚）
    $samples = @()
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $n = 0; $c0 = (Get-Process -Id $p.Id).CPU; $w0 = [DateTime]::UtcNow
    while ($sw.Elapsed.TotalSeconds -lt $Duration) {
        [WP]::PostMessage($hwnd, [WP]::WM_MOUSEWHEEL, $wparam, $lparam) | Out-Null
        $n++
        Start-Sleep -Milliseconds (1000.0 / $Rate)
        if ($n -gt 0 -and $n % 60 -eq 0) {
            Start-Sleep -Milliseconds 200
            $c1 = (Get-Process -Id $p.Id).CPU; $wall = ([DateTime]::UtcNow - $w0).TotalSeconds
            $samples += [math]::Round((($c1 - $c0) / $wall / $Cores) * 100, 1)
            $c0 = $c1; $w0 = [DateTime]::UtcNow
        }
    }
    Start-Sleep -Milliseconds 400
    $hAfter = Shot $hwnd
    "POST shotHash=$hAfter  ($($hBefore -eq $hAfter ? 'SAME=没切图!' : 'DIFF=在切图'))"

    $avg = if ($samples.Count) { ($samples | Measure-Object -Average).Average } else { 0 }
    $mx  = if ($samples.Count) { ($samples | Measure-Object -Maximum).Maximum } else { 0 }
    "[IMG-CPU] SUMMARY wheels=$n  avg=$([math]::Round($avg,1))  max=$([math]::Round($mx,1))  cores=$Cores  samples=$($samples -join ',')"
}
finally { if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
