#![allow(unsafe_code)]
//! 裸 unsafe 叶子子模块 `raw::predictor`：VP8 D1 帧内预测（原 `simd/predict.rs`）与 VP8L
//! 预测器 / 色变换逆变换（原 `simd/predictor.rs`）的全部 `#[target_feature]` SIMD 内核 +
//! 安全门面整体迁入。上层 `simd/predict.rs` / `simd/predictor.rs` 以 `#![forbid(unsafe_code)]`
//! + 薄委托显式依赖本模块。算法 / 常量 / 调用顺序与隔离前逐字节一致（字节恒等是硬约束）。

use super::super::*;

// ---------------------------------------------------------------------------
// D1：帧内预测向量化（SSSE3；C# `Vp8Decoder.PredictSubblockVector`/`PredictLarge*` 同款：
// 4×4 十模式经一个 16 字节 edge 向量 E=[L3,L3,L2,L1,L0,P,A0..A7,A7,A7] + avg2/avg3 向量 +
// 逐模式 pshufb 重排；16×16/8×8 整块按 V/H/TM/DC 分派）。向量与 vp8.rs 标量逐字节一致断言见模块底部。
// ---------------------------------------------------------------------------


/// 4×4 预测分派：SSSE3 向量核；无 SIMD → vp8.rs 标量（单一语义源）。
/// 休眠：产线走 `predict_subblock_forced`（pipeline.rs「不可再走 simd::predict_subblock」），仅测试/探针用。
pub(in super::super) fn predict_subblock(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    mode: u8,
    above: &[u8; 9],
) {
    #[cfg(target_arch = "x86_64")]
    if part_simd_on("pred", "ssse3") {
        unsafe { predict_subblock_ssse3(plane, pitch, row0, col0, mode, above) }
        return;
    }
    super::super::vp8::predict_subblock(plane, pitch, row0, col0, mode, above);
}

/// 预判定后的 4×4 预测核直调：`part_simd_on("pred","ssse3")` 已由调用方提升出子块循环
/// （该判定含 CPU 特性探测与多个 OnceLock，逐块调用在汇编上是真函数 call；判定结果在整帧
/// 解码内恒定，`reconstruct_macroblock` 每宏块取一次）。**Safety**：调用方须保证 ssse3
/// 可用（如先判定返回 true），且 `row0/col0/pitch` 在平面内——其余语义同
/// `predict_subblock` 的 SSSE3 分支，输出逐字节一致。
#[inline(always)]
pub(crate) unsafe fn predict_subblock_forced(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    mode: u8,
    above: &[u8; 9],
) {
    // x86_64：内核使用 x86-only intrinsic，仅在该目标编译（SSSE3 核指令可用性由调用方保证）。
    #[cfg(target_arch = "x86_64")]
    {
        unsafe { predict_subblock_ssse3(plane, pitch, row0, col0, mode, above) }
        return;
    }
    // 非 x86_64：x86 intrinsic 不存在，恒走标量兜底（运行时不达：调用方
    // `part_simd_on("pred","ssse3")` 在非 x86_64 恒为 false）。
    #[cfg(not(target_arch = "x86_64"))]
    {
        super::super::vp8::predict_subblock(plane, pitch, row0, col0, mode, above);
    }
}

