use super::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

// ---------------------------------------------------------------------------
// YUV→RGB 定点转换（对齐 vp8.rs `yuv_to_rgb`：`(s*k)>>8` 各项、bias 减后再 `>>6`，无重结合）
// ---------------------------------------------------------------------------

/// 标量单像素转换（直接复用 `vp8::yuv_to_rgb` 的算术源，保证 SIMD 与标量同序同 truncation）。
#[inline]
pub(crate) fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    super::vp8::yuv_to_rgb(y, u, v)
}

/// 一行 YUV→RGBA8（alpha 恒 255）：AVX2 每 32 像素批量（含 16/8px 段收尾）+ 标量尾；
/// 无 AVX2 则 SSE4.1 每 8 像素批量 + 标量尾；无 SIMD 全标量。逐字节恒等于标量
/// `vp8::yuv_to_rgb`（各路径同一算术源，见内核注释）。
/// 输入三平面各 `n` 字节，输出 `4*n` 字节。
pub(crate) fn yuv_row_to_rgba(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8]) {
    yuv_row_to_rgba_alpha(y, u, v, None, out);
}

/// 一行 YUV→RGBA8，alpha 来自可选 alpha 行（`Some` 时每像素第 4 字节 = alpha[i]；
/// `None` 时恒 255）。批量结构与 [`yuv_row_to_rgba`] 完全同源，仅 A 通道由常量 255 改为
/// 逐组载入 alpha 行——`Some` 时逐字节恒等于「旧 `yuv_row_to_rgba` + 事后标量
/// `rgba[i*4+3]=alpha[i]`」两遍合成（lossy-alpha 发射融合后单遍写出的字节契约）。
/// 输入三平面加 alpha 行各 `n` 字节，输出 `4*n` 字节。
pub(crate) fn yuv_row_to_rgba_alpha(y: &[u8], u: &[u8], v: &[u8], alpha: Option<&[u8]>, out: &mut [u8]) {
    debug_assert_eq!(y.len(), u.len());
    debug_assert_eq!(y.len(), v.len());
    debug_assert_eq!(out.len(), y.len() * 4);
    if let Some(a) = alpha {
        debug_assert_eq!(a.len(), y.len());
    }
    let n = y.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 16 {
        // 默认 u16-mulhi 变体（`KIMG_WEBP_YUV_U16=0` 回退 i32 定点；两者逐字节一致）。
        let u16 = yuv_u16_enabled();
        let mut col = 0usize;
        unsafe {
            // alpha 行按列偏移取指针（None=常量 255 填 A 通道，与旧 `yuv_row_to_rgba` 恒等）。
            let aptr = |col: usize| alpha.map(|a| a.as_ptr().add(col));
            // 主批量：32 像素/步（YUV 加宽内核）。opaque（alpha=None）热路径把 α 判定提到行级
            // 循环外一次，进入「无 α 融合」循环（32px 每迭代无 `testb r9b`）；alpha=Some 走原逻辑。
            if alpha.is_none() {
                while col + 32 <= n {
                    if u16 {
                        yuv32_to_rgba_avx2_u16_opaque(
                            y.as_ptr().add(col),
                            u.as_ptr().add(col),
                            v.as_ptr().add(col),
                            out.as_mut_ptr().add(col * 4),
                        );
                    } else {
                        yuv32_to_rgba_avx2(
                            y.as_ptr().add(col),
                            u.as_ptr().add(col),
                            v.as_ptr().add(col),
                            None,
                            out.as_mut_ptr().add(col * 4),
                        );
                    }
                    col += 32;
                }
            } else {
                while col + 32 <= n {
                    if u16 {
                        yuv32_to_rgba_avx2_u16(
                            y.as_ptr().add(col),
                            u.as_ptr().add(col),
                            v.as_ptr().add(col),
                            aptr(col),
                            out.as_mut_ptr().add(col * 4),
                        );
                    } else {
                        yuv32_to_rgba_avx2(
                            y.as_ptr().add(col),
                            u.as_ptr().add(col),
                            v.as_ptr().add(col),
                            aptr(col),
                            out.as_mut_ptr().add(col * 4),
                        );
                    }
                    col += 32;
                }
            }
            while col + 16 <= n {
                if u16 {
                    yuv16_to_rgba_avx2_u16(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
                        aptr(col),
                        out.as_mut_ptr().add(col * 4),
                    );
                } else {
                    yuv16_to_rgba_avx2(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
                        aptr(col),
                        out.as_mut_ptr().add(col * 4),
                    );
                }
                col += 16;
            }
            // AVX2 隐含 SSE4.1：剩余 8..16 像素段走 8px SSE4.1 核。
            while col + 8 <= n {
                yuv8_to_rgba_sse41(
                    y.as_ptr().add(col),
                    u.as_ptr().add(col),
                    v.as_ptr().add(col),
                    aptr(col),
                    out.as_mut_ptr().add(col * 4),
                );
                col += 8;
            }
            // 尾列（0..8 像素）逐像素标量。
            for i in col..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                let a = alpha.map_or(255, |a| a[i]);
                let o = i * 4;
                *out.get_unchecked_mut(o) = r;
                *out.get_unchecked_mut(o + 1) = g;
                *out.get_unchecked_mut(o + 2) = b;
                *out.get_unchecked_mut(o + 3) = a;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse4.1") && n >= 8 {
        let mut col = 0usize;
        unsafe {
            let aptr = |col: usize| alpha.map(|a| a.as_ptr().add(col));
            while col + 8 <= n {
                yuv8_to_rgba_sse41(
                    y.as_ptr().add(col),
                    u.as_ptr().add(col),
                    v.as_ptr().add(col),
                    aptr(col),
                    out.as_mut_ptr().add(col * 4),
                );
                col += 8;
            }
            // 尾列（0..8 像素）逐像素标量。
            for i in col..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                let a = alpha.map_or(255, |a| a[i]);
                let o = i * 4;
                *out.get_unchecked_mut(o) = r;
                *out.get_unchecked_mut(o + 1) = g;
                *out.get_unchecked_mut(o + 2) = b;
                *out.get_unchecked_mut(o + 3) = a;
            }
        }
        return;
    }
    for i in 0..n {
        let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
        let a = alpha.map_or(255, |a| a[i]);
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = a;
    }
}

