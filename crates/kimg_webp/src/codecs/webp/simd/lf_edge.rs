use super::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

// ---------------------------------------------------------------------------
// D2v3：宏块级熔合环路滤波（C# `Vp8Decoder.FilterMacroblock` + `Vp8LoopFilter` 同款）
// ---------------------------------------------------------------------------
// 一个宏块把全 12 个核熔合成一次调用：luma 竖直×4、chroma 竖直×2（U/V 打包成 16-lane）、
// luma 水平×4、chroma 水平×2（U/V 打包）。边次序与 vp8.rs `filter_macroblock_row` 标量完全
// 一致（左 MB 竖 → 内竖×3 → 上 MB 横 → 内横×3），熔合只改调用形态、不改任何算术/读序 →
// 逐字节相同。U/V 打包把两平面拼一个向量：lane 逐样本独立，不做任何跨 U/V 混合，纯省调用与
// 向量粒度；掩码 lane 结果为原值，故「从不对 lane 跳过」也保证未滤 lane 字节不变。

/// 读一条 16-lane 色度行（低 8 = U、高 8 = V，同偏移行各 8 字节）—— C# `LoadPair` 同款。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_uv_pair_row(u: *const u8, v: *const u8, off: isize) -> __m128i { unsafe {
    let ul = _mm_loadl_epi64(u.offset(off) as *const __m128i);
    let vl = _mm_loadl_epi64(v.offset(off) as *const __m128i);
    _mm_unpacklo_epi64(ul, vl)
}}

/// 写回一条 16-lane 色度行（低 8 → U、高 8 → V）—— C# `StorePair` 同款。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_uv_pair_store(u: *mut u8, v: *mut u8, off: isize, val: __m128i) { unsafe {
    _mm_storel_epi64(u.offset(off) as *mut __m128i, val);
    _mm_storel_epi64(v.offset(off) as *mut __m128i, _mm_srli_si128(val, 8));
}}

