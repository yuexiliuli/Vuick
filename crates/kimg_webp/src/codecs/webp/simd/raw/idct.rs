#![allow(unsafe_code)]
//! 裸 unsafe 叶子子模块 `raw::idct`：VP8 DCT 逆变换（两遍 butterfly / i16±i32 域 pass2）/ DC
//! 捷径（双块 + 单块）的全部 SSE2 SIMD 内核 + 分派门面整体迁入（原 `simd/idct.rs`）。
//! 上层 `simd/idct.rs` 以 `#![forbid(unsafe_code)]` + 薄委托显式依赖本模块。算法 / 常量 /
//! 调用顺序与隔离前逐字节一致（字节恒等是硬约束，禁改算术）。

use super::super::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

// 算术：RFC 6386 §14.4 与 vp8.rs `inverse_dct_short` 完全同序——用 k1=20091 / k2=-30068 的
// i16-mulhi 技巧（K = k + 65536，故 (x*K)>>16 = (x*k)>>16 + x，见 libwebp dec_sse2.c 注释），
// 两个 4×4 块（水平相邻）各一行装进一个 128-bit 寄存器做并行 butterfly；中间 i16 回绕与标量
// `as i16` 逐字节一致；末段 (x+4)>>3 用先加 4 再 srai_epi16（=算术右移，等价标量整除方向）。
// 第二遍默认走 **i16 域 pass2**（add-4-then-srai，逐 op 对齐 libwebp `Transform_SSE2`，可达域
// ∥c∥≤2212 与标量逐字节一致）；`KIMG_WEBP_IDCT_I16=0` 强制旧 i32 域 pass2（K93 基线，全输入域
// 恒等）。字节==标量唯一标准；随机 parity 断言见模块底部。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn idct_add_two_sse2(
    coeff_a: *const i16,
    coeff_b: *const i16,
    dst: *mut u8,
    stride: isize,
    i16_en: bool,
) {
    use std::arch::x86_64::*;
    unsafe {
        let k1 = _mm_set1_epi16(20091);
        let k2 = _mm_set1_epi16(-30068);
        // 各读 4 个 i16（低 64 位），与第二块同排拼成 8 个 i16（两指针独立取数，无连续缓冲拷贝）。
        let la = |o: usize| _mm_loadl_epi64(coeff_a.add(o) as *const __m128i);
        let lb = |o: usize| _mm_loadl_epi64(coeff_b.add(o) as *const __m128i);
        let in0 = _mm_unpacklo_epi64(la(0), lb(0));
        let in1 = _mm_unpacklo_epi64(la(4), lb(4));
        let in2 = _mm_unpacklo_epi64(la(8), lb(8));
        let in3 = _mm_unpacklo_epi64(la(12), lb(12));
        // 第一遍（行混合）。
        let a = _mm_add_epi16(in0, in2);
        let b = _mm_sub_epi16(in0, in2);
        let c = _mm_add_epi16(
            _mm_sub_epi16(_mm_mulhi_epi16(in1, k2), _mm_mulhi_epi16(in3, k1)),
            _mm_sub_epi16(in1, in3),
        );
        let d = _mm_add_epi16(
            _mm_add_epi16(_mm_mulhi_epi16(in1, k1), _mm_mulhi_epi16(in3, k2)),
            _mm_add_epi16(in1, in3),
        );
        let tmp0 = _mm_add_epi16(a, d);
        let tmp1 = _mm_add_epi16(b, c);
        let tmp2 = _mm_sub_epi16(b, c);
        let tmp3 = _mm_sub_epi16(a, d);
        // 转置。
        let (t0, t1, t2, t3) = transpose_2_4x4_16b_sse2(tmp0, tmp1, tmp2, tmp3);
        // 第二遍：默认 i16 域 pass2（add-4-then-srai，逐 op 对齐 libwebp Transform_SSE2，可达域
        // ∥c∥≤2212 与标量恒等）；`KIMG_WEBP_IDCT_I16=0`（或测试 override）强制旧 i32 域 pass2
        //（全输入域与标量恒等，K93 基线 A/B 侧）。
        let (s0, s1, s2, s3) = if i16_en {
            idct_pass2_shift16_sse2(t0, t1, t2, t3)
        } else {
            // 高位乘项：`mh16(x,k2)`=`(x*(-30068))>>16`=标量 `(x*35468)>>16` - x（-30068≡35468 mod 2^16），
            // 故 `(x*35468)>>16 = mh16(x,k2) + x`；`mh16(x,k1)` 即标量 `(x*20091)>>16`。各 i16 项
            // 符号扩展到 i32 后相加，恒等标量两遍 butterfly 的 i32 中间量（值域容 i32，不溢出）。
            let m1k2 = _mm_mulhi_epi16(t1, k2); // (t1*35468)>>16 - t1
            let m3k1 = _mm_mulhi_epi16(t3, k1); // (t3*20091)>>16
            let m12 = _mm_mulhi_epi16(t1, k1); // (t1*20091)>>16
            let m32 = _mm_mulhi_epi16(t3, k2); // (t3*35468)>>16 - t3
            // 拆 A/B 两半并 i16→i32 符号扩展（interleave 各半，保持两块同构独立计算）。
            let sign_ext = |x: __m128i| {
                let sm = _mm_srai_epi16(x, 15);
                (_mm_unpacklo_epi16(x, sm), _mm_unpackhi_epi16(x, sm))
            };
            let (t0a, t0b) = sign_ext(t0);
            let (t1a, t1b) = sign_ext(t1);
            let (t2a, t2b) = sign_ext(t2);
            let (t3a, t3b) = sign_ext(t3);
            let (m1k2a, m1k2b) = sign_ext(m1k2);
            let (m3k1a, m3k1b) = sign_ext(m3k1);
            let (m12a, m12b) = sign_ext(m12);
            let (m32a, m32b) = sign_ext(m32);
            let (sa0, sa1, sa2, sa3) =
                idct_pass2_shift32_sse2(t0a, t1a, t2a, t3a, m1k2a, m3k1a, m12a, m32a);
            let (sb0, sb1, sb2, sb3) =
                idct_pass2_shift32_sse2(t0b, t1b, t2b, t3b, m1k2b, m3k1b, m12b, m32b);
            // 打包回 i16（(x+4)>>3 结果恒落 i16 域，饱和=精确）并交还 A/B 组合布局。
            let pack_ab = |a: __m128i, b: __m128i| {
                let z = _mm_setzero_si128();
                _mm_unpacklo_epi64(_mm_packs_epi32(a, z), _mm_packs_epi32(b, z))
            };
            (
                pack_ab(sa0, sb0),
                pack_ab(sa1, sb1),
                pack_ab(sa2, sb2),
                pack_ab(sa3, sb3),
            )
        };
        let (r0, r1, r2, r3) = transpose_2_4x4_16b_sse2(s0, s1, s2, s3);
        // 加回像素并 clamp [0,255]（packus）再存 8 字节每行。
        let zero = _mm_setzero_si128();
        let load_row = |o: isize| _mm_loadl_epi64(dst.offset(o) as *const __m128i);
        let store_row = |s: __m128i, o: isize| _mm_storel_epi64(dst.offset(o) as *mut __m128i, s);
        store_row(_mm_packus_epi16(_mm_add_epi16(_mm_unpacklo_epi8(load_row(0), zero), r0), s0), 0);
        store_row(_mm_packus_epi16(_mm_add_epi16(_mm_unpacklo_epi8(load_row(stride), zero), r1), s1), stride);
        store_row(
            _mm_packus_epi16(_mm_add_epi16(_mm_unpacklo_epi8(load_row(2 * stride), zero), r2), s2),
            2 * stride,
        );
        store_row(
            _mm_packus_epi16(_mm_add_epi16(_mm_unpacklo_epi8(load_row(3 * stride), zero), r3), s3),
            3 * stride,
        );
    }
}

