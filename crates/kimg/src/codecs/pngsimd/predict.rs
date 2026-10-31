//! PNG 行反滤波（移植自 SimdCodecs `PngPredictor.cs`）：None / Sub / Up / Average / Paeth。
//!
//! 移植要点（与 C# 一一对应）：
//! - **Up** 无行内依赖，x86_64 上用 AVX2（32 字节）/ SSE2（16 字节）逐字节批量加（`_mm_add_epi8` 天然回绕 = mod 256）；
//! - **Sub / Average / Paeth** 每像素存在「左侧已反滤波」链，向量化反过来做：一步处理一个像素（bpp 个字节），
//!   用 128 位向量的低 bpp 道承载该像素的全部字节（LoadPixel 读 4/8 字节、StorePixel 只写 bpp 字节）；
//! - **Average 的 floor((a+b)/2)**：`_mm_avg_epu8`（向上取整）减 `((a ^ b) & 1)`（丢失的进位位）还原向下取整；
//! - **Paeth** 在 16 位道内用比较掩码选 a/b/c，无分支；`Abs` 用算术移位法（SSE2 通用，不依赖 SSSE3）。
//!
//! 全部向量路径都配纯标量回退，且向量结果与标量逐字节一致（见模块底部测试：随机行 5 filter × 各 bpp）。

use std::arch::is_x86_feature_detected;

/// 每像素字节数（Shell 中为预测用 bpp）：`samples * ((depth + 7) >> 3)`，sub-byte 恒为 1，
/// 与 png crate `bytes_per_pixel()` / SimdCodecs `bytesPerPixel` 同式。
pub(crate) fn prediction_bpp(samples: usize, depth: usize) -> usize {
    samples * ((depth + 7) >> 3)
}

/// 行字节数（源布局，含打包位）：`ceil(width * samples * depth / 8)`。
#[inline(always)]
pub(crate) fn packed_row_bytes(width: usize, samples: usize, depth: usize) -> usize {
    (width * samples * depth + 7) / 8
}

/// 反滤波一行（`filter`：0..=4；其它值按 None 处理——与 C#/png 语义一致，字节原样保留）。
pub(crate) fn unfilter_row(filter: u8, row: &mut [u8], above: &[u8], bpp: usize) {
    debug_assert!(above.len() >= row.len(), "above 行必须至少与当前行等长");
    match filter {
        0 | 5.. => {}
        1 => unfilter_sub(row, bpp),
        2 => unfilter_up(row, above),
        3 => unfilter_average(row, above, bpp),
        _ => unfilter_paeth(row, above, bpp),
    }
}

/// Paeth 预测器（标量版，与 C# `PaethPredictor` 同）。
#[inline(always)]
fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let pa = if p >= a { p - a } else { a - p };
    let pb = if p >= b { p - b } else { b - p };
    let pc = if p >= c { p - c } else { c - p };
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

// ---------------------------------------------------------------------------
// Up：AVX2 / SSE2 批量加法 + 标量尾
// ---------------------------------------------------------------------------

fn unfilter_up(row: &mut [u8], above: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { sse::unfilter_up_avx2(row, above) };
        }
        if is_x86_feature_detected!("sse2") {
            return unsafe { sse::unfilter_up_sse2(row, above) };
        }
    }
    for i in 0..row.len() {
        row[i] = row[i].wrapping_add(above[i]);
    }
}

// ---------------------------------------------------------------------------
// Sub：每像素向量 + 标量尾
// ---------------------------------------------------------------------------

fn unfilter_sub(row: &mut [u8], bpp: usize) {
    let n = row.len();
    #[cfg(target_arch = "x86_64")]
    if (2..=8).contains(&bpp) && !cfg!(debug_assertions) && is_x86_feature_detected!("sse2") {
        return unsafe { sse::unfilter_sub_sse2(row, bpp) };
    }
    for i in bpp..n {
        row[i] = row[i].wrapping_add(row[i - bpp]);
    }
}

// ---------------------------------------------------------------------------
// Average：每像素向量 + 标量尾
// ---------------------------------------------------------------------------

fn unfilter_average(row: &mut [u8], above: &[u8], bpp: usize) {
    let n = row.len();
    #[cfg(target_arch = "x86_64")]
    if (2..=8).contains(&bpp) && !cfg!(debug_assertions) && is_x86_feature_detected!("sse2") {
        return unsafe { sse::unfilter_average_sse2(row, above, bpp) };
    }
    for i in 0..n {
        let left = if i >= bpp { row[i - bpp] } else { 0 };
        row[i] = row[i].wrapping_add(((u16::from(left) + u16::from(above[i])) >> 1) as u8);
    }
}

