//! HEIF 色彩转换（`color.rs` `to_rgba_420_row_scalar`）的 AVX2 向量内核，与标量逐位字节恒等。
//!
//! - `to_rgba_420_row_avx2`：8-pixel 步进（调用方挡 `width<8` 与 identity），i32 8-lane 载荷：
//!   u16 采样 → `_mm256_cvtepu16_epi32` 加宽，色度 4 采样复制扩展（c0 c0 c1 c1 c2 c2 c3 c3）、
//!   系数 broadcast `_mm256_mullo_epi32`、`+32768`、`_mm256_srai_epi32 16`（算术右移）、
//!   `max/min` clamp [0,255]、pack 成 RGBA 字节序 32B store。与 C# `HeifColor.cs`
//!   `ToRgba420RowV256` 同一算术（C# color ~5× 快的来源）。
//!
//! 字节恒等纪律（对 C# `ToRgba420RowV256` 逐位一致，也即对 `color.rs` 标量逐位一致）：
//! - 逐像素语义：`yy = y·(Y−YOffset)+2^15`；`r=(yy+CrR·v)>>16`、`g=(yy−CbG·u−CrG·v)>>16`、
//!   `b=(yy+CbB·u)>>16`，全部 i32 回绕运算（`_mm256_mullo_epi32`=低 32 位积 ≡ `wrapping_mul`，
//!   `_mm256_add_epi32`/`_mm256_sub_epi32`=32 位回绕加减）；
//! - `_mm256_srai_epi32`=算术 `>>`（vpsrad），clamp 在 pack 之前；
//! - pack 顺序：`packus_epi32(r,g)` → `packus_epi32(b,α)` → `packus_epi16`，再 `_mm256_shuffle_epi8`
//!   每 128-bit lane 掩码 `[0,4,8,12,1,5,9,13,2,6,10,14,3,7,11,15]` 组 R,G,B,A，与标量 4B/pixel
//!   写序一致；不引入饱和/近似（clamp 后值域 [0,255]，pack 即精确窄化）。
//!
//! 叶子隔离（对标 `hevc/transform_simd.rs` 与 kimg_webp `simd/raw/`）：本模块是 `color.rs`
//! 的 unsafe 下沉点；`to_rgba_420_row_avx2` 是**安全**入口（unsafe 内核封在函数体内，先自证缓冲
//! 前置），调用方（`color.rs` `to_rgba_420_row` 分派）只需 `is_x86_feature_detected!` 运行时检出
//! 即可直接调用，未检出回标量。

#![allow(unsafe_code)]
#![cfg(target_arch = "x86_64")]

use std::arch::x86_64::*;

use super::color::Coefficients;

/// AVX2 8-lane 行入口（安全，仿 `hevc/transform_simd.rs` `inverse_add_avx2` 先例）：把 `y`/`cb`/`cr`
/// 行的前 `floor(width/8)*8` 个像素转成 RGBA8 写到 `out`，返回已处理像素数（8 的倍数）；
/// 余下 `width % 8` 像素由调用方以标量补尾。
///
/// 函数签名安全：内部先自证调用前置（`width >= 8`、非 identity、`y.len() >= width`、
/// `cb.len() >= (width+1)>>1`、`cr.len() >= (width+1)>>1`、`out.len() >= 4·n`，
/// n = `floor(width/8)*8` 为实际写入的像素数），
/// 再把 unsafe 内核封在 `unsafe {}` 内调用（叶子隔离：调用方无需 unsafe）。前置不满足返回 0
/// （调用方回标量）。AVX2 需调用方先 `is_x86_feature_detected!("avx2")` 运行时检出后再调。
pub(super) fn to_rgba_420_row_avx2(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    out: &mut [u8],
    width: usize,
    k: &Coefficients,
) -> usize {
    let n = (width / 8) * 8;
    if n == 0 || k.identity {
        return 0;
    }
    let chroma = (width + 1) >> 1;
    if y.len() < width || cb.len() < chroma || cr.len() < chroma || out.len() < n * 4 {
        return 0;
    }
    // SAFETY: 上方已自证全部读写下标界内（与标量 debug_assert 同前置）；本函数非 target_feature，
    // AVX2 可用性由调用方经 is_x86_feature_detected! 检出后保证（与 transform_simd::inverse_add_avx2
    // 完全同一姿势——安全包装内 unsafe{} 调 target_feature 内核）。
    unsafe { avx2_kernel(y, cb, cr, out, width, k) }
}