/// 单个 4×4 块的逆 DCT + 残差写回（1-block SSE2，字节==既有标量）。
/// 与 `idct_add_two_sse2` 共用同一套两遍 butterfly + i16 域 pass2（K93 双路径：默认 i16、
/// `KIMG_WEBP_IDCT_I16=0` 走 i32 基线），但 B 块不拼装
/// （无配对/合批）：i4x4 块间存在重建像素串行依赖（块 b 的左上下文读块 a 的「重建后」像素，
/// 必须先 predict b 再逐块 add，故禁止 2 块合批——S7/K15 同版约束）。末段加回像素并 clamp
/// [0,255] 只落低 4 像素/行（严格 4 字节读写，右缘块不越出平面右端，同 `dc_add_two_sse2` 姿势）。
#[cfg(target_arch = "x86_64")]
// Coder Iteration 4：`#[inline]`（`#[inline(always)]` 与 `#[target_feature]` 互斥，E0658）——
// sse2 ⊆ ssse3，使 `reconstruct_i4_mb_ssse3` 内本核内联（非 sse2 调用方受特征限制仍为 call）；
// 消 i4 每块真 `callq` 与系数栈往返。
#[inline]
#[target_feature(enable = "sse2")]
unsafe fn idct_add_one_sse2(coeff: *const i16, dst: *mut u8, stride: isize, i16_en: bool) {
    use std::arch::x86_64::*;
    unsafe {
        let k1 = _mm_set1_epi16(20091);
        let k2 = _mm_set1_epi16(-30068);
        let z = _mm_setzero_si128();
        // 四行系数各读 4 个 i16（低 64）。
        let in0 = _mm_loadl_epi64(coeff.add(0) as *const __m128i);
        let in1 = _mm_loadl_epi64(coeff.add(4) as *const __m128i);
        let in2 = _mm_loadl_epi64(coeff.add(8) as *const __m128i);
        let in3 = _mm_loadl_epi64(coeff.add(12) as *const __m128i);
        // 第一遍（行混合）：与 `idct_add_two_sse2` 同序（高 64 恒 0，仅 A 半边载值）。
        let a = _mm_add_epi16(in0, in2);
        let b = _mm_sub_epi16(in0, in2);
        let c = _mm_add_epi16(
            _mm_sub_epi16(_mm_mulhi_epi16(in1, k2), _mm_mulhi_epi16(in3, k1)),
            _mm_sub_epi16(in1, in3),
        );
        let d = _mm_add_epi16(
            _mm_add_epi16(_mm_mulhi_epi16(in1, k1), _mm_mulhi_epi16(in3, k2)),
            _mm_add_epi16(in1, in3),
        );
        let tmp0 = _mm_add_epi16(a, d);
        let tmp1 = _mm_add_epi16(b, c);
        let tmp2 = _mm_sub_epi16(b, c);
        let tmp3 = _mm_sub_epi16(a, d);
        // 转置：返回各寄存器低 64 = 列 i（4 行），高 64 恒 0。
        let (t0, t1, t2, t3) = transpose_2_4x4_16b_sse2(tmp0, tmp1, tmp2, tmp3);
        // 第二遍：默认 i16 域 pass2（8-lane，A 低 64 / B 高 64 恒 0 → 仅低 4 列携带有效值，与
        // `idct_add_two_sse2` 同算术）；`KIMG_WEBP_IDCT_I16=0` 强制旧 i32 域 pass2（基线，全幅恒等）。
        let (s0, s1, s2, s3) = if i16_en {
            idct_pass2_shift16_sse2(t0, t1, t2, t3)
        } else {
            // 只对低 64（A 半边）符号扩展做 i32 pass2（高位乘项同理，高半恒 0）。
            let sign_ext = |x: __m128i| _mm_unpacklo_epi16(x, _mm_srai_epi16(x, 15));
            let m1k2 = sign_ext(_mm_mulhi_epi16(t1, k2));
            let m3k1 = sign_ext(_mm_mulhi_epi16(t3, k1));
            let m12 = sign_ext(_mm_mulhi_epi16(t1, k1));
            let m32 = sign_ext(_mm_mulhi_epi16(t3, k2));
            let (s0, s1, s2, s3) = idct_pass2_shift32_sse2(
                sign_ext(t0),
                sign_ext(t1),
                sign_ext(t2),
                sign_ext(t3),
                m1k2,
                m3k1,
                m12,
                m32,
            );
            // 打包回 i16（列布局，高 64 恒 0）。
            (
                _mm_packs_epi32(s0, z),
                _mm_packs_epi32(s1, z),
                _mm_packs_epi32(s2, z),
                _mm_packs_epi32(s3, z),
            )
        };
        // 转置回行布局（r_i = 行 i 残差 4 i16，高 64 恒 0）。
        let (r0, r1, r2, r3) = transpose_2_4x4_16b_sse2(s0, s1, s2, s3);
        // 每行载 4 像素（零扩展）、加残差、packus clamp、落 4 字节（右缘不越界）。
        let load_row = |o: isize| {
            _mm_unpacklo_epi8(_mm_cvtsi32_si128(std::ptr::read_unaligned(dst.offset(o) as *const u32) as i32), z)
        };
        let store_row = |s: __m128i, o: isize| {
            let px = _mm_packus_epi16(_mm_add_epi16(load_row(o), s), z);
            std::ptr::write_unaligned(dst.offset(o) as *mut u32, _mm_cvtsi128_si32(px) as u32);
        };
        store_row(r0, 0);
        store_row(r1, stride);
        store_row(r2, 2 * stride);
        store_row(r3, 3 * stride);
    }
}

