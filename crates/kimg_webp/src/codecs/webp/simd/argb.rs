use super::*;

// ---------------------------------------------------------------------------
// VP8L ARGB→RGBA8 输出（SSSE3 字节重排）与 Subtract-Green 逆变换（SSE4.1）——
// 对齐 vp8l.rs `to_rgba` / `apply_subtract_green`（mod 256 逐通道加，无进位）
// ---------------------------------------------------------------------------

/// ARGB 像素（u32：A<<24|R<<16|G<<8|B，内存小端字节序 [B,G,R,A]）→ RGBA8 字节。
/// 即 vp8l.rs `to_rgba` 的字节序交换（B/R 对调）；AVX2 每 8 像素一次 shuffle，SSSE3 4 像素
/// 收尾、尾列标量（AVX2 隐含 SSSE3）。字节恒等：shuffle 逐字节独立。
pub(crate) fn argb_to_rgba(pixels: &[u32], out: &mut [u8]) {
    debug_assert_eq!(out.len(), pixels.len() * 4);
    let n = pixels.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 8 {
        let mut i = 0usize;
        unsafe {
            while i + 8 <= n {
                argb8_to_rgba_avx2(pixels.as_ptr().add(i).cast::<u8>(), out.as_mut_ptr().add(i * 4));
                i += 8;
            }
            while i + 4 <= n {
                argb4_to_rgba_ssse3(pixels.as_ptr().add(i).cast::<u8>(), out.as_mut_ptr().add(i * 4));
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let o = i * 4;
                *out.get_unchecked_mut(o) = (p >> 16) as u8;
                *out.get_unchecked_mut(o + 1) = (p >> 8) as u8;
                *out.get_unchecked_mut(o + 2) = p as u8;
                *out.get_unchecked_mut(o + 3) = (p >> 24) as u8;
                i += 1;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && n >= 4 {
        let mut i = 0usize;
        unsafe {
            while i + 4 <= n {
                argb4_to_rgba_ssse3(pixels.as_ptr().add(i).cast::<u8>(), out.as_mut_ptr().add(i * 4));
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let o = i * 4;
                *out.get_unchecked_mut(o) = (p >> 16) as u8;
                *out.get_unchecked_mut(o + 1) = (p >> 8) as u8;
                *out.get_unchecked_mut(o + 2) = p as u8;
                *out.get_unchecked_mut(o + 3) = (p >> 24) as u8;
                i += 1;
            }
        }
        return;
    }
    for (i, &p) in pixels.iter().enumerate() {
        out[i * 4] = (p >> 16) as u8;
        out[i * 4 + 1] = (p >> 8) as u8;
        out[i * 4 + 2] = p as u8;
        out[i * 4 + 3] = (p >> 24) as u8;
    }
}

/// 原地版 ARGB→RGBA 收尾（`decode()` 复用同一缓冲，省整帧 RGBA 拷贝/分配）：
/// 每个 u32 的字节 0(B) 与 2(R) 对调（字节 3=A、1=G 原样）→ 内存序即 [R,G,B,A]。
/// 与 `argb_to_rgba` 逐字节同语义；AVX2/SSSE3 用 32/16 字节 shuffle 原地读写
/// （load→shuffle→store 同一地址，32 字节全量覆写，安全）。
pub(crate) fn argb_to_rgba_inplace(pixels: &mut [u32]) {
    let n = pixels.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 8 {
        unsafe {
            let p = pixels.as_mut_ptr().cast::<u8>();
            let mut i = 0usize;
            while i + 8 <= n {
                argb8_to_rgba_inplace_avx2(p.add(i * 4));
                i += 8;
            }
            while i + 4 <= n {
                argb4_to_rgba_inplace_ssse3(p.add(i * 4));
                i += 4;
            }
            while i < n {
                let b = *pixels.get_unchecked(i);
                *pixels.get_unchecked_mut(i) =
                    (b & 0xff00_ff00) | ((b & 0xff) << 16) | ((b >> 16) & 0xff);
                i += 1;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && n >= 4 {
        unsafe {
            let p = pixels.as_mut_ptr().cast::<u8>();
            let mut i = 0usize;
            while i + 4 <= n {
                argb4_to_rgba_inplace_ssse3(p.add(i * 4));
                i += 4;
            }
            while i < n {
                let b = *pixels.get_unchecked(i);
                *pixels.get_unchecked_mut(i) =
                    (b & 0xff00_ff00) | ((b & 0xff) << 16) | ((b >> 16) & 0xff);
                i += 1;
            }
        }
        return;
    }
    for px in pixels {
        let b = *px;
        *px = (b & 0xff00_ff00) | ((b & 0xff) << 16) | ((b >> 16) & 0xff);
    }
}

/// AVX2：8 像素 [B,G,R,A]×8 → [R,G,B,A]×8（原地 B/R 对调）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn argb8_to_rgba_inplace_avx2(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm256_loadu_si256(px as *const __m256i);
    // 低 16 字节 4 像素与高 16 字节 4 像素同一 mask（高半区索引 16..31 映射到 shuf 常量）。
    let shuf = _mm256_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
                                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm256_shuffle_epi8(v, shuf);
    _mm256_storeu_si256(px as *mut __m256i, out_v);
    }
}

/// AVX2：8 像素 [B,G,R,A]×8 → [R,G,B,A]×8（写独立输出缓冲）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn argb8_to_rgba_avx2(px: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm256_loadu_si256(px as *const __m256i);
    let shuf = _mm256_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
                                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm256_shuffle_epi8(v, shuf);
    _mm256_storeu_si256(out as *mut __m256i, out_v);
    }
}

/// SSSE3：4 像素 [B,G,R,A]×4 → [R,G,B,A]×4（字节重排：每像素取原 2,1,0,3）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn argb4_to_rgba_ssse3(px: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm_loadu_si128(px as *const __m128i); // [B0,G0,R0,A0, B1,G1,R1,A1, ...]
    let shuf = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm_shuffle_epi8(v, shuf);
    _mm_storeu_si128(out as *mut __m128i, out_v);
    }
}