/// AVX2 8-lane 行内核（唯一 unsafe 下沉点，只经上方安全包装调用，本函数不对外）。
///
/// # Safety
///
/// 调用前置（与 `color::to_rgba_420_row_scalar` 相同，安全包装内已验证）：`width >= 8`、
/// `k.identity == false`、`y.len() >= width`、`cb.len() >= (width+1)>>1`、
/// `cr.len() >= (width+1)>>1`、`out.len() >= 4·(width/8)·8`（只写已处理的 n 像素）；
/// AVX2 经调用方运行时检出。
#[target_feature(enable = "avx2")]
unsafe fn avx2_kernel(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    out: &mut [u8],
    width: usize,
    k: &Coefficients,
) -> usize {
    // 正文在显式 unsafe 块内：target_feature 上下文里的 intrinsics/指针运算按 edition-2024
    // `unsafe_op_in_unsafe_fn` 写明安全前提（与 hevc/transform_simd.rs 的 avx2_kernel 同姿势）。
    unsafe {
    let n = (width / 8) * 8;
    let ybase = y.as_ptr();
    let cbbase = cb.as_ptr();
    let crbase = cr.as_ptr();
    let obase = out.as_mut_ptr();

    let v_yofs = _mm256_set1_epi32(k.y_offset);
    let v_cofs = _mm256_set1_epi32(k.c_offset);
    let v_y = _mm256_set1_epi32(k.y);
    let v_cr_r = _mm256_set1_epi32(k.cr_r);
    let v_cb_g = _mm256_set1_epi32(k.cb_g);
    let v_cr_g = _mm256_set1_epi32(k.cr_g);
    let v_cb_b = _mm256_set1_epi32(k.cb_b);
    let v_half = _mm256_set1_epi32(32768);
    let v_min = _mm256_setzero_si256();
    let v_max = _mm256_set1_epi32(255);
    let v_alpha = _mm256_set1_epi32(255);

    // 4 个色度采样各复制给自己的两个亮度采样：c0 c0 c1 c1 c2 c2 c3 c3（8×u16，shuffle 后加宽）。
    let dup = _mm_setr_epi8(0, 1, 0, 1, 2, 3, 2, 3, 4, 5, 4, 5, 6, 7, 6, 7);
    // pack 后每 128-bit lane 是 [r0 r1 r2 r3 g0 g1 g2 g3 b0 b1 b2 b3 α α α α]，本掩码把它重排成
    // [r0 g0 b0 α r1 g1 b1 α r2 g2 b2 α r3 g3 b3 α]，即标量 RGBA 写序。
    let pack_shuffle = _mm256_setr_epi8(
        0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15, //
        0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15,
    );

    let mut i = 0usize;
    while i < n {
        // 亮度：8×u16 → 8×i32（零扩展与 `i32::from(u16)` 同值），减 YOffset。
        let y8 = _mm_loadu_si128(ybase.add(i) as *const __m128i);
        let mut yv = _mm256_cvtepu16_epi32(y8);
        yv = _mm256_sub_epi32(yv, v_yofs);

        // 色度：ci = i>>1 起的 4 个采样复制扩展，加宽后减 COffset（i32 域减与 C#
        // u16 减再加宽逐值等价——wrap 情形结果一致）。
        let ci = i >> 1;
        let cb4 = _mm_loadl_epi64(cbbase.add(ci) as *const __m128i);
        let cbs = _mm_shuffle_epi8(cb4, dup);
        let mut uv = _mm256_cvtepu16_epi32(cbs);
        uv = _mm256_sub_epi32(uv, v_cofs);

        let cr4 = _mm_loadl_epi64(crbase.add(ci) as *const __m128i);
        let crs = _mm_shuffle_epi8(cr4, dup);
        let mut vv = _mm256_cvtepu16_epi32(crs);
        vv = _mm256_sub_epi32(vv, v_cofs);

        // yy = Y·luma + 2^15（回绕加）。
        let yy = _mm256_add_epi32(_mm256_mullo_epi32(yv, v_y), v_half);

        // r = clamp((yy + CrR·v) >> 16, 0, 255)
        let r_raw = _mm256_add_epi32(yy, _mm256_mullo_epi32(v_cr_r, vv));
        let r = _mm256_min_epi32(_mm256_max_epi32(_mm256_srai_epi32::<16>(r_raw), v_min), v_max);

        // g = clamp((yy − CbG·u − CrG·v) >> 16, 0, 255)
        let g_raw = _mm256_sub_epi32(
            _mm256_sub_epi32(yy, _mm256_mullo_epi32(v_cb_g, uv)),
            _mm256_mullo_epi32(v_cr_g, vv),
        );
        let g = _mm256_min_epi32(_mm256_max_epi32(_mm256_srai_epi32::<16>(g_raw), v_min), v_max);

        // b = clamp((yy + CbB·u) >> 16, 0, 255)
        let b_raw = _mm256_add_epi32(yy, _mm256_mullo_epi32(v_cb_b, uv));
        let b = _mm256_min_epi32(_mm256_max_epi32(_mm256_srai_epi32::<16>(b_raw), v_min), v_max);

        // 打包：每 128-bit lane 变 [r0..r3, g0..g3, b0..b3, α×4]（u8），shuffle 成 RGBA 写序。
        let rb = _mm256_packus_epi32(r, g);
        let ba = _mm256_packus_epi32(b, v_alpha);
        let packed = _mm256_packus_epi16(rb, ba);
        let rgba = _mm256_shuffle_epi8(packed, pack_shuffle);
        _mm256_storeu_si256(obase.add(i * 4) as *mut __m256i, rgba);
        i += 8;
    }
    n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::heif::color::{to_rgba_420_row_scalar, Coefficients};

    /// xorshift64* 确定性 PRNG（跨平台稳定，无需 rand 依赖）。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    fn rand_vec(rng: &mut Rng, len: usize, max: u16) -> Vec<u16> {
        (0..len)
            .map(|_| (rng.next() % (max as u64 + 1)) as u16)
            .collect()
    }

    /// 直接对拍内核（不含尾段）：随机输入下，avx2 核覆盖的多像素段与标量逐字节相等。
    /// 标量段用真正标量行函数（非重复实现）作 oracle，只取向量覆盖范围比对。
    #[test]
    fn avx2_kernel_matches_scalar_randomised() {
        if !kimg_core::simd::detect("avx2") {
            eprintln!("SKIP: 无 AVX2，向量核不参与比较");
            return;
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // 权值域：8/10/12/14/16-bit 尺度的真实采样范围，外加 8-bit 系数 × 全 u16 域（乘积溢出 i32，
        // 直接对拍标量的回绕语义）；矩阵覆盖 601/709/4/7/2020。
        let ranges: &[(u8, u16, u16)] = &[
            (8, 255, 255),
            (10, 1023, 1023),
            (12, 4095, 4095),
            (14, 16383, 16383),
            (16, 65535, 65535),
            (8, u16::MAX, u16::MAX),
        ];
        let mut tried = 0usize;
        for &(bd, y_max, c_max) in ranges {
            for &matrix in &[1, 2, 4, 7, 9] {
                for &full in &[false, true] {
                    for &width in &[8usize, 9, 17, 31, 32, 64] {
                        let k = Coefficients::for_matrix(matrix, full, bd as i32);
                        let chroma = (width + 1) >> 1;
                        let y = rand_vec(&mut rng, width, y_max);
                        let cb = rand_vec(&mut rng, chroma, c_max);
                        let cr = rand_vec(&mut rng, chroma, c_max);
                        let done = (width / 8) * 8;
                        let mut full_row = vec![0u8; width * 4];
                        to_rgba_420_row_scalar(&y, &cb, &cr, &mut full_row, width, &k);
                        let mut simd_out = vec![0u8; done * 4];
                        let got = to_rgba_420_row_avx2(&y, &cb, &cr, &mut simd_out, width, &k);
                        assert_eq!(got, done, "返回已处理像素数应为 {done}");
                        compare_rows(
                            "avx2-kernel",
                            &full_row[..done * 4],
                            &simd_out,
                            matrix,
                            full,
                            bd,
                            width,
                        );
                        tried += 1;
                    }
                }
            }
        }
        assert!(tried > 0, "随机对拍未执行任何一轮");
    }

    fn compare_rows(tag: &str, a: &[u8], b: &[u8], matrix: i32, full: bool, bd: u8, width: usize) {
        assert_eq!(a.len(), b.len(), "{tag}: 段长不一致");
        for (pix, chunk) in a.chunks_exact(4).enumerate() {
            let other = &b[pix * 4..pix * 4 + 4];
            if chunk != other {
                panic!(
                    "{tag} 首差 @ x={pix} (matrix={matrix} full={full} bd={bd} width={width}): \
                     标量={chunk:?} 向量={other:?}"
                );
            }
        }
    }
}
