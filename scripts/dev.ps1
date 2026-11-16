# scripts/dev.ps1
# 模式纪律 runner：双 target-dir 隔离「改码→run」与「改测试→test」两循环（spec §7）
param(
  [Parameter(Mandatory=$true, Position=0)][ValidateSet('run','test','check')][string]$Mode,
  [Parameter(ValueFromRemainingArguments=$true)][string[]]$CargoArgs
)

$targetTestDir = Join-Path $PSScriptRoot "..\target\testmode"

# 把参数切成「cargo 参数」与「-- 后的测试参数」两段；已含 `--` 不再追加（防重复）
$cargoArgsOnly = @()
$testArgs = @()
$afterSep = $false
foreach ($a in $CargoArgs) {
  if (-not $afterSep -and $a -eq '--') { $afterSep = $true; continue }
  if ($afterSep) { $testArgs += $a } else { $cargoArgsOnly += $a }
}

switch ($Mode) {
  # run 用默认 target\debug（exe 路径不变）：显式清掉 test/check 可能残留的
  # $env:CARGO_TARGET_DIR，防同一 shell 会话切 run 后 build 误入 target\testmode。
  'run'   {
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    & cargo build --bin vuick @cargoArgsOnly
  }
  'test'  {
    $env:CARGO_TARGET_DIR = $targetTestDir
    $needThreads = $true
    foreach ($t in $testArgs) { if ($t -like '--test-threads*') { $needThreads = $false; break } }
    if ($needThreads) { $extra = @('--', '--test-threads=1') } else { $extra = @('--') }
    & cargo test -p shell @cargoArgsOnly @extra @testArgs
  }
  'check' {
    $env:CARGO_TARGET_DIR = $targetTestDir
    & cargo check -p shell --tests @cargoArgsOnly
  }
}
# 提醒语去数字、写真实机理：同 target-dir 下 lib 的 test/non-test cfg 产物切换才是重编源，
# dev.ps1 用独立 target\testmode 物理隔离两套产物，正常走本脚本永不互撞。
Write-Host "dev.ps1 提醒：裸 cargo build 与 cargo test 同目录会因 lib 的 test/non-test cfg 产物切换反复重编——请用本脚本（test 走独立 target\testmode 隔离）。"
exit $LASTEXITCODE