#[cfg(target_arch = "x86_64")]
// Coder Iteration 4：`#[inline]`（`#[inline(always)]` 与 `#[target_feature]` 互斥，E0658）——
// 仅当调用方启用 ssse3 时 LLVM 才内联（非 ssse3 调用方受特征不符限制仍为 call，不变性不受影响），
// 使 `reconstruct_i4_mb_ssse3` 内本核内联、above 向量不再经栈往返（壳≈30%/AC 块 的主消除对象）。
#[inline]
#[target_feature(enable = "ssse3")]
unsafe fn predict_subblock_ssse3(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    mode: u8,
    above: &[u8; 9],
) {
    use std::arch::x86_64::*;
    unsafe {
    let base = plane.as_mut_ptr();
    // 左列 4 字节（列 col0-1，平面偏移 row*pitch + col0）。
    let l0 = *base.add(row0 * pitch + col0);
    let l1 = *base.add((row0 + 1) * pitch + col0);
    let l2 = *base.add((row0 + 2) * pitch + col0);
    let l3 = *base.add((row0 + 3) * pitch + col0);
    let a = above; // a[0]=P, a[1..8]=A0..A7
    let top_left = a[0];

    if mode == 0 {
        // DC：sum=4+Σabove[1..5]+Σleft，/8 落全块。
        let sum = 4u32 + u32::from(a[1]) + u32::from(a[2]) + u32::from(a[3]) + u32::from(a[4])
            + u32::from(l0) + u32::from(l1) + u32::from(l2) + u32::from(l3);
        let fill = ((sum >> 3) * 0x0101_0101) as u32;
        let out = base.add(row0 * pitch + 1 + col0);
        std::ptr::write_unaligned(out as *mut u32, fill);
        std::ptr::write_unaligned(out.add(pitch) as *mut u32, fill);
        std::ptr::write_unaligned(out.add(2 * pitch) as *mut u32, fill);
        std::ptr::write_unaligned(out.add(3 * pitch) as *mut u32, fill);
        return;
    }

    // E = [L3 L3 L2 L1 L0 P A0 A1 | A2 A3 A4 A5 A6 A7 A7 A7]。
    let lo = u64::from(l3) | (u64::from(l3) << 8) | (u64::from(l2) << 16) | (u64::from(l1) << 24)
        | (u64::from(l0) << 32) | (u64::from(top_left) << 40) | (u64::from(a[1]) << 48)
        | (u64::from(a[2]) << 56);
    let hi = u64::from(a[3]) | (u64::from(a[4]) << 8) | (u64::from(a[5]) << 16) | (u64::from(a[6]) << 24)
        | (u64::from(a[7]) << 32) | (u64::from(a[8]) << 40) | (u64::from(a[8]) << 48)
        | (u64::from(a[8]) << 56);
    let e = _mm_set_epi64x(hi as i64, lo as i64);

    let result;
    if mode == 1 {
        // TM：i16 lane `clamp(A[c] + L[r] - P)`，两行一组。
        let zero = _mm_setzero_si128();
        let max255 = _mm_set1_epi16(255);
        let tlp = _mm_set1_epi16(top_left as i16);
        let above_rep = _mm_shuffle_epi8(e, _mm_setr_epi8(6, 7, 8, 9, 6, 7, 8, 9, 6, 7, 8, 9, 6, 7, 8, 9));
        let left_rep = _mm_shuffle_epi8(e, _mm_setr_epi8(4, 4, 4, 4, 3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1));
        let alo = _mm_unpacklo_epi8(above_rep, zero);
        let ahi = _mm_unpackhi_epi8(above_rep, zero);
        let llo = _mm_unpacklo_epi8(left_rep, zero);
        let lhi = _mm_unpackhi_epi8(left_rep, zero);
        let s_lo = _mm_min_epi16(_mm_max_epi16(_mm_sub_epi16(_mm_add_epi16(alo, llo), tlp), zero), max255);
        let s_hi = _mm_min_epi16(_mm_max_epi16(_mm_sub_epi16(_mm_add_epi16(ahi, lhi), tlp), zero), max255);
        result = _mm_packus_epi16(s_lo, s_hi);
    } else {
        // 模式 2..=9：next=E[k+1]、next2=E[k+2]；avg2=avg(E,E+1)；avg3=avg(E+1, floor((E+E+2)/2))。
        let next = _mm_shuffle_epi8(e, _mm_setr_epi8(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 15));
        let next2 = _mm_shuffle_epi8(e, _mm_setr_epi8(2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 15, 15));
        let avg2v = _mm_avg_epu8(e, next);
        let mut outer = _mm_avg_epu8(e, next2);
        outer = _mm_sub_epi8(
            outer,
            _mm_and_si128(_mm_xor_si128(e, next2), _mm_set1_epi8(1)),
        );
        let avg3v = _mm_avg_epu8(next, outer);
        let off = (mode as usize - 2) * 32;
        let t = &SUBBLOCK_SHUFFLES[off..off + 32];
        let a3 = _mm_shuffle_epi8(avg3v, _mm_loadu_si128(t.as_ptr() as *const __m128i));
        let a2 = _mm_shuffle_epi8(avg2v, _mm_loadu_si128(t.as_ptr().add(16) as *const __m128i));
        result = _mm_or_si128(a3, a2);
    }

    // 结果行序 = 16 字节的 4 个 u32 lane（行 0..3），写 4 行各 4 字节。
    let out = base.add(row0 * pitch + 1 + col0);
    let r0 = _mm_cvtsi128_si32(result) as u32;
    let r1 = _mm_cvtsi128_si32(_mm_srli_si128(result, 4)) as u32;
    let r2 = _mm_cvtsi128_si32(_mm_srli_si128(result, 8)) as u32;
    let r3 = _mm_cvtsi128_si32(_mm_srli_si128(result, 12)) as u32;
    std::ptr::write_unaligned(out as *mut u32, r0);
    std::ptr::write_unaligned(out.add(pitch) as *mut u32, r1);
    std::ptr::write_unaligned(out.add(2 * pitch) as *mut u32, r2);
    std::ptr::write_unaligned(out.add(3 * pitch) as *mut u32, r3);
    }
}

