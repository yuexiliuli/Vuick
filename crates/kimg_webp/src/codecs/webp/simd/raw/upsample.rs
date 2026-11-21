#![allow(unsafe_code)]
// fancy 上采样内核逐寄存器传参，具名构参破坏字节恒等算术单源性 → 放行。
#![allow(clippy::too_many_arguments)]
//! 裸 unsafe 叶子子模块 `raw::upsample`：fancy 上采样（SSE2/SSSE3/AVX2 16-bit lane）的全部
//! `#[target_feature]` SIMD 内核 + 安全门面整体迁入（原 `simd/upsample.rs`）。上层
//! `simd/upsample.rs` 以 `#![forbid(unsafe_code)]` + 薄委托显式依赖本模块。算法 / 常量 /
//! 调用顺序与隔离前逐字节一致（字节恒等是硬约束）。

use super::super::*;

// ---------------------------------------------------------------------------
// fancy 上采样（SSE2 16-bit lane；语义对齐 vp8.rs `upsample_fancy`：`3*cur+adj` 的 t 行 +
// 偶数/奇数对的 `(3t±邻居+8)>>4`，无重结合）
// ---------------------------------------------------------------------------

/// fancy 上采样一行 chroma：`current`/`adjacent` 各 `cw` 字节 → 输出 `width` 字节。
/// SIMD（SSSE3）处理中间列对（col=1..cw-2），首/末列与 `cw<=1` 走标量（边界算术特殊）。
/// 便捷包装（生产热路径用 [`upsample_fancy_into`] 复用缓冲；本包装供测试/独立调用）。
#[cfg_attr(not(test), allow(dead_code))]
pub(in super::super) fn upsample_fancy_row(current: &[u8], adjacent: &[u8], width: usize) -> Vec<u8> {
    let mut out = vec![0u8; width];
    let mut t = vec![0u16; current.len() + 8];
    upsample_fancy_into(current, adjacent, &mut out, &mut t);
    out
}

/// fancy 上采样写入调用方缓冲（复用 `out`/`t_ws`，避免逐行分配；`t_ws` 须 ≥ `cw+8`）。
/// 生产热路径改走 [`upsample_fancy_into_uv`]（U/V 合并）；本函数保留为标量对拍参考/测试路径。
#[cfg_attr(not(test), allow(dead_code))]
pub(in super::super) fn upsample_fancy_into(current: &[u8], adjacent: &[u8], out: &mut [u8], t_ws: &mut [u16]) {
    debug_assert_eq!(current.len(), adjacent.len());
    let cw = current.len();
    debug_assert!(out.len() >= 2 * cw - 1);
    let width = out.len();
    let t = &mut t_ws[..cw + 8];
    // 仅清零 8 个读安全 padding（`t[0..cw]` 紧随其后整体覆写，无需先零）；padding 值恒零、
    // 只作末组 SSE 越界读的保护，不参与任何输出字节（逐字节与全场清零恒等）。
    t[cw..].fill(0);
    // t 行 `3*cur + adj`：AVX2 每 32 列一组、SSE2 每 16 列一组（u16 16-lane），不足尾标量；
    // 经 unpack 零扩展后逐值等于标量（3*255+255=1020 < 2^16 无溢出），`t[0..cw]` 写满语义不变。
    let mut col = 0usize;
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") {
        let cp = current.as_ptr();
        let ap = adjacent.as_ptr();
        let tp = t.as_mut_ptr();
        unsafe {
            while col + 32 <= cw {
                upsample_fancy_trow_avx2(cp.add(col), ap.add(col), tp.add(col));
                col += 32;
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse2") {
        let cp = current.as_ptr();
        let ap = adjacent.as_ptr();
        let tp = t.as_mut_ptr();
        unsafe {
            while col + 16 <= cw {
                upsample_fancy_trow_sse2(cp.add(col), ap.add(col), tp.add(col));
                col += 16;
            }
        }
    }
    while col < cw {
        t[col] = 3 * u16::from(current[col]) + u16::from(adjacent[col]);
        col += 1;
    }
    let out = &mut out[..width];

    // 边界列（0/1 与末列）标量（`upsample_fancy` 同款）。
    let first = t[0];
    out[0] = ((4 * first + 8) >> 4) as u8;
    if width > 1 {
        let next = if cw > 1 { t[1] } else { first };
        out[1] = ((3 * first + next + 8) >> 4) as u8;
    }
    if cw > 1 {
        let last_col = cw - 1;
        let c = t[last_col];
        let oc = last_col * 2;
        out[oc] = ((3 * c + t[last_col - 1] + 8) >> 4) as u8;
        if oc + 1 < width {
            out[oc + 1] = ((4 * c + 8) >> 4) as u8;
        }
    }

    // 中间列对（col=1..cw-2）：AVX2 16 对/步 主批量 → 共享收尾；无 AVX2 直接 ssse3/标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && cw >= 18 {
        let mut col = 1usize;
        unsafe {
            // 16 对/步（C# `ChromaPairShuffleIndices` 同款）：current=t[col..col+16]、
            // 左邻 t[col-1..col+15]、右邻 t[col+1..col+17]，数学右读 ≤ t[cw-1]。
            while col + 17 <= cw {
                upsample_fancy_avx2(t.as_ptr().add(col), out.as_mut_ptr().add(col * 2));
                col += 16;
            }
        }
        if col < cw.saturating_sub(1) {
            upsample_fancy_tail(t, out, col, cw);
        }
        return;
    }
    // 中间列对（col=1..cw-2）：每 4 列一组 SSSE3 推 8 个输出字节；尾列标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        let mut col = 1usize;
        unsafe {
            while col + 4 < cw {
                upsample_fancy_sse2(t.as_ptr().add(col - 1), out.as_mut_ptr().add(col * 2));
                col += 4;
            }
        }
        while col < cw.saturating_sub(1) {
            let c = t[col];
            let oc = col * 2;
            out[oc] = ((3 * c + t[col - 1] + 8) >> 4) as u8;
            out[oc + 1] = ((3 * c + t[col + 1] + 8) >> 4) as u8;
            col += 1;
        }
        return;
    }

    // 无 SIMD / 未满 6 列：直接复用 vp8 标量（单一算术源，边界语义零漂移）。
    out.copy_from_slice(&super::vp8::upsample_fancy(current, adjacent, width));
}

