//! x86-64 AVX2 热路径（IDCT / YCbCr→RGB(A) 颜色换算 / 双线性上采样）。
//!
//! 逐句复刻 zune-jpeg 0.5.15 的 AVX2 实现（`idct/avx2.rs`、`color_convert/avx.rs`、
//! `upsampler/avx2.rs`、`unsafe_utils_avx2.rs`），zune 自身保证与标量路径逐字节恒等，
//! 本项目以 `jpeg_parity`（对标旧标量 zune 后端）为最终硬闸。
//!
//! 门控纪律（K93）：内核用 `#[target_feature(enable = "avx2")]` 编译；
//! 调用侧仅当运行时检测 `std::arch::is_x86_feature_detected!("avx2")` 通过（且未被
//! `KIMG_JPEG_AVX2=0/false/off` 关闭）才走 AVX2 内核，否则回退标量——禁止裸发直调。
//! 非 x86_64 一律编译期走标量。AVX2 与标量路径的逐字节恒等断言见本模块底部单测。

// SIMD 内核文件统一豁免 `unsafe_op_in_unsafe_fn`（edition 2024；调用点本身仍是
// `unsafe fn`，门控在分发层保证）。
#![allow(unsafe_op_in_unsafe_fn)]

use crate::codecs::jpeg::color::{ycbcr_to_rgb_inner_16_scalar, ycbcr_to_rgba_inner_16_scalar};
use crate::codecs::jpeg::idct::{idct4x4 as idct4x4_scalar, idct_int as idct_int_scalar};
use crate::codecs::jpeg::upsampler::{
    upsample_horizontal as uh_scalar, upsample_hv as uhv_scalar, upsample_vertical as uv_scalar,
};

/// AVX2 是否可用（进程内缓存一次：env 关闭判断 + 运行时特性检测）。
/// `KIMG_JPEG_AVX2=0/false/off` → 强制标量（A/B 对照）；缺省按 `is_x86_feature_detected!`。
/// 首次访问读一次 env + 特性检测，之后仅 OnceLock 静态读——热环开销可忽略。
#[inline]
pub(crate) fn avx2_enabled() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            let v = std::env::var("KIMG_JPEG_AVX2").unwrap_or_default();
            let t = v.trim();
            if t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off") {
                return false;
            }
            std::arch::is_x86_feature_detected!("avx2")
        })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// AVX2 内核（仅 x86_64 编译；调用侧必须经 `avx2_enabled()` 门控后 unsafe 调用）
// ---------------------------------------------------------------------------
#[cfg(target_arch = "x86_64")]
mod kernels {
    use core::arch::x86_64::*;

    const SCALE_BITS: i32 = 512 + 65536 + (128 << 17);

    /// `_MM_SHUFFLE()` 副本（stable 可用，无需 nightly）。
    #[inline]
    const fn shuffle(z: i32, y: i32, x: i32, w: i32) -> i32 {
        (z << 6) | (y << 4) | (x << 2) | w
    }

    /// 8×8（i32 lane）转置（复刻 `unsafe_utils_avx2.rs::transpose`，参数改为 `__m256i`）。
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn transpose(
        v0: &mut __m256i,
        v1: &mut __m256i,
        v2: &mut __m256i,
        v3: &mut __m256i,
        v4: &mut __m256i,
        v5: &mut __m256i,
        v6: &mut __m256i,
        v7: &mut __m256i,
    ) {
        macro_rules! merge_epi32 {
            ($a:expr, $b:expr) => {{
                let va = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>($a);
                let vb = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>($b);
                (_mm256_unpacklo_epi32(va, vb), _mm256_unpackhi_epi32(va, vb))
            }};
        }
        macro_rules! merge_epi64 {
            ($a:expr, $b:expr) => {{
                let va = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>($a);
                let vb = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>($b);
                (_mm256_unpacklo_epi64(va, vb), _mm256_unpackhi_epi64(va, vb))
            }};
        }
        macro_rules! merge_si128 {
            ($a:expr, $b:expr) => {{
                let va = _mm256_permute2x128_si256::<{ shuffle(0, 2, 0, 0) }>($a, $b);
                let vb = _mm256_permute2x128_si256::<{ shuffle(0, 3, 0, 1) }>($a, $b);
                (va, vb)
            }};
        }

        let (w0, w1) = merge_epi32!(*v0, *v1);
        let (w2, w3) = merge_epi32!(*v2, *v3);
        let (w4, w5) = merge_epi32!(*v4, *v5);
        let (w6, w7) = merge_epi32!(*v6, *v7);

        let (x0, x1) = merge_epi64!(w0, w2);
        let (x2, x3) = merge_epi64!(w1, w3);
        let (x4, x5) = merge_epi64!(w4, w6);
        let (x6, x7) = merge_epi64!(w5, w7);

        let (t0, t1) = merge_si128!(x0, x4);
        let (t2, t3) = merge_si128!(x1, x5);
        let (t4, t5) = merge_si128!(x2, x6);
        let (t6, t7) = merge_si128!(x3, x7);

        *v0 = t0;
        *v1 = t1;
        *v2 = t2;
        *v3 = t3;
        *v4 = t4;
        *v5 = t5;
        *v6 = t6;
        *v7 = t7;
    }

    /// i16 clamp 到 0..255（复刻 `idct/avx2.rs::clamp_avx`）。
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn clamp_avx(reg: __m256i) -> __m256i {
        let min_s = _mm256_set1_epi16(0);
        let max_s = _mm256_set1_epi16(255);

        let max_v = _mm256_max_epi16(reg, min_s); //max(a,0)
        _mm256_min_epi16(max_v, max_s) //min(max(a,0),255)
    }

    /// pack → clamp → 反 shuffle → 交错存入（复刻 `idct/avx2.rs` 的 `permute_store!`）。
    macro_rules! permute_store {
        ($x:expr, $y:expr, $index:ident, $out:ident, $stride:ident) => {{
            let a = _mm256_packs_epi32($x, $y);

            // 打包后 clamp（一次可夹更多值）
            let b = clamp_avx(a);
            let mut tmp = [0; 8];
            // 反 shuffle
            let c = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>(b);

            // 存第一向量
            _mm_storeu_si128(
                ($out)
                    .get_mut($index..$index + 8)
                    .unwrap_or(&mut tmp)
                    .as_mut_ptr()
                    .cast(),
                _mm256_extractf128_si256::<0>(c),
            );
            $index += $stride;
            // 第二向量
            _mm_storeu_si128(
                ($out)
                    .get_mut($index..$index + 8)
                    .unwrap()
                    .as_mut_ptr()
                    .cast(),
                _mm256_extractf128_si256::<1>(c),
            );
            $index += $stride;
        }};
    }