/// SSE4.1：8 像素 i32 lane 定点 YUV→RGBA。
///
/// 每个分量 `(x*c)>>8`：`c≤33050`、`x≤255`，积 ≤ 8.4M < 2^31，`mullo_epi32` 低 32 位即精确积，
/// `srai_epi32 >>8/>>6` 为有符号算术右移（负数向负扩展，与 Rust `i32>>` 一致）；
/// 中间 `bias` 加减在 i32 内不溢出，最后 `max/min` 到 [0,255]、`packs_epi32`+`packus_epi16`
/// 饱和落 u8（值已在 [0,255]，饱和即恒等），再按字节交错成 RGBA。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn yuv8_to_rgba_sse41(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    a: Option<*const u8>,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {

    // 非对齐 4 字节读（行偏移/列偏移均不保证 4 对齐）：read_unaligned 避免 UB。
    let y0 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned(y as *const u32) as i32));
    let y1 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned((y as *const u32).add(1)) as i32));
    let u0 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned(u as *const u32) as i32));
    let u1 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned((u as *const u32).add(1)) as i32));
    let v0 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned(v as *const u32) as i32));
    let v1 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned((v as *const u32).add(1)) as i32));

    let c19077 = _mm_set1_epi32(19077);
    let c26149 = _mm_set1_epi32(26149);
    let c6419 = _mm_set1_epi32(6419);
    let c13320 = _mm_set1_epi32(13320);
    let c33050 = _mm_set1_epi32(33050);
    let c14234 = _mm_set1_epi32(14234);
    let c8708 = _mm_set1_epi32(8708);
    let c17685 = _mm_set1_epi32(17685);

    // 闭包内联到一次 channel 计算（4 像素）。
    macro_rules! channel {
        ($yv:expr, $uv:expr, $vv:expr) => {{
            let yv = $yv;
            let uv = $uv;
            let vv = $vv;
            let yt = _mm_srai_epi32(_mm_mullo_epi32(yv, c19077), 8);
            let vr = _mm_srai_epi32(_mm_mullo_epi32(vv, c26149), 8);
            let ug = _mm_srai_epi32(_mm_mullo_epi32(uv, c6419), 8);
            let vg = _mm_srai_epi32(_mm_mullo_epi32(vv, c13320), 8);
            let ub = _mm_srai_epi32(_mm_mullo_epi32(uv, c33050), 8);
            let r = _mm_srai_epi32(_mm_sub_epi32(_mm_add_epi32(yt, vr), c14234), 6);
            let g = _mm_srai_epi32(_mm_add_epi32(_mm_sub_epi32(_mm_sub_epi32(yt, ug), vg), c8708), 6);
            let b = _mm_srai_epi32(_mm_sub_epi32(_mm_add_epi32(yt, ub), c17685), 6);
            let zero = _mm_setzero_si128();
            let c255 = _mm_set1_epi32(255);
            (
                _mm_min_epi32(_mm_max_epi32(r, zero), c255),
                _mm_min_epi32(_mm_max_epi32(g, zero), c255),
                _mm_min_epi32(_mm_max_epi32(b, zero), c255),
            )
        }};
    }

    let (r0, g0, b0) = channel!(y0, u0, v0);
    let (r1, g1, b1) = channel!(y1, u1, v1);

    // i32（已 clamp [0,255]）→ i16（饱和=恒等）→ u8，得 8 像素的平面字节 [R..][G..][B..]。
    let r16 = _mm_packs_epi32(r0, r1);
    let g16 = _mm_packs_epi32(g0, g1);
    let b16 = _mm_packs_epi32(b0, b1);
    let r8 = _mm_packus_epi16(r16, r16);
    let g8 = _mm_packus_epi16(g16, g16);
    let b8 = _mm_packus_epi16(b16, b16);
    let a8 = match a {
        // alpha 行 8 字节 → A 平面（低 8 字节为像素序列，unpacklo 只取低 8）。
        Some(p) => _mm_loadl_epi64(p as *const __m128i),
        None => _mm_set1_epi8(-1),
    };

    // 三平面 → RGBA 交错：先 r/g 与 b/α 字节交错成两路 16 位 lane，再两两交错成 RGBA 四元组
    // （unpacklo_epi16 才得到 [r,g,b,α] 的字节次序：lane0=(r0,g0)、lane1=(b0,α) 交替）。
    let rg = _mm_unpacklo_epi8(r8, g8);
    let ba = _mm_unpacklo_epi8(b8, a8);
    let out_lo = _mm_unpacklo_epi16(rg, ba);
    let out_hi = _mm_unpackhi_epi16(rg, ba);
    _mm_storeu_si128(out as *mut __m128i, out_lo);
    _mm_storeu_si128((out as *mut __m128i).add(1), out_hi);
    }
}