/// SSE2：一次载入 `current`/`adjacent` 的 16 字节 → unpack 零扩展成 16 u16 → 算
/// `t = 3*current + adjacent` 写 16 个 u16。与标量 `t[i] = 3*cur[i] + adj[i]` 逐值相等
/// （3*255+255=1020 < u16::MAX 无溢出；调用方保证 `col+16 ≤ cw` 不越界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn upsample_fancy_trow_sse2(cur: *const u8, adj: *const u8, t: *mut u16) {
    use std::arch::x86_64::*;
    unsafe {
        let c = _mm_loadu_si128(cur as *const __m128i);
        let a = _mm_loadu_si128(adj as *const __m128i);
        let three = _mm_set1_epi16(3);
        let zero = _mm_setzero_si128();
        let clo = _mm_unpacklo_epi8(c, zero);
        let chi = _mm_unpackhi_epi8(c, zero);
        let alo = _mm_unpacklo_epi8(a, zero);
        let ahi = _mm_unpackhi_epi8(a, zero);
        let tlo = _mm_add_epi16(_mm_mullo_epi16(clo, three), alo);
        let thi = _mm_add_epi16(_mm_mullo_epi16(chi, three), ahi);
        _mm_storeu_si128(t as *mut __m128i, tlo);
        _mm_storeu_si128((t as *mut __m128i).add(1), thi);
    }
}

/// AVX2：一次载入 `current`/`adjacent` 的 32 字节 → 拆低/高 16 字节并宽成 32 u16 → 算
/// `t = 3*current + adjacent` 写 32 个 u16（t[col..col+32]）。与标量逐值相等，逐字节等于两次
/// [`upsample_fancy_trow_sse2`]（每 16 列组算术同源；调用方保证 `col+32 ≤ cw` 不越界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn upsample_fancy_trow_avx2(cur: *const u8, adj: *const u8, t: *mut u16) {
    use std::arch::x86_64::*;
    unsafe {
        let c = _mm256_loadu_si256(cur as *const __m256i);
        let a = _mm256_loadu_si256(adj as *const __m256i);
        let three = _mm256_set1_epi16(3);
        let _zero = _mm256_setzero_si256();
        // 低 16 列 → t[col..col+16]，高 16 列 → t[col+16..col+32]。
        let clo = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(c));
        let chi = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(c, 1));
        let alo = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(a));
        let ahi = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(a, 1));
        let tlo = _mm256_add_epi16(_mm256_mullo_epi16(clo, three), alo);
        let thi = _mm256_add_epi16(_mm256_mullo_epi16(chi, three), ahi);
        _mm256_storeu_si256(t as *mut __m256i, tlo);
        _mm256_storeu_si256((t as *mut __m256i).add(1), thi);
    }
}