    /// 完整 8×8 IDCT（复刻 `idct/avx2.rs::idct_avx2`）。
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_lines, clippy::similar_names, unused_assignments)]
    pub(crate) unsafe fn idct_avx2(
        in_vector: &mut [i32; 64],
        out_vector: &mut [i16],
        stride: usize,
    ) {
        let mut pos = 0;

        // 载入寄存器（i16→i32 符号扩展，全程扩展精度计算，最后收敛回 i16）
        let rw0 = _mm256_loadu_si256(in_vector[00..].as_ptr().cast());
        let rw1 = _mm256_loadu_si256(in_vector[08..].as_ptr().cast());
        let rw2 = _mm256_loadu_si256(in_vector[16..].as_ptr().cast());
        let rw3 = _mm256_loadu_si256(in_vector[24..].as_ptr().cast());
        let rw4 = _mm256_loadu_si256(in_vector[32..].as_ptr().cast());
        let rw5 = _mm256_loadu_si256(in_vector[40..].as_ptr().cast());
        let rw6 = _mm256_loadu_si256(in_vector[48..].as_ptr().cast());
        let rw7 = _mm256_loadu_si256(in_vector[56..].as_ptr().cast());

        // 再载入第一行偏 1 的 8 个元素（只查 AC 项，不查 DC）
        let rw8 = _mm256_loadu_si256(in_vector[1..].as_ptr().cast());

        let mut bitmap = _mm256_or_si256(rw1, rw2);
        bitmap = _mm256_or_si256(bitmap, rw3);
        bitmap = _mm256_or_si256(bitmap, rw4);
        bitmap = _mm256_or_si256(bitmap, rw5);
        bitmap = _mm256_or_si256(bitmap, rw6);
        bitmap = _mm256_or_si256(bitmap, rw7);
        bitmap = _mm256_or_si256(bitmap, rw8);

        if _mm256_testz_si256(bitmap, bitmap) == 1 {
            // 63 个 AC 全零：整块为 `(coeff[0] + 4 + 1024) >> 3` clamp 后铺满（同 idct_int_1x1）
            let coeff = ((in_vector[0].wrapping_add(4 + 1024)) >> 3).clamp(0, 255) as i16;
            let idct_value = _mm_set1_epi16(coeff);

            macro_rules! store {
                ($p:ident, $value:expr) => {
                    _mm_storeu_si128(
                        out_vector
                            .get_mut($p..$p + 8)
                            .unwrap()
                            .as_mut_ptr()
                            .cast(),
                        $value,
                    );
                    $p += stride;
                };
            }
            store!(pos, idct_value);
            store!(pos, idct_value);
            store!(pos, idct_value);
            store!(pos, idct_value);

            store!(pos, idct_value);
            store!(pos, idct_value);
            store!(pos, idct_value);
            store!(pos, idct_value);

            return;
        }

        let mut row0 = rw0;
        let mut row1 = rw1;
        let mut row2 = rw2;
        let mut row3 = rw3;
        let mut row4 = rw4;
        let mut row5 = rw5;
        let mut row6 = rw6;
        let mut row7 = rw7;

        // 一次 IDCT pass（标量的直接寄存器翻译：加法/乘法均环绕，与标量 wa/ws/wm 恒等）
        macro_rules! dct_pass {
            ($SCALE_BITS:expr, $scale:tt) => {
                // even part
                let p1 =
                    _mm256_mullo_epi32(_mm256_add_epi32(row2, row6), _mm256_set1_epi32(2217));

                let mut t2 = _mm256_add_epi32(p1, _mm256_mullo_epi32(row6, _mm256_set1_epi32(-7567)));
                let mut t3 = _mm256_add_epi32(p1, _mm256_mullo_epi32(row2, _mm256_set1_epi32(3135)));

                let mut t0 = _mm256_slli_epi32::<12>(_mm256_add_epi32(row0, row4));
                let mut t1 = _mm256_slli_epi32::<12>(_mm256_sub_epi32(row0, row4));

                let x0 =
                    _mm256_add_epi32(_mm256_add_epi32(t0, t3), _mm256_set1_epi32($SCALE_BITS));
                let x3 =
                    _mm256_add_epi32(_mm256_sub_epi32(t0, t3), _mm256_set1_epi32($SCALE_BITS));
                let x1 =
                    _mm256_add_epi32(_mm256_add_epi32(t1, t2), _mm256_set1_epi32($SCALE_BITS));
                let x2 =
                    _mm256_add_epi32(_mm256_sub_epi32(t1, t2), _mm256_set1_epi32($SCALE_BITS));

                let p3 = _mm256_add_epi32(row7, row3);
                let p4 = _mm256_add_epi32(row5, row1);
                let p1 = _mm256_add_epi32(row7, row1);
                let p2 = _mm256_add_epi32(row5, row3);
                let p5 = _mm256_mullo_epi32(_mm256_add_epi32(p3, p4), _mm256_set1_epi32(4816));

                t0 = _mm256_mullo_epi32(row7, _mm256_set1_epi32(1223));
                t1 = _mm256_mullo_epi32(row5, _mm256_set1_epi32(8410));
                t2 = _mm256_mullo_epi32(row3, _mm256_set1_epi32(12586));
                t3 = _mm256_mullo_epi32(row1, _mm256_set1_epi32(6149));

                let p1 = _mm256_add_epi32(p5, _mm256_mullo_epi32(p1, _mm256_set1_epi32(-3685)));
                let p2 = _mm256_add_epi32(p5, _mm256_mullo_epi32(p2, _mm256_set1_epi32(-10497)));
                let p3 = _mm256_mullo_epi32(p3, _mm256_set1_epi32(-8034));
                let p4 = _mm256_mullo_epi32(p4, _mm256_set1_epi32(-1597));

                t3 = _mm256_add_epi32(t3, _mm256_add_epi32(p1, p4));
                t2 = _mm256_add_epi32(t2, _mm256_add_epi32(p2, p3));
                t1 = _mm256_add_epi32(t1, _mm256_add_epi32(p2, p4));
                t0 = _mm256_add_epi32(t0, _mm256_add_epi32(p1, p3));

                row0 = _mm256_srai_epi32::<$scale>(_mm256_add_epi32(x0, t3));
                row1 = _mm256_srai_epi32::<$scale>(_mm256_add_epi32(x1, t2));
                row2 = _mm256_srai_epi32::<$scale>(_mm256_add_epi32(x2, t1));
                row3 = _mm256_srai_epi32::<$scale>(_mm256_add_epi32(x3, t0));

                row4 = _mm256_srai_epi32::<$scale>(_mm256_sub_epi32(x3, t0));
                row5 = _mm256_srai_epi32::<$scale>(_mm256_sub_epi32(x2, t1));
                row6 = _mm256_srai_epi32::<$scale>(_mm256_sub_epi32(x1, t2));
                row7 = _mm256_srai_epi32::<$scale>(_mm256_sub_epi32(x0, t3));
            };
        }

        // 处理行
        dct_pass!(512, 10);
        transpose(
            &mut row0, &mut row1, &mut row2, &mut row3, &mut row4, &mut row5, &mut row6, &mut row7,
        );

        // 处理列
        dct_pass!(SCALE_BITS, 17);
        transpose(
            &mut row0, &mut row1, &mut row2, &mut row3, &mut row4, &mut row5, &mut row6, &mut row7,
        );

        // pack 并写回数组
        permute_store!(row0, row1, pos, out_vector, stride);
        permute_store!(row2, row3, pos, out_vector, stride);
        permute_store!(row4, row5, pos, out_vector, stride);
        permute_store!(row6, row7, pos, out_vector, stride);
    }

    /// 仅左上 4×4 子块非零的 IDCT（复刻 `idct/avx2.rs::idct_avx2_4x4`）。
    #[target_feature(enable = "avx2")]
    #[allow(clippy::similar_names, unused_assignments)]
    pub(crate) unsafe fn idct_avx2_4x4(
        in_vector: &mut [i32; 64],
        out_vector: &mut [i16],
        stride: usize,
    ) {
        let rw0 = _mm256_loadu_si256(in_vector[00..].as_ptr().cast());
        let rw1 = _mm256_loadu_si256(in_vector[08..].as_ptr().cast());
        let rw2 = _mm256_loadu_si256(in_vector[16..].as_ptr().cast());
        let rw3 = _mm256_loadu_si256(in_vector[24..].as_ptr().cast());

        let mut row0 = rw0;
        let mut row1 = rw1;
        let mut row2 = rw2;
        let mut row3 = rw3;
        let mut row4 = rw0;
        let mut row5 = rw0;
        let mut row6 = rw0;
        let mut row7 = rw0;

        {
            row0 = _mm256_slli_epi32::<12>(row0);
            row0 = _mm256_add_epi32(row0, _mm256_set1_epi32(512));

            let i2 = row2;

            let p1 = _mm256_mullo_epi32(i2, _mm256_set1_epi32(2217));
            let p3 = _mm256_mullo_epi32(i2, _mm256_set1_epi32(5352));

            let x0 = _mm256_add_epi32(row0, p3);
            let x1 = _mm256_add_epi32(row0, p1);
            let x2 = _mm256_sub_epi32(row0, p1);
            let x3 = _mm256_sub_epi32(row0, p3);

            // odd part
            let i4 = row3;
            let i3 = row1;

            let p5 = _mm256_mullo_epi32(_mm256_add_epi32(i4, i3), _mm256_set1_epi32(4816));

            let p1 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i3, _mm256_set1_epi32(-3685)));
            let p2 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i4, _mm256_set1_epi32(-10497)));

            let t3 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i3, _mm256_set1_epi32(867)));
            let t2 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i4, _mm256_set1_epi32(-5945)));

            let t1 = _mm256_add_epi32(p2, _mm256_mullo_epi32(i3, _mm256_set1_epi32(-1597)));
            let t0 = _mm256_add_epi32(p1, _mm256_mullo_epi32(i4, _mm256_set1_epi32(-8034)));

            row0 = _mm256_srai_epi32::<10>(_mm256_add_epi32(x0, t3));
            row1 = _mm256_srai_epi32::<10>(_mm256_add_epi32(x1, t2));
            row2 = _mm256_srai_epi32::<10>(_mm256_add_epi32(x2, t1));
            row3 = _mm256_srai_epi32::<10>(_mm256_add_epi32(x3, t0));

            row4 = _mm256_srai_epi32::<10>(_mm256_sub_epi32(x3, t0));
            row5 = _mm256_srai_epi32::<10>(_mm256_sub_epi32(x2, t1));
            row6 = _mm256_srai_epi32::<10>(_mm256_sub_epi32(x1, t2));
            row7 = _mm256_srai_epi32::<10>(_mm256_sub_epi32(x0, t3));
        }

        transpose(
            &mut row0, &mut row1, &mut row2, &mut row3, &mut row4, &mut row5, &mut row6, &mut row7,
        );

        {
            let i2 = row2;
            let i0 = row0;

            row0 = _mm256_slli_epi32::<12>(i0);
            let t0 = _mm256_add_epi32(row0, _mm256_set1_epi32(SCALE_BITS));

            let t2 = _mm256_mullo_epi32(i2, _mm256_set1_epi32(2217));
            let t3 = _mm256_mullo_epi32(i2, _mm256_set1_epi32(5352));

            let x0 = _mm256_add_epi32(t0, t3);
            let x3 = _mm256_sub_epi32(t0, t3);
            let x1 = _mm256_add_epi32(t0, t2);
            let x2 = _mm256_sub_epi32(t0, t2);

            // odd part
            let i3 = row3;
            let i1 = row1;

            let p5 = _mm256_mullo_epi32(_mm256_add_epi32(i3, i1), _mm256_set1_epi32(4816));

            let p1 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i1, _mm256_set1_epi32(-3685)));
            let p2 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i3, _mm256_set1_epi32(-10497)));

            let t3 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i1, _mm256_set1_epi32(867)));
            let t2 = _mm256_add_epi32(p5, _mm256_mullo_epi32(i3, _mm256_set1_epi32(-5945)));

            let t1 = _mm256_add_epi32(p2, _mm256_mullo_epi32(i1, _mm256_set1_epi32(-1597)));
            let t0 = _mm256_add_epi32(p1, _mm256_mullo_epi32(i3, _mm256_set1_epi32(-8034)));

            row0 = _mm256_srai_epi32::<17>(_mm256_add_epi32(x0, t3));
            row1 = _mm256_srai_epi32::<17>(_mm256_add_epi32(x1, t2));
            row2 = _mm256_srai_epi32::<17>(_mm256_add_epi32(x2, t1));
            row3 = _mm256_srai_epi32::<17>(_mm256_add_epi32(x3, t0));

            row4 = _mm256_srai_epi32::<17>(_mm256_sub_epi32(x3, t0));
            row5 = _mm256_srai_epi32::<17>(_mm256_sub_epi32(x2, t1));
            row6 = _mm256_srai_epi32::<17>(_mm256_sub_epi32(x1, t2));
            row7 = _mm256_srai_epi32::<17>(_mm256_sub_epi32(x0, t3));
        }

        transpose(
            &mut row0, &mut row1, &mut row2, &mut row3, &mut row4, &mut row5, &mut row6, &mut row7,
        );

        let mut pos = 0;

        // pack 并写回数组
        permute_store!(row0, row1, pos, out_vector, stride);
        permute_store!(row2, row3, pos, out_vector, stride);
        permute_store!(row4, row5, pos, out_vector, stride);
        permute_store!(row6, row7, pos, out_vector, stride);
    }

    /// 256-bit 零向量清零 `i32` 窗口（`tmp` 块装配用；字节恒等的写零）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn zero_i32_window(tmp: &mut [i32; 64], clobber_len: usize) {
        let zero = _mm256_setzero_si256();
        let mut i = 0;
        while i + 8 <= clobber_len {
            _mm256_storeu_si256(tmp[i..].as_mut_ptr().cast(), zero);
            i += 8;
        }
        for v in &mut tmp[i..clobber_len] {
            *v = 0;
        }
    }

    // -----------------------------------------------------------------------
    // 颜色换算（复刻 `color_convert/avx.rs`）
    // -----------------------------------------------------------------------
    use crate::codecs::jpeg::color as color;
    use crate::codecs::jpeg::upsampler as upsampler;
    use color::CB_CF;
    use color::C_G_CB_COEF_2;
    use color::C_G_CR_COEF_1;
    use color::CR_CF;
    use color::YUV_RND;
    use color::Y_CF;

    // 复用标量颜色模块的系数常量构造 madd 用的 i32 系数（小端：低 16 位为第一个系数）。
    const R_AVX_COEF: i32 =
        i32::from_ne_bytes([CR_CF.to_ne_bytes()[0], CR_CF.to_ne_bytes()[1], 0, 0]);
    const B_AVX_COEF: i32 =
        i32::from_ne_bytes([0, 0, CB_CF.to_ne_bytes()[0], CB_CF.to_ne_bytes()[1]]);
    const G_COEF_AVX_COEF: i32 = i32::from_ne_bytes([
        C_G_CR_COEF_1.to_ne_bytes()[0],
        C_G_CR_COEF_1.to_ne_bytes()[1],
        C_G_CB_COEF_2.to_ne_bytes()[0],
        C_G_CB_COEF_2.to_ne_bytes()[1],
    ]);

    /// 基础 YCbCr→RGB（不做 clamp，供 RGB/RGBA 两路径复用；复刻 `avx.rs::ycbcr_to_rgb_baseline_no_clamp`）。
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn ycbcr_to_rgb_baseline_no_clamp(
        y: &[i16; 16],
        cb: &[i16; 16],
        cr: &[i16; 16],
    ) -> (__m256i, __m256i, __m256i) {
        // 载入 {y, cb, cr} 各 16 个 i16
        let y_c = _mm256_loadu_si256(y.as_ptr().cast());
        let cb_c = _mm256_loadu_si256(cb.as_ptr().cast());
        let cr_c = _mm256_loadu_si256(cr.as_ptr().cast());

        let y_coeff = _mm256_set1_epi32(i32::from(Y_CF));
        let cr_coeff = _mm256_set1_epi32(R_AVX_COEF);
        let cb_coeff = _mm256_set1_epi32(B_AVX_COEF);
        let cg_coeff = _mm256_set1_epi32(G_COEF_AVX_COEF);
        let v_rnd = _mm256_set1_epi32(i32::from(YUV_RND));
        let uv_bias = _mm256_set1_epi16(128);

        // UV 打包成 [U,V]（x86 小端）：`cb<<8 | cr` → 每 i16 lane 字节为 [cr, cb]
        let v_0 = _mm256_slli_epi16::<8>(cb_c);
        let u_v_8 = _mm256_or_si256(v_0, cr_c);

        let mut u_v_lo = _mm256_unpacklo_epi8(u_v_8, _mm256_setzero_si256());
        let mut u_v_hi = _mm256_unpackhi_epi8(u_v_8, _mm256_setzero_si256());

        let mut y_lo = _mm256_unpacklo_epi16(y_c, _mm256_setzero_si256());
        let mut y_hi = _mm256_unpackhi_epi16(y_c, _mm256_setzero_si256());

        u_v_lo = _mm256_sub_epi16(u_v_lo, uv_bias);
        u_v_hi = _mm256_sub_epi16(u_v_hi, uv_bias);

        y_lo = _mm256_madd_epi16(y_lo, y_coeff);
        y_hi = _mm256_madd_epi16(y_hi, y_coeff);

        let mut r_lo = _mm256_madd_epi16(u_v_lo, cr_coeff);
        let mut r_hi = _mm256_madd_epi16(u_v_hi, cr_coeff);

        let mut g_lo = _mm256_madd_epi16(u_v_lo, cg_coeff);
        let mut g_hi = _mm256_madd_epi16(u_v_hi, cg_coeff);

        // 该次序利于降低寄存器压力
        y_lo = _mm256_add_epi32(y_lo, v_rnd);
        y_hi = _mm256_add_epi32(y_hi, v_rnd);

        let mut b_lo = _mm256_madd_epi16(u_v_lo, cb_coeff);
        let mut b_hi = _mm256_madd_epi16(u_v_hi, cb_coeff);

        r_lo = _mm256_add_epi32(r_lo, y_lo);
        r_hi = _mm256_add_epi32(r_hi, y_hi);

        g_lo = _mm256_add_epi32(g_lo, y_lo);
        g_hi = _mm256_add_epi32(g_hi, y_hi);

        b_lo = _mm256_add_epi32(b_lo, y_lo);
        b_hi = _mm256_add_epi32(b_hi, y_hi);

        r_lo = _mm256_srai_epi32::<14>(r_lo);
        r_hi = _mm256_srai_epi32::<14>(r_hi);

        g_lo = _mm256_srai_epi32::<14>(g_lo);
        g_hi = _mm256_srai_epi32::<14>(g_hi);

        b_lo = _mm256_srai_epi32::<14>(b_lo);
        b_hi = _mm256_srai_epi32::<14>(b_hi);

        let r = _mm256_packus_epi32(r_lo, r_hi);
        let g = _mm256_packus_epi32(g_lo, g_hi);
        let b = _mm256_packus_epi32(b_lo, b_hi);

        (r, g, b)
    }

    /// YCbCr → RGB（16 像素写 48 字节；复刻 `avx.rs::ycbcr_to_rgb_avx2`）。
    #[inline]
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn ycbcr_to_rgb_avx2(
        y: &[i16; 16],
        cb: &[i16; 16],
        cr: &[i16; 16],
        out: &mut [u8],
        offset: &mut usize,
    ) {
        let (mut r, mut g, mut b) = ycbcr_to_rgb_baseline_no_clamp(y, cb, cr);

        r = _mm256_packus_epi16(r, _mm256_setzero_si256());
        g = _mm256_packus_epi16(g, _mm256_setzero_si256());
        b = _mm256_packus_epi16(b, _mm256_setzero_si256());

        r = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>(r);
        g = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>(g);
        b = _mm256_permute4x64_epi64::<{ shuffle(3, 1, 2, 0) }>(b);

        let sh_r = _mm256_setr_epi8(
            0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2, 13, 8,
            3, 14, 9, 4, 15, 10, 5,
        );
        let sh_g = _mm256_setr_epi8(
            5, 0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2, 13,
            8, 3, 14, 9, 4, 15, 10,
        );
        let sh_b = _mm256_setr_epi8(
            10, 5, 0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2,
            13, 8, 3, 14, 9, 4, 15,
        );

        let r0 = _mm256_shuffle_epi8(r, sh_r);
        let g0 = _mm256_shuffle_epi8(g, sh_g);
        let b0 = _mm256_shuffle_epi8(b, sh_b);

        let m0 = _mm256_setr_epi8(
            0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0,
            0, -1, 0, 0, -1, 0, 0,
        );
        let m1 = _mm256_setr_epi8(
            0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, 0, -1, 0, 0, -1, 0, 0, -1,
            0, 0, -1, 0, 0, -1, 0,
        );

        let p0 = _mm256_blendv_epi8(_mm256_blendv_epi8(r0, g0, m0), b0, m1);
        let p1 = _mm256_blendv_epi8(_mm256_blendv_epi8(g0, b0, m0), r0, m1);
        let p2 = _mm256_blendv_epi8(_mm256_blendv_epi8(b0, r0, m0), g0, m1);

        let rgb0 = _mm256_permute2x128_si256::<32>(p0, p1);
        let rgb1 = _mm256_permute2x128_si256::<48>(p2, p0);

        _mm256_storeu_si256(out.as_mut_ptr().cast(), rgb0);
        _mm_storeu_si128(out[32..].as_mut_ptr().cast(), _mm256_castsi256_si128(rgb1));

        *offset += 48;
    }

    /// YCbCr → RGBA（16 像素写 64 字节，A=255；复刻 `avx.rs::ycbcr_to_rgba_avx2`）。
    #[inline]
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn ycbcr_to_rgba_avx2(
        y: &[i16; 16],
        cb: &[i16; 16],
        cr: &[i16; 16],
        out: &mut [u8],
        offset: &mut usize,
    ) {
        let tmp: &mut [u8; 64] = out
            .get_mut(*offset..*offset + 64)
            .expect("Slice to small cannot write")
            .try_into()
            .unwrap();

        let (r, g, b) = ycbcr_to_rgb_baseline_no_clamp(y, cb, cr);

        // pack 成 u8（无符号饱和）：c=RG 交错、d=BA(255) 交错
        let c = _mm256_packus_epi16(r, g);
        let d = _mm256_packus_epi16(b, _mm256_set1_epi16(255));
        // 转置 + 通道交织
        let e = _mm256_unpacklo_epi8(c, d);
        let f = _mm256_unpackhi_epi8(c, d);
        let g = _mm256_unpacklo_epi8(e, f);
        let h = _mm256_unpackhi_epi8(e, f);

        // 反 packus 打乱
        let i = _mm256_permute2x128_si256::<{ shuffle(3, 2, 1, 0) }>(g, h);
        let j = _mm256_permute2x128_si256::<{ shuffle(1, 2, 3, 0) }>(g, h);
        let k = _mm256_permute2x128_si256::<{ shuffle(3, 2, 0, 1) }>(g, h);
        let l = _mm256_permute2x128_si256::<{ shuffle(0, 3, 2, 1) }>(g, h);

        let m = _mm256_blend_epi32::<0b1111_0000>(i, j);
        let n = _mm256_blend_epi32::<0b1111_0000>(k, l);

        // 存
        _mm256_storeu_si256(tmp.as_mut_ptr().cast(), m);
        _mm256_storeu_si256(tmp[32..].as_mut_ptr().cast(), n);

        *offset += 64;
    }

    // -----------------------------------------------------------------------
    // 上采样（复刻 `upsampler/avx2.rs`）
    // -----------------------------------------------------------------------

    /// 水平 2x 双线性上采样（len≥18 走向量，小宽回退标量）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn upsample_horizontal_avx2(
        input: &[i16],
        in_near: &[i16],
        in_far: &[i16],
        scratch: &mut [i16],
        output: &mut [i16],
    ) {
        assert_eq!(input.len() * 2, output.len());
        assert!(input.len() > 2);

        let len = input.len();

        if len < 18 {
            return upsampler::upsample_horizontal(input, in_near, in_far, scratch, output);
        }

        // 前两个像素
        output[0] = input[0];
        output[1] = (input[0] * 3 + input[1] + 2) >> 2;

        let v_three = _mm256_set1_epi16(3);
        let v_two = _mm256_set1_epi16(2);

        let upsample16 = |input: &[i16; 18], output: &mut [i16; 32]| {
            let in_ptr = input.as_ptr();
            let out_ptr = output.as_mut_ptr();

            // 载入 3 个窗口向量（共 18 个 i16，全界内）
            let (v_prev, v_curr, v_next) = unsafe {
                (
                    _mm256_loadu_si256(in_ptr.add(0) as *const __m256i),
                    _mm256_loadu_si256(in_ptr.add(1) as *const __m256i),
                    _mm256_loadu_si256(in_ptr.add(2) as *const __m256i),
                )
            };

            let v_common = _mm256_add_epi16(_mm256_mullo_epi16(v_curr, v_three), v_two);

            let v_even = _mm256_srai_epi16::<2>(_mm256_add_epi16(v_common, v_prev));
            let v_odd = _mm256_srai_epi16::<2>(_mm256_add_epi16(v_common, v_next));

            let v_res_1 = _mm256_unpacklo_epi16(v_even, v_odd);
            let v_res_2 = _mm256_unpackhi_epi16(v_even, v_odd);

            let v_final_1 = _mm256_permute2x128_si256::<0x20>(v_res_1, v_res_2);
            let v_final_2 = _mm256_permute2x128_si256::<0x31>(v_res_1, v_res_2);

            // 32 个 i16 输出全界内
            unsafe {
                _mm256_storeu_si256(out_ptr as *mut __m256i, v_final_1);
                _mm256_storeu_si256(out_ptr.add(16) as *mut __m256i, v_final_2);
            }
        };

        for (input, output) in input
            .windows(18)
            .step_by(16)
            .zip(output[2..].chunks_exact_mut(32))
        {
            upsample16(input.try_into().unwrap(), output.try_into().unwrap());
        }

        // 上采样余量（可能与上面有重叠，重叠处数值一致，无妨）
        if let Some(rest_input) = input.last_chunk::<18>() {
            let end = output.len() - 2;
            if let Some(rest_output) = output[..end].last_chunk_mut::<32>() {
                upsample16(rest_input, rest_output);
            }
        }

        // 最后两个像素
        output[output.len() - 2] = (3 * input[len - 1] + input[len - 2] + 2) >> 2;
        output[output.len() - 1] = input[len - 1];
    }

    /// 垂直 2x 双线性上采样（len≥16 走向量，小宽回退标量）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn upsample_vertical_avx2(
        input: &[i16],
        in_near: &[i16],
        in_far: &[i16],
        scratch: &mut [i16],
        output: &mut [i16],
    ) {
        assert_eq!(input.len() * 2, output.len());
        assert_eq!(in_near.len(), input.len());
        assert_eq!(in_far.len(), input.len());

        let len = input.len();

        if len < 16 {
            return upsampler::upsample_vertical(input, in_near, in_far, scratch, output);
        }

        let middle = output.len() / 2;
        let (out_top, out_bottom) = output.split_at_mut(middle);

        let v_three = _mm256_set1_epi16(3);
        let v_two = _mm256_set1_epi16(2);

        let upsample16 = |input: &[i16; 16],
                          in_near: &[i16; 16],
                          in_far: &[i16; 16],
                          out_top: &mut [i16; 16],
                          out_bottom: &mut [i16; 16]| {
            // 各 16 个 i16 全界内
            let (v_in, v_near, v_far) = unsafe {
                (
                    _mm256_loadu_si256(input.as_ptr() as *const __m256i),
                    _mm256_loadu_si256(in_near.as_ptr() as *const __m256i),
                    _mm256_loadu_si256(in_far.as_ptr() as *const __m256i),
                )
            };

            let v_common = _mm256_add_epi16(_mm256_mullo_epi16(v_in, v_three), v_two);

            let v_out_top = _mm256_srai_epi16::<2>(_mm256_add_epi16(v_common, v_near));
            let v_out_bottom = _mm256_srai_epi16::<2>(_mm256_add_epi16(v_common, v_far));

            // 各 16 个 i16 输出全界内
            unsafe {
                _mm256_storeu_si256(out_top.as_mut_ptr() as *mut __m256i, v_out_top);
                _mm256_storeu_si256(out_bottom.as_mut_ptr() as *mut __m256i, v_out_bottom);
            }
        };

        let chunks = input
            .chunks_exact(16)
            .zip(in_near.chunks_exact(16))
            .zip(in_far.chunks_exact(16))
            .zip(out_top.chunks_exact_mut(16))
            .zip(out_bottom.chunks_exact_mut(16));

        for ((((input, in_near), in_far), out_top), out_bottom) in chunks {
            upsample16(
                input.try_into().unwrap(),
                in_near.try_into().unwrap(),
                in_far.try_into().unwrap(),
                out_top.try_into().unwrap(),
                out_bottom.try_into().unwrap(),
            );
        }

        // 上采样余量（可能与上面有重叠，无妨）
        if let Some(rest) = input.last_chunk::<16>() {
            if let Some(rest_near) = in_near.last_chunk::<16>() {
                if let Some(rest_far) = in_far.last_chunk::<16>() {
                    if let Some(mut rest_top) = out_top.last_chunk_mut::<16>() {
                        if let Some(mut rest_bottom) = out_bottom.last_chunk_mut::<16>() {
                            upsample16(rest, rest_near, rest_far, &mut rest_top, &mut rest_bottom);
                        }
                    }
                }
            }
        }
    }

    /// 4:2:0（HV）2x2 上采样（先垂直后水平）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn upsample_hv_avx2(
        input: &[i16],
        in_near: &[i16],
        in_far: &[i16],
        scratch_space: &mut [i16],
        output: &mut [i16],
    ) {
        assert_eq!(input.len() * 4, output.len());
        assert!(input.len() * 2 <= scratch_space.len());
        let scratch_space = &mut scratch_space[..input.len() * 2];

        upsample_vertical_avx2(input, in_near, in_far, &mut [], scratch_space);

        let scratch_half = scratch_space.len() / 2;
        let output_half = output.len() / 2;

        let (scratch_top, scratch_bottom) = scratch_space.split_at_mut(scratch_half);
        let (out_top, out_bottom) = output.split_at_mut(output_half);

        let mut t = [0];
        upsample_horizontal_avx2(scratch_top, &[], &[], &mut t, out_top);
        upsample_horizontal_avx2(scratch_bottom, &[], &[], &mut t, out_bottom);
    }
}

