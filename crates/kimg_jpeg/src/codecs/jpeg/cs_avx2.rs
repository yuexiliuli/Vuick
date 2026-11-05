//! C# 数值核心的 AVX2 向量化端口（逐字节恒等 `DecodeTo`）。
//!
//! 分别镜像 `Sdcb.SimdCodecs.Jpeg` 的 `JpegIdct.Idct8x8V256`、
//! `JpegResample.*V256`、`JpegColor.YccRow*V256`（+ `PackSaturate`）。忠实于参照：
//! - IDCT：8 lane = 8 列（列 pass 一次算 8 列）/ 8 行（行 pass），lane 内 op 序与
//!   标量 `cs::idct_1d` 完全一致；两次 `Transpose` 还原行序。
//! - 颜色：8 lane = 8 像素，`g=(y+cb·KGb)+cr·KGr` 括号序在 lane 内保留；取整
//!   `trunc(v+0.5)` + 饱和 pack（= `ToByte` 的 min/max + trunc，偶像素层恒等）。
//! - 上采样：纯整数 u16/i16 逐元素，`T=3n+f`、`(v+8)>>4` 与标量逐位恒等。
//!
//! 硬纪律（K93）：所有内核 `#[target_feature(enable = "avx2")]`（禁 fma）；调用侧经
//! `avx2::avx2_enabled()` 门控后 unsafe 调用；AVX2 分支内不调用非 target_feature 的
//! 浮点共享代码，标量尾一律就地镜像成 AVX2 帧内联代码（整数助手如 `t_cs` 用
//! `#[inline(always)]`）。非 x86_64 编译期整模块空。

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

use super::cs::{
    output_count, BIAS, C1, C2, C3, C5, C6, C7, K4, KB, KGB, KGR, KR, R_H2, R_HV2, R_ONE_TO_ONE,
    R_V2,
};

#[cfg(target_arch = "x86_64")]
mod kernels {
    use super::*;
    use core::arch::x86_64::*;

    // ------------------------------------------------------------------
    // f32 1-D 变换 & 8×8 浮点转置（`JpegIdct.Idct1D(Vector256<f32>)` / `Transpose`）
    // ------------------------------------------------------------------