/// 两个水平相邻 4×4 块的逆 DCT + 残差写回（字节==既有标量）。
/// `coeff_a`/`coeff_b` 为 [i16;16]，`col0` 为块 `a` 的左上列（帧坐标），块 `b` 在 `col0+4`。
/// 标量回退：逐块走 `vp8::inverse_dct_add`（恒以 has_ac=true —— 对全零块/仅 DC 块结果与
/// 既有短路逐字节一致，见 K 注释：零块残差为 0、DC-only 全 16 像素 (dc+4)>>3 同恒等式）。
/// 休眠：已被 `inverse_dct_add_two_forced` 取代（预判定直调），仅测试/探针用。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn inverse_dct_add_two(
    coeff_a: &[i16; 16],
    coeff_b: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
) {
    #[cfg(target_arch = "x86_64")]
    if part_simd_on("dct", "sse2") {
        // 两个入参是分离数组：内核直接按两指针各取 4 个 i16 拼装，不再拷 32 个 i16 连续缓冲
        // （原先的 `buf` 拷贝是每次调用 64B 的纯开销；两指针加载寄存器内容逐字节相同）。
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            idct_add_two_sse2(coeff_a.as_ptr(), coeff_b.as_ptr(), dst, pitch as isize, idct_i16_enabled());
        }
        return;
    }
    super::vp8::inverse_dct_add(coeff_a, plane, pitch, row0, col0, true);
    super::vp8::inverse_dct_add(coeff_b, plane, pitch, row0, col0 + 4, true);
}