/// 16×16/8×8 整块预测分派：V/H/DC/TM 四模式的整行广播；无 SIMD → 标量。
pub(in super::super) fn predict_large(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    size: usize,
    mode: u8,
    has_top: bool,
    has_left: bool,
    above: &[u8],
) {
    #[cfg(target_arch = "x86_64")]
    if part_simd_on("pred", "ssse3") {
        unsafe { predict_large_ssse3(plane, pitch, row0, col0, size, mode, has_top, has_left, above) }
        return;
    }
    super::super::vp8::predict_large(plane, pitch, row0, col0, size, mode, has_top, has_left, above);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn predict_large_ssse3(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    size: usize,
    mode: u8,
    has_top: bool,
    has_left: bool,
    above: &[u8],
) {
    use std::arch::x86_64::*;
    unsafe {
    let base = plane.as_mut_ptr();
    let out0 = base.add(row0 * pitch + 1 + col0);
    match mode {
        1 => {
            // V：整行 = 上一行（size 字节广播写 size 行）。
            if size == 16 {
                let row = _mm_loadu_si128(above[1..17].as_ptr() as *const __m128i);
                for r in 0..16 {
                    _mm_storeu_si128(out0.add(r * pitch) as *mut __m128i, row);
                }
            } else {
                let row = _mm_loadl_epi64(above[1..9].as_ptr() as *const __m128i);
                for r in 0..8 {
                    _mm_storel_epi64(out0.add(r * pitch) as *mut __m128i, row);
                }
            }
        }
        2 => {
            // H：每行填左列单字节。
            for r in 0..size {
                let v = *base.add((row0 + r) * pitch + col0);
                let row = _mm_set1_epi8(v as i8);
                if size == 16 {
                    _mm_storeu_si128(out0.add(r * pitch) as *mut __m128i, row);
                } else {
                    _mm_storel_epi64(out0.add(r * pitch) as *mut __m128i, row);
                }
            }
        }
        3 => {
            // TM：clamp(左 + 上 - 左上)，i16 lane。
            let tl = i16::from(above[0]);
            let zero = _mm_setzero_si128();
            let max255 = _mm_set1_epi16(255);
            let (alo, ahi) = if size == 16 {
                let v = _mm_loadu_si128(above[1..17].as_ptr() as *const __m128i);
                (_mm_unpacklo_epi8(v, zero), _mm_unpackhi_epi8(v, zero))
            } else {
                let v = _mm_loadl_epi64(above[1..9].as_ptr() as *const __m128i);
                (_mm_unpacklo_epi8(v, zero), _mm_setzero_si128())
            };
            for r in 0..size {
                let left = i32::from(*base.add((row0 + r) * pitch + col0));
                let delta = _mm_set1_epi16((left - i32::from(tl)) as i16);
                let s_lo = _mm_min_epi16(_mm_max_epi16(_mm_add_epi16(alo, delta), zero), max255);
                let s_hi = _mm_min_epi16(_mm_max_epi16(_mm_add_epi16(ahi, delta), zero), max255);
                let row = _mm_packus_epi16(s_lo, s_hi);
                if size == 16 {
                    _mm_storeu_si128(out0.add(r * pitch) as *mut __m128i, row);
                } else {
                    _mm_storel_epi64(out0.add(r * pitch) as *mut __m128i, row);
                }
            }
        }
        _ => {
            // DC：仅累计界内边缘。
            let mut sum = 0i32;
            if has_top {
                for i in 0..size {
                    sum += i32::from(above[1 + i]);
                }
            }
            if has_left {
                for i in 0..size {
                    sum += i32::from(*base.add((row0 + i) * pitch + col0));
                }
            }
            let value = if has_top && has_left {
                (sum + size as i32) / (2 * size as i32)
            } else if has_top || has_left {
                (sum + (size as i32) / 2) / size as i32
            } else {
                128
            } as u8;
            let row = _mm_set1_epi8(value as i8);
            for r in 0..size {
                if size == 16 {
                    _mm_storeu_si128(out0.add(r * pitch) as *mut __m128i, row);
                } else {
                    _mm_storel_epi64(out0.add(r * pitch) as *mut __m128i, row);
                }
            }
        }
    }
    }
}

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

// ---------------------------------------------------------------------------
// RGBA8 ↔ RGB8 通道重排（4→3 抽条 / 3→4 扩写，alpha 恒 255）——双转换去成本
// （K53 测量：to_rgba8 的 Rgb8 分支像素循环 22.7ms/8.6MP 是大头；SSSE3 扩写 ~6-13ms）。
// 字节恒等：ssse3 只做逐字节重排 + OR 常量 0xFF，不经任何算术。
// ---------------------------------------------------------------------------

/// RGBA8 → RGB8 抽条（4→3）：SSSE3 每 4 像素（16B 入 → 12B 出 = 8B 存 + 4B 存），
/// 尾像素标量（同现有 `chunks_exact` 语义）。无 SSSE3 → 全标量。输入长度须为 4 的倍数。
pub(in super::super) fn rgba8_to_rgb8(src: &[u8], dst: &mut [u8]) {
    debug_assert!(src.len() % 4 == 0);
    debug_assert_eq!(dst.len(), src.len() / 4 * 3);
    let n = src.len() / 4;
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    {
        if simd_on("ssse3") {
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            unsafe {
                while i + 4 <= n {
                    let v = _mm_loadu_si128(s.add(i * 4) as *const __m128i);
                    let out = _mm_shuffle_epi8(
                        v,
                        _mm_setr_epi8(0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 0, 0, 0, 0),
                    );
                    let o = d.add(i * 3);
                    _mm_storel_epi64(o as *mut __m128i, out);
                    let hi =
                        _mm_cvtsi128_si32(_mm_srli_si128(out, 8)) as u32;
                    std::ptr::write_unaligned(o.add(8) as *mut u32, hi);
                    i += 4;
                }
            }
        }
    }
    // 尾像素 + 无 SIMD 标量兜底（与既有 `chunks_exact(4) → copy_from_slice(&px[..3])` 同字节）。
    for (px, out) in src[i * 4..].chunks_exact(4).zip(dst[i * 3..].chunks_exact_mut(3)) {
        out.copy_from_slice(&px[..3]);
    }
}

/// RGB8 → RGBA8 扩写（3→4，alpha 恒 255）：SSSE3 每 4 像素（12B 读 → 16B 写；12B 读
/// 不越界：offset=(n-4)*3 恰 12B ≤ n*3），尾像素标量。无 SSSE3 → 全标量。
/// 与 `Pixel::to_rgba` 的 `[r,g,b,255]` 逐字节一致。
pub(in super::super) fn rgb8_to_rgba8(src: &[u8], dst: &mut [u8]) {
    debug_assert!(src.len() % 3 == 0);
    debug_assert_eq!(dst.len(), src.len() / 3 * 4);
    let n = src.len() / 3;
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    {
        if simd_on("ssse3") {
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            unsafe {
                while i + 4 <= n {
                    let lo = _mm_loadl_epi64(s.add(i * 3) as *const __m128i);
                    let hi32 = std::ptr::read_unaligned(s.add(i * 3 + 8) as *const u32) as i32;
                    let hi = _mm_cvtsi32_si128(hi32);
                    let data = _mm_unpacklo_epi64(lo, hi);
                    // pshufb 索引 0xFF（位 7 置位）→ 输出字节 0；OR 常量 0xFF 得 alpha=255。
                    let shuf = _mm_setr_epi8(0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1);
                    let alpha = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
                    let out = _mm_shuffle_epi8(data, shuf);
                    _mm_storeu_si128(
                        d.add(i * 4) as *mut __m128i,
                        _mm_or_si128(out, alpha),
                    );
                    i += 4;
                }
            }
        }
    }
    // 尾像素 + 无 SIMD 标量兜底（与既有 `[r,g,b,255]` 逐字节一致）。
    for (px, out) in src[i * 3..].chunks_exact(3).zip(dst[i * 4..].chunks_exact_mut(4)) {
        out[0] = px[0];
        out[1] = px[1];
        out[2] = px[2];
        out[3] = 255;
    }
}

// ---------------------------------------------------------------------------
// VP8L 预测器 / 色变换逆变换：块段分派 + SSE2/AVX2（字节==标量）
// 端口 libwebp `lossless_sse2.c` / `lossless_avx2.c` 批处理核（Average2 改 floor、
// ClampedAddSubtractHalf/Full、选择器 SAD 逐字节同）；行/段/末列边界语义严格对齐
// vp8l.rs 标量（标量版即字节正确蓝本，生成本文所有数值预期）。输入 u32 ARGB 小端序
// 内存 [B,G,R,A]×4 → 16 字节向量，`_mm_add_epi8` 即 `add_argb`（逐字节 mod 256）。
// 门控：`KIMG_WEBP_NO_SIMD=1` 一票否决（回退 vp8l.rs 整段标量）；预测器走 SSE2 核，
// 色变换优先 AVX2 8-px、回退 SSE4.1 4-px。
// ---------------------------------------------------------------------------

/// VP8L 预测器 SIMD 就绪（x86_64 且 SSE2 可用且 `KIMG_WEBP_NO_SIMD` 未关）。
pub(in super::super) fn vp8l_predictor_ready() -> bool {
    simd_on("sse2")
}

/// 段内恒定 mode 的单像素标量预测（内部像素：x>0 且 y>0，无边缘分支）。
/// 返回 `None` = mode 非法（>13，调用方整段回退 → vp8l.rs 产出结构化 Err）。
#[inline(always)]
pub(in super::super) fn pred_scalar_one(mode: u32, left: u32, top: u32, top_left: u32, top_right: u32) -> Option<u32> {
    use super::super::vp8l::{average, clamp_add, clamp_half, select, BLACK};
    Some(match mode {
        0 => BLACK,
        1 => left,
        2 => top,
        3 => top_right,
        4 => top_left,
        5 => average(average(left, top_right), top),
        6 => average(left, top_left),
        7 => average(left, top),
        8 => average(top_left, top),
        9 => average(top, top_right),
        10 => average(average(left, top_left), average(top, top_right)),
        11 => select(left, top, top_left),
        12 => clamp_add(left, top, top_left),
        13 => clamp_half(average(left, top), top_left),
        _ => return None,
    })
}

/// 整段恒定 mode 标量批处理（与 vp8l.rs 逐像素语义逐字节一致；含段首左侧读取）。
/// mode 非法（>13）时无操作（调用方已预检，不出现）。
#[inline]
fn pred_add_scalar(mode: u32, dst: *mut u32, upper: *const u32, n: usize) {
    if n == 0 {
        return;
    }
    unsafe {
        let mut left = *dst.sub(1);
        for i in 0..n {
            let s = *dst.add(i);
            let t = *upper.add(i);
            let tl = *upper.add(i).sub(1);
            let tr = *upper.add(i).add(1);
            let pred = pred_scalar_one(mode, left, t, tl, tr).expect("mode 已预检合法");
            let r = super::super::vp8l::add_argb(s, pred);
            *dst.add(i) = r;
            left = r;
        }
    }
}

/// 逐字节 floor 均值 `(a+b)>>1`（16 通道）：`pavgb - ((a^b)&1)`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn avg2_epu8(a: __m128i, b: __m128i) -> __m128i {
    let avg1 = _mm_avg_epu8(a, b);
    let one = _mm_and_si128(_mm_xor_si128(a, b), _mm_set1_epi8(1));
    _mm_sub_epi8(avg1, one)
}