// ---------------------------------------------------------------------------
// 门控分发（各热调用点使用；AVX2 关闭或无/不支持时回退标量）
// ---------------------------------------------------------------------------

/// 完整 8×8 IDCT 分发（`len>10` 路径）。
#[inline]
pub(crate) fn idct_int(in_vector: &mut [i32; 64], out_vector: &mut [i16], stride: usize) {
    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            kernels::idct_avx2(in_vector, out_vector, stride);
        }
        return;
    }
    idct_int_scalar(in_vector, out_vector, stride)
}

/// 4×4 IDCT 分发（`2<len<=10` 路径；`idct_int_1x1` 保持标量）。
#[inline]
pub(crate) fn idct4x4(in_vector: &mut [i32; 64], out_vector: &mut [i16], stride: usize) {
    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            kernels::idct_avx2_4x4(in_vector, out_vector, stride);
        }
        return;
    }
    idct4x4_scalar(in_vector, out_vector, stride)
}

/// 16 像素 YCbCr→RGB 分发（调用形状对齐标量 `ycbcr_to_rgb_inner_16_scalar`）。
#[inline]
pub(crate) fn ycbcr_to_rgb_16(
    y: &[i16; 16],
    cb: &[i16; 16],
    cr: &[i16; 16],
    output: &mut [u8],
    pos: &mut usize,
) {
    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            kernels::ycbcr_to_rgb_avx2(y, cb, cr, output, pos);
        }
        return;
    }
    ycbcr_to_rgb_inner_16_scalar::<false>(y, cb, cr, output, pos)
}