/// AVX2：16 像素（8×i32 lane × 2 组）定点 YUV→RGBA 核心（已 vpmovzxbd 提升的 6 个 8-i32 堆）。
/// 算术与 [`yuv8_to_rgba_sse41`] 严格同源（`(x*c)>>8` → bias 减 → `>>6` → min/max clamp [0,255]，
/// `mullo_epi32`+`srai_epi32` 语义一致），低 8 像素/高 8 像素各一遍通道，再按 128-bit 段 pack 成
/// 16 字节平面、256-bit 逐 lane unpack 交错成 RGBA——逐字节等于两个独立的 8px SSE4.1 调用。
/// `out` 写出 64 字节（16 像素 RGBA）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_core_avx2(
    y0: __m256i,
    y1: __m256i,
    u0: __m256i,
    u1: __m256i,
    v0: __m256i,
    v1: __m256i,
    a: Option<*const u8>,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let c19077 = _mm256_set1_epi32(19077);
        let c26149 = _mm256_set1_epi32(26149);
        let c6419 = _mm256_set1_epi32(6419);
        let c13320 = _mm256_set1_epi32(13320);
        let c33050 = _mm256_set1_epi32(33050);
        let c14234 = _mm256_set1_epi32(14234);
        let c8708 = _mm256_set1_epi32(8708);
        let c17685 = _mm256_set1_epi32(17685);

        macro_rules! channel {
            ($yv:expr, $uv:expr, $vv:expr) => {{
                let yv = $yv;
                let uv = $uv;
                let vv = $vv;
                let yt = _mm256_srai_epi32(_mm256_mullo_epi32(yv, c19077), 8);
                let vr = _mm256_srai_epi32(_mm256_mullo_epi32(vv, c26149), 8);
                let ug = _mm256_srai_epi32(_mm256_mullo_epi32(uv, c6419), 8);
                let vg = _mm256_srai_epi32(_mm256_mullo_epi32(vv, c13320), 8);
                let ub = _mm256_srai_epi32(_mm256_mullo_epi32(uv, c33050), 8);
                let r = _mm256_srai_epi32(_mm256_sub_epi32(_mm256_add_epi32(yt, vr), c14234), 6);
                let g = _mm256_srai_epi32(_mm256_add_epi32(_mm256_sub_epi32(_mm256_sub_epi32(yt, ug), vg), c8708), 6);
                let b = _mm256_srai_epi32(_mm256_sub_epi32(_mm256_add_epi32(yt, ub), c17685), 6);
                let zero = _mm256_setzero_si256();
                let c255 = _mm256_set1_epi32(255);
                (
                    _mm256_min_epi32(_mm256_max_epi32(r, zero), c255),
                    _mm256_min_epi32(_mm256_max_epi32(g, zero), c255),
                    _mm256_min_epi32(_mm256_max_epi32(b, zero), c255),
                )
            }};
        }

        let (r0, g0, b0) = channel!(y0, u0, v0);
        let (r1, g1, b1) = channel!(y1, u1, v1);

        // 三平面 → 16 字节平面（低 128 位=8 像素，高 128 位=另 8 像素）。
        let r8 = pack8_bytes_avx2(r0, r1);
        let g8 = pack8_bytes_avx2(g0, g1);
        let b8 = pack8_bytes_avx2(b0, b1);
        let a8 = match a {
            // alpha 行 16 字节 → A 平面：低 128 位 = 前 8 像素、高 128 位 = 后 8 像素
            // （与 r/g/b 平面同布局，unpacklo/unpackhi 逐 lane 交替取像素段）。
            Some(p) => _mm256_set_m128i(
                _mm_loadl_epi64(p.add(8) as *const __m128i),
                _mm_loadl_epi64(p as *const __m128i),
            ),
            None => _mm256_set1_epi8(-1),
        };

        // 三平面 → RGBA 交错（256-bit unpacklo_epi8/16 逐 128-bit lane，无跨 lane 干扰）：
        // 每 lane 得 [r,g,b,α] 四元组字节序，与 SSE4.1 的 8px 组逐一相同。
        let rg = _mm256_unpacklo_epi8(r8, g8);
        let ba = _mm256_unpacklo_epi8(b8, a8);
        let out_lo = _mm256_unpacklo_epi16(rg, ba);
        let out_hi = _mm256_unpackhi_epi16(rg, ba);
        // 逐 lane unpack 后 out_lo=[px0-3,px8-11]、out_hi=[px4-7,px12-15]（lane0/lane1），
        // 不能顺序直存（会写成 [0-3][8-11][4-7][12-15]）。先 vperm2i128 按 128-bit 块重排成
        // [0-3][4-7]（=out_lo.l0,out_hi.l0）与 [8-11][12-15]（=out_lo.l1,out_hi.l1）再存。
        // 立即数编码：imm[1:0] 选 lane0 源、imm[5:4] 选 lane1 源（imm[3]/imm[7] 为 lane0/1 清零位）。
        let p01 = _mm256_permute2x128_si256(out_lo, out_hi, 0x20); // [lo.l0, hi.l0] = px0-7
        let p23 = _mm256_permute2x128_si256(out_lo, out_hi, 0x31); // [lo.l1, hi.l1] = px8-15
        _mm256_storeu_si256(out as *mut __m256i, p01);
        _mm256_storeu_si256((out as *mut __m256i).add(1), p23);
    }
}

/// AVX2：16 像素（8×i32 lane × 2 组）定点 YUV→RGBA 包装（载入并宽 + 核心）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_to_rgba_avx2(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    a: Option<*const u8>,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        // 每 8 字节段（8 像素）经 vpmovzxbd 提升到 ymm 的 8 个 i32 lane（非对齐 8 字节读）。
        let y0 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(y as *const __m128i));
        let y1 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(y.add(8) as *const __m128i));
        let u0 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(u as *const __m128i));
        let u1 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(u.add(8) as *const __m128i));
        let v0 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(v as *const __m128i));
        let v1 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(v.add(8) as *const __m128i));
        yuv16_core_avx2(y0, y1, u0, u1, v0, v1, a, out);
    }
}

/// AVX2：32 像素/步（Req: 16→32px 加宽）。载 32 字节 → 拆低/高 16 字节两组，每组视作两个
/// 8 字节 qword（低 16 像素组：低 qword+`srli 8`；高 16 像素组：高 128 位 + `srli 8`）经
/// vpmovzxbd 提升 → [`yuv16_core_avx2`]，写 128 字节 RGBA。算术与 16px 内核逐位相同 → 逐字节
/// 恒等于两个相邻 16px 调用；循环步长减半、单次 32B 载入替代两次 16B，依赖链交错提 ILP。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv32_to_rgba_avx2(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    a: Option<*const u8>,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let yb = _mm256_loadu_si256(y as *const __m256i);
        let ub = _mm256_loadu_si256(u as *const __m256i);
        let vb = _mm256_loadu_si256(v as *const __m256i);
        let ylo = _mm256_castsi256_si128(yb);
        let yhi = _mm256_extracti128_si256(yb, 1);
        let ulo = _mm256_castsi256_si128(ub);
        let uhi = _mm256_extracti128_si256(ub, 1);
        let vlo = _mm256_castsi256_si128(vb);
        let vhi = _mm256_extracti128_si256(vb, 1);
        // 低 16 像素组：qword0 = 低 128 位原样（字节 0..7），qword1 = srli8（字节 8..15）。
        let g0 = _mm256_cvtepu8_epi32(ylo);
        let g1 = _mm256_cvtepu8_epi32(_mm_srli_si128(ylo, 8));
        let u0 = _mm256_cvtepu8_epi32(ulo);
        let u1 = _mm256_cvtepu8_epi32(_mm_srli_si128(ulo, 8));
        let v0 = _mm256_cvtepu8_epi32(vlo);
        let v1 = _mm256_cvtepu8_epi32(_mm_srli_si128(vlo, 8));
        yuv16_core_avx2(g0, g1, u0, u1, v0, v1, a, out);
        // 高 16 像素组：qword0 = 高 128 位（字节 16..23），qword1 = srli8（字节 24..31）。
        let g0 = _mm256_cvtepu8_epi32(yhi);
        let g1 = _mm256_cvtepu8_epi32(_mm_srli_si128(yhi, 8));
        let u0 = _mm256_cvtepu8_epi32(uhi);
        let u1 = _mm256_cvtepu8_epi32(_mm_srli_si128(uhi, 8));
        let v0 = _mm256_cvtepu8_epi32(vhi);
        let v1 = _mm256_cvtepu8_epi32(_mm_srli_si128(vhi, 8));
        yuv16_core_avx2(g0, g1, u0, u1, v0, v1, a.map(|p| p.add(16)), out.add(64));
    }
}