    /// `Idct1D(Vector256<float>)` 逐句镜像：每 lane 的 op 序 = `cs::idct_1d` 标量。
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments)]
    unsafe fn idct_1d256(
        f0: __m256,
        f1: __m256,
        f2: __m256,
        f3: __m256,
        f4: __m256,
        f5: __m256,
        f6: __m256,
        f7: __m256,
    ) -> (__m256, __m256, __m256, __m256, __m256, __m256, __m256, __m256) {
        let k4 = _mm256_set1_ps(K4);
        let k2 = _mm256_set1_ps(C2);
        let k6 = _mm256_set1_ps(C6);
        let k1 = _mm256_set1_ps(C1);
        let k3 = _mm256_set1_ps(C3);
        let k5 = _mm256_set1_ps(C5);
        let k7 = _mm256_set1_ps(C7);
        let a0 = _mm256_mul_ps(_mm256_add_ps(f0, f4), k4);
        let a1 = _mm256_mul_ps(_mm256_sub_ps(f0, f4), k4);
        let b0 = _mm256_add_ps(_mm256_mul_ps(f2, k2), _mm256_mul_ps(f6, k6));
        let b1 = _mm256_sub_ps(_mm256_mul_ps(f2, k6), _mm256_mul_ps(f6, k2));
        let e0 = _mm256_add_ps(a0, b0);
        let e1 = _mm256_add_ps(a1, b1);
        let e2 = _mm256_sub_ps(a1, b1);
        let e3 = _mm256_sub_ps(a0, b0);
        let o0 = _mm256_add_ps(
            _mm256_add_ps(_mm256_add_ps(_mm256_mul_ps(f1, k1), _mm256_mul_ps(f3, k3)),
                _mm256_mul_ps(f5, k5)),
            _mm256_mul_ps(f7, k7),
        );
        let o1 = _mm256_sub_ps(
            _mm256_sub_ps(_mm256_sub_ps(_mm256_mul_ps(f1, k3), _mm256_mul_ps(f3, k7)),
                _mm256_mul_ps(f5, k1)),
            _mm256_mul_ps(f7, k5),
        );
        let o2 = _mm256_add_ps(
            _mm256_add_ps(_mm256_sub_ps(_mm256_mul_ps(f1, k5), _mm256_mul_ps(f3, k1)),
                _mm256_mul_ps(f5, k7)),
            _mm256_mul_ps(f7, k3),
        );
        let o3 = _mm256_sub_ps(
            _mm256_add_ps(
                _mm256_sub_ps(_mm256_mul_ps(f1, k7), _mm256_mul_ps(f3, k5)),
                _mm256_mul_ps(f5, k3),
            ),
            _mm256_mul_ps(f7, k1),
        );
        let s0 = _mm256_add_ps(e0, o0);
        let s1 = _mm256_add_ps(e1, o1);
        let s2 = _mm256_add_ps(e2, o2);
        let s3 = _mm256_add_ps(e3, o3);
        let s4 = _mm256_sub_ps(e3, o3);
        let s5 = _mm256_sub_ps(e2, o2);
        let s6 = _mm256_sub_ps(e1, o1);
        let s7 = _mm256_sub_ps(e0, o0);
        (s0, s1, s2, s3, s4, s5, s6, s7)
    }

    /// 8×8 f32 转置（`JpegIdct.Transpose` Vector256 分支逐句镜像）。
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments)]
    unsafe fn transpose_ps(
        r0: &mut __m256,
        r1: &mut __m256,
        r2: &mut __m256,
        r3: &mut __m256,
        r4: &mut __m256,
        r5: &mut __m256,
        r6: &mut __m256,
        r7: &mut __m256,
    ) {
        let t0 = _mm256_unpacklo_ps(*r0, *r1);
        let t1 = _mm256_unpackhi_ps(*r0, *r1);
        let t2 = _mm256_unpacklo_ps(*r2, *r3);
        let t3 = _mm256_unpackhi_ps(*r2, *r3);
        let t4 = _mm256_unpacklo_ps(*r4, *r5);
        let t5 = _mm256_unpackhi_ps(*r4, *r5);
        let t6 = _mm256_unpacklo_ps(*r6, *r7);
        let t7 = _mm256_unpackhi_ps(*r6, *r7);
        let u0 = _mm256_shuffle_ps(t0, t2, 0x44);
        let u1 = _mm256_shuffle_ps(t0, t2, 0xEE);
        let u2 = _mm256_shuffle_ps(t1, t3, 0x44);
        let u3 = _mm256_shuffle_ps(t1, t3, 0xEE);
        let u4 = _mm256_shuffle_ps(t4, t6, 0x44);
        let u5 = _mm256_shuffle_ps(t4, t6, 0xEE);
        let u6 = _mm256_shuffle_ps(t5, t7, 0x44);
        let u7 = _mm256_shuffle_ps(t5, t7, 0xEE);
        *r0 = _mm256_permute2f128_ps(u0, u4, 0x20);
        *r1 = _mm256_permute2f128_ps(u1, u5, 0x20);
        *r2 = _mm256_permute2f128_ps(u2, u6, 0x20);
        *r3 = _mm256_permute2f128_ps(u3, u7, 0x20);
        *r4 = _mm256_permute2f128_ps(u0, u4, 0x31);
        *r5 = _mm256_permute2f128_ps(u1, u5, 0x31);
        *r6 = _mm256_permute2f128_ps(u2, u6, 0x31);
        *r7 = _mm256_permute2f128_ps(u3, u7, 0x31);
    }

    /// 装载 8 个 i16（自然序一行）→ 8 个 f32（`JpegIdct.LoadRow256`）。
    #[target_feature(enable = "avx2")]
    unsafe fn load_row_f32(p: *const i16, row: usize) -> __m256 {
        let p = p.add(row * 8) as *const __m128i;
        let v = _mm256_cvtepi16_epi32(_mm_loadu_si128(p));
        _mm256_cvtepi32_ps(v)
    }

    /// 完整 8×8 IDCT（`JpegIdct.Idct8x8V256`；无 half/sparse 化简，与 `cs::idct8x8` 全量一致）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn idct8x8_avx2(data: &[i16; 64], out: &mut [u8], offset: usize, stride: usize) {
        let d = data.as_ptr();
        let f0 = load_row_f32(d, 0);
        let f1 = load_row_f32(d, 1);
        let f2 = load_row_f32(d, 2);
        let f3 = load_row_f32(d, 3);
        let f4 = load_row_f32(d, 4);
        let f5 = load_row_f32(d, 5);
        let f6 = load_row_f32(d, 6);
        let f7 = load_row_f32(d, 7);

        // 列 pass（lane j = 列 j）
        let (mut r0, mut r1, mut r2, mut r3, mut r4, mut r5, mut r6, mut r7) =
            idct_1d256(f0, f1, f2, f3, f4, f5, f6, f7);
        transpose_ps(&mut r0, &mut r1, &mut r2, &mut r3, &mut r4, &mut r5, &mut r6, &mut r7);
        // 行 pass（lane j = 行 j）
        let (mut c0, mut c1, mut c2, mut c3, mut c4, mut c5, mut c6, mut c7) =
            idct_1d256(r0, r1, r2, r3, r4, r5, r6, r7);
        transpose_ps(&mut c0, &mut c1, &mut c2, &mut c3, &mut c4, &mut c5, &mut c6, &mut c7);

        // 输出：trunc(s + 128.5) + 饱和 pack（= `ToSample` 的 trunc + clamp[0,255]）。
        // packssdw→packuswb≡`trunc(min(max(s+128.5,0),255))`（C# 注释同款论证）。
        let bias = _mm256_set1_ps(BIAS);
        let p01 = _mm256_packs_epi32(
            _mm256_cvttps_epi32(_mm256_add_ps(c0, bias)),
            _mm256_cvttps_epi32(_mm256_add_ps(c1, bias)),
        );
        let p23 = _mm256_packs_epi32(
            _mm256_cvttps_epi32(_mm256_add_ps(c2, bias)),
            _mm256_cvttps_epi32(_mm256_add_ps(c3, bias)),
        );
        let p45 = _mm256_packs_epi32(
            _mm256_cvttps_epi32(_mm256_add_ps(c4, bias)),
            _mm256_cvttps_epi32(_mm256_add_ps(c5, bias)),
        );
        let p67 = _mm256_packs_epi32(
            _mm256_cvttps_epi32(_mm256_add_ps(c6, bias)),
            _mm256_cvttps_epi32(_mm256_add_ps(c7, bias)),
        );
        let row_order = _mm256_setr_epi32(0, 4, 1, 5, 2, 6, 3, 7);
        let b0123 = _mm256_permutevar8x32_epi32(_mm256_packus_epi16(p01, p23), row_order);
        let b4567 = _mm256_permutevar8x32_epi32(_mm256_packus_epi16(p45, p67), row_order);
        let dst = out.as_mut_ptr().add(offset);
        let l0 = _mm256_extracti128_si256::<0>(b0123);
        let l1 = _mm256_extracti128_si256::<1>(b0123);
        let l2 = _mm256_extracti128_si256::<0>(b4567);
        let l3 = _mm256_extracti128_si256::<1>(b4567);
        let rows = [
            _mm_cvtsi128_si64(l0) as u64,
            _mm_extract_epi64::<1>(l0) as u64,
            _mm_cvtsi128_si64(l1) as u64,
            _mm_extract_epi64::<1>(l1) as u64,
            _mm_cvtsi128_si64(l2) as u64,
            _mm_extract_epi64::<1>(l2) as u64,
            _mm_cvtsi128_si64(l3) as u64,
            _mm_extract_epi64::<1>(l3) as u64,
        ];
        for (r, &v) in rows.iter().enumerate() {
            core::ptr::write_unaligned(dst.add(r * stride) as *mut u64, v);
        }
    }

    // ------------------------------------------------------------------
    // 16× 整数色度上采样（`JpegResample.*V256` + 标量尾；纯整数无取整）
    // ------------------------------------------------------------------

    /// `T(i) = 3·near[i] + far[i]`（与 `cs::t` 同义，AVX2 帧内就地镜像）。
    #[inline(always)]
    fn t_cs(plane: &[u8], near: usize, far: usize, i: usize) -> i32 {
        3 * i32::from(plane[near + i]) + i32::from(plane[far + i])
    }

    /// 16 个 u8 → 16 个 u16（零扩展；`JpegResample.Load16`）。
    #[target_feature(enable = "avx2")]
    unsafe fn load16_u8_epi16(p: *const u8) -> __m256i {
        _mm256_cvtepu8_epi16(_mm_loadu_si128(p as *const __m128i))
    }

    /// 按 `kind` 走对应向量核 + 标量尾，整行写满 `dst`（`JpegResample.Row16` 等义）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn row16_avx2(
        kind: u8,
        dst: &mut [u16],
        plane: &[u8],
        in_near: usize,
        in_far: usize,
        w: usize,
        hs: usize,
    ) {
        let dst_p = dst.as_mut_ptr();
        let three = _mm256_set1_epi16(3);
        match kind {
            R_ONE_TO_ONE => {
                let near = plane.as_ptr().add(in_near);
                let mut i = 0usize;
                while i + 16 <= w {
                    let v = _mm256_slli_epi16(load16_u8_epi16(near.add(i)), 4);
                    _mm256_storeu_si256(dst_p.add(i) as *mut __m256i, v);
                    i += 16;
                }
                for i in i..w {
                    dst[i] = (u16::from(plane[in_near + i])) << 4;
                }
            }
            R_V2 => {
                let near = plane.as_ptr().add(in_near);
                let far = plane.as_ptr().add(in_far);
                let mut i = 0usize;
                while i + 16 <= w {
                    let a = _mm256_mullo_epi16(load16_u8_epi16(near.add(i)), three);
                    let b = _mm256_add_epi16(a, load16_u8_epi16(far.add(i)));
                    let v = _mm256_slli_epi16(b, 2);
                    _mm256_storeu_si256(dst_p.add(i) as *mut __m256i, v);
                    i += 16;
                }
                for i in i..w {
                    dst[i] = ((3 * i32::from(plane[in_near + i]) + i32::from(plane[in_far + i])) << 2)
                        as u16;
                }
            }
            R_H2 | R_HV2 => {
                // H2：near≡far；HV2：竖直混合近/远源行（`cs::row16` 同款路由）
                let far = if kind == R_H2 { in_near } else { in_far };
                hv2_row_avx2(dst_p, plane, in_near, far, w, three);
            }
            _ => {
                // Generic：逐 i 复制 hs 份（`cs::row16` 同款标量）。
                for i in 0..w {
                    let v = (u16::from(plane[in_near + i])) << 4;
                    for j in 0..hs {
                        dst[i * hs + j] = v;
                    }
                }
            }
        }
    }

    /// `Hv2Row`（`JpegResample.Hv2Row` + `Hv2V256`）：边缘竖直-only 混合，向量覆盖
    /// i=1..w-1 的连续 16 位组，余量回标量。整数逐位恒等。
    #[target_feature(enable = "avx2")]
    unsafe fn hv2_row_avx2(dst: *mut u16, plane: &[u8], near: usize, far: usize, w: usize, three: __m256i) {
        let t1_0 = t_cs(plane, near, far, 0);
        if w == 1 {
            let v = (t1_0 << 2) as u16;
            *dst = v;
            *dst.add(1) = v;
            return;
        }
        *dst = (t1_0 << 2) as u16;
        let np = plane.as_ptr().add(near);
        let fp = plane.as_ptr().add(far);
        let mut i = 1usize;
        while i + 16 <= w {
            let nprev = np.add(i - 1);
            let fprev = fp.add(i - 1);
            let tp = _mm256_add_epi16(_mm256_mullo_epi16(load16_u8_epi16(nprev), three), load16_u8_epi16(fprev));
            let tc = _mm256_add_epi16(_mm256_mullo_epi16(load16_u8_epi16(np.add(i)), three), load16_u8_epi16(fp.add(i)));
            let a = _mm256_add_epi16(_mm256_mullo_epi16(tp, three), tc);
            let b = _mm256_add_epi16(_mm256_mullo_epi16(tc, three), tp);
            let lo = _mm256_unpacklo_epi16(a, b);
            let hi = _mm256_unpackhi_epi16(a, b);
            let p0 = _mm256_permute2f128_si256(lo, hi, 0x20);
            let p1 = _mm256_permute2f128_si256(lo, hi, 0x31);
            _mm256_storeu_si256(dst.add(2 * i - 1) as *mut __m256i, p0);
            _mm256_storeu_si256(dst.add(2 * i + 15) as *mut __m256i, p1);
            i += 16;
        }
        // 标量余量（滚动 t1，与 `cs::hv2_row` 同构；整数恒等）
        let mut t1 = t_cs(plane, near, far, i - 1);
        for q in i..w {
            let t0 = t1;
            t1 = t_cs(plane, near, far, q);
            *dst.add(2 * q - 1) = (3 * t0 + t1) as u16;
            *dst.add(2 * q) = (3 * t1 + t0) as u16;
        }
        *dst.add(2 * w - 1) = (t_cs(plane, near, far, w - 1) << 2) as u16;
    }

    /// `Row`：`Row16` 后 `(v + 8) >> 4` 取整到 8 位（向量尾微量）。等价 `cs::row8`。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn row8_avx2(
        kind: u8,
        dst: &mut [u8],
        scratch: &mut [u16],
        plane: &[u8],
        in_near: usize,
        in_far: usize,
        w: usize,
        hs: usize,
    ) {
        row16_avx2(kind, scratch, plane, in_near, in_far, w, hs);
        let n = output_count(kind, w, hs);
        let d = dst.as_mut_ptr();
        let add8 = _mm256_set1_epi16(8);
        let mut i = 0usize;
        while i + 16 <= n {
            let v = _mm256_loadu_si256(scratch.as_ptr().add(i) as *const __m256i);
            let x = _mm256_srli_epi16(_mm256_add_epi16(v, add8), 4);
            let p = _mm256_packus_epi16(x, x);
            _mm_storeu_si128(d.add(i) as *mut __m128i, _mm256_castsi256_si128(p));
            i += 16;
        }
        for i in i..n {
            dst[i] = ((scratch[i] + 8) >> 4) as u8;
        }
    }

    // ------------------------------------------------------------------
    // T.871 浮点颜色换算（`JpegColor.Ycc` + `PackSaturate`；lane = 像素）
    // ------------------------------------------------------------------

    /// 8 个 u8 → 8 个 i32（零扩展，`JpegColor.Load8(ref byte)`）。
    #[target_feature(enable = "avx2")]
    unsafe fn load8_u8_epi32(p: *const u8) -> __m256i {
        _mm256_cvtepu8_epi32(_mm_loadl_epi64(p as *const __m128i))
    }

    /// 8 个 u16 → 8 个 i32（零扩展，`Load8(ref ushort)`）。
    #[target_feature(enable = "avx2")]
    unsafe fn load8_u16_epi32(p: *const u16) -> __m256i {
        _mm256_cvtepu16_epi32(_mm_loadu_si128(p as *const __m128i))
    }

    /// 8 像素 YCbCr → 32 字节 RGBA（`Ycc` lane 恒等 + `PackSaturate` bgr=false）。
    #[target_feature(enable = "avx2")]
    unsafe fn ycc_pack_rgba(y: __m256i, cb: __m256i, cr: __m256i) -> __m256i {
        let kr = _mm256_set1_ps(KR);
        let kgb = _mm256_set1_ps(KGB);
        let kgr = _mm256_set1_ps(KGR);
        let kb = _mm256_set1_ps(KB);
        let yv = _mm256_cvtepi32_ps(y);
        let cbv = _mm256_cvtepi32_ps(cb);
        let crv = _mm256_cvtepi32_ps(cr);
        let r = _mm256_add_ps(yv, _mm256_mul_ps(crv, kr));
        let g = _mm256_add_ps(
            _mm256_add_ps(yv, _mm256_mul_ps(cbv, kgb)),
            _mm256_mul_ps(crv, kgr),
        );
        let b = _mm256_add_ps(yv, _mm256_mul_ps(cbv, kb));
        let half = _mm256_set1_ps(0.5);
        let ri = _mm256_cvttps_epi32(_mm256_add_ps(r, half));
        let gi = _mm256_cvttps_epi32(_mm256_add_ps(g, half));
        let bi = _mm256_cvttps_epi32(_mm256_add_ps(b, half));
        // packssdw + packuswb = trunc(v+0.5) 后 clamp[0,255]（`PackSaturate`）
        let lo = _mm256_packs_epi32(ri, gi);
        let hi = _mm256_packs_epi32(bi, _mm256_set1_epi32(255));
        let px = _mm256_packus_epi16(lo, hi);
        let shuf = _mm256_setr_epi8(
            0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15, 0, 4, 8, 12, 1, 5, 9, 13, 2,
            6, 10, 14, 3, 7, 11, 15,
        );
        _mm256_shuffle_epi8(px, shuf)
    }

    /// 8 像素 RGBA（含 255 alpha）→ 目标 `out_bpp`（3=RGB / 4=RGBA）写入。
    #[target_feature(enable = "avx2")]
    unsafe fn store_bpp8(px: __m256i, dst: *mut u8, out_bpp: usize) {
        if out_bpp == 4 {
            _mm256_storeu_si256(dst as *mut __m256i, px);
        } else {
            // out_bpp == 3：每 lane 去掉 alpha（vpshufb 通道内重组）
            let m3 = _mm_setr_epi8(0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 15, 15, 15, 15);
            let lo_i = _mm256_castsi256_si128(px); // p0..p3（RGBA）
            let hi_i = _mm256_extracti128_si256::<1>(px); // p4..p7（RGBA）
            let rlo = _mm_shuffle_epi8(lo_i, m3);
            let rhi = _mm_shuffle_epi8(hi_i, m3);
            _mm_storel_epi64(dst as *mut __m128i, rlo);
            _mm_storeu_si32(dst.add(8) as *mut u8, _mm_srli_si128(rlo, 8));
            _mm_storel_epi64(dst.add(12) as *mut __m128i, rhi);
            _mm_storeu_si32(dst.add(20) as *mut u8, _mm_srli_si128(rhi, 8));
        }
    }

    /// 16× 色度（subsampled）：`cb16 − 2048` 先 int 后转 f32（`YccRow16V256` + 标量尾）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn ycc_row16_avx2(
        y: &[u8],
        cb16: &[u16],
        cr16: &[u16],
        out: &mut [u8],
        n: usize,
        out_bpp: usize,
    ) {
        let bias = _mm256_set1_epi32(2048);
        let yp = y.as_ptr();
        let cbp = cb16.as_ptr();
        let crp = cr16.as_ptr();
        let mut i = 0usize;
        while i + 8 <= n {
            let yv = load8_u8_epi32(yp.add(i));
            let cbv = _mm256_sub_epi32(load8_u16_epi32(cbp.add(i)), bias);
            let crv = _mm256_sub_epi32(load8_u16_epi32(crp.add(i)), bias);
            let px = ycc_pack_rgba(yv, cbv, crv);
            store_bpp8(px, out.as_mut_ptr().add(i * out_bpp), out_bpp);
            i += 8;
        }
        // 标量尾（`cs::ycc_row16` 逐句镜像）
        for i in i..n {
            let yf = f32::from(y[i]);
            let cb = (i32::from(cb16[i]) - 2048) as f32;
            let cr = (i32::from(cr16[i]) - 2048) as f32;
            let r = yf + cr * KR;
            let g = (yf + cb * KGB) + cr * KGR;
            let b = yf + cb * KB;
            let o = i * out_bpp;
            out[o] = (r + 0.5f32).clamp(0.0, 255.0) as i32 as u8;
            out[o + 1] = (g + 0.5f32).clamp(0.0, 255.0) as i32 as u8;
            out[o + 2] = (b + 0.5f32).clamp(0.0, 255.0) as i32 as u8;
            for _ in 3..out_bpp {
                out[o + 3] = 255;
            }
        }
    }

    /// 8 位全分辨率色度（4:4:4）：`(cb << 4) − 2048` 先 int 后转 f32（`YccRow8V256` + 标量尾）。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn ycc_row8_avx2(
        y: &[u8],
        cb: &[u8],
        cr: &[u8],
        out: &mut [u8],
        n: usize,
        out_bpp: usize,
    ) {
        let bias = _mm256_set1_epi32(2048);
        let yp = y.as_ptr();
        let cbp = cb.as_ptr();
        let crp = cr.as_ptr();
        let mut i = 0usize;
        while i + 8 <= n {
            let yv = load8_u8_epi32(yp.add(i));
            let cbv = _mm256_sub_epi32(_mm256_slli_epi32(load8_u8_epi32(cbp.add(i)), 4), bias);
            let crv = _mm256_sub_epi32(_mm256_slli_epi32(load8_u8_epi32(crp.add(i)), 4), bias);
            let px = ycc_pack_rgba(yv, cbv, crv);
            store_bpp8(px, out.as_mut_ptr().add(i * out_bpp), out_bpp);
            i += 8;
        }
        // 标量尾（`cs::ycc_row8` 逐句镜像）
        for i in i..n {
            let yf = f32::from(y[i]);
            let cb = ((i32::from(cb[i]) << 4) - 2048) as f32;
            let cr = ((i32::from(cr[i]) << 4) - 2048) as f32;
            let r = yf + cr * KR;
            let g = (yf + cb * KGB) + cr * KGR;
            let b = yf + cb * KB;
            let o = i * out_bpp;
            out[o] = (r + 0.5f32).clamp(0.0, 255.0) as i32 as u8;
            out[o + 1] = (g + 0.5f32).clamp(0.0, 255.0) as i32 as u8;
            out[o + 2] = (b + 0.5f32).clamp(0.0, 255.0) as i32 as u8;
            for _ in 3..out_bpp {
                out[o + 3] = 255;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) use kernels::{idct8x8_avx2, row16_avx2, row8_avx2, ycc_row16_avx2, ycc_row8_avx2};