/// 中间列对收尾：从 `col` 起 4 对/步 SSSE3 + 标量尾（与 `upsample_fancy` 标量同算术）。
/// 供 AVX2 16 对/步主批量推进后复用（SSSE3 未启用时全标量）。
#[cfg(target_arch = "x86_64")]
fn upsample_fancy_tail(t: &[u16], out: &mut [u8], mut col: usize, cw: usize) {
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        unsafe {
            // 窗口 t[col-1..col+5]（6 个 u16）+ 8 填充保证 8 u16 读安全：col+5 ≤ cw。
            while col + 4 < cw {
                upsample_fancy_sse2(t.as_ptr().add(col - 1), out.as_mut_ptr().add(col * 2));
                col += 4;
            }
        }
    }
    while col < cw.saturating_sub(1) {
        let c = t[col];
        let oc = col * 2;
        out[oc] = ((3 * c + t[col - 1] + 8) >> 4) as u8;
        out[oc + 1] = ((3 * c + t[col + 1] + 8) >> 4) as u8;
        col += 1;
    }
}

/// fancy 上采样 U/V 两行一次完成（参照 D2 `lf_uv_pair` 的 U/V 双平面打包先例，把两次
/// `upsample_fancy_into` 合并成同一循环构建两 t 行 + 同一循环处理两输出平面，省散布往返）。
/// 语义与逐平面调用 [`upsample_fancy_into`] 完全一致（逐字节）：t 行 `3*cur+adj`、边界列、
/// 中间列对 `(3*t±邻居+8)>>4` 全同算术，仅调用形态合并。`t_ws` 须 ≥ `2*(cw+8)`（低半 U、高半 V）。
pub(in super::super) fn upsample_fancy_into_uv(
    u_cur: &[u8],
    u_adj: &[u8],
    v_cur: &[u8],
    v_adj: &[u8],
    u_out: &mut [u8],
    v_out: &mut [u8],
    t_ws: &mut [u16],
) {
    let cw = u_cur.len();
    debug_assert_eq!(u_cur.len(), u_adj.len());
    debug_assert_eq!(v_cur.len(), v_adj.len());
    debug_assert_eq!(u_cur.len(), v_cur.len());
    debug_assert!(u_out.len() >= 2 * cw - 1);
    debug_assert_eq!(u_out.len(), v_out.len());
    let width = u_out.len();
    debug_assert!(t_ws.len() >= 2 * (cw + 8));
    let (tu, tv) = t_ws.split_at_mut(cw + 8);
    // padding 值恒零、只作末组 SSE 越界读保护（语义同单平面版）。
    tu[cw..].fill(0);
    tv[cw..].fill(0);
    // t 行合并构建：同一循环内向量化两平面（AVX2 32 列 / SSE2 16 列每组），尾部标量。
    let mut col = 0usize;
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") {
        let ucp = u_cur.as_ptr();
        let uap = u_adj.as_ptr();
        let vcp = v_cur.as_ptr();
        let vap = v_adj.as_ptr();
        let tup = tu.as_mut_ptr();
        let tvp = tv.as_mut_ptr();
        unsafe {
            while col + 32 <= cw {
                upsample_fancy_trow_avx2(ucp.add(col), uap.add(col), tup.add(col));
                upsample_fancy_trow_avx2(vcp.add(col), vap.add(col), tvp.add(col));
                col += 32;
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse2") {
        let ucp = u_cur.as_ptr();
        let uap = u_adj.as_ptr();
        let vcp = v_cur.as_ptr();
        let vap = v_adj.as_ptr();
        let tup = tu.as_mut_ptr();
        let tvp = tv.as_mut_ptr();
        unsafe {
            while col + 16 <= cw {
                upsample_fancy_trow_sse2(ucp.add(col), uap.add(col), tup.add(col));
                upsample_fancy_trow_sse2(vcp.add(col), vap.add(col), tvp.add(col));
                col += 16;
            }
        }
    }
    while col < cw {
        tu[col] = 3 * u16::from(u_cur[col]) + u16::from(u_adj[col]);
        tv[col] = 3 * u16::from(v_cur[col]) + u16::from(v_adj[col]);
        col += 1;
    }
    let uo = &mut u_out[..width];
    let vo = &mut v_out[..width];

    // 边界列（U/V 同款算术，两平面并算）。
    let (fu, fv) = (tu[0], tv[0]);
    uo[0] = ((4 * fu + 8) >> 4) as u8;
    vo[0] = ((4 * fv + 8) >> 4) as u8;
    if width > 1 {
        let (nu, nv) = if cw > 1 { (tu[1], tv[1]) } else { (fu, fv) };
        uo[1] = ((3 * fu + nu + 8) >> 4) as u8;
        vo[1] = ((3 * fv + nv + 8) >> 4) as u8;
    }
    if cw > 1 {
        let last_col = cw - 1;
        let (cu, cv) = (tu[last_col], tv[last_col]);
        let oc = last_col * 2;
        uo[oc] = ((3 * cu + tu[last_col - 1] + 8) >> 4) as u8;
        vo[oc] = ((3 * cv + tv[last_col - 1] + 8) >> 4) as u8;
        if oc + 1 < width {
            uo[oc + 1] = ((4 * cu + 8) >> 4) as u8;
            vo[oc + 1] = ((4 * cv + 8) >> 4) as u8;
        }
    }

    // 中间列对（col=1..cw-2）：AVX2 16 对/步 主批量（U/V 同一迭代）→ 共享收尾；否则 ssse3/标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && cw >= 18 {
        let mut col = 1usize;
        unsafe {
            // 16 对/步（C# `ChromaPairShuffleIndices` 同款）：右读 ≤ t[cw-1]。
            while col + 17 <= cw {
                upsample_fancy_avx2(tu.as_ptr().add(col), uo.as_mut_ptr().add(col * 2));
                upsample_fancy_avx2(tv.as_ptr().add(col), vo.as_mut_ptr().add(col * 2));
                col += 16;
            }
        }
        if col < cw.saturating_sub(1) {
            upsample_fancy_tail(tu, uo, col, cw);
            upsample_fancy_tail(tv, vo, col, cw);
        }
        return;
    }
    // 中间列对（col=1..cw-2）：每 4 列一组 SSSE3 各推 8 字节（同一迭代内 U/V），尾列标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        let mut col = 1usize;
        unsafe {
            // 窗口 t[col-1..col+5]（6 个 u16）+ 8 填充保证 8 u16 读安全：col+5 ≤ cw。
            while col + 4 < cw {
                upsample_fancy_sse2(tu.as_ptr().add(col - 1), uo.as_mut_ptr().add(col * 2));
                upsample_fancy_sse2(tv.as_ptr().add(col - 1), vo.as_mut_ptr().add(col * 2));
                col += 4;
            }
        }
        while col < cw.saturating_sub(1) {
            let oc = col * 2;
            let cu = tu[col];
            let cv = tv[col];
            uo[oc] = ((3 * cu + tu[col - 1] + 8) >> 4) as u8;
            uo[oc + 1] = ((3 * cu + tu[col + 1] + 8) >> 4) as u8;
            vo[oc] = ((3 * cv + tv[col - 1] + 8) >> 4) as u8;
            vo[oc + 1] = ((3 * cv + tv[col + 1] + 8) >> 4) as u8;
            col += 1;
        }
        return;
    }

    // 无 SIMD / 未满 6 列：直接复用 vp8 标量（单一算术源，边界语义零漂移）。
    uo.copy_from_slice(&super::vp8::upsample_fancy(u_cur, u_adj, width));
    vo.copy_from_slice(&super::vp8::upsample_fancy(v_cur, v_adj, width));
}