/// 单像素 `select(a=top, b=left, c=top_left)`（模式 11，libwebp `Select_SSE2` 同款）：
/// pa=|T-TL|、pb=|L-TL|，pb<=pa → 取 a（top），否则取 b（left）——与 vp8l 标量
/// `dl<dt 取 left 否则 top` 逐位一致（相等时同取 top）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn select_u32(a: u32, b: u32, c: u32) -> u32 { unsafe {
    let zero = _mm_setzero_si128();
    let va = _mm_cvtsi32_si128(a as i32);
    let vb = _mm_cvtsi32_si128(b as i32);
    let vc = _mm_cvtsi32_si128(c as i32);
    let ac = _mm_or_si128(_mm_subs_epu8(va, vc), _mm_subs_epu8(vc, va));
    let bc = _mm_or_si128(_mm_subs_epu8(vb, vc), _mm_subs_epu8(vc, vb));
    let pa = _mm_unpacklo_epi8(ac, zero);
    let pb = _mm_unpacklo_epi8(bc, zero);
    let diff = _mm_sub_epi16(pb, pa);
    let mut out = [0i16; 8];
    _mm_storeu_si128(out.as_mut_ptr() as *mut __m128i, diff);
    let pa_minus_pb = out[0] + out[1] + out[2] + out[3];
    if pa_minus_pb <= 0 {
        a
    } else {
        b
    }
}}

