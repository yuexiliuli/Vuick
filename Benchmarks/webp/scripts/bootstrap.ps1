# bootstrap.ps1 — WebP 复现套件装配（幂等，可重复运行；多机可用，源码路径不写死）。
# 源全部可经「名参数 或 环境变量」指定；未给/不存在只警告跳过（K64 语义：快照进 corpus 才稳定，
# 不依赖会被重排/清理的外部目录）。
#   -corpusDirs   外部 WEBP 语料目录（可 ; 分隔多个）  env: KIMG_CORPUS_EXT
#   -dll          libwebp FULL DLL 路径                env: LIBWEBP_DLL
#   -csharpTable  C# 2 列 current 表路径               env: KIMG_CSHARP_TABLE
# 仓内语料默认 = 本仓库 target/imgbench-corpus/WEBP（相对推导，不写死盘符）；可经 env KIMG_CORPUS_INTERNAL 覆盖。
param(
    [string]$CorpusDirs  = $env:KIMG_CORPUS_EXT,
    [string]$Dll         = $env:LIBWEBP_DLL,
    [string]$CSharpTable = $env:KIMG_CSHARP_TABLE
)
$ErrorActionPreference = 'Stop'

$SuiteRoot  = Split-Path -Parent $PSScriptRoot                   # ...\Benchmarks\webp
$Benchmarks = Split-Path -Parent $SuiteRoot                      # ...\Benchmarks（共享语料/ C# 表所在）
$RepoRoot   = Split-Path -Parent $Benchmarks                     # ...\gpui-editor（本仓库，相对推导）

# 仓内语料（相对本仓库推导；可在别机用 KIMG_CORPUS_INTERNAL 覆盖）
$InclCorpus = if ($env:KIMG_CORPUS_INTERNAL) { $env:KIMG_CORPUS_INTERNAL } else { Join-Path $RepoRoot 'target\imgbench-corpus\WEBP' }

$webpDst   = Join-Path $Benchmarks 'corpus\webp'
$csharpDst = Join-Path $Benchmarks 'corpus\csharp'
$dllDst    = Join-Path $SuiteRoot 'bin'
$logsDst   = Join-Path $SuiteRoot 'logs'

foreach ($d in @($webpDst, $csharpDst, $dllDst, $logsDst)) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}

Write-Output '[bootstrap] 复现套件根:'
Write-Output "  suite_root = $SuiteRoot"

# --- 语料快照（按文件名去重）---
$seen = @{}
if (Test-Path -LiteralPath $InclCorpus) {
    Get-ChildItem -LiteralPath $InclCorpus -Filter *.webp -File | ForEach-Object {
        $seen[$_.Name] = $true
        Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $webpDst $_.Name) -Force
    }
    Write-Output "  corpus 源(仓内): $InclCorpus"
} else {
    Write-Warning "仓内语料源缺失，跳过（继续）：$InclCorpus"
}
foreach ($src in @($CorpusDirs -split ';' | ForEach-Object { $_.Trim() })) {
    if (-not $src) { continue }
    if (-not (Test-Path -LiteralPath $src)) {
        Write-Warning "外部语料源缺失，跳过（继续）：$src"
        continue
    }
    $n = 0
    Get-ChildItem -LiteralPath $src -Filter *.webp -File | ForEach-Object {
        $seen[$_.Name] = $true
        Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $webpDst $_.Name) -Force
        $n++
    }
    Write-Output "  corpus 源(外部): $src -> 拷贝 $n 份"
}
$webpCount = $seen.Count
$onDisk    = (Get-ChildItem -LiteralPath $webpDst -Filter *.webp -File).Count

# --- libwebp DLL（仅参数/env；缺则跳过 — 探针可用 LIBWEBP_DLL 现场覆盖）---
if ($Dll) {
    if (Test-Path -LiteralPath $Dll) {
        Copy-Item -LiteralPath $Dll -Destination (Join-Path $dllDst 'libwebp.dll') -Force
    } else {
        Write-Warning "libwebp DLL 源不存在，跳过：$Dll"
    }
} elseif (-not (Test-Path -LiteralPath (Join-Path $dllDst 'libwebp.dll'))) {
    Write-Warning "未提供 libwebp DLL 源（-dll 或 env LIBWEBP_DLL）且本地无既有；libwebp 参照列不可用"
}
$dllOk = Test-Path -LiteralPath (Join-Path $dllDst 'libwebp.dll')

# --- C# 表（仅参数/env；缺则本机用 C# quick 再生成，见 README）---
$csDstFile = Join-Path $csharpDst 'csharp_table.log'
if ($CSharpTable) {
    if (Test-Path -LiteralPath $CSharpTable) {
        Copy-Item -LiteralPath $CSharpTable -Destination $csDstFile -Force
    } else {
        Write-Warning "C# 表源不存在，跳过：$CSharpTable"
    }
} elseif (-not (Test-Path -LiteralPath $csDstFile)) {
    Write-Warning "未提供 C# 表源（-csharpTable 或 env KIMG_CSHARP_TABLE）且本地无既有表；kimg/C# 对账需先有 2 列 current 表（见 README『C# 表再生成』）"
}
# C# quick 原始 stdout 是 6 列（| name | libwebp | baseline | current | ÷b | ÷l |），compare_refs 只吃
# 2 列 `| name | ms |` 且要 current 列。检测 6 列并就地转 2 列 current（幂等：已是 2 列不动、[check] 行跳过）。
if (Test-Path -LiteralPath $csDstFile) {
    $firstRow = @(Get-Content -LiteralPath $csDstFile -TotalCount 200 | Where-Object { $_ -match '^\s*\|\s*`[^`]+`\s*\|\s*[\d.]+\s*\|\s*\S+\s*\|\s*[\d.]+' } | Select-Object -First 1)
    if ($firstRow.Count -gt 0) {
        $rows = New-Object System.Collections.Generic.List[string]
        foreach ($l in Get-Content -LiteralPath $csDstFile) {
            $m = [regex]::Match($l, '^\s*\|\s*`([^`]+)`\s*\|\s*[\d.]+\s*\|\s*\S+\s*\|\s*([\d.]+)')
            if ($m.Success -and $m.Groups[1].Value -ne 'Input') {
                $rows.Add("| ``$($m.Groups[1].Value)`` | $($m.Groups[2].Value) |")
            }
        }
        if ($rows.Count -gt 0) {
            [System.IO.File]::WriteAllLines($csDstFile, $rows, [System.Text.UTF8Encoding]::new($false))
            Write-Output "  csharp_table.log 已由 6 列原始转 2 列 current（$($rows.Count) 行）"
        }
    }
}
$tableLines = if (Test-Path -LiteralPath $csDstFile) { (Get-Content -LiteralPath $csDstFile).Count } else { 0 }

Write-Output '[bootstrap] 完成：'
Write-Output "  webp = $webpCount 个唯一文件名（仓内 20 + 外部可扩充；落盘 $onDisk）"
Write-Output "  libwebp.dll 存在 = $dllOk"
Write-Output "  csharp_table.log 行数 = $tableLines（>=75 为完整）"