/// 16 像素 YCbCr→RGBA 分发（调用形状对齐标量 `ycbcr_to_rgba_inner_16_scalar`）。
#[inline]
pub(crate) fn ycbcr_to_rgba_16(
    y: &[i16; 16],
    cb: &[i16; 16],
    cr: &[i16; 16],
    output: &mut [u8],
    pos: &mut usize,
) {
    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            kernels::ycbcr_to_rgba_avx2(y, cb, cr, output, pos);
        }
        return;
    }
    ycbcr_to_rgba_inner_16_scalar::<false>(y, cb, cr, output, pos)
}

/// 水平 2x 上采样分发（UpSampler 签名）。
#[inline]
pub(crate) fn upsample_horizontal(
    input: &[i16],
    in_near: &[i16],
    in_far: &[i16],
    scratch: &mut [i16],
    output: &mut [i16],
) {
    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            kernels::upsample_horizontal_avx2(input, in_near, in_far, scratch, output);
        }
        return;
    }
    uh_scalar(input, in_near, in_far, scratch, output)
}

/// 垂直 2x 上采样分发（UpSampler 签名）。
#[inline]
pub(crate) fn upsample_vertical(
    input: &[i16],
    in_near: &[i16],
    in_far: &[i16],
    scratch: &mut [i16],
    output: &mut [i16],
) {
    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            kernels::upsample_vertical_avx2(input, in_near, in_far, scratch, output);
        }
        return;
    }
    uv_scalar(input, in_near, in_far, scratch, output)
}