/// 预判定后的双块逆变换核直调：`part_simd_on("dct","sse2")` 已由调用方提升出 i16/UV 对循环
/// （同 `idct_add_one_forced` 姿势，避免逐对特性判定 call）；`i16_en` 为调用方提升出循环的
/// `idct_i16_enabled()` 结果（K93 双路径，值在整帧解码内恒定），透传内核省去逐对 OnceLock
/// 读取 + 分支。**Safety**：调用方须保证 sse2
/// 可用（如先判定返回 true），越界语义同 `inverse_dct_add_two` 的 SSE2 分支（`row0/col0`
/// 落在平面内）。
#[inline]
pub(crate) unsafe fn inverse_dct_add_two_forced(
    coeff_a: &[i16; 16],
    coeff_b: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    i16_en: bool,
) {
    // x86_64：内核使用 x86-only intrinsic，仅在该目标编译（SSE2 核指令可用性由调用方保证）。
    #[cfg(target_arch = "x86_64")]
    {
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            idct_add_two_sse2(coeff_a.as_ptr(), coeff_b.as_ptr(), dst, pitch as isize, i16_en);
        }
        return;
    }
    // 非 x86_64：x86 intrinsic 不存在，恒走标量全变换兜底（运行时不达：调用方
    // `part_simd_on("dct","sse2")` 在非 x86_64 恒为 false）。
    #[cfg(not(target_arch = "x86_64"))]
    {
        super::vp8::inverse_dct_add(coeff_a, plane, pitch, row0, col0, true);
        super::vp8::inverse_dct_add(coeff_b, plane, pitch, row0, col0 + 4, true);
    }
}