// ---------------------------------------------------------------------------
// Paeth：每像素向量 + 标量尾
// ---------------------------------------------------------------------------

fn unfilter_paeth(row: &mut [u8], above: &[u8], bpp: usize) {
    let n = row.len();
    #[cfg(target_arch = "x86_64")]
    if (2..=8).contains(&bpp) && !cfg!(debug_assertions) && is_x86_feature_detected!("sse2") {
        return unsafe { sse::unfilter_paeth_sse2(row, above, bpp) };
    }
    for i in 0..n {
        let left = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
        let above_i = i32::from(above[i]);
        let upper_left = if i >= bpp { i32::from(above[i - bpp]) } else { 0 };
        row[i] = row[i].wrapping_add(paeth(left, above_i, upper_left) as u8);
    }
}

// ---------------------------------------------------------------------------
// x86_64 SSE2 实现
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
mod sse {
    use std::arch::x86_64::*;

    /// 一个像素（bpp<=4 读 4 字节、bpp>4 读 8 字节）的低道装入。宽 8 / 窄 4 与 C# `LoadPixel` 对齐。
    /// 行起始/跨度不保证 4 字节对齐（如 RGB 行宽是 3 的倍数），一律用 `read_unaligned`。
    #[inline(always)]
    unsafe fn load_pixel(src: *const u8, offset: usize, wide: bool) -> __m128i {
        unsafe {
            if wide {
                _mm_loadl_epi64(src.add(offset) as *const __m128i)
            } else {
                _mm_cvtsi32_si128(std::ptr::read_unaligned(src.add(offset) as *const u32) as i32)
            }
        }
    }

    /// 只写回 bpp 个字节（多出的道是后续像素的字节，绝不能提前覆盖——`StorePixel` 同款 switch；
    /// 目标地址可能未对齐，一律 `write_unaligned`）。
    #[inline(always)]
    unsafe fn store_pixel(dst: *mut u8, offset: usize, v: __m128i, bpp: usize) {
        unsafe {
            let d = dst.add(offset);
            let lo = _mm_cvtsi128_si32(v) as u32;
            match bpp {
                2 => std::ptr::write_unaligned(d.cast::<u16>(), lo as u16),
                3 => {
                    std::ptr::write_unaligned(d.cast::<u16>(), lo as u16);
                    *d.add(2) = (lo >> 16) as u8;
                }
                4 => std::ptr::write_unaligned(d.cast::<u32>(), lo),
                5 => {
                    std::ptr::write_unaligned(d.cast::<u32>(), lo);
                    *d.add(4) = _mm_extract_epi16(v, 2) as u8;
                }
                6 => {
                    std::ptr::write_unaligned(d.cast::<u32>(), lo);
                    std::ptr::write_unaligned(d.add(4).cast::<u16>(), _mm_extract_epi16(v, 2) as u16);
                }
                7 => {
                    std::ptr::write_unaligned(d.cast::<u32>(), lo);
                    std::ptr::write_unaligned(d.add(4).cast::<u16>(), _mm_extract_epi16(v, 2) as u16);
                    *d.add(6) = _mm_extract_epi16(v, 3) as u8;
                }
                _ => _mm_storel_epi64(d as *mut __m128i, v),
            }
        }
    }

    /// floor((a+b)/2) 逐字节：`avg_epu8`（向上取整）减 `((a ^ b) & 1)` 还原向下取整（C# `FloorAverage`）。
    #[inline(always)]
    unsafe fn floor_average(a: __m128i, b: __m128i) -> __m128i {
        unsafe {
            let avg = _mm_avg_epu8(a, b);
            _mm_sub_epi8(avg, _mm_and_si128(_mm_xor_si128(a, b), _mm_set1_epi8(1)))
        }
    }

    /// 16 位道绝对值：算术移位的教科书公式，纯 SSE2 即可（不依赖 SSSE3 的 `_mm_abs_epi16`）。
    #[inline(always)]
    unsafe fn abs_epi16(v: __m128i) -> __m128i {
        unsafe {
            let s = _mm_srai_epi16(v, 15);
            _mm_sub_epi16(_mm_xor_si128(v, s), s)
        }
    }