/// SSE2：一次载入 `top`/`cur` 各 16 字节 → 零扩展成 16 u16 → 写两行 t（**共享一次装入/乘加**）：
/// 奇行 `t_odd = 3*top + cur`、偶行 `t_even = 3*cur + top`。与标量逐值相等（3*255+255=1020
/// < u16::MAX 无溢出；调用方保证 `col+16 ≤ cw` 不越界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn upsample_fancy_trow_pair_sse2(
    top: *const u8,
    cur: *const u8,
    t_odd: *mut u16,
    t_even: *mut u16,
) {
    use std::arch::x86_64::*;
    unsafe {
        let tp = _mm_loadu_si128(top as *const __m128i);
        let cp = _mm_loadu_si128(cur as *const __m128i);
        let three = _mm_set1_epi16(3);
        let z = _mm_setzero_si128();
        let (tlo, thi) = (_mm_unpacklo_epi8(tp, z), _mm_unpackhi_epi8(tp, z));
        let (clo, chi) = (_mm_unpacklo_epi8(cp, z), _mm_unpackhi_epi8(cp, z));
        _mm_storeu_si128(t_odd as *mut __m128i, _mm_add_epi16(_mm_mullo_epi16(tlo, three), clo));
        _mm_storeu_si128(
            t_odd.add(8) as *mut __m128i,
            _mm_add_epi16(_mm_mullo_epi16(thi, three), chi),
        );
        _mm_storeu_si128(t_even as *mut __m128i, _mm_add_epi16(_mm_mullo_epi16(clo, three), tlo));
        _mm_storeu_si128(
            t_even.add(8) as *mut __m128i,
            _mm_add_epi16(_mm_mullo_epi16(chi, three), thi),
        );
    }
}