/// 单个 4×4 块的逆 DCT + 残差写回（i4x4 用，字节==既有标量）。
/// `coeff` 为 [i16;16]。SIMD 走单块核 `idct_add_one_sse2`（与 i16 的 DCT 部件同门控
/// `part_simd_on("dct","sse2")`）；标量回退恒 has_ac=true（对仅 DC 块与既有捷径逐字节一致，
/// 见 `inverse_dct_has_ac_shortcut_equivalence`）。调用方在 AC 位为准时进入本入口，DC 捷径仍走标量。
/// 休眠：已被 `idct_add_one_forced` 取代，仅测试用（test 引用）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn inverse_dct_add_one(
    coeff: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
) {
    #[cfg(target_arch = "x86_64")]
    if part_simd_on("dct", "sse2") {
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            idct_add_one_sse2(coeff.as_ptr(), dst, pitch as isize, idct_i16_enabled());
        }
        return;
    }
    super::vp8::inverse_dct_add(coeff, plane, pitch, row0, col0, true);
}

/// 预判定后的单块逆变换核直调：`part_simd_on("dct","sse2")` 已由调用方提升出 i4 子块循环
/// （同 `predict_subblock_forced` 姿势，避免逐块特性判定 call）；`i16_en` 为调用方提升出循环的
/// `idct_i16_enabled()` 结果（K93 双路径，值在整帧解码内恒定），透传内核省去逐块 OnceLock
/// 读取 + 分支。**Safety**：调用方须保证
/// sse2 可用（如先判定返回 true），越界语义同 `inverse_dct_add_one` 的 SSE2 分支（`row0/col0`
/// 落在平面内）；仅 DC（无 AC）块亦走本核——`idct_add_one_sse2` 对仅 DC 块与标量 DC 捷径
/// 逐字节一致（`inverse_dct_has_ac_shortcut_equivalence`）。
#[inline(always)]
pub(crate) unsafe fn idct_add_one_forced(
    coeff: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    i16_en: bool,
) {
    // x86_64：内核使用 x86-only intrinsic，仅在该目标编译（SSE2 核指令可用性由调用方保证）。
    #[cfg(target_arch = "x86_64")]
    {
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            idct_add_one_sse2(coeff.as_ptr(), dst, pitch as isize, i16_en);
        }
        return;
    }
    // 非 x86_64：x86 intrinsic 不存在，恒走标量全变换兜底（运行时不达：调用方
    // `part_simd_on("dct","sse2")` 在非 x86_64 恒为 false）。
    #[cfg(not(target_arch = "x86_64"))]
    {
        super::vp8::inverse_dct_add(coeff, plane, pitch, row0, col0, true);
    }
}

/// 两个水平相邻 4×4 块仅 DC（无 AC）的逆变换捷径：残差恒 `(dc+4)>>3`（与完整两遍变换对
/// 仅 DC 块逐字节一致，既有 `inverse_dct_has_ac_shortcut_equivalence` 已证）。SSE2 下省去
/// 两遍 butterfly，直接 16 像素加 DC；无 SIMD 回退逐块 `vp8::inverse_dct_add(has_ac=true)`
/// （对 [dc,0..0] 块与捷径同输出）。调用方以 `ac_mask` 双块位全 0 判定后进入本路径。
/// 休眠：已被 `dc_add_two_forced` 取代，仅测试用（test 引用）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn dc_add_two(
    dc_a: i16,
    dc_b: i16,
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
) {
    #[cfg(target_arch = "x86_64")]
    if part_simd_on("dct", "sse2") {
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            dc_add_two_sse2(dc_a, dc_b, dst, pitch as isize);
        }
        return;
    }
    let mut ca = [0i16; 16];
    let mut cb = [0i16; 16];
    ca[0] = dc_a;
    cb[0] = dc_b;
    super::vp8::inverse_dct_add(&ca, plane, pitch, row0, col0, true);
    super::vp8::inverse_dct_add(&cb, plane, pitch, row0, col0 + 4, true);
}

