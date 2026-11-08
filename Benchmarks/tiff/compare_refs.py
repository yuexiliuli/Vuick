#!/usr/bin/env python3
"""TIFF 双路对账：解析探针 `KIMG_TIFF_VS_CSHARP perfile` 行 × C# 参照表（`|`name`| <ms> |`），
输出 geomean(ours/C#)、Sigma、worst 前 5 与**按布局类别(文件名 `__` 后缀 = `<layout>_<comp>[_pred]`，
无 `__` = native)的 geomean 表**。TIFF 全部文件 RGBA8 双端可比（L8/RGB8 补 alpha=FF → RGBA8）。

用法: python compare_refs.py <acceptance.log> <tiff_csharp.log>
- <acceptance.log>  由 scripts\\run_acceptance.ps1 生成（含探针 perfile 行）
- <tiff_csharp.log> 由 scripts\\run_csharp_ref.ps1 生成（C# 单核参照表）
探针有而 C# 缺失（或反之）的文件不计入 matched/geomean，单独列出。结尾打印主口径一行。
"""
import math
import re
import sys
from pathlib import Path

PERFILE = re.compile(
    r'KIMG_TIFF_VS_CSHARP perfile\s+(\S+)\s+(\d+)x(\d+)\s+color=(\S+)\s+'
    r'layout=(\w+)\s+ours=([\d.]+)'
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


def layout_of(name: str) -> str:
    stem = name.rsplit('.', 1)[0]
    if '__' in stem:
        return stem.split('__')[1]
    return 'native'


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
                'color': m.group(4),
                'layout': m.group(5),
                'ours': float(m.group(6)),
            }
    if not rows:
        print('未解析到 KIMG_TIFF_VS_CSHARP perfile 行', file=sys.stderr)
        return 2

    # 双路合并：只计「探针与 C# 都有」的文件（TIFF 全 rgba8 可比）。
    tri = []
    excluded = []
    for name, r in rows.items():
        c = cs.get(name)
        if c is None:
            excluded.append((name, 'csharp-缺失'))
            continue
        if r['layout'] != 'rgba8':
            excluded.append((name, f"layout={r['layout']}"))
            continue
        tri.append((name, r['ours'], c))
    for name in cs:
        if name not in rows:
            excluded.append((name, 'probe-缺失'))
    matched = len(tri)
    oc = [o / c for (_, o, c) in tri]

    print(f'== tiff[RGBA8 比较集] matched={matched}（探针 {len(rows)} 文件，C# {len(cs)} 文件）')
    if excluded:
        print('   不计入: ' + '； '.join(f'{n}（{why}）' for n, why in excluded))
    print(f'   geomean ours/C# = {geomean(oc):.3f}    ours 总 ms（比较集）= {sum(t[1] for t in tri):.3f}')
    print(
        f'   Sigma ours={sigma([t[1] for t in tri]):.3f} C#={sigma([t[2] for t in tri]):.3f} '
        f'ratio_sigma={sigma([o / c for (_, o, c) in tri]):.3f}'
    )

    worst = sorted(tri, key=lambda t: t[1] / t[2], reverse=True)[:5]
    worst_strs = [f'{n} ours/C#={o / c:.3f} (ours={o:.3f} c#={c:.3f})' for n, o, c in worst]
    print('   worst 前 5（按 ours/C# 降序）：' + '； '.join(worst_strs))

    # 按布局类别 geomean 表。
    print()
    print('| layout 类别 | files | ours geomean(ms) | C# geomean(ms) | ours/C# geomean |')
    print('|---|---|---|---|---|')
    by_cat = {}
    for n, o, c in tri:
        by_cat.setdefault(layout_of(n), []).append((n, o, c))
    for cat in sorted(by_cat):
        sub = by_cat[cat]
        go = geomean([o for (_, o, _) in sub])
        gc = geomean([c for (_, _, c) in sub])
        print(f'| {cat} | {len(sub)} | {go:.3f} | {gc:.3f} | {go / gc:.3f} |')

    # 逐文件双列表（对账可读用）
    print()
    print('| file | layout | ours_ms | csharp_ms | ours/C# |')
    print('|---|---|---|---|---|')
    for n, o, c in tri:
        print(f'| {n} | {layout_of(n)} | {o:.3f} | {c:.3f} | {o / c:.3f} |')

    print()
    print(f'ours÷C# geomean={geomean(oc):.3f}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
