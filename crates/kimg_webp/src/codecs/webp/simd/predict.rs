use super::*;

// ---------------------------------------------------------------------------
// D1：帧内预测向量化（SSSE3；C# `Vp8Decoder.PredictSubblockVector`/`PredictLarge*` 同款：
// 4×4 十模式经一个 16 字节 edge 向量 E=[L3,L3,L2,L1,L0,P,A0..A7,A7,A7] + avg2/avg3 向量 +
// 逐模式 pshufb 重排；16×16/8×8 整块按 V/H/TM/DC 分派）。向量与 vp8.rs 标量逐字节一致断言见模块底部。
// ---------------------------------------------------------------------------


/// 4×4 预测分派：SSSE3 向量核；无 SIMD → vp8.rs 标量（单一语义源）。
/// 休眠：产线走 `predict_subblock_forced`（pipeline.rs「不可再走 simd::predict_subblock」），仅测试/探针用。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn predict_subblock(
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
    super::vp8::predict_subblock(plane, pitch, row0, col0, mode, above);
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
        super::vp8::predict_subblock(plane, pitch, row0, col0, mode, above);
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
pub(crate) fn predict_large(
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
    super::vp8::predict_large(plane, pitch, row0, col0, size, mode, has_top, has_left, above);
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