/// AVX2：一次载入 `top`/`cur` 各 32 字节 → 宽成 32 u16 → 写两行 t（奇 `3*top+cur`、偶
/// `3*cur+top`，共享一次装入/乘加）。与标量逐值相等，逐字节等于两次
/// [`upsample_fancy_trow_pair_sse2`] 的每 16 列组（调用方保证 `col+32 ≤ cw`）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn upsample_fancy_trow_pair_avx2(
    top: *const u8,
    cur: *const u8,
    t_odd: *mut u16,
    t_even: *mut u16,
) {
    use std::arch::x86_64::*;
    unsafe {
        let tp = _mm256_loadu_si256(top as *const __m256i);
        let cp = _mm256_loadu_si256(cur as *const __m256i);
        let three = _mm256_set1_epi16(3);
        let (tlo, thi) = (
            _mm256_cvtepu8_epi16(_mm256_castsi256_si128(tp)),
            _mm256_cvtepu8_epi16(_mm256_extracti128_si256(tp, 1)),
        );
        let (clo, chi) = (
            _mm256_cvtepu8_epi16(_mm256_castsi256_si128(cp)),
            _mm256_cvtepu8_epi16(_mm256_extracti128_si256(cp, 1)),
        );
        _mm256_storeu_si256(
            t_odd as *mut __m256i,
            _mm256_add_epi16(_mm256_mullo_epi16(tlo, three), clo),
        );
        _mm256_storeu_si256(
            t_odd.add(16) as *mut __m256i,
            _mm256_add_epi16(_mm256_mullo_epi16(thi, three), chi),
        );
        _mm256_storeu_si256(
            t_even as *mut __m256i,
            _mm256_add_epi16(_mm256_mullo_epi16(clo, three), tlo),
        );
        _mm256_storeu_si256(
            t_even.add(16) as *mut __m256i,
            _mm256_add_epi16(_mm256_mullo_epi16(chi, three), thi),
        );
    }
}

