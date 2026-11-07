#!/usr/bin/env python3
"""HEIF acceptance 对账：解析 perf_acceptance_geo 的 GEO/GEO-SUM 行 + C# quick 参照，输出合并表与 geomean。

用法: python compare_refs.py <acceptance.log>
acceptance.log 由 scripts\\run_acceptance.ps1 生成（含并行 + KIMG_HEIF_PARALLEL=0 两段，各自 GEO-SUM）。
C# 参照优先读 Benchmarks\\corpus\\csharp\\heif_csharp_quick.log（再生成版），缺失回退探针内嵌值。
"""
import re
import sys
from pathlib import Path

def main():
    if len(sys.argv) < 2:
        print(__doc__); return 1
    log = Path(sys.argv[1]).read_text(encoding='utf-8', errors='replace')
    suite = Path(__file__).resolve().parent
    csharp_path = suite.parent / 'corpus' / 'csharp' / 'heif_csharp_quick.log'

    # C# 参照表（文件→RGBA_ms）
    cs = {}
    if csharp_path.exists():
        for line in csharp_path.read_text(encoding='utf-8', errors='replace').splitlines():
            if not line or line.startswith('#') or line.startswith('file'):
                continue
            parts = line.split()
            if len(parts) >= 3:
                try: cs[parts[0]] = float(parts[2])
                except ValueError: pass

    # Rust 探针行
    rows = []   # (mode, file, tiles, rust_ms, cs_ms, ratio, cls)
    sums = []   # (mode, label, n, geo)
    mode = None
    for line in log.splitlines():
        if 'parallel acceptance' in line.lower():
            mode = 'parallel'
        elif 'single-core' in line.lower():
            mode = 'single'
        m = re.match(r'GEO\s+(\S+): tiles=(\d+) rust=([\d.]+) ms cs=([\d.]+) ms ratio=([\d.]+)(.*)', line)
        if m and mode:
            rows.append((mode, m.group(1), int(m.group(2)), float(m.group(3)),
                         float(m.group(4)), float(m.group(5)), m.group(6).strip()))
            continue
        m = re.match(r'GEO-SUM\s+(\d+)\s*([^:]+):\s*n=\d+\s*geomean\(ratio\)=([\d.]+)', line)
        if m and mode:
            sums.append((mode, m.group(1) + m.group(2).strip(), float(m.group(3))))

    if not rows and not sums:
        print('未解析到 GEO 行 — 检查 log 是否含 perf_acceptance_geo 输出与模式标记'); return 2

    # 合并表
    print(f'# HEIF acceptance 对账（rust 探针内嵌 C# 值 vs {csharp_path.name} 参照）')
    print('| mode | file | tiles | rust_ms | cs_ms(内嵌) | cs_ms(参照) | ratio | cls |')
    print('|---|---|---|---|---|---|---|---|')
    for mode, f, t, r, c, ratio, cls in rows:
        cref = cs.get(f)
        csrc = f'{cref:.1f}' if cref is not None else '-'
        print(f'| {mode} | {f} | {t} | {r:.1f} | {c:.1f} | {csrc} | {ratio:.3f} | {cls} |')

    print('\n# Geomean')
    for mode, label, g in sorted(sums):
        mark = 'PASS<1.0' if g < 1.0 else 'FAIL>=1'
        print(f'| {mode:<8} | {label:<22} | {g:.3f} | {mark} |')
    if not sums:
        print('（无 GEO-SUM）')
    return 0

if __name__ == '__main__':
    sys.exit(main())