/// 预测器段分派（mode 恒定，n 像素；`dst` 段首、`upper` 顶行、`left` 段首左侧已变换值）。
/// 运行时按 CPU 选核（AVX2 8 宽并行 → SSE2 4 宽 → 标量）；调用方已确保 sse2 在位。
/// 返回 false = mode 非法（>13，调用方整段回退标量）。
#[cfg(target_arch = "x86_64")]
fn predictor_segment(mode: u32, dst: *mut u32, upper: *const u32, n: usize, left: u32) -> bool {
    if mode > 13 {
        return false;
    }
    match mode {
        // 无 left 依赖：优先 AVX2 8 宽，回退 SSE2 4 宽
        // （0 黑 / 2 顶 / 3 顶右 / 4 顶左 / 8 均值(TL,T) / 9 均值(T,TR)）。
        0 | 2 | 3 | 4 | 8 | 9 => {
            if simd_on("avx2") {
                unsafe {
                    pred_add_parallel_avx2(mode, dst, upper, n);
                }
            } else {
                unsafe {
                    pred_add_parallel(mode, dst, upper, n);
                }
            }
        }
        // mode 1：left 前缀和（4 宽 log-step 扫描）。
        1 => unsafe {
            pred_add_prefix(dst, n, left);
        },
        // left 串行链：5/6/7/11/12/13 用「left 寄存器断代 + 整向量算」核
        // （5 avg3、6/7 avg2、11 select、12 clamp-add、13 clamp-half；add=1 条 `_mm_add_epi8`）。
        5 => unsafe {
            pred_add_avg3_serial(dst, upper, n, left);
        },
        6 | 7 => unsafe {
            pred_add_avg2_serial(mode, dst, upper, n, left);
        },
        11 => unsafe {
            pred_add_select_serial(dst, upper, n, left);
        },
        12 => unsafe {
            pred_add_clamp_add_serial(dst, upper, n, left);
        },
        13 => unsafe {
            pred_add_clamp_half_serial(dst, upper, n, left);
        },
        // 10 保持整段标量（命中占比低时可后补核）。
        _ => pred_add_scalar(mode, dst, upper, n),
    }
    true
}

/// AVX2 8 宽并行预测-加（模式 0/2/3/4/8/9；长剩余交回 SSE2 4 宽）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse2")]
unsafe fn pred_add_parallel_avx2(mode: u32, dst: *mut u32, upper: *const u32, n: usize) { unsafe {
    let black = _mm256_set1_epi32(0xff00_0000u32 as i32);
    let mut i = 0usize;
    while i + 8 <= n {
        let src = _mm256_loadu_si256(dst.add(i) as *const __m256i);
        let pred = match mode {
            0 => black,
            2 => _mm256_loadu_si256(upper.add(i) as *const __m256i),
            3 => _mm256_loadu_si256(upper.add(i).add(1) as *const __m256i),
            4 => _mm256_loadu_si256(upper.add(i).sub(1) as *const __m256i),
            8 => avg2_epu8_256(
                _mm256_loadu_si256(upper.add(i).sub(1) as *const __m256i),
                _mm256_loadu_si256(upper.add(i) as *const __m256i),
            ),
            9 => avg2_epu8_256(
                _mm256_loadu_si256(upper.add(i) as *const __m256i),
                _mm256_loadu_si256(upper.add(i).add(1) as *const __m256i),
            ),
            _ => unreachable!(),
        };
        _mm256_storeu_si256(
            dst.add(i) as *mut __m256i,
            _mm256_add_epi8(src, pred),
        );
        i += 8;
    }
    if i < n {
        pred_add_parallel(mode, dst.add(i), upper.add(i), n - i);
    }
}}