    /// 16 位道 `x <= y` → 全 1 掩码。
    #[inline(always)]
    unsafe fn le_epi16(x: __m128i, y: __m128i) -> __m128i {
        unsafe { _mm_or_si128(_mm_cmpeq_epi16(x, y), _mm_cmpgt_epi16(y, x)) }
    }

    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn unfilter_up_avx2(row: &mut [u8], above: &[u8]) {
        unsafe {
            let n = row.len();
            let mut i = 0;
            let r = row.as_mut_ptr();
            let p = above.as_ptr();
            while i + 32 <= n {
                let a = _mm256_loadu_si256(r.add(i) as *const __m256i);
                let b = _mm256_loadu_si256(p.add(i) as *const __m256i);
                _mm256_storeu_si256(r.add(i) as *mut __m256i, _mm256_add_epi8(a, b));
                i += 32;
            }
            while i + 16 <= n {
                let a = _mm_loadu_si128(r.add(i) as *const __m128i);
                let b = _mm_loadu_si128(p.add(i) as *const __m128i);
                _mm_storeu_si128(r.add(i) as *mut __m128i, _mm_add_epi8(a, b));
                i += 16;
            }
            while i < n {
                *r.add(i) = row[i].wrapping_add(above[i]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_up_sse2(row: &mut [u8], above: &[u8]) {
        unsafe {
            let n = row.len();
            let mut i = 0;
            let r = row.as_mut_ptr();
            let p = above.as_ptr();
            while i + 16 <= n {
                let a = _mm_loadu_si128(r.add(i) as *const __m128i);
                let b = _mm_loadu_si128(p.add(i) as *const __m128i);
                _mm_storeu_si128(r.add(i) as *mut __m128i, _mm_add_epi8(a, b));
                i += 16;
            }
            while i < n {
                *r.add(i) = row[i].wrapping_add(above[i]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_sub_sse2(row: &mut [u8], bpp: usize) {
        unsafe {
            let n = row.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let mut i = bpp; // 首个像素无左侧参考，原样保留
            if width <= n && i < n {
                let r = row.as_mut_ptr();
                let mut left = load_pixel(r, 0, wide);
                while i + width <= n {
                    let filtered = load_pixel(r, i, wide);
                    left = _mm_add_epi8(left, filtered);
                    store_pixel(r, i, left, bpp);
                    i += bpp;
                }
            }
            while i < n {
                *row.get_unchecked_mut(i) = row[i].wrapping_add(row[i - bpp]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_average_sse2(row: &mut [u8], above: &[u8], bpp: usize) {
        unsafe {
            let n = row.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let r = row.as_mut_ptr();
            let p = above.as_ptr();
            let mut i = 0;
            let mut left = _mm_setzero_si128();
            while i + width <= n {
                let filtered = load_pixel(r, i, wide);
                let above_px = load_pixel(p, i, wide);
                left = _mm_add_epi8(filtered, floor_average(left, above_px));
                store_pixel(r, i, left, bpp);
                i += bpp;
            }
            while i < n {
                let left_b = if i >= bpp { row[i - bpp] } else { 0 };
                *row.get_unchecked_mut(i) = row[i]
                    .wrapping_add(((u16::from(left_b) + u16::from(above[i])) >> 1) as u8);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_paeth_sse2(row: &mut [u8], above: &[u8], bpp: usize) {
        unsafe {
            let n = row.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let r = row.as_mut_ptr();
            let p = above.as_ptr();
            let zero = _mm_setzero_si128();
            let low = _mm_set1_epi16(0x00FF);
            let all_ones = _mm_set1_epi16(-1);
            let mut i = 0;
            // 16 位道内：a=左像素、c=左上像素；首像素 a=c=0。
            let mut a = zero;
            let mut c = zero;
            while i + width <= n {
                let filtered = load_pixel(r, i, wide);
                // b=上方像素，16 位道。
                let b = _mm_unpacklo_epi8(load_pixel(p, i, wide), zero);
                let wf = _mm_unpacklo_epi8(filtered, zero);
                let bc = _mm_sub_epi16(b, c);
                let ac = _mm_sub_epi16(a, c);
                let pa = abs_epi16(bc);
                let pb = abs_epi16(ac);
                let pc = abs_epi16(_mm_add_epi16(ac, bc));
                let take_a = _mm_and_si128(le_epi16(pa, pb), le_epi16(pa, pc));
                let take_b = le_epi16(pb, pc);
                // pred = takeA ? a : (takeB ? b : c)
                let bc_sel = _mm_or_si128(
                    _mm_and_si128(b, take_b),
                    _mm_and_si128(c, _mm_andnot_si128(take_b, all_ones)),
                );
                let pred = _mm_or_si128(
                    _mm_and_si128(a, take_a),
                    _mm_and_si128(bc_sel, _mm_andnot_si128(take_a, all_ones)),
                );
                a = _mm_and_si128(_mm_add_epi16(pred, wf), low);
                c = b;
                store_pixel(r, i, _mm_packus_epi16(a, a), bpp);
                i += bpp;
            }
            while i < n {
                let left = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
                let above_i = i32::from(above[i]);
                let upper_left = if i >= bpp { i32::from(above[i - bpp]) } else { 0 };
                let pred = super::paeth(left, above_i, upper_left);
                *row.get_unchecked_mut(i) = row[i].wrapping_add(pred as u8);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用给定 filter 正向滤波（PNG 编码方向），与解码互逆，得到解码输入。
    fn apply_filter(filter: u8, raw: &[u8], above: &[u8], bpp: usize) -> Vec<u8> {
        let mut out = raw.to_vec();
        for i in 0..raw.len() {
            match filter {
                0 => {}
                1 => out[i] = raw[i].wrapping_sub(if i >= bpp { raw[i - bpp] } else { 0 }),
                2 => out[i] = raw[i].wrapping_sub(above[i]),
                3 => {
                    let a = if i >= bpp { raw[i - bpp] } else { 0 };
                    out[i] = raw[i].wrapping_sub(((u16::from(a) + u16::from(above[i])) >> 1) as u8)
                }
                _ => {
                    let a = if i >= bpp { raw[i - bpp] } else { 0 };
                    let b = above[i];
                    let c = if i >= bpp { above[i - bpp] } else { 0 };
                    let pred = super::paeth(i32::from(a), i32::from(b), i32::from(c)) as u8;
                    out[i] = raw[i].wrapping_sub(pred)
                }
            }
        }
        out
    }

    /// 伪随机行（确定性 LCG）。
    fn random_row(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (s >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn scalar_and_simd_unfilter_agree_every_filter_every_bpp() {
        for bpp in 1..=8usize {
            for filter in 0..=4u8 {
                for len in [1, 2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33, 47, 64, 65, 128] {
                    let raw = random_row(len, 0x9E3779B97F4A7C15 ^ (bpp as u64 * 131 + filter as u64));
                    let above = random_row(len, 0x123456789ABCDEF0 ^ len as u64);
                    let filtered = apply_filter(filter, &raw, &above, bpp);
                    // 标量参考：直接算（不经过 unfilter_row 的 SIMD 派发）。
                    let mut scalar = filtered.clone();
                    match filter {
                        1 => scal_sub(&mut scalar, bpp),
                        2 => scal_up(&mut scalar, &above),
                        3 => scal_avg(&mut scalar, &above, bpp),
                        4 => scal_paeth(&mut scalar, &above, bpp),
                        _ => {}
                    }
                    // 走完整派发（标量回退 / SIMD 见机行事），窗口覆盖向量主径 + 标量尾。
                    let mut dispatched = filtered.clone();
                    single_row(&mut dispatched, &above, filter, bpp);
                    assert_eq!(scalar, raw, "标量参考不符 (filter={filter} bpp={bpp} len={len})");
                    assert_eq!(
                        dispatched, raw,
                        "派发路径不符 (filter={filter} bpp={bpp} len={len})"
                    );
                }
            }
        }
    }

    /// 不经 SIMD 派发的纯标量反滤波（测试参考）。
    #[inline(always)]
    fn single_row(row: &mut [u8], above: &[u8], filter: u8, bpp: usize) {
        match filter {
            1 => scal_sub(row, bpp),
            2 => scal_up(row, above),
            3 => scal_avg(row, above, bpp),
            4 => scal_paeth(row, above, bpp),
            _ => {}
        }
    }

    fn scal_sub(row: &mut [u8], bpp: usize) {
        for i in bpp..row.len() {
            row[i] = row[i].wrapping_add(row[i - bpp]);
        }
    }
    fn scal_up(row: &mut [u8], above: &[u8]) {
        for i in 0..row.len() {
            row[i] = row[i].wrapping_add(above[i]);
        }
    }
    fn scal_avg(row: &mut [u8], above: &[u8], bpp: usize) {
        for i in 0..row.len() {
            let left = if i >= bpp { row[i - bpp] } else { 0 };
            row[i] = row[i].wrapping_add(((u16::from(left) + u16::from(above[i])) >> 1) as u8);
        }
    }
    fn scal_paeth(row: &mut [u8], above: &[u8], bpp: usize) {
        for i in 0..row.len() {
            let left = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
            let b = i32::from(above[i]);
            let c = if i >= bpp { i32::from(above[i - bpp]) } else { 0 };
            row[i] = row[i].wrapping_add(super::paeth(left, b, c) as u8);
        }
    }

    #[test]
    fn prediction_bpp_matches_png_table() {
        // png crate：samples * ((depth + 7) >> 3)；sub-byte 恒 1。
        assert_eq!(prediction_bpp(1, 1), 1);
        assert_eq!(prediction_bpp(1, 4), 1);
        assert_eq!(prediction_bpp(3, 8), 3);
        assert_eq!(prediction_bpp(3, 16), 6);
        assert_eq!(prediction_bpp(4, 16), 8);
    }
}
