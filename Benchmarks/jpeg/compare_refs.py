#!/usr/bin/env python3
"""JPEG 三路对账：解析探针 `KIMG_JPEG_VS_TURBO perfile` 行 × C# 参照表（`|`name`| <ms> |`），
输出三列 geomean、Sigma 与 worst 前 5。

用法: python compare_refs.py <acceptance.log> <jpeg_csharp.log>
- <acceptance.log>  由 scripts\\run_acceptance.ps1 生成（含探针 perfile 行）
- <jpeg_csharp.log> 由 scripts\\run_csharp_ref.ps1 生成（C# 单核参照表）
同名缺失文件（探针有而 C# 无）不计入 matched/geomean。结尾打印主口径一行供一眼看。
"""
import math
import re
import sys
from pathlib import Path

PERFILE = re.compile(
    r'KIMG_JPEG_VS_TURBO perfile\s+(\S+)\s+(\d+)x(\d+)\s+gray=([01])\s+'
    r'ours=([\d.]+)\s+turbo=([\d.]+)\s+ratio_ours_turbo=([\d.]+)'
)
CS_ROW = re.compile(r'^\s*\|\s*`([^`]+)`\s*\|\s*([\d.]+)\s*\|')


def geomean(xs):
    if not xs:
        return 0.0
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def sigma(xs):
    if len(xs) < 2:
        return 0.0
    m = sum(xs) / len(xs)
    return math.sqrt(sum((x - m) ** 2 for x in xs) / len(xs))


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 1
    acc_path = Path(sys.argv[1])
    cs_path = Path(sys.argv[2])
    acc = acc_path.read_text(encoding='utf-8', errors='replace')

    cs = {}
    if cs_path.exists():
        for line in cs_path.read_text(encoding='utf-8', errors='replace').splitlines():
            m = CS_ROW.match(line)
            if m:
                try:
                    cs[m.group(1)] = float(m.group(2))
                except ValueError:
                    pass

    rows = {}
    for line in acc.splitlines():
        m = PERFILE.match(line)
        if m:
            rows[m.group(1)] = {
                'w': int(m.group(2)),
                'h': int(m.group(3)),
                'gray': m.group(4) == '1',
                'ours': float(m.group(5)),
                'turbo': float(m.group(6)),
            }
    if not rows:
        print('未解析到 KIMG_JPEG_VS_TURBO perfile 行', file=sys.stderr)
        return 2

    # 三路合并：只计探针与 C# 都有的文件（matched 口径）
    tri = []
    dropped = []
    for name, r in rows.items():
        c = cs.get(name)
        if c is None:
            dropped.append(name)
            continue
        tri.append((name, r['ours'], r['turbo'], c))
    matched = len(tri)

    ot = [o / t for (_, o, t, _) in tri]
    oc = [o / c for (_, o, _, c) in tri]
    ct = [c / t for (_, _, t, c) in tri]

    print(f'== jpeg: matched={matched}（探针 {len(rows)} 文件，C# {len(cs)} 文件）')
    if dropped:
        print(f'   dropped（探针有而 C# 无，不计入）: {", ".join(dropped)}')
    print(f'   geomean ours/turbo = {geomean(ot):.3f}    geomean ours/C# = {geomean(oc):.3f}    geomean C#/turbo = {geomean(ct):.3f}')
    print(f'   Sigma ours={sigma([t[1] for t in tri]):.3f} turbo={sigma([t[2] for t in tri]):.3f} C#={sigma([t[3] for t in tri]):.3f}')

    worst = sorted(tri, key=lambda t: t[1] / t[2], reverse=True)[:5]
    worst_strs = []
    for name, o, t, c in worst:
        worst_strs.append(
            f'{name} ours/turbo={o / t:.3f} ours/C#={o / c:.3f} (ours={o:.3f} turbo={t:.3f} c#={c:.3f})'
        )
    print('   worst 前 5（按 ours/turbo 降序）：' + '； '.join(worst_strs))

    # 逐文件三列表（对账可读用）
    print()
    print('| file | ours_ms | turbo_ms | csharp_ms | ours/turbo | ours/C# |')
    print('|---|---|---|---|---|---|')
    for name, o, t, c in tri:
        print(f'| {name} | {o:.3f} | {t:.3f} | {c:.3f} | {o / t:.3f} | {o / c:.3f} |')

    print()
    print(f'ours÷turbo geomean={geomean(ot):.3f} / ours÷C# geomean={geomean(oc):.3f}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