/// 256 位逐字节 floor 均值（AVX2）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse2")]
unsafe fn avg2_epu8_256(a: __m256i, b: __m256i) -> __m256i {
    let avg1 = _mm256_avg_epu8(a, b);
    let one = _mm256_and_si256(_mm256_xor_si256(a, b), _mm256_set1_epi8(1));
    _mm256_sub_epi8(avg1, one)
}

/// 4 宽并行预测-加（模式 0/2/3/4/8/9；n≥1，段首左侧已变换值 `left` 不参与预测）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_parallel(mode: u32, dst: *mut u32, upper: *const u32, n: usize) { unsafe {
    let black = _mm_set1_epi32(0xff00_0000u32 as i32);
    let mut i = 0usize;
    while i + 4 <= n {
        let src = _mm_loadu_si128(dst.add(i) as *const __m128i);
        let pred = match mode {
            0 => black,
            2 => _mm_loadu_si128(upper.add(i) as *const __m128i),
            3 => _mm_loadu_si128(upper.add(i).add(1) as *const __m128i),
            4 => _mm_loadu_si128(upper.add(i).sub(1) as *const __m128i),
            8 => avg2_epu8(
                _mm_loadu_si128(upper.add(i).sub(1) as *const __m128i),
                _mm_loadu_si128(upper.add(i) as *const __m128i),
            ),
            9 => avg2_epu8(
                _mm_loadu_si128(upper.add(i) as *const __m128i),
                _mm_loadu_si128(upper.add(i).add(1) as *const __m128i),
            ),
            _ => unreachable!(),
        };
        _mm_storeu_si128(dst.add(i) as *mut __m128i, _mm_add_epi8(src, pred));
        i += 4;
    }
    if i < n {
        // 标量尾（段首左侧自取 *dst.sub(1)；不越界：段在行内且 x≥1）。
        pred_add_scalar(mode, dst.add(i), upper.add(i), n - i);
    }
}}

/// mode 1 前缀和（log-step 扫描，4 宽；`left` = 段首左侧已变换值）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_prefix(dst: *mut u32, n: usize, left_init: u32) { unsafe {
    let mut i = 0usize;
    let mut prev = _mm_set1_epi32(left_init as i32);
    while i + 4 <= n {
        let src = _mm_loadu_si128(dst.add(i) as *const __m128i);
        let s0 = _mm_add_epi8(src, _mm_slli_si128(src, 4));
        let s1 = _mm_add_epi8(s0, _mm_slli_si128(s0, 8));
        let res = _mm_add_epi8(s1, prev);
        _mm_storeu_si128(dst.add(i) as *mut __m128i, res);
        prev = _mm_shuffle_epi32(res, 0xff);
        i += 4;
    }
    if i < n {
        let mut left = if i >= 4 { *dst.add(i - 1) } else { left_init };
        while i < n {
            let s = *dst.add(i);
            let r = super::super::vp8l::add_argb(s, left);
            *dst.add(i) = r;
            left = r;
            i += 1;
        }
    }
}}

/// left 串行链模式 6/7（avg2）：`left` 全程驻留向量寄存器，`add_argb` = `_mm_add_epi8`（1 条）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_avg2_serial(mode: u32, dst: *mut u32, upper: *const u32, n: usize, left_init: u32) { unsafe {
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        // mode 7 只读 T；mode 6 只读 TL（不必读 TR）。
        let o = if mode == 7 { *upper.add(i) } else { *upper.add(i).sub(1) };
        let pred = avg2_epu8(lv, _mm_cvtsi32_si128(o as i32));
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}}

/// left 串行链模式 5（avg3：avg(avg(L,TR),T)）：向量驻留。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_avg3_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) { unsafe {
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        let tv = _mm_cvtsi32_si128(*upper.add(i) as i32);
        let trv = _mm_cvtsi32_si128(*upper.add(i).add(1) as i32);
        let a1 = avg2_epu8(lv, trv);
        let pred = avg2_epu8(a1, tv);
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}}

/// left 串行链模式 11（select）：SAD 判定需横向求和 → 每像素提取标量做比较。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_select_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) { unsafe {
    let mut left = left_init;
    for i in 0..n {
        let s = *dst.add(i);
        let t = *upper.add(i);
        let tl = *upper.add(i).sub(1);
        let pred = select_u32(t, left, tl);
        let res = _mm_add_epi8(
            _mm_cvtsi32_si128(s as i32),
            _mm_cvtsi32_si128(pred as i32),
        );
        let r = _mm_cvtsi128_si32(res) as u32;
        *dst.add(i) = r;
        left = r;
    }
}}

/// left 串行链模式 12（clamp-add）：i16 lane `clamp(L+T-TL)`，`add`=1 条。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_clamp_add_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) { unsafe {
    let zero = _mm_setzero_si128();
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        let l16 = _mm_unpacklo_epi8(lv, zero);
        let t16 = _mm_unpacklo_epi8(_mm_cvtsi32_si128(*upper.add(i) as i32), zero);
        let tl16 = _mm_unpacklo_epi8(_mm_cvtsi32_si128(*upper.add(i).sub(1) as i32), zero);
        let all = _mm_sub_epi16(_mm_add_epi16(l16, t16), tl16);
        let pred = _mm_packus_epi16(all, zero);
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}}