/// 4:2:0（HV）上采样分发（UpSampler 签名）。
#[inline]
pub(crate) fn upsample_hv(
    input: &[i16],
    in_near: &[i16],
    in_far: &[i16],
    scratch: &mut [i16],
    output: &mut [i16],
) {
    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            kernels::upsample_hv_avx2(input, in_near, in_far, scratch, output);
        }
        return;
    }
    uhv_scalar(input, in_near, in_far, scratch, output)
}

/// 清零 `tmp[..clobber_len]`（i32 窗口）。AVX2 开启时用 256-bit 零向量 store，
/// 否则回退 `fill(0)`（memset）。字节恒等（写零），仅装配路径微调。
#[inline]
pub(crate) fn zero_tmp_window(tmp: &mut [i32; 64], clobber_len: usize) {
    #[cfg(target_arch = "x86_64")]
    if clobber_len >= 8 && avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 AVX2；clobber_len>=8 保证至少一个完整 256-bit。
        unsafe {
            kernels::zero_i32_window(tmp, clobber_len);
        }
        return;
    }
    tmp[..clobber_len].fill(0);
}

// ---------------------------------------------------------------------------
// 单测：标量 ≡ AVX2 逐字节恒等（硬闸）。CPU 无 AVX2 时直接 return 跳过。
// ---------------------------------------------------------------------------
#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::kernels;
    use crate::codecs::jpeg::color::{ycbcr_to_rgb_inner_16_scalar, ycbcr_to_rgba_inner_16_scalar};
    use crate::codecs::jpeg::idct::{idct4x4, idct_int};
    use crate::codecs::jpeg::upsampler::{upsample_horizontal, upsample_hv, upsample_vertical};

    fn avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }

    /// 简单 LCG（xorshift 风格），确定性伪随机。
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_add(0x9E3779B97F4A7C15))
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        /// [-bound, bound) 内随机 i32。
        fn i32(&mut self, bound: i32) -> i32 {
            let r = if bound <= 0 {
                0
            } else {
                ((self.next() >> 32) as i32).rem_euclid(bound)
            };
            r
        }
        fn i16_0_to_255(&mut self) -> i16 {
            ((self.next() >> 32) % 256) as i16
        }
        fn i16_mid(&mut self) -> i16 {
            (self.i32(512) - 256) as i16
        }
    }

    /// 随机 8×8 系数块（-bound..bound；含少量极端值）。
    fn rand_block(rng: &mut Lcg, bound: i32) -> [i32; 64] {
        let mut b = [0i32; 64];
        // 极端饱和值也有（模拟量化表放大）
        for v in &mut b[..8] {
            *v = if (rng.next() & 7) == 0 {
                [i32::MIN / 1024, i32::MAX / 1024, -1 << 24, 1 << 24][(rng.next() % 4) as usize]
            } else {
                rng.i32(bound)
            };
        }
        for v in &mut b[8..] {
            *v = rng.i32(bound);
        }
        b
    }

    #[test]
    fn idct_int_scalar_eq_avx2() {
        if !avx2() {
            return;
        }
        let mut rng = Lcg::new(0x1111);
        // 多种形态：全零 / 纯 DC / 稀疏 / 稠密
        for case in 0..400 {
            let mut in_s: [i32; 64];
            if case % 4 == 0 {
                in_s = [0i32; 64];
            } else if case % 4 == 1 {
                in_s = [0i32; 64];
                in_s[0] = rng.i32(4096);
            } else {
                in_s = rand_block(&mut rng, 30000);
            }
            let mut in_v = in_s;

            let mut out_s = [0i16; 64];
            let mut out_v = [0i16; 64];
            idct_int(&mut in_s, &mut out_s, 8);
            // SAFETY: 测试已门控 AVX2 支持
            unsafe {
                kernels::idct_avx2(&mut in_v, &mut out_v, 8);
            }
            assert_eq!(out_s, out_v, "idct_int 案例 {case} 不恒等");
        }
    }

    /// 稠密 full-idct 吞吐微基准（release `#[ignore]`）：逐块 `idct_avx2`（每次函数调用）vs 标量。
    /// 2026-11-05 批量 IDCT 调研沉淀：直接回答「单块内核逐块调用（Win64 callee-saved xmm 盘存 /
    /// 常数重载）是否构成可摊薄的大头」。实测（本机）single_avx2=74ns/block、scalar=569ns/block，
    /// batch(k=4)=77ns/block → 逐块调用开销≈中性，批量集成反而加拷贝/收口净负 → 判定不投。可复用。
    #[test]
    #[ignore]
    fn idct_throughput_microbench() {
        if !avx2() {
            println!("IDCT_MICRO skip: no avx2");
            return;
        }
        use std::time::Instant;

        // 语料：稠密随机块（模拟全 full-idct 图像如 Chipboard）
        let mut rng = Lcg::new(0x7777);
        let blocks_n = 128usize;
        let mut blocks: Vec<[i32; 64]> =
            (0..blocks_n).map(|_| rand_block(&mut rng, 30000)).collect();
        let mut out = vec![0i16; blocks_n * 8 * 8];

        let iters = 20_000usize;
        let mut sink: u64 = 0;

        // 逐块 AVX2（K=1 函数调用形态）
        let t0 = Instant::now();
        for _ in 0..iters {
            for (i, blk) in blocks.iter_mut().enumerate() {
                // SAFETY: 已门控 AVX2
                unsafe {
                    kernels::idct_avx2(blk, &mut out[i * 8..], 8);
                }
            }
            sink = sink.wrapping_add(out[0] as u64);
        }
        let t_single = t0.elapsed().as_secs_f64() * 1e9 / (iters as f64 * blocks_n as f64);

        // 标量
        let t0 = Instant::now();
        for _ in 0..iters {
            for (i, blk) in blocks.iter_mut().enumerate() {
                crate::codecs::jpeg::idct::idct_int(blk, &mut out[i * 8..], 8);
            }
            sink = sink.wrapping_add(out[0] as u64);
        }
        let t_scalar = t0.elapsed().as_secs_f64() * 1e9 / (iters as f64 * blocks_n as f64);

        println!(
            "IDCT_MICRO ns/block single_avx2={t_single:.2} scalar={t_scalar:.2} (scalar/single={:.3})",
            t_scalar / t_single
        );
        assert_ne!(sink, u64::MAX); // 防优化
    }

    #[test]
    fn idct4x4_scalar_eq_avx2() {
        if !avx2() {
            return;
        }
        let mut rng = Lcg::new(0x2222);
        for case in 0..200 {
            // 仅左上 4×4 子块非零
            let mut in_s = [0i32; 64];
            for r in 0..4 {
                for c in 0..4 {
                    in_s[r * 8 + c] = rng.i32(30000);
                }
            }
            let mut in_v = in_s;

            let mut out_s = [0i16; 64];
            let mut out_v = [0i16; 64];
            idct4x4(&mut in_s, &mut out_s, 8);
            // SAFETY: 测试已门控 AVX2 支持
            unsafe {
                kernels::idct_avx2_4x4(&mut in_v, &mut out_v, 8);
            }
            assert_eq!(out_s, out_v, "idct4x4 案例 {case} 不恒等");
        }
    }

    #[test]
    fn color_scalar_eq_avx2() {
        if !avx2() {
            return;
        }
        let mut rng = Lcg::new(0x3333);
        for case in 0..200 {
            let y: [i16; 16] = core::array::from_fn(|_| rng.i16_0_to_255());
            let cb: [i16; 16] = core::array::from_fn(|_| rng.i16_0_to_255());
            let cr: [i16; 16] = core::array::from_fn(|_| rng.i16_0_to_255());

            let mut out_s = [0u8; 64];
            let mut out_v = [0u8; 64];
            let mut pos_s = 0usize;
            let mut pos_v = 0usize;
            ycbcr_to_rgb_inner_16_scalar::<false>(&y, &cb, &cr, &mut out_s, &mut pos_s);
            // SAFETY: 测试已门控 AVX2 支持
            unsafe {
                kernels::ycbcr_to_rgb_avx2(&y, &cb, &cr, &mut out_v, &mut pos_v);
            }
            assert_eq!(&out_s[..48], &out_v[..48], "rgb 案例 {case} 不恒等");

            let mut out_s = [0u8; 64];
            let mut out_v = [0u8; 64];
            let mut pos_s = 0usize;
            let mut pos_v = 0usize;
            ycbcr_to_rgba_inner_16_scalar::<false>(&y, &cb, &cr, &mut out_s, &mut pos_s);
            // SAFETY: 测试已门控 AVX2 支持
            unsafe {
                kernels::ycbcr_to_rgba_avx2(&y, &cb, &cr, &mut out_v, &mut pos_v);
            }
            assert_eq!(&out_s[..64], &out_v[..64], "rgba 案例 {case} 不恒等");
        }
    }

    #[test]
    fn upsampler_scalar_eq_avx2() {
        if !avx2() {
            return;
        }
        let mut rng = Lcg::new(0x4444);

        // 水平：涵盖 <18（AVX 内回落标量）、边界 18/19/20、与批量 32/48
        for &len in &[8usize, 16, 18, 19, 20, 31, 32, 33, 48, 65] {
            let input: Vec<i16> = (0..len).map(|_| rng.i16_mid()).collect();
            let mut out_s = vec![0i16; input.len() * 2];
            let mut out_v = vec![0i16; input.len() * 2];
            upsample_horizontal(&input, &[], &[], &mut [], &mut out_s);
            // SAFETY: 测试已门控 AVX2 支持
            unsafe {
                kernels::upsample_horizontal_avx2(&input, &[], &[], &mut [], &mut out_v);
            }
            assert_eq!(out_s, out_v, "水平上采样 len={len} 不恒等");
        }

        // 垂直：<16 与边界/批量
        for &len in &[8usize, 15, 16, 17, 32, 48, 65] {
            let input: Vec<i16> = (0..len).map(|_| rng.i16_mid()).collect();
            let in_near: Vec<i16> = (0..len).map(|_| rng.i16_mid()).collect();
            let in_far: Vec<i16> = (0..len).map(|_| rng.i16_mid()).collect();
            let mut out_s = vec![0i16; input.len() * 2];
            let mut out_v = vec![0i16; input.len() * 2];
            upsample_vertical(&input, &in_near, &in_far, &mut [], &mut out_s);
            // SAFETY: 测试已门控 AVX2 支持
            unsafe {
                kernels::upsample_vertical_avx2(&input, &in_near, &in_far, &mut [], &mut out_v);
            }
            assert_eq!(out_s, out_v, "垂直上采样 len={len} 不恒等");
        }

        // HV：scratch ≥ 2*len
        for &len in &[8usize, 16, 17, 32, 48] {
            let input: Vec<i16> = (0..len).map(|_| rng.i16_mid()).collect();
            let in_near: Vec<i16> = (0..len).map(|_| rng.i16_mid()).collect();
            let in_far: Vec<i16> = (0..len).map(|_| rng.i16_mid()).collect();
            let mut scratch_s = vec![0i16; len * 2];
            let mut scratch_v = vec![0i16; len * 2];
            let mut out_s = vec![0i16; input.len() * 4];
            let mut out_v = vec![0i16; input.len() * 4];
            upsample_hv(&input, &in_near, &in_far, &mut scratch_s, &mut out_s);
            // SAFETY: 测试已门控 AVX2 支持
            unsafe {
                kernels::upsample_hv_avx2(&input, &in_near, &in_far, &mut scratch_v, &mut out_v);
            }
            assert_eq!(out_s, out_v, "HV 上采样 len={len} 不恒等");
        }
    }
}