/// 预判定后的双块 DC 捷径核直调：`part_simd_on("dct","sse2")` 已由调用方提升出 i16/UV 对循环
/// （同 `dc_add_one_forced` 姿势）。**Safety**：调用方须保证 sse2 可用，且该对块确为纯 DC
/// （调用方按 `ac_mask` 双块位 0 分发）；越界语义同 `dc_add_two`（`row0/col0` 落在平面内）。
#[inline]
pub(crate) unsafe fn dc_add_two_forced(
    dc_a: i16,
    dc_b: i16,
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
) {
    // x86_64：内核使用 x86-only intrinsic，仅在该目标编译（SSE2 核指令可用性由调用方保证）。
    #[cfg(target_arch = "x86_64")]
    {
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            dc_add_two_sse2(dc_a, dc_b, dst, pitch as isize);
        }
        return;
    }
    // 非 x86_64：x86 intrinsic 不存在，恒走标量全变换兜底（运行时不达：调用方
    // `part_simd_on("dct","sse2")` 在非 x86_64 恒为 false）。
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut ca = [0i16; 16];
        let mut cb = [0i16; 16];
        ca[0] = dc_a;
        cb[0] = dc_b;
        super::vp8::inverse_dct_add(&ca, plane, pitch, row0, col0, true);
        super::vp8::inverse_dct_add(&cb, plane, pitch, row0, col0 + 4, true);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn dc_add_two_sse2(dc_a: i16, dc_b: i16, dst: *mut u8, stride: isize) {
    use std::arch::x86_64::*;
    unsafe {
        // 残差 = (dc+4)>>3（i32 算术再截 i16；值域 [-4095,4095] 恒落 i16，与标量 `as i16` 相等）。
        let da = _mm_set1_epi16(((i32::from(dc_a) + 4) >> 3) as i16);
        let db = _mm_set1_epi16(((i32::from(dc_b) + 4) >> 3) as i16);
        let zero = _mm_setzero_si128();
        for r in 0..4isize {
            let o = r * stride;
            // 每行载 4 像素（块 a 在 dst，块 b 在 dst+4）。用严格 4 字节非对齐读写：
            // 右缘块 b 的 +4..+8 超出平面右端（pitch=cw+1），不能用 loadl_epi64 读 8 字节。
            let a4 = std::ptr::read_unaligned(dst.offset(o) as *const u32);
            let b4 = std::ptr::read_unaligned(dst.offset(o + 4) as *const u32);
            let ba = _mm_unpacklo_epi8(_mm_cvtsi32_si128(a4 as i32), zero);
            let bb = _mm_unpacklo_epi8(_mm_cvtsi32_si128(b4 as i32), zero);
            let sa = _mm_packus_epi16(_mm_add_epi16(ba, da), zero);
            let sb = _mm_packus_epi16(_mm_add_epi16(bb, db), zero);
            std::ptr::write_unaligned(dst.offset(o) as *mut u32, _mm_cvtsi128_si32(sa) as u32);
            std::ptr::write_unaligned(dst.offset(o + 4) as *mut u32, _mm_cvtsi128_si32(sb) as u32);
        }
    }
}