/// left 串行链模式 13（clamp-half：clamp(avg(L,T) + (avg-TL)/2)）：向量驻留。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_clamp_half_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) { unsafe {
    let zero = _mm_setzero_si128();
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        let tv = _mm_cvtsi32_si128(*upper.add(i) as i32);
        let tlv = _mm_cvtsi32_si128(*upper.add(i).sub(1) as i32);
        let avgv = avg2_epu8(lv, tv);
        let am16 = _mm_unpacklo_epi8(avgv, zero);
        let t16 = _mm_unpacklo_epi8(tlv, zero);
        let a1 = _mm_sub_epi16(am16, t16);
        let bgt = _mm_srli_epi16(_mm_cmpgt_epi16(t16, am16), 15);
        let a2 = _mm_add_epi16(a1, bgt);
        let a3 = _mm_srai_epi16(a2, 1);
        let a4 = _mm_add_epi16(am16, a3);
        let pred = _mm_packus_epi16(a4, zero);
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}}

/// VP8L 预测器逆变换（整个图像；段分派见 `predictor_segment`）。`None` = 模式非法
/// 或 SIMD 未就绪 → 调用方回退 vp8l.rs 标量整段（非法模式产出结构化 Err，绝不半途改动像素）。
pub(in super::super) fn predictor_inverse(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) -> Option<()> {
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse2") {
        // 预检：全部 mode 合法（>13 不动像素直接回退）。
        for &d in data {
            if ((d >> 8) & 0xff) > 13 {
                return None;
            }
        }
        unsafe {
            predictor_inverse_sse2(pixels, width, height, bits, data);
        }
        return Some(());
    }
    None
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn predictor_inverse_sse2(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) { unsafe {
    if width == 0 || height == 0 {
        return;
    }
    let block_w = (width + (1usize << bits) - 1) >> bits;
    let p = pixels.as_mut_ptr();
    let bitsu = bits as usize;

    // y=0：首像素 = in+BLACK，其余 = left 前缀和（模式无关）。
    {
        let residual = *p;
        let r0 = super::super::vp8l::add_argb(residual, super::super::vp8l::BLACK);
        *p = r0;
        if width > 1 {
            predictor_segment(1, p.add(1), p, width - 1, r0);
        }
    }

    for y in 1..height {
        let row = y * width;
        let top = p.add(row - width);
        // x==0：预测 = top（模式无关）。
        let r0 = super::super::vp8l::add_argb(*p.add(row), *top);
        *p.add(row) = r0;
        if width <= 1 {
            continue;
        }
        // 块段分派 x∈[1, width-1)（末列单独标量处理，TR 卷绕语义同标量）。
        let yb = y >> bitsu;
        let mut x = 1usize;
        while x < width - 1 {
            let bx = x >> bitsu;
            let mode = (*data.get_unchecked(yb * block_w + bx) >> 8) & 0xff;
            let seg_end = (((bx + 1) << bitsu)).min(width);
            let run_end = seg_end.min(width - 1);
            if run_end <= x {
                break;
            }
            predictor_segment(mode, p.add(row + x), top.add(x), run_end - x, *p.add(row + x - 1));
            x = run_end;
        }
        // 末列（width-1）：TR 卷绕 = 本行首像素（已变换）。
        let i = row + width - 1;
        let bx = (width - 1) >> bitsu;
        let mode = (*data.get_unchecked(yb * block_w + bx) >> 8) & 0xff;
        let left = *p.add(i - 1);
        let t = *p.add(i - width);
        let tl = *p.add(i - width - 1);
        let tr = *p.add(row); // 本行首像素
        if let Some(pred) = pred_scalar_one(mode, left, t, tl, tr) {
            *p.add(i) = super::super::vp8l::add_argb(*p.add(i), pred);
        }
    }
}}

// ---------------------------------------------------------------------------
// VP8L 色变换逆变换（`Vector<int>` lane 算术：绿→红→蓝→α 顺序与 `color_delta` 标量
// `(i8*t * i8*g)>>5` 逐位一致）。AVX2 8 宽主、SSE4.1 4 宽回退、尾列标量。
// ---------------------------------------------------------------------------

/// 块内恒定 cte 的整段标量色变换（与 vp8l.rs 标量逐字节一致）。
#[inline]
unsafe fn color_run_scalar(dst: *mut u32, n: usize, cte: u32) { unsafe {
    for i in 0..n {
        let p = *dst.add(i);
        let green = (p >> 8) & 0xff;
        let red = (p >> 16) & 0xff;
        let blue = p & 0xff;
        let red_delta = super::super::vp8l::color_delta(cte & 0xff, green);
        let blue_delta = super::super::vp8l::color_delta((cte >> 8) & 0xff, green);
        let red_for_blue = (red as i32 + red_delta) & 0xff;
        let new_blue = (blue as i32 + blue_delta
            + super::super::vp8l::color_delta((cte >> 16) & 0xff, red_for_blue as u32))
            & 0xff;
        *dst.add(i) = (p & 0xff00_ff00) | ((red_for_blue as u32) << 16) | (new_blue as u32);
    }
}}