/// AVX2 helper：两个 8-i32 lane 分量堆（各低/高 128 位含 4 像素）→ 16 字节平面字节序。
/// 低 128 位 = 前 8 像素、高 128 位 = 后 8 像素（每段先 4 i32 → 4 i16 → 4 u8，饱和即恒等，
/// 值已在 [0,255]，与 SSE4.1 的 pack 同款）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn pack8_bytes_avx2(a: __m256i, b: __m256i) -> __m256i {
    use std::arch::x86_64::*;
    unsafe {
        let al = _mm256_castsi256_si128(a);
        let ah = _mm256_extracti128_si256(a, 1);
        let bl = _mm256_castsi256_si128(b);
        let bh = _mm256_extracti128_si256(b, 1);
        let lo = _mm_packus_epi16(_mm_packs_epi32(al, ah), _mm_setzero_si128());
        let hi = _mm_packus_epi16(_mm_packs_epi32(bl, bh), _mm_setzero_si128());
        _mm256_set_m128i(hi, lo)
    }
}

/// AVX2 u16 16px 通道（已左移 8 的 16-u16 y/u/v + **已组装的 A 平面**）：mulhi → max-sub → srli6 →
/// packus → 逐 lane 交错 → 返回两段 32 字节 RGBA（`p01`=px{i..i+7}、`p23`=px{i+8..i+15}）。
/// **无 α 分支**——`a8` 由调用方组装（opaque 传 0xFF 广播、alpha 传平面），16px 核与 32px
/// 融合核共享同一算法体，指令一次编排不复用宏。算术与标量 `(x*c)>>8` 逐位同截断（见
/// [`yuv16_to_rgba_avx2_u16`] 文档）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_pack_avx2_u16(yv: __m256i, uv: __m256i, vv: __m256i, a8: __m256i) -> (__m256i, __m256i) {
    use std::arch::x86_64::*;
    unsafe {
        let k_y = _mm256_set1_epi16(19077);
        let k_vr = _mm256_set1_epi16(26149);
        let k_ug = _mm256_set1_epi16(6419);
        let k_vg = _mm256_set1_epi16(13320);
        // 33050 > i16::MAX → 补码表示（-32486 = 0x811A）；mulhi_epu16 按 u16 位模式读，恒为 33050。
        let k_ub = _mm256_set1_epi16(-32486);
        let b_r = _mm256_set1_epi16(14234);
        let b_g = _mm256_set1_epi16(8708);
        let b_b = _mm256_set1_epi16(17685);

        let yt = _mm256_mulhi_epu16(yv, k_y);
        let vr = _mm256_mulhi_epu16(vv, k_vr);
        let ug = _mm256_mulhi_epu16(uv, k_ug);
        let vg = _mm256_mulhi_epu16(vv, k_vg);
        let ub = _mm256_mulhi_epu16(uv, k_ub);

        // 负数偏置：max(和, bias)-bias 在 u16 域钳 0，等价标量负值走 srai 后 clamp 0。
        let rd = _mm256_srli_epi16(
            _mm256_sub_epi16(_mm256_max_epu16(_mm256_add_epi16(yt, vr), b_r), b_r),
            6,
        );
        let gn = _mm256_add_epi16(ug, vg);
        let gr = _mm256_srli_epi16(
            _mm256_sub_epi16(_mm256_max_epu16(_mm256_add_epi16(yt, b_g), gn), gn),
            6,
        );
        let bl = _mm256_srli_epi16(
            _mm256_sub_epi16(_mm256_max_epu16(_mm256_add_epi16(yt, ub), b_b), b_b),
            6,
        );

        // 每 16 i16 → 16 u8 平面：packus(r, 0) 得低 128 位 [px0-7,0×8]、高 128 位 [px8-15,0×8]，
        // 与 i32 路径的 16 字节平面同构（unpack 逐 lane 取低 8 字节即像素段）。
        let r8 = _mm256_packus_epi16(rd, _mm256_setzero_si256());
        let g8 = _mm256_packus_epi16(gr, _mm256_setzero_si256());
        let b8 = _mm256_packus_epi16(bl, _mm256_setzero_si256());
        let rg = _mm256_unpacklo_epi8(r8, g8);
        let ba = _mm256_unpacklo_epi8(b8, a8);
        let out_lo = _mm256_unpacklo_epi16(rg, ba);
        let out_hi = _mm256_unpackhi_epi16(rg, ba);
        let p01 = _mm256_permute2x128_si256(out_lo, out_hi, 0x20);
        let p23 = _mm256_permute2x128_si256(out_lo, out_hi, 0x31);
        (p01, p23)
    }
}

/// AVX2 u16 16px 核心（已左移 8 的 16-u16 y/u/v）：[`yuv16_pack_avx2_u16`] + 两次 256-bit 存储。
/// `a` 为 None（opaque）时 A 平面 = 0xFF 广播（字节恒等）。`out` 写出 64 字节（16 像素 RGBA）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_core_avx2_u16(yv: __m256i, uv: __m256i, vv: __m256i, a: Option<*const u8>, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
        let a8 = match a {
            // alpha 行 16 字节 → A 平面：低 128 位 = 前 8 像素、高 128 位 = 后 8 像素
            // （与 r/g/b 平面同布局，unpacklo/unpackhi 逐 lane 交替取像素段）。
            Some(p) => _mm256_set_m128i(
                _mm_loadl_epi64(p.add(8) as *const __m128i),
                _mm_loadl_epi64(p as *const __m128i),
            ),
            None => _mm256_set1_epi8(-1),
        };
        let (p01, p23) = yuv16_pack_avx2_u16(yv, uv, vv, a8);
        // 与 i32 变体同款交错+重排后存两段。
        _mm256_storeu_si256(out as *mut __m256i, p01);
        _mm256_storeu_si256((out as *mut __m256i).add(1), p23);
    }
}