/// 竖直色度边（U/V 打包，8+8=16 lane）：`u`/`v` 指向 q0 列（行 uv_row、列 uvx），每侧 8 行 8 列
/// 经 Load16x8/Store16x8 转置成 8 列、每列 16 lane（低 8 = U 行、高 8 = V 行）一次滤完 ——
/// C# `ChromaVertical` 同款。补齐 D2v2 `lf_filter_edge_sse2` 的 8 宽竖直标量回退。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_v8_pair(
    u: *mut u8,
    v: *mut u8,
    stride: isize,
    interior: i32,
    edge: i32,
    hev: i32,
    macroblock_edge: bool,
    simple: bool,
) { unsafe {
    let ul = u.offset(-4);
    let vl = v.offset(-4);
    let mut c = load16x8(ul, stride, vl, stride);
    if simple {
        let (p0n, q0n) = lf_do_filter2(c[2], c[3], c[4], c[5], edge);
        c[3] = p0n;
        c[4] = q0n;
    } else {
        let mx = lf_max_diff(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
        let mask = lf_mask_composite(
            lf_needs_filter(_mm_set1_epi8(-1), c[2], c[3], c[4], c[5], edge),
            mx,
            interior,
        );
        // 全零 mask：任何 lane 都无需改（输出==输入），跳过 store（c 原值不变则存回既存既等）。
        if _mm_movemask_epi8(mask) == 0 {
            return;
        }
        if macroblock_edge {
            let (p2n, p1n, p0n, q0n, q1n, q2n) =
                lf_do_filter6(c[1], c[2], c[3], c[4], c[5], c[6], mask, hev);
            c[1] = p2n;
            c[2] = p1n;
            c[3] = p0n;
            c[4] = q0n;
            c[5] = q1n;
            c[6] = q2n;
        } else {
            let (p1n, p0n, q0n, q1n) = lf_do_filter4(c[2], c[3], c[4], c[5], mask, hev);
            c[2] = p1n;
            c[3] = p0n;
            c[4] = q0n;
            c[5] = q1n;
        }
    }
    store16x8(ul, stride, vl, stride, c);
}}

/// 水平色度边（U/V 打包）：`u`/`v` 指向 q0 行（行 uv_row±k、列 uvx），同偏移行各 8 字节拼一个
/// 16-lane 向量一次滤完 —— C# `ChromaHorizontal` 同款 LoadPair/StorePair。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_h_pair(
    u: *mut u8,
    v: *mut u8,
    stride: isize,
    interior: i32,
    edge: i32,
    hev: i32,
    macroblock_edge: bool,
    simple: bool,
) {
    unsafe {
        if simple {
            let (p0n, q0n) = lf_do_filter2(
                lf_uv_pair_row(u, v, -2 * stride),
                lf_uv_pair_row(u, v, -1 * stride),
                lf_uv_pair_row(u, v, 0),
                lf_uv_pair_row(u, v, stride),
                edge,
            );
            lf_uv_pair_store(u, v, -1 * stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            return;
        }
        let p3 = lf_uv_pair_row(u, v, -4 * stride);
        let p2 = lf_uv_pair_row(u, v, -3 * stride);
        let p1 = lf_uv_pair_row(u, v, -2 * stride);
        let p0 = lf_uv_pair_row(u, v, -1 * stride);
        let q0 = lf_uv_pair_row(u, v, 0);
        let q1 = lf_uv_pair_row(u, v, stride);
        let q2 = lf_uv_pair_row(u, v, 2 * stride);
        let q3 = lf_uv_pair_row(u, v, 3 * stride);
        let mx = lf_max_diff(p3, p2, p1, p0, q0, q1, q2, q3);
        let mask = lf_mask_composite(
            lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge),
            mx,
            interior,
        );
        // 全零 mask：任何 lane 都无需改（输出==输入），跳过 store。
        if _mm_movemask_epi8(mask) == 0 {
            return;
        }
        if macroblock_edge {
            let (p2n, p1n, p0n, q0n, q1n, q2n) = lf_do_filter6(p2, p1, p0, q0, q1, q2, mask, hev);
            lf_uv_pair_store(u, v, -3 * stride, p2n);
            lf_uv_pair_store(u, v, -2 * stride, p1n);
            lf_uv_pair_store(u, v, -1 * stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            lf_uv_pair_store(u, v, stride, q1n);
            lf_uv_pair_store(u, v, 2 * stride, q2n);
        } else {
            let (p1n, p0n, q0n, q1n) = lf_do_filter4(p1, p0, q0, q1, mask, hev);
            lf_uv_pair_store(u, v, -2 * stride, p1n);
            lf_uv_pair_store(u, v, -1 * stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            lf_uv_pair_store(u, v, stride, q1n);
        }
    }
}

/// 宏块级熔合环路滤波入口（生产；C# `Vp8Decoder.FilterMacroblock` 同款）。一次宏块完成
/// 全 12 个 16-lane 核调用，边次序与标量 `vp8::filter_macroblock_row` 完全一致，逐字节相同。
/// 参数为滤波强度实值（调用方已算好 interior/edge_limit/sub_limit/hev）。
/// 指针语义：`yc` 指向 (行 mby*16、列 mbx*16) 的 q0（= center = y*pitch+1+x），`uc`/`vc` 同理
/// 指向色度 q0；跨块读（左列 -4 / 上行 -4）落在解码平面 4 像素边距内（与标量同界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn lf_filter_macroblock(
    yc: *mut u8,
    y_pitch: isize,
    uc: *mut u8,
    vc: *mut u8,
    uv_pitch: isize,
    left: bool,
    top: bool,
    inner: bool,
    simple: bool,
    interior: i32,
    edge_limit: i32,
    sub_limit: i32,
    hev: i32,
) { unsafe {
    // 竖直段：左 MB 边 → 内竖×3（与标量同序：后边读前边已滤像素）。
    if left {
        lf_edge_v16(yc.offset(-4), y_pitch, interior, edge_limit, hev, true, simple);
        if !simple {
            lf_edge_v8_pair(uc, vc, uv_pitch, interior, edge_limit, hev, true, false);
        }
    }
    if inner {
        lf_edge_v16(yc, y_pitch, interior, sub_limit, hev, false, simple);
        lf_edge_v16(yc.offset(4), y_pitch, interior, sub_limit, hev, false, simple);
        lf_edge_v16(yc.offset(8), y_pitch, interior, sub_limit, hev, false, simple);
        if !simple {
            lf_edge_v8_pair(uc.offset(4), vc.offset(4), uv_pitch, interior, sub_limit, hev, false, false);
        }
    }
    // 水平段：上 MB 边 → 内横×3。
    if top {
        lf_edge_h(yc, y_pitch, 16, interior, edge_limit, hev, true, simple);
        if !simple {
            lf_edge_h_pair(uc, vc, uv_pitch, interior, edge_limit, hev, true, false);
        }
    }
    if inner {
        lf_edge_h(yc.offset(4 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        lf_edge_h(yc.offset(8 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        lf_edge_h(yc.offset(12 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        if !simple {
            lf_edge_h_pair(
                uc.offset(4 * uv_pitch),
                vc.offset(4 * uv_pitch),
                uv_pitch,
                interior,
                sub_limit,
                hev,
                false,
                false,
            );
        }
    }
}}



