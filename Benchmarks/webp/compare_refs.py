#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""WebP 单线程性能对账：DLL 探针 per-file(kimg vs libwebp-FULL) × C# quick per-file。
用法: python compare_refs.py <dll_probe_log> <csharp_quick_log> [--all-scen]
输出: lossy/lossless 的 geomean：kimg/libwebp、C#/libwebp、kimg/C#；逐文件 hot/worst。
"""
import sys, math, re

def geomean(vals):
    if not vals:
        return float('nan')
    return math.exp(sum(math.log(v) for v in vals) / len(vals))

def parse_dll(path):
    rows = {}  # name -> (scen, kimg_ms, lw_ms)
    for line in open(path, encoding='utf-8', errors='replace'):
        m = re.match(r'KIMG_WEBP_VS_DLL perfile (\S+) (\S+) (\d+)x(\d+) mb=\S+ kimg_new=([\d.]+) libwebp=([\d.]+) ratio_knew_lib=([\d.]+)', line)
        if m:
            scen, name, w, h, k, l, r = m.groups()
            rows.setdefault(name, []).append((scen, float(k), float(l)))
    # 同名去重（多源同位）：取 kimg 最小（最暖态）
    out = {}
    for name, lst in rows.items():
        lst.sort(key=lambda x: x[1])
        out[name] = lst[0]
    return out

def parse_csharp(path):
    rows = {}  # name -> csharp_ms
    for line in open(path, encoding='utf-8', errors='replace'):
        m = re.match(r'\|\s*`([^`]+)`\s*\|\s*([\d.]+)', line)
        if m:
            rows[m.group(1)] = float(m.group(2))
    return rows

def fmt(ratio):
    return f"{ratio:.3f}"

def main():
    dll_path, csharp_path = sys.argv[1], sys.argv[2]
    all_scen = '--all-scen' in sys.argv[3:]
    dll = parse_dll(dll_path)
    csh = parse_csharp(csharp_path)
    for scen in (['static-lossy', 'static-lossless', 'alpha-lossless'] if all_scen else ['static-lossy']):
        kl, cl, kc, files = [], [], [], []
        for name, (s, k, l) in dll.items():
            if s != scen:
                continue
            if name.startswith('tiny-'):
                continue
            if name not in csh:
                continue
            c = csh[name]
            kl.append(k / l); cl.append(c / l); kc.append(k / c)
            files.append((name, k, l, c))
        if not files:
            print(f"== {scen}: no matched files")
            continue
        # 排序展示最差文件
        worst = sorted(files, key=lambda t: t[1] / t[2], reverse=True)
        print(f"== {scen}: matched={len(files)}")
        print(f"   geomean kimg/libwebp = {fmt(geomean(kl))}   geomean C#/libwebp = {fmt(geomean(cl))}   geomean kimg/C# = {fmt(geomean(kc))}")
        print(f"   Sigma kimg=Σ{sum(t[1] for t in files):.0f}ms  libwebp=Σ{sum(t[2] for t in files):.0f}ms  C#=Σ{sum(t[3] for t in files):.0f}ms")
        for name, k, l, c in worst[:5]:
            print(f"   worst {name}: kimg/libwebp={k/l:.2f}  kimg/C#={k/c:.2f}  (kimg={k:.1f} lw={l:.1f} c#={c:.1f})")

if __name__ == '__main__':
    main()