/// AVX2 u16-mulhi 变体：16 像素/步。与 [`yuv16_to_rgba_avx2`]（i32 定点）逐字节相等——对标
/// C# `WebpDecoder.ConvertRowAvx2`（libwebp 式 vpmulhuw）：像素字节先放到 u16 高位（左移 8），
/// `mulhi_epu16(px<<8, k) = (px*k)>>8` 与标量 `(x*c)>>8` 逐位同截断；bias 减用 max-then-subtract
/// 在 u16 域钳 0（预偏置和恒 < 2^16：yTerm≤19020、r 和≤45077、g 两项和≤27728/19688、b 和≤51890，
/// 均不溢出），末段 `srli >>6` 逻辑右移 + packus 饱和把负数/超限映射到 [0,255]——与标量
/// `srai>>6` 后 clamp 逐字节同。
/// 门控：`KIMG_WEBP_YUV_U16=1` 时生产走本变体（实验性；任一对拍差一字节即弃，保持 i32 方案）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_to_rgba_avx2_u16(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    a: Option<*const u8>,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        // 16 像素载入 → 16 i16（低位=像素）→ 左移 8 让像素落高字节（=×256）。
        let yv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(y as *const __m128i)), 8);
        let uv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(u as *const __m128i)), 8);
        let vv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(v as *const __m128i)), 8);
        yuv16_core_avx2_u16(yv, uv, vv, a, out);
    }
}

/// AVX2 u16-mulhi 变体：32 像素/步**单趟融合**（Req: 16→32px 加宽 + convert 单趟化）。载 32
/// 字节 ×3 平面一次，`vpunpcklbw/hbw(zero, x)` 零展开**同一拍既提升 i16 又完成像素高字节放置**
/// （替代旧两子趟的 vpmovzxbw + vpsllw(8) + vextracti128 ×6），两组 16-u16 共享
/// [`yuv16_pack_avx2_u16`] 算法体，末段按 128 位半区交错存储得到 px0-31 顺序——无移位、
/// 无跨 lane 搬运。算术逐位同 16px 内核 → 逐字节恒等于两个相邻 16px 内核调用；
/// 循环步长减半、单次 3×32B 载入替代两子趟的 6×vextract+6×vpmovzxbw+6×vpsllw，依赖链交错提 ILP。
/// `a` 为 None 时 A 平面 = 0xFF 广播（与 [`yuv32_to_rgba_avx2_u16_opaque`] 字节相同）。
/// `out` 写出 128 字节（32 像素 RGBA）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv32_to_rgba_avx2_u16(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    a: Option<*const u8>,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        // 32 字节（32 像素）一次载入。
        let yb = _mm256_loadu_si256(y as *const __m256i);
        let ub = _mm256_loadu_si256(u as *const __m256i);
        let vb = _mm256_loadu_si256(v as *const __m256i);
        // 单趟零展开：unpacklo 取各 128-bit lane 低 8 字节 → 低 lane px0-7、高 lane px16-23；
        // unpackhi 取各 lane 高 8 字节 → 低 lane px8-15、高 lane px24-31；像素一并落 i16 高字节。
        let z = _mm256_setzero_si256();
        let ya = _mm256_unpacklo_epi8(z, yb);
        let yb2 = _mm256_unpackhi_epi8(z, yb);
        let ua = _mm256_unpacklo_epi8(z, ub);
        let ub2 = _mm256_unpackhi_epi8(z, ub);
        let va = _mm256_unpacklo_epi8(z, vb);
        let vb2 = _mm256_unpackhi_epi8(z, vb);
        // A 组=[px0-7, px16-23]、B 组=[px8-15, px24-31] 的 A 平面按同 lane 排布取。
        let a8a = match a {
            Some(p) => _mm256_set_m128i(
                _mm_loadl_epi64(p.add(16) as *const __m128i),
                _mm_loadl_epi64(p as *const __m128i),
            ),
            None => _mm256_set1_epi8(-1),
        };
        let a8b = match a {
            Some(p) => _mm256_set_m128i(
                _mm_loadl_epi64(p.add(24) as *const __m128i),
                _mm_loadl_epi64(p.add(8) as *const __m128i),
            ),
            None => _mm256_set1_epi8(-1),
        };
        // 两路 16-u16 通道 → 按 128 位半区交错存储，拼出 px0-31 的 RGBA 顺序。
        let (a01, a23) = yuv16_pack_avx2_u16(ya, ua, va, a8a);
        let (b01, b23) = yuv16_pack_avx2_u16(yb2, ub2, vb2, a8b);
        _mm256_storeu_si256(out as *mut __m256i, a01);          // px0-7
        _mm256_storeu_si256((out as *mut __m256i).add(1), b01); // px8-15
        _mm256_storeu_si256((out as *mut __m256i).add(2), a23); // px16-23
        _mm256_storeu_si256((out as *mut __m256i).add(3), b23); // px24-31
    }
}

/// AVX2 u16-mulhi 变体：32 像素/步**无 α 单趟融合**（opaque 热路径）。与 [`yuv32_to_rgba_avx2_u16`]
/// 逐字节相同，仅 A 平面恒 0xFF 广播、不接 `Option`→ 循环内无 `testb r9b` 判定（opaque 分支在
/// 行级外提一次进入本核）。`out` 写出 128 字节（32 像素 RGBA）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv32_to_rgba_avx2_u16_opaque(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let yb = _mm256_loadu_si256(y as *const __m256i);
        let ub = _mm256_loadu_si256(u as *const __m256i);
        let vb = _mm256_loadu_si256(v as *const __m256i);
        let z = _mm256_setzero_si256();
        let ya = _mm256_unpacklo_epi8(z, yb);
        let yb2 = _mm256_unpackhi_epi8(z, yb);
        let ua = _mm256_unpacklo_epi8(z, ub);
        let ub2 = _mm256_unpackhi_epi8(z, ub);
        let va = _mm256_unpacklo_epi8(z, vb);
        let vb2 = _mm256_unpackhi_epi8(z, vb);
        let a8 = _mm256_set1_epi8(-1);
        let (a01, a23) = yuv16_pack_avx2_u16(ya, ua, va, a8);
        let (b01, b23) = yuv16_pack_avx2_u16(yb2, ub2, vb2, a8);
        _mm256_storeu_si256(out as *mut __m256i, a01);
        _mm256_storeu_si256((out as *mut __m256i).add(1), b01);
        _mm256_storeu_si256((out as *mut __m256i).add(2), a23);
        _mm256_storeu_si256((out as *mut __m256i).add(3), b23);
    }
}