/// 单个 4×4 块仅 DC（无 AC）的逆变换捷径（`dc_add_two_sse2` 的单块版）：残差恒
/// `(dc+4)>>3`（与完整两遍变换对仅 DC 块逐字节一致，同 `dc_add_two_sse2` 的既有等价性）。
/// 每行只读写 4 字节（块右缘不越出平面右端，同 `idct_add_one_sse2` 姿势）。i4 路径按块级
/// AC 位（`ac_mask`）把纯 DC 块分发到本核——省去两遍 butterfly + 转置（~118 uops → ~20）。
#[cfg(target_arch = "x86_64")]
// Coder Iteration 4：`#[inline]`（`#[inline(always)]` 与 `#[target_feature]` 互斥，E0658）——
// 同上，i4 DC 捷径核于 `reconstruct_i4_mb_ssse3` 内内联，消逐块 `callq` 与栈往返。
#[inline]
#[target_feature(enable = "sse2")]
unsafe fn dc_add_one_sse2(dc: i16, dst: *mut u8, stride: isize) {
    use std::arch::x86_64::*;
    unsafe {
        let d = _mm_set1_epi16(((i32::from(dc) + 4) >> 3) as i16);
        let zero = _mm_setzero_si128();
        for r in 0..4isize {
            let o = r * stride;
            let a4 = std::ptr::read_unaligned(dst.offset(o) as *const u32);
            let ba = _mm_unpacklo_epi8(_mm_cvtsi32_si128(a4 as i32), zero);
            let sa = _mm_packus_epi16(_mm_add_epi16(ba, d), zero);
            std::ptr::write_unaligned(dst.offset(o) as *mut u32, _mm_cvtsi128_si32(sa) as u32);
        }
    }
}

/// 预判定后的单块 DC 捷径核直调：`part_simd_on("dct","sse2")` 已由调用方提升出 i4 子块循环
/// （同 `idct_add_one_forced` 姿势）。**Safety**：调用方须保证 sse2 可用，且本块确为纯 DC
/// （调用方按 `ac_mask` 位 0 分发，见 `reconstruct_macroblock` i4 分支）；越界语义同
/// `dc_add_two`（`row0/col0` 落在平面内）。
#[inline(always)]
pub(crate) unsafe fn dc_add_one_forced(dc: i16, plane: &mut [u8], pitch: usize, row0: usize, col0: usize) {
    // x86_64：内核使用 x86-only intrinsic，仅在该目标编译（SSE2 核指令可用性由调用方保证）。
    #[cfg(target_arch = "x86_64")]
    {
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            dc_add_one_sse2(dc, dst, pitch as isize);
        }
        return;
    }
    // 非 x86_64：x86 intrinsic 不存在，恒走标量全变换兜底（运行时不达：调用方
    // `part_simd_on("dct","sse2")` 在非 x86_64 恒为 false）。
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut c = [0i16; 16];
        c[0] = dc;
        super::vp8::inverse_dct_add(&c, plane, pitch, row0, col0, true);
    }
}

/// 第二遍一维逆变换的单块 4×i32 版本：入参为已符号扩展的 i32 列向量 t0..t3 与对应高位乘项
/// `m1k2`=`(t1*35468)>>16 - t1`、`m3k1`=`(t3*20091)>>16`、`m12`=`(t1*20091)>>16`、
/// `m32`=`(t3*35468)>>16 - t3`。全 i32 相加（无 i16 回绕）后算术右移 3，与标量
/// `(transform_1d 输出 + 4) >> 3` 逐字节一致；结果恒落 i16 域（≤±16 位），可直接饱和打包。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn idct_pass2_shift32_sse2(
    t0: __m128i,
    t1: __m128i,
    t2: __m128i,
    t3: __m128i,
    m1k2: __m128i,
    m3k1: __m128i,
    m12: __m128i,
    m32: __m128i,
) -> (__m128i, __m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    let four = _mm_set1_epi32(4);
    let a = _mm_add_epi32(_mm_add_epi32(t0, t2), four);
    let b = _mm_sub_epi32(_mm_add_epi32(t0, four), t2);
    let c = _mm_sub_epi32(_mm_add_epi32(m1k2, t1), _mm_add_epi32(t3, m3k1));
    let d = _mm_add_epi32(_mm_add_epi32(_mm_add_epi32(t1, m12), m32), t3);
    let s0 = _mm_srai_epi32(_mm_add_epi32(a, d), 3);
    let s1 = _mm_srai_epi32(_mm_add_epi32(b, c), 3);
    let s2 = _mm_srai_epi32(_mm_sub_epi32(b, c), 3);
    let s3 = _mm_srai_epi32(_mm_sub_epi32(a, d), 3);
    (s0, s1, s2, s3)
}