/// SSE4.1 4 宽色变换核。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn color_run_sse2(dst: *mut u32, n: usize, cte: u32) { unsafe {
    let cr = _mm_set1_epi32((cte as u8 as i8) as i32);
    let cb = _mm_set1_epi32(((cte >> 8) as u8 as i8) as i32);
    let cb2 = _mm_set1_epi32(((cte >> 16) as u8 as i8) as i32);
    let m255 = _mm_set1_epi32(0xff);
    let m_ag = _mm_set1_epi32(0xff00_ff00u32 as i32);
    let mut i = 0usize;
    while i + 4 <= n {
        let vp = _mm_loadu_si128(dst.add(i) as *const __m128i);
        let g = _mm_and_si128(_mm_srai_epi32(vp, 8), m255);
        let gs = _mm_srai_epi32(_mm_slli_epi32(g, 24), 24);
        let r8 = _mm_and_si128(_mm_srai_epi32(vp, 16), m255);
        let b8 = _mm_and_si128(vp, m255);
        let rd = _mm_srai_epi32(_mm_mullo_epi32(gs, cr), 5);
        let rp = _mm_and_si128(_mm_add_epi32(r8, rd), m255);
        let rs = _mm_srai_epi32(_mm_slli_epi32(rp, 24), 24);
        let bd1 = _mm_srai_epi32(_mm_mullo_epi32(gs, cb), 5);
        let bd2 = _mm_srai_epi32(_mm_mullo_epi32(rs, cb2), 5);
        let bp = _mm_and_si128(_mm_add_epi32(_mm_add_epi32(b8, bd1), bd2), m255);
        let out = _mm_or_si128(
            _mm_and_si128(vp, m_ag),
            _mm_or_si128(_mm_slli_epi32(rp, 16), bp),
        );
        _mm_storeu_si128(dst.add(i) as *mut __m128i, out);
        i += 4;
    }
    if i < n {
        color_run_scalar(dst.add(i), n - i, cte);
    }
}}

/// AVX2 8 宽色变换核（长剩余交回 4 宽 SSE4.1）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse4.1")]
unsafe fn color_run_avx2(dst: *mut u32, n: usize, cte: u32) { unsafe {
    let cr = _mm256_set1_epi32((cte as u8 as i8) as i32);
    let cb = _mm256_set1_epi32(((cte >> 8) as u8 as i8) as i32);
    let cb2 = _mm256_set1_epi32(((cte >> 16) as u8 as i8) as i32);
    let m255 = _mm256_set1_epi32(0xff);
    let m_ag = _mm256_set1_epi32(0xff00_ff00u32 as i32);
    let mut i = 0usize;
    while i + 8 <= n {
        let vp = _mm256_loadu_si256(dst.add(i) as *const __m256i);
        let g = _mm256_and_si256(_mm256_srai_epi32(vp, 8), m255);
        let gs = _mm256_srai_epi32(_mm256_slli_epi32(g, 24), 24);
        let r8 = _mm256_and_si256(_mm256_srai_epi32(vp, 16), m255);
        let b8 = _mm256_and_si256(vp, m255);
        let rd = _mm256_srai_epi32(_mm256_mullo_epi32(gs, cr), 5);
        let rp = _mm256_and_si256(_mm256_add_epi32(r8, rd), m255);
        let rs = _mm256_srai_epi32(_mm256_slli_epi32(rp, 24), 24);
        let bd1 = _mm256_srai_epi32(_mm256_mullo_epi32(gs, cb), 5);
        let bd2 = _mm256_srai_epi32(_mm256_mullo_epi32(rs, cb2), 5);
        let bp = _mm256_and_si256(_mm256_add_epi32(_mm256_add_epi32(b8, bd1), bd2), m255);
        let out = _mm256_or_si256(
            _mm256_and_si256(vp, m_ag),
            _mm256_or_si256(_mm256_slli_epi32(rp, 16), bp),
        );
        _mm256_storeu_si256(dst.add(i) as *mut __m256i, out);
        i += 8;
    }
    if i < n {
        color_run_sse2(dst.add(i), n - i, cte);
    }
}}

/// VP8L 色变换逆变换（整个图像；`bits` 块位宽、`data` 块乘数网格）。返回 false = SIMD
/// 未就绪（调用方回退 vp8l.rs 标量整段）。
pub(in super::super) fn color_transform_inverse(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) -> bool {
    let (avx2, sse4) = (simd_on("avx2"), simd_on("sse4.1"));
    if !avx2 && !sse4 {
        return false;
    }
    if width == 0 || height == 0 {
        return true;
    }
    let block_w = (width + (1usize << bits) - 1) >> bits;
    let bitsu = bits as usize;
    let d = data.as_ptr();
    let px = pixels.as_mut_ptr();
    for y in 0..height {
        let row = y * width;
        for bx in 0..block_w {
            let cte = unsafe { *d.add((y >> bitsu) * block_w + bx) };
            let start = bx << bitsu;
            let n = (((bx + 1) << bitsu).min(width)) - start;
            if n == 0 {
                continue;
            }
            let dst = unsafe { px.add(row + start) };
            unsafe {
                if avx2 {
                    color_run_avx2(dst, n, cte);
                } else {
                    color_run_sse2(dst, n, cte);
                }
            }
        }
    }
    true
}