// ---------------------------------------------------------------------------
// RGB3（3 通道）直出：opaque 静态有损路径消 RGBA4 中间帧（4→3 转换 / 整帧 RGBA 分配）。
// 算术与上方 `yuv_row_to_rgba` 完全同源（同一 channel!/u16 计算），仅末段平面交错由 4 步长
// 改 3 步长、alpha 不写 → 每像素前 3 字节恒等于 RGBA 路径（`yuv_row_rgb3_matches_rgba`
// 硬闸断言）。以下按 8/16/32px 镜像 RGBA 核，共用 `store_rgb3_planes8` 把三平面交错存成
// 24 字节 RGB 段。
// ---------------------------------------------------------------------------

/// 三平面（各 8 相关字节，低 8 字节为 r/g/b 像素序列）→ 24 字节 RGB 交错存储（两组 4 像素：
/// 移位拼接 `[r4 | g4<<4 | b4<<8]` 后 pshufb 抽 12 字节）。仅字节重排 + 移位/OR，不经算术
/// → 与「RGBA 前 3 字节」逐字节恒等。`out` 不保证对齐（storel_epi64/未对齐 u32 写安全）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn store_rgb3_planes8(r8: __m128i, g8: __m128i, b8: __m128i, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
        let shuf = _mm_setr_epi8(0, 4, 8, 1, 5, 9, 2, 6, 10, 3, 7, 11, 0, 0, 0, 0);
        // 保留低 4 字节的掩码（剔除平面高位，防 OR 拼接时 r/g/b 段互相污染：`_mm_srli_si128`
        // 只做位移不清高位，未掩码会把 r4..r7 叠进 g 段、g4..g7 叠进 b 段）。
        let keep4 = _mm_set_epi32(0, 0, 0, -1);
        // 两组 4 像素（`_mm_srli_si128` 位移为编译期立即数，须显式展开）。
        // 低 4 像素：r/g/b 各自低 4 字节 → [r0..r3 | g0..g3 | b0..b3] → 抽 12 字节 RGB。
        let r4 = _mm_and_si128(_mm_srli_si128(r8, 0), keep4);
        let g4 = _mm_and_si128(_mm_srli_si128(g8, 0), keep4);
        let b4 = _mm_and_si128(_mm_srli_si128(b8, 0), keep4);
        let tmp = _mm_or_si128(
            _mm_or_si128(r4, _mm_slli_si128(g4, 4)),
            _mm_slli_si128(b4, 8),
        );
        let out12 = _mm_shuffle_epi8(tmp, shuf);
        _mm_storel_epi64(out as *mut __m128i, out12);
        std::ptr::write_unaligned(
            out.add(8) as *mut u32,
            _mm_cvtsi128_si32(_mm_srli_si128(out12, 8)) as u32,
        );
        // 高 4 像素：r/g/b 各自字节 4..7。
        let r4 = _mm_and_si128(_mm_srli_si128(r8, 4), keep4);
        let g4 = _mm_and_si128(_mm_srli_si128(g8, 4), keep4);
        let b4 = _mm_and_si128(_mm_srli_si128(b8, 4), keep4);
        let tmp = _mm_or_si128(
            _mm_or_si128(r4, _mm_slli_si128(g4, 4)),
            _mm_slli_si128(b4, 8),
        );
        let out12 = _mm_shuffle_epi8(tmp, shuf);
        let o = out.add(12);
        _mm_storel_epi64(o as *mut __m128i, out12);
        std::ptr::write_unaligned(
            o.add(8) as *mut u32,
            _mm_cvtsi128_si32(_mm_srli_si128(out12, 8)) as u32,
        );
    }
}

/// SSE4.1：8 像素定点 YUV→RGB3。数学与 [`yuv8_to_rgba_sse41`] 逐位相同（同一 `channel!` 宏），
/// 仅末段由 RGBA 交错改为 `store_rgb3_planes8`（24 字节）。`out` 写出 24 字节。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn yuv8_to_rgb3_sse41(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let y0 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned(y as *const u32) as i32));
        let y1 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned((y as *const u32).add(1)) as i32));
        let u0 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned(u as *const u32) as i32));
        let u1 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned((u as *const u32).add(1)) as i32));
        let v0 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned(v as *const u32) as i32));
        let v1 = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(std::ptr::read_unaligned((v as *const u32).add(1)) as i32));

        let c19077 = _mm_set1_epi32(19077);
        let c26149 = _mm_set1_epi32(26149);
        let c6419 = _mm_set1_epi32(6419);
        let c13320 = _mm_set1_epi32(13320);
        let c33050 = _mm_set1_epi32(33050);
        let c14234 = _mm_set1_epi32(14234);
        let c8708 = _mm_set1_epi32(8708);
        let c17685 = _mm_set1_epi32(17685);

        macro_rules! channel {
            ($yv:expr, $uv:expr, $vv:expr) => {{
                let yv = $yv;
                let uv = $uv;
                let vv = $vv;
                let yt = _mm_srai_epi32(_mm_mullo_epi32(yv, c19077), 8);
                let vr = _mm_srai_epi32(_mm_mullo_epi32(vv, c26149), 8);
                let ug = _mm_srai_epi32(_mm_mullo_epi32(uv, c6419), 8);
                let vg = _mm_srai_epi32(_mm_mullo_epi32(vv, c13320), 8);
                let ub = _mm_srai_epi32(_mm_mullo_epi32(uv, c33050), 8);
                let r = _mm_srai_epi32(_mm_sub_epi32(_mm_add_epi32(yt, vr), c14234), 6);
                let g = _mm_srai_epi32(_mm_add_epi32(_mm_sub_epi32(_mm_sub_epi32(yt, ug), vg), c8708), 6);
                let b = _mm_srai_epi32(_mm_sub_epi32(_mm_add_epi32(yt, ub), c17685), 6);
                let zero = _mm_setzero_si128();
                let c255 = _mm_set1_epi32(255);
                (
                    _mm_min_epi32(_mm_max_epi32(r, zero), c255),
                    _mm_min_epi32(_mm_max_epi32(g, zero), c255),
                    _mm_min_epi32(_mm_max_epi32(b, zero), c255),
                )
            }};
        }

        let (r0, g0, b0) = channel!(y0, u0, v0);
        let (r1, g1, b1) = channel!(y1, u1, v1);

        let r16 = _mm_packs_epi32(r0, r1);
        let g16 = _mm_packs_epi32(g0, g1);
        let b16 = _mm_packs_epi32(b0, b1);
        let r8 = _mm_packus_epi16(r16, r16);
        let g8 = _mm_packus_epi16(g16, g16);
        let b8 = _mm_packus_epi16(b16, b16);
        store_rgb3_planes8(r8, g8, b8, out);
    }
}