/// 行对 fancy 上采样（U/V × 奇/偶两输出行一次完成，libwebp `UpsampleLinePair` 同趟 top/bottom
/// 语义）：对相邻 chroma 行对 `(top=C_k, cur=C_{k+1})` 生成输出行 **2k+1（奇）** 的整行 U/V
/// fancy（t = 3*top + cur）与 **2k+2（偶）** 的整行 U/V fancy（t = 3*cur + top）。与逐行两次
/// 调用 [`upsample_fancy_into_uv`] 完全一致（**同算术、逐字节**，`upsample_pair_matches_twice_into_uv`
/// 对拍 0 差）：两 chroma 行只装入一次、t 行融合构建（四行 t 同一循环）、fancy 中间列循环把
/// U/V × 奇/偶 四 t 行并进同一迭代（比逐行两次省一次 chroma 内存往返 + 循环 scaffold 一半）。
/// 语义取舍：与各输出行独立调用相比，加法树完全一致（t = 3*cur+adj 只是把「当前行」在奇偶行
/// 的交换显式化），无任何重结合 → 字节恒等由构造保证。`t_ws` ≥ **4**(cw+8) u16：低半 U（奇/偶）、
/// 高半 V（奇/偶），每行 cw+8 尾填充供 SSE 越界读保护。
pub(in super::super) fn upsample_fancy_into_uv_pair(
    u_top: &[u8],
    u_cur: &[u8],
    v_top: &[u8],
    v_cur: &[u8],
    u_odd_out: &mut [u8],
    v_odd_out: &mut [u8],
    u_even_out: &mut [u8],
    v_even_out: &mut [u8],
    t_ws: &mut [u16],
) {
    let cw = u_top.len();
    debug_assert_eq!(u_top.len(), u_cur.len());
    debug_assert_eq!(v_top.len(), v_cur.len());
    debug_assert_eq!(u_top.len(), v_top.len());
    let width = u_odd_out.len();
    debug_assert_eq!(u_odd_out.len(), v_odd_out.len());
    debug_assert_eq!(u_odd_out.len(), u_even_out.len());
    debug_assert_eq!(u_even_out.len(), v_even_out.len());
    debug_assert!(t_ws.len() >= 4 * (cw + 8));
    let (tu, tv) = t_ws.split_at_mut(2 * (cw + 8));
    let (tu_odd, tu_even) = tu.split_at_mut(cw + 8);
    let (tv_odd, tv_even) = tv.split_at_mut(cw + 8);
    // padding（恒 0）只作末组 SSE 越界读保护，不参与任何输出字节（语义同单行版）。
    tu_odd[cw..].fill(0);
    tu_even[cw..].fill(0);
    tv_odd[cw..].fill(0);
    tv_even[cw..].fill(0);
    // t 行融合构建：AVX2 32 列 / SSE2 16 列每组（U/V 各一对），尾部标量。
    let mut col = 0usize;
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") {
        let (u_tp, u_cp) = (u_top.as_ptr(), u_cur.as_ptr());
        let (v_tp, v_cp) = (v_top.as_ptr(), v_cur.as_ptr());
        unsafe {
            while col + 32 <= cw {
                upsample_fancy_trow_pair_avx2(
                    u_tp.add(col),
                    u_cp.add(col),
                    tu_odd.as_mut_ptr().add(col),
                    tu_even.as_mut_ptr().add(col),
                );
                upsample_fancy_trow_pair_avx2(
                    v_tp.add(col),
                    v_cp.add(col),
                    tv_odd.as_mut_ptr().add(col),
                    tv_even.as_mut_ptr().add(col),
                );
                col += 32;
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse2") {
        let (u_tp, u_cp) = (u_top.as_ptr(), u_cur.as_ptr());
        let (v_tp, v_cp) = (v_top.as_ptr(), v_cur.as_ptr());
        unsafe {
            while col + 16 <= cw {
                upsample_fancy_trow_pair_sse2(
                    u_tp.add(col),
                    u_cp.add(col),
                    tu_odd.as_mut_ptr().add(col),
                    tu_even.as_mut_ptr().add(col),
                );
                upsample_fancy_trow_pair_sse2(
                    v_tp.add(col),
                    v_cp.add(col),
                    tv_odd.as_mut_ptr().add(col),
                    tv_even.as_mut_ptr().add(col),
                );
                col += 16;
            }
        }
    }
    while col < cw {
        let ut = u16::from(u_top[col]);
        let uc = u16::from(u_cur[col]);
        let vt = u16::from(v_top[col]);
        let vc = u16::from(v_cur[col]);
        tu_odd[col] = 3 * ut + uc;
        tu_even[col] = 3 * uc + ut;
        tv_odd[col] = 3 * vt + vc;
        tv_even[col] = 3 * vc + vt;
        col += 1;
    }
    let w = width;
    let (uo_o, vo_o) = (&mut u_odd_out[..w], &mut v_odd_out[..w]);
    let (uo_e, vo_e) = (&mut u_even_out[..w], &mut v_even_out[..w]);

    // 边界列（U/V × 奇/偶 四 t 行同款标量算术，`upsample_fancy_into_uv` 边界逐项对应）。
    let (fuo, feo, fvo, fve) = (tu_odd[0], tu_even[0], tv_odd[0], tv_even[0]);
    uo_o[0] = ((4 * fuo + 8) >> 4) as u8;
    uo_e[0] = ((4 * feo + 8) >> 4) as u8;
    vo_o[0] = ((4 * fvo + 8) >> 4) as u8;
    vo_e[0] = ((4 * fve + 8) >> 4) as u8;
    if width > 1 {
        let (nuo, neo, nvo, nve) = if cw > 1 {
            (tu_odd[1], tu_even[1], tv_odd[1], tv_even[1])
        } else {
            (fuo, feo, fvo, fve)
        };
        uo_o[1] = ((3 * fuo + nuo + 8) >> 4) as u8;
        uo_e[1] = ((3 * feo + neo + 8) >> 4) as u8;
        vo_o[1] = ((3 * fvo + nvo + 8) >> 4) as u8;
        vo_e[1] = ((3 * fve + nve + 8) >> 4) as u8;
    }
    if cw > 1 {
        let last = cw - 1;
        let oc = last * 2;
        uo_o[oc] = ((3 * tu_odd[last] + tu_odd[last - 1] + 8) >> 4) as u8;
        uo_e[oc] = ((3 * tu_even[last] + tu_even[last - 1] + 8) >> 4) as u8;
        vo_o[oc] = ((3 * tv_odd[last] + tv_odd[last - 1] + 8) >> 4) as u8;
        vo_e[oc] = ((3 * tv_even[last] + tv_even[last - 1] + 8) >> 4) as u8;
        if oc + 1 < width {
            uo_o[oc + 1] = ((4 * tu_odd[last] + 8) >> 4) as u8;
            uo_e[oc + 1] = ((4 * tu_even[last] + 8) >> 4) as u8;
            vo_o[oc + 1] = ((4 * tv_odd[last] + 8) >> 4) as u8;
            vo_e[oc + 1] = ((4 * tv_even[last] + 8) >> 4) as u8;
        }
    }

    // 中间列对（col=1..cw-2）：AVX2 16 对/步（U/V × 奇/偶 四 t 行同一迭代）→ 共享收尾。
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && cw >= 18 {
        let mut col = 1usize;
        unsafe {
            while col + 17 <= cw {
                upsample_fancy_avx2(tu_odd.as_ptr().add(col), uo_o.as_mut_ptr().add(col * 2));
                upsample_fancy_avx2(tu_even.as_ptr().add(col), uo_e.as_mut_ptr().add(col * 2));
                upsample_fancy_avx2(tv_odd.as_ptr().add(col), vo_o.as_mut_ptr().add(col * 2));
                upsample_fancy_avx2(tv_even.as_ptr().add(col), vo_e.as_mut_ptr().add(col * 2));
                col += 16;
            }
        }
        if col < cw.saturating_sub(1) {
            upsample_fancy_tail(tu_odd, uo_o, col, cw);
            upsample_fancy_tail(tu_even, uo_e, col, cw);
            upsample_fancy_tail(tv_odd, vo_o, col, cw);
            upsample_fancy_tail(tv_even, vo_e, col, cw);
        }
        return;
    }
    // 中间列对（col=1..cw-2）：每 4 列一组 SSSE3（四 t 行同一迭代），尾列标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        let mut col = 1usize;
        unsafe {
            while col + 4 < cw {
                upsample_fancy_sse2(tu_odd.as_ptr().add(col - 1), uo_o.as_mut_ptr().add(col * 2));
                upsample_fancy_sse2(tu_even.as_ptr().add(col - 1), uo_e.as_mut_ptr().add(col * 2));
                upsample_fancy_sse2(tv_odd.as_ptr().add(col - 1), vo_o.as_mut_ptr().add(col * 2));
                upsample_fancy_sse2(tv_even.as_ptr().add(col - 1), vo_e.as_mut_ptr().add(col * 2));
                col += 4;
            }
        }
        while col < cw.saturating_sub(1) {
            let oc = col * 2;
            uo_o[oc] = ((3 * tu_odd[col] + tu_odd[col - 1] + 8) >> 4) as u8;
            uo_o[oc + 1] = ((3 * tu_odd[col] + tu_odd[col + 1] + 8) >> 4) as u8;
            uo_e[oc] = ((3 * tu_even[col] + tu_even[col - 1] + 8) >> 4) as u8;
            uo_e[oc + 1] = ((3 * tu_even[col] + tu_even[col + 1] + 8) >> 4) as u8;
            vo_o[oc] = ((3 * tv_odd[col] + tv_odd[col - 1] + 8) >> 4) as u8;
            vo_o[oc + 1] = ((3 * tv_odd[col] + tv_odd[col + 1] + 8) >> 4) as u8;
            vo_e[oc] = ((3 * tv_even[col] + tv_even[col - 1] + 8) >> 4) as u8;
            vo_e[oc + 1] = ((3 * tv_even[col] + tv_even[col + 1] + 8) >> 4) as u8;
            col += 1;
        }
        return;
    }

    // 无 SIMD / 未满 6 列：逐行复用 vp8 标量（单一算术源，边界语义零漂移）。
    uo_o.copy_from_slice(&super::vp8::upsample_fancy(u_top, u_cur, width));
    uo_e.copy_from_slice(&super::vp8::upsample_fancy(u_cur, u_top, width));
    vo_o.copy_from_slice(&super::vp8::upsample_fancy(v_top, v_cur, width));
    vo_e.copy_from_slice(&super::vp8::upsample_fancy(v_cur, v_top, width));
}