/// SSSE3：4 像素原地 B/R 对调（load→shuffle→store 同一地址，16 字节全量覆写）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn argb4_to_rgba_inplace_ssse3(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm_loadu_si128(px as *const __m128i);
    let shuf = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm_shuffle_epi8(v, shuf);
    _mm_storeu_si128(px as *mut __m128i, out_v);
    }
}

/// Subtract-Green 逆变换（标量语义 `apply_subtract_green`）：R/G 通道 + G 通道（mod 256）。
/// 像素内存字节 [B,G,R,A]：把 G 字节加到 B 与 R 字节，G/A 原样保留。AVX2 每 8 像素、
/// SSE4.1 每 4 像素、尾列标量（AVX2 隐含 SSE4.1）。
pub(crate) fn subtract_green(pixels: &mut [u32]) {
    let n = pixels.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 8 {
        let mut i = 0usize;
        unsafe {
            while i + 8 <= n {
                subtract_green8_avx2(pixels.as_mut_ptr().add(i).cast::<u8>());
                i += 8;
            }
            while i + 4 <= n {
                subtract_green4_sse41(pixels.as_mut_ptr().add(i).cast::<u8>());
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let green = (p >> 8) & 0xff;
                let red = (((p >> 16) & 0xff).wrapping_add(green)) & 0xff;
                let blue = ((p & 0xff).wrapping_add(green)) & 0xff;
                *pixels.get_unchecked_mut(i) = (p & 0xff00_ff00) | (red << 16) | blue;
                i += 1;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse4.1") && n >= 8 {
        let mut i = 0usize;
        unsafe {
            while i + 4 <= n {
                subtract_green4_sse41(pixels.as_mut_ptr().add(i).cast::<u8>());
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let green = (p >> 8) & 0xff;
                let red = (((p >> 16) & 0xff).wrapping_add(green)) & 0xff;
                let blue = ((p & 0xff).wrapping_add(green)) & 0xff;
                *pixels.get_unchecked_mut(i) = (p & 0xff00_ff00) | (red << 16) | blue;
                i += 1;
            }
        }
        return;
    }
    for p in pixels {
        let green = (*p >> 8) & 0xff;
        let red = (((*p >> 16) & 0xff).wrapping_add(green)) & 0xff;
        let blue = ((*p & 0xff).wrapping_add(green)) & 0xff;
        *p = (*p & 0xff00_ff00) | (red << 16) | blue;
    }
}

/// SSE4.1：4 像素 Subtract-Green。`sum = px + G`（全通道 +G，mod 256）后，用 blendv 把
/// G(byte1)/A(byte3) 恢复为原值，B(byte0)/R(byte2) 保留加和。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn subtract_green4_sse41(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm_loadu_si128(px as *const __m128i);
    let gb = _mm_shuffle_epi8(v, _mm_setr_epi8(1, 1, 1, 1, 5, 5, 5, 5, 9, 9, 9, 9, 13, 13, 13, 13));
    let sum = _mm_add_epi8(v, gb);
    // mask 0xFF00FF00（LE bytes [00,FF,00,FF]）：byte1/byte3 取原值（P），byte0/2 取 sum。
    let res = _mm_blendv_epi8(sum, v, _mm_set1_epi32(0xFF00FF00u32 as i32));
    _mm_storeu_si128(px as *mut __m128i, res);
    }
}

/// AVX2：8 像素 Subtract-Green（同 `subtract_green4_sse41` 算术，256 位 1 步 8 像素）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse4.1")]
unsafe fn subtract_green8_avx2(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm256_loadu_si256(px as *const __m256i);
    // 每个 16 字节半区：byte1 广播到该像素 4 字节（G→[G,G,G,G]）。
    let gb = _mm256_shuffle_epi8(v, _mm256_setr_epi8(1, 1, 1, 1, 5, 5, 5, 5, 9, 9, 9, 9, 13, 13, 13, 13,
                                                      1, 1, 1, 1, 5, 5, 5, 5, 9, 9, 9, 9, 13, 13, 13, 13));
    let sum = _mm256_add_epi8(v, gb);
    let res = _mm256_blendv_epi8(sum, v, _mm256_set1_epi32(0xFF00FF00u32 as i32));
    _mm256_storeu_si256(px as *mut __m256i, res);
    }
}