/// AVX2：16 像素 i32 定点 YUV→RGB3 核心。数学与 [`yuv16_core_avx2`] 逐位相同（同一 `channel!`），
/// 仅末段 `pack8_bytes_avx2` 平面按 128 位半区各调 `store_rgb3_planes8`（24+24 字节）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_core_avx2_rgb3(
    y0: __m256i,
    y1: __m256i,
    u0: __m256i,
    u1: __m256i,
    v0: __m256i,
    v1: __m256i,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let c19077 = _mm256_set1_epi32(19077);
        let c26149 = _mm256_set1_epi32(26149);
        let c6419 = _mm256_set1_epi32(6419);
        let c13320 = _mm256_set1_epi32(13320);
        let c33050 = _mm256_set1_epi32(33050);
        let c14234 = _mm256_set1_epi32(14234);
        let c8708 = _mm256_set1_epi32(8708);
        let c17685 = _mm256_set1_epi32(17685);

        macro_rules! channel {
            ($yv:expr, $uv:expr, $vv:expr) => {{
                let yv = $yv;
                let uv = $uv;
                let vv = $vv;
                let yt = _mm256_srai_epi32(_mm256_mullo_epi32(yv, c19077), 8);
                let vr = _mm256_srai_epi32(_mm256_mullo_epi32(vv, c26149), 8);
                let ug = _mm256_srai_epi32(_mm256_mullo_epi32(uv, c6419), 8);
                let vg = _mm256_srai_epi32(_mm256_mullo_epi32(vv, c13320), 8);
                let ub = _mm256_srai_epi32(_mm256_mullo_epi32(uv, c33050), 8);
                let r = _mm256_srai_epi32(_mm256_sub_epi32(_mm256_add_epi32(yt, vr), c14234), 6);
                let g = _mm256_srai_epi32(_mm256_add_epi32(_mm256_sub_epi32(_mm256_sub_epi32(yt, ug), vg), c8708), 6);
                let b = _mm256_srai_epi32(_mm256_sub_epi32(_mm256_add_epi32(yt, ub), c17685), 6);
                let zero = _mm256_setzero_si256();
                let c255 = _mm256_set1_epi32(255);
                (
                    _mm256_min_epi32(_mm256_max_epi32(r, zero), c255),
                    _mm256_min_epi32(_mm256_max_epi32(g, zero), c255),
                    _mm256_min_epi32(_mm256_max_epi32(b, zero), c255),
                )
            }};
        }

        let (r0, g0, b0) = channel!(y0, u0, v0);
        let (r1, g1, b1) = channel!(y1, u1, v1);

        let r8 = pack8_bytes_avx2(r0, r1);
        let g8 = pack8_bytes_avx2(g0, g1);
        let b8 = pack8_bytes_avx2(b0, b1);
        store_rgb3_planes8(_mm256_castsi256_si128(r8), _mm256_castsi256_si128(g8), _mm256_castsi256_si128(b8), out);
        store_rgb3_planes8(_mm256_extracti128_si256(r8, 1), _mm256_extracti128_si256(g8, 1), _mm256_extracti128_si256(b8, 1), out.add(24));
    }
}

/// AVX2：16 像素 i32 定点 YUV→RGB3 包装（载入并宽 + 核心）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_to_rgb3_avx2(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let y0 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(y as *const __m128i));
        let y1 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(y.add(8) as *const __m128i));
        let u0 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(u as *const __m128i));
        let u1 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(u.add(8) as *const __m128i));
        let v0 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(v as *const __m128i));
        let v1 = _mm256_cvtepu8_epi32(_mm_loadl_epi64(v.add(8) as *const __m128i));
        yuv16_core_avx2_rgb3(y0, y1, u0, u1, v0, v1, out);
    }
}

/// AVX2：32 像素/步 i32 定点 YUV→RGB3（对照 [`yuv32_to_rgba_avx2`]：载 32 字节拆两组 ×
/// `yuv16_core_avx2_rgb3`，写 48+48 字节）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv32_to_rgb3_avx2(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let yb = _mm256_loadu_si256(y as *const __m256i);
        let ub = _mm256_loadu_si256(u as *const __m256i);
        let vb = _mm256_loadu_si256(v as *const __m256i);
        let g0 = _mm256_cvtepu8_epi32(_mm256_castsi256_si128(yb));
        let g1 = _mm256_cvtepu8_epi32(_mm_srli_si128(_mm256_castsi256_si128(yb), 8));
        let u0 = _mm256_cvtepu8_epi32(_mm256_castsi256_si128(ub));
        let u1 = _mm256_cvtepu8_epi32(_mm_srli_si128(_mm256_castsi256_si128(ub), 8));
        let v0 = _mm256_cvtepu8_epi32(_mm256_castsi256_si128(vb));
        let v1 = _mm256_cvtepu8_epi32(_mm_srli_si128(_mm256_castsi256_si128(vb), 8));
        yuv16_core_avx2_rgb3(g0, g1, u0, u1, v0, v1, out);
        let g0 = _mm256_cvtepu8_epi32(_mm256_extracti128_si256(yb, 1));
        let g1 = _mm256_cvtepu8_epi32(_mm_srli_si128(_mm256_extracti128_si256(yb, 1), 8));
        let u0 = _mm256_cvtepu8_epi32(_mm256_extracti128_si256(ub, 1));
        let u1 = _mm256_cvtepu8_epi32(_mm_srli_si128(_mm256_extracti128_si256(ub, 1), 8));
        let v0 = _mm256_cvtepu8_epi32(_mm256_extracti128_si256(vb, 1));
        let v1 = _mm256_cvtepu8_epi32(_mm_srli_si128(_mm256_extracti128_si256(vb, 1), 8));
        yuv16_core_avx2_rgb3(g0, g1, u0, u1, v0, v1, out.add(48));
    }
}