/// SSSE3：处理 `t_base` 处的 8 个 u16（t[col-1..col+6]，后者可有填充），对 4 个连续
/// 中间列（col..col+3）各算一对输出 `out[(col)*2 .. col*2+8]`：
/// `even=(3*t[i]+t[i-1]+8)>>4`、`odd=(3*t[i]+t[i+1]+8)>>4`（全部正数，逻辑右移即对）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn upsample_fancy_sse2(t_base: *const u16, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    // t_base = &t[col-1]；载 8 个 u16（调用方保证 col+4<cw 且 t 尾填充 ≥8）。
    let w = _mm_loadu_si128(t_base as *const __m128i); // lanes: t[col-1..col+6]
    let three = _mm_set1_epi16(3);
    let bias8 = _mm_set1_epi16(8);

    // 四个中间列 mid = t[col..col+3]（lanes 1..4）。
    let mid = _mm_srli_si128(w, 2);
    // 左邻 left = t[col-1..col+2]（lanes 0..3）。
    let left = w;
    // 右邻 right = t[col+1..col+4]（lanes 2..5）。
    let right = _mm_srli_si128(w, 4);

    let even = _mm_srli_epi16(_mm_add_epi16(_mm_add_epi16(_mm_mullo_epi16(mid, three), left), bias8), 4);
    let odd = _mm_srli_epi16(_mm_add_epi16(_mm_add_epi16(_mm_mullo_epi16(mid, three), right), bias8), 4);

    // packus：u16（≤255）→ u8。偶数 in 字节 0..7、奇数 in 字节 8..15；shuffle 取每 16 位对的
    // 偶/奇字节交错成 [e0,o0,e1,o1,e2,o2,e3,o3]（mask 各输出位 p[0/8]、p[1/9]…）。
    let p = _mm_packus_epi16(even, odd);
    let shuf = _mm_set_epi8(-1, -1, -1, -1, -1, -1, -1, -1, 11, 3, 10, 2, 9, 1, 8, 0);
    let interleaved = _mm_shuffle_epi8(p, shuf);
    _mm_storel_epi64(out as *mut __m128i, interleaved);
    }
}