/// 第二遍一维逆变换的 i16 域版本（8-lane A/B 组合布局，与 libwebp `Transform_SSE2` 水平遍
/// 逐 op 对齐）：先 `dc=T0+4`（**add-4 先于合**，随后 a/b/c/d 全 i16 回绕算术 mod 2^16），
/// mulhi 项为 K1=20091/K2=-30068 之 `(x*k)>>16`（K = k + 2^16 技巧），c/d 两项按 libwebp
/// 「先算差/和（c3/d3）再算子项差/和（c4/d4）最后合」的求值顺序，末 `srai_epi16(x,3)`
/// （算术右移，等价标量 i32 `(x+4)>>3` 的整除方向）。
///
/// **语义（显式记录，对抗评审定案）**：∃ ∥c∥>2212（码流可达）时 i16 中间量回绕与 i32 相位差 →
/// 本核即不等于 i32（标量/SIMD）；可达域 ∥c∥≤2212 内任一 i16 中间量不越 ±32767
/// （`idct_add_two_reachable_domain_parity` 直断），与本文件 i32 pass2 ≡ 标量逐字节一致。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn idct_pass2_shift16_sse2(
    t0: __m128i,
    t1: __m128i,
    t2: __m128i,
    t3: __m128i,
) -> (__m128i, __m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    let k1 = _mm_set1_epi16(20091);
    let k2 = _mm_set1_epi16(-30068);
    let four = _mm_set1_epi16(4);
    // 与 libwebp dec_sse2.c 水平遍同序：dc=T0+4 → a=dc+T2、b=dc-T2。
    let dc = _mm_add_epi16(t0, four);
    let a = _mm_add_epi16(dc, t2);
    let b = _mm_sub_epi16(dc, t2);
    // c = MUL(t1,K2) − MUL(t3,K1) = mulhi(t1,k2) − mulhi(t3,k1) + t1 − t3（先差后合）。
    let c1 = _mm_mulhi_epi16(t1, k2);
    let c2 = _mm_mulhi_epi16(t3, k1);
    let c3 = _mm_sub_epi16(t1, t3);
    let c4 = _mm_sub_epi16(c1, c2);
    let c = _mm_add_epi16(c3, c4);
    // d = MUL(t1,K1) + MUL(t3,K2) = mulhi(t1,k1) + mulhi(t3,k2) + t1 + t3（先和后合）。
    let d1 = _mm_mulhi_epi16(t1, k1);
    let d2 = _mm_mulhi_epi16(t3, k2);
    let d3 = _mm_add_epi16(t1, t3);
    let d4 = _mm_add_epi16(d1, d2);
    let d = _mm_add_epi16(d3, d4);
    let s0 = _mm_srai_epi16(_mm_add_epi16(a, d), 3);
    let s1 = _mm_srai_epi16(_mm_add_epi16(b, c), 3);
    let s2 = _mm_srai_epi16(_mm_sub_epi16(b, c), 3);
    let s3 = _mm_srai_epi16(_mm_sub_epi16(a, d), 3);
    (s0, s1, s2, s3)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn transpose_2_4x4_16b_sse2(
    in0: __m128i,
    in1: __m128i,
    in2: __m128i,
    in3: __m128i,
) -> (__m128i, __m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    let t00 = _mm_unpacklo_epi16(in0, in1);
    let t01 = _mm_unpacklo_epi16(in2, in3);
    let t10 = _mm_unpackhi_epi16(in0, in1);
    let t11 = _mm_unpackhi_epi16(in2, in3);
    let t20 = _mm_unpacklo_epi32(t00, t01);
    let t21 = _mm_unpacklo_epi32(t10, t11);
    let t22 = _mm_unpackhi_epi32(t00, t01);
    let t23 = _mm_unpackhi_epi32(t10, t11);
    (
        _mm_unpacklo_epi64(t20, t21),
        _mm_unpackhi_epi64(t20, t21),
        _mm_unpacklo_epi64(t22, t23),
        _mm_unpackhi_epi64(t22, t23),
    )
}