/// AVX2 u16-mulhi 变体：16 像素 YUV→RGB3 核心。数学与 [`yuv16_core_avx2_u16`] 逐位相同，
/// 仅末段平面按 128 位半区分割后 `store_rgb3_planes8`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_core_avx2_u16_rgb3(yv: __m256i, uv: __m256i, vv: __m256i, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
        let k_y = _mm256_set1_epi16(19077);
        let k_vr = _mm256_set1_epi16(26149);
        let k_ug = _mm256_set1_epi16(6419);
        let k_vg = _mm256_set1_epi16(13320);
        let k_ub = _mm256_set1_epi16(-32486);
        let b_r = _mm256_set1_epi16(14234);
        let b_g = _mm256_set1_epi16(8708);
        let b_b = _mm256_set1_epi16(17685);

        let yt = _mm256_mulhi_epu16(yv, k_y);
        let vr = _mm256_mulhi_epu16(vv, k_vr);
        let ug = _mm256_mulhi_epu16(uv, k_ug);
        let vg = _mm256_mulhi_epu16(vv, k_vg);
        let ub = _mm256_mulhi_epu16(uv, k_ub);

        let rd = _mm256_srli_epi16(
            _mm256_sub_epi16(_mm256_max_epu16(_mm256_add_epi16(yt, vr), b_r), b_r),
            6,
        );
        let gn = _mm256_add_epi16(ug, vg);
        let gr = _mm256_srli_epi16(
            _mm256_sub_epi16(_mm256_max_epu16(_mm256_add_epi16(yt, b_g), gn), gn),
            6,
        );
        let bl = _mm256_srli_epi16(
            _mm256_sub_epi16(_mm256_max_epu16(_mm256_add_epi16(yt, ub), b_b), b_b),
            6,
        );

        let r8 = _mm256_packus_epi16(rd, _mm256_setzero_si256());
        let g8 = _mm256_packus_epi16(gr, _mm256_setzero_si256());
        let b8 = _mm256_packus_epi16(bl, _mm256_setzero_si256());
        store_rgb3_planes8(_mm256_castsi256_si128(r8), _mm256_castsi256_si128(g8), _mm256_castsi256_si128(b8), out);
        store_rgb3_planes8(_mm256_extracti128_si256(r8, 1), _mm256_extracti128_si256(g8, 1), _mm256_extracti128_si256(b8, 1), out.add(24));
    }
}

/// AVX2 u16-mulhi 变体：16 像素/步 YUV→RGB3 包装。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_to_rgb3_avx2_u16(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let yv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(y as *const __m128i)), 8);
        let uv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(u as *const __m128i)), 8);
        let vv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(v as *const __m128i)), 8);
        yuv16_core_avx2_u16_rgb3(yv, uv, vv, out);
    }
}

/// AVX2 u16-mulhi 变体：32 像素/步 YUV→RGB3（对照 [`yuv32_to_rgba_avx2_u16`]）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv32_to_rgb3_avx2_u16(
    y: *const u8,
    u: *const u8,
    v: *const u8,
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        let yb = _mm256_loadu_si256(y as *const __m256i);
        let ub = _mm256_loadu_si256(u as *const __m256i);
        let vb = _mm256_loadu_si256(v as *const __m256i);
        let yl = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(yb));
        let yh = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(yb, 1));
        let ul = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(ub));
        let uh = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(ub, 1));
        let vl = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(vb));
        let vh = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(vb, 1));
        yuv16_core_avx2_u16_rgb3(_mm256_slli_epi16(yl, 8), _mm256_slli_epi16(ul, 8), _mm256_slli_epi16(vl, 8), out);
        yuv16_core_avx2_u16_rgb3(_mm256_slli_epi16(yh, 8), _mm256_slli_epi16(uh, 8), _mm256_slli_epi16(vh, 8), out.add(48));
    }
}

/// 一行 YUV→RGB3（alpha 不写，3 字节/像素）：AVX2/SSE4.1/标量逐档镜像 [`yuv_row_to_rgba`] 的
/// 调度与内核（u16-mulhi 变体经 `yuv_u16_enabled` 同款门控）。每像素前 3 字节恒等于
/// `yuv_row_to_rgba` 同输入——新 RGB3 直出路径与旧 RGBA 路径的字节契约。
/// 输入三平面各 `n` 字节，输出 `3*n` 字节。
pub(crate) fn yuv_row_to_rgb3(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8]) {
    debug_assert_eq!(y.len(), u.len());
    debug_assert_eq!(y.len(), v.len());
    debug_assert_eq!(out.len(), y.len() * 3);
    let n = y.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 16 {
        let u16 = yuv_u16_enabled();
        let mut col = 0usize;
        unsafe {
            while col + 32 <= n {
                if u16 {
                    yuv32_to_rgb3_avx2_u16(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
                        out.as_mut_ptr().add(col * 3),
                    );
                } else {
                    yuv32_to_rgb3_avx2(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
                        out.as_mut_ptr().add(col * 3),
                    );
                }
                col += 32;
            }
            while col + 16 <= n {
                if u16 {
                    yuv16_to_rgb3_avx2_u16(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
                        out.as_mut_ptr().add(col * 3),
                    );
                } else {
                    yuv16_to_rgb3_avx2(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
                        out.as_mut_ptr().add(col * 3),
                    );
                }
                col += 16;
            }
            while col + 8 <= n {
                yuv8_to_rgb3_sse41(
                    y.as_ptr().add(col),
                    u.as_ptr().add(col),
                    v.as_ptr().add(col),
                    out.as_mut_ptr().add(col * 3),
                );
                col += 8;
            }
            for i in col..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                let o = i * 3;
                *out.get_unchecked_mut(o) = r;
                *out.get_unchecked_mut(o + 1) = g;
                *out.get_unchecked_mut(o + 2) = b;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse4.1") && n >= 8 {
        let mut col = 0usize;
        unsafe {
            while col + 8 <= n {
                yuv8_to_rgb3_sse41(
                    y.as_ptr().add(col),
                    u.as_ptr().add(col),
                    v.as_ptr().add(col),
                    out.as_mut_ptr().add(col * 3),
                );
                col += 8;
            }
            for i in col..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                let o = i * 3;
                *out.get_unchecked_mut(o) = r;
                *out.get_unchecked_mut(o + 1) = g;
                *out.get_unchecked_mut(o + 2) = b;
            }
        }
        return;
    }
    for i in 0..n {
        let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
        out[i * 3] = r;
        out[i * 3 + 1] = g;
        out[i * 3 + 2] = b;
    }
}