/// AVX2：16 对/步（C# `UpsampleFancyVector128Core` 的 Avx2 段同款）。`t_base=&t[pair]`，
/// 载 16 个 u16 current（t[pair..pair+16]）+ 左邻（pair-1）+ 右邻（pair+1）各 16 u16 →
/// `even=(3*cur+left+8)>>4`、`odd=(3*cur+right+8)>>4` → packus 分开偶/奇 → 逐 128-bit lane
/// shuffle 交错（每 lane 内 8 对、lanes 各持 pairs 0-7/8-15，天然按输出序）→ 32 字节输出。
/// 算术同 [`upsample_fancy_sse2`]（`mid*3+左/右邻+8 >>4`），逐字节等于两次 4 对 SSE2 十分之一
/// 组（调用方保证 `pair+17 ≤ cw`：右读 t[pair+16] ≤ t[cw-1]，数学恒正不越界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn upsample_fancy_avx2(t_base: *const u16, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let w = _mm256_loadu_si256(t_base as *const __m256i);        // t[pair..pair+16]
    let left = _mm256_loadu_si256(t_base.sub(1) as *const __m256i); // t[pair-1..pair+15]
    let right = _mm256_loadu_si256(t_base.add(1) as *const __m256i); // t[pair+1..pair+17]
    let three = _mm256_set1_epi16(3);
    let bias8 = _mm256_set1_epi16(8);

    let even = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(_mm256_mullo_epi16(w, three), left), bias8), 4);
    let odd = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(_mm256_mullo_epi16(w, three), right), bias8), 4);

    // packus：偶 16 列 → 低/高 128 位各 8 字节，奇 16 列 → 同样布局；每 lane 内 shuffle 把该
    // lane 的偶/奇字节交错成 [e0,o0,e1,o1,...]（每 lane 全 16 字节：0..15 完整逐对交错）。
    let p = _mm256_packus_epi16(even, odd);
    let shuf = _mm256_set_epi8(
        15, 7, 14, 6, 13, 5, 12, 4, 11, 3, 10, 2, 9, 1, 8, 0,
        15, 7, 14, 6, 13, 5, 12, 4, 11, 3, 10, 2, 9, 1, 8, 0,
    );
    let interleaved = _mm256_shuffle_epi8(p, shuf);
    _mm256_storeu_si256(out as *mut __m256i, interleaved);
    }
}
