//! PNG 行反滤波（移植自 SimdCodecs `PngPredictor.cs`）：None / Sub / Up / Average / Paeth。
//!
//! 移植要点（与 C# 一一对应）：
//! - **Up** 无行内依赖，x86_64 上用 AVX2（32 字节）/ SSE2（16 字节）逐字节批量加（`_mm_add_epi8` 天然回绕 = mod 256）；
//! - **Sub / Average / Paeth** 每像素存在「左侧已反滤波」链，向量化反过来做：一步处理一个像素（bpp 个字节），
//!   用 128 位向量的低 bpp 道承载该像素的全部字节（LoadPixel 读 4/8 字节、StorePixel 只写 bpp 字节）；
//! - **Average 的 floor((a+b)/2)**：`_mm_avg_epu8`（向上取整）减 `((a ^ b) & 1)`（丢失的进位位）还原向下取整；
//! - **Paeth** 主径用 **8 位 lane 的 FPNGE 等价公式**（image-png #539/#632 已证与标准 Paeth 对
//!   2^24 全输入等价）：`pa=|b-c|`、`pb=|a-c|`、`pc=((a<c)==(c<b)) ? |pa-pb| : 255`，全部在
//!   u8 道内无分支完成（`_mm_max_epu8/min_epu8/cmpeq_epi8/blendv_epi8` 等），16/32 道一次处理
//!   一个像素的全部字节——比 16 位拓宽版短了去拓宽/回窄的指令链。u8 比较是无符号（`_mm_cmplt_epu8`
//!   语义经 0x80 翻转 + 有符号 `_mm_cmpgt_epi8` 表达），`<=` 用 `min(x,y)==x` 表达，避开 signed 坑。
//!   旧 16 位道实现改名 `unfilter_paeth_sse2_16lane` 保留作回退（新核 parity 不过即切回）；
//! - 左像素链是串行的（一个像素依赖其左侧反滤波结果），故按「每像素一步、bpp 个字节作道」向量化，
//!   与 libpng SSE2 / FPNG 同构；非 bpp 对齐尾部走标量收尾。
//!
//! 全部向量路径都配纯标量回退，且向量结果与标量逐字节一致（见模块底部测试：随机行 5 filter × 各 bpp、
//! Paeth 8 位 lane 专项大样本随机 parity）。

/// 每像素字节数（Shell 中为预测用 bpp）：`samples * ((depth + 7) >> 3)`，sub-byte 恒为 1，
/// 与 png crate `bytes_per_pixel()` / SimdCodecs `bytesPerPixel` 同式。
pub(crate) fn prediction_bpp(samples: usize, depth: usize) -> usize {
    samples * ((depth + 7) >> 3)
}

/// 行字节数（源布局，含打包位）：`ceil(width * samples * depth / 8)`。
#[inline(always)]
pub(crate) fn packed_row_bytes(width: usize, samples: usize, depth: usize) -> usize {
    (width * samples * depth).div_ceil(8)
}

/// Paeth 核实验开关（+ 回退保护）：`KIMG_PNG_PAETH_LEGACY=1` 强制走旧 16 位 lane 核
/// (`unfilter_paeth_*_16lane`)，用于 A/B 新旧核的全管线对比/在新核异常时切回。
/// 默认新 8 位 lane 核；`KIMG_PNG_NO_SIMD=1` 仍整体走标量。
fn paeth_legacy_forced() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        std::env::var("KIMG_PNG_PAETH_LEGACY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
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

/// 反滤波一行到**独立目标** `dst`（A2 src≠dst）：`src` 为过滤后字节（只读原始），`dst` 承接
/// 反滤波结果（其左侧字节供 Sub/Avg/Paeth 引用，故整行顺序从左到右写出），`above` 为上一行
/// 反滤波结果（只读）。与 [`unfilter_row`]（就地）数学上一一对应；用于「直接反滤波进输出行/
/// 行缓冲」避免一次整行拷贝。
///
/// 16bit 等大端样本按 **u8 逐字节语义** 运算（不引入 16 位进位），与 png crate / SimdCodecs 一致。
pub(crate) fn unfilter_to(filter: u8, src: &[u8], dst: &mut [u8], above: &[u8], bpp: usize) {
    debug_assert_eq!(src.len(), dst.len(), "src 与 dst 必须等长");
    debug_assert!(above.len() >= dst.len(), "above 行必须至少与当前行等长");
    match filter {
        0 | 5.. => dst.copy_from_slice(src),
        1 => unfilter_sub_to(src, dst, bpp),
        2 => unfilter_up_to(src, dst, above),
        3 => unfilter_average_to(src, dst, above, bpp),
        _ => unfilter_paeth_to(src, dst, above, bpp),
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
        if kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "avx2") {
            return unsafe { sse::unfilter_up_avx2(row, above) };
        }
        if kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "sse2") {
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
    if !cfg!(debug_assertions) && !kimg_core::simd::env_off("KIMG_PNG_NO_SIMD") {
        if bpp == 1 {
            // bpp==1 每字节一个像素，串行链用「块前缀和」向量化（16/32 字节块 + 跨块 carry 传播）。
            if kimg_core::simd::detect("avx2") {
                return unsafe { sse::unfilter_sub_bpp1_avx2(row) };
            }
            if kimg_core::simd::detect("sse2") {
                return unsafe { sse::unfilter_sub_bpp1_sse2(row) };
            }
        } else if (2..=8).contains(&bpp) && kimg_core::simd::detect("sse2") {
            return unsafe { sse::unfilter_sub_sse2(row, bpp) };
        }
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
    if (2..=8).contains(&bpp)
        && !cfg!(debug_assertions)
        && kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "sse2")
    {
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
    if (2..=8).contains(&bpp)
        && !cfg!(debug_assertions)
        && !kimg_core::simd::env_off("KIMG_PNG_NO_SIMD")
    {
        if paeth_legacy_forced() {
            // 回退：旧 16 位 lane 实现（SSE2 通用）。
            if kimg_core::simd::detect("sse2") {
                return unsafe { sse::unfilter_paeth_sse2_16lane(row, above, bpp) };
            }
        } else {
            // 8 位 lane FPNGE 主径：AVX2 256 位 / SSE4.1 128 位，逐像素一步（bpp 字节作道）。
            if kimg_core::simd::detect("avx2") {
                return unsafe { sse::unfilter_paeth_avx2(row, above, bpp) };
            }
            if kimg_core::simd::detect("sse4.1") {
                return unsafe { sse::unfilter_paeth_sse41(row, above, bpp) };
            }
            if kimg_core::simd::detect("sse2") {
                return unsafe { sse::unfilter_paeth_sse2_16lane(row, above, bpp) };
            }
        }
    }
    for i in 0..n {
        let left = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
        let above_i = i32::from(above[i]);
        let upper_left = if i >= bpp {
            i32::from(above[i - bpp])
        } else {
            0
        };
        row[i] = row[i].wrapping_add(paeth(left, above_i, upper_left) as u8);
    }
}

// ---------------------------------------------------------------------------
// unfilter_to 的标量实现（src 只读 / dst 写满）。向量主径见下方 `sse::*_to_*`。
// ---------------------------------------------------------------------------

fn unfilter_up_to(src: &[u8], dst: &mut [u8], above: &[u8]) {
    let n = dst.len();
    #[cfg(target_arch = "x86_64")]
    {
        if kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "avx2") {
            return unsafe { sse::unfilter_up_to_avx2(src, dst, above) };
        }
        if kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "sse2") {
            return unsafe { sse::unfilter_up_to_sse2(src, dst, above) };
        }
    }
    for i in 0..n {
        dst[i] = src[i].wrapping_add(above[i]);
    }
}

fn unfilter_sub_to(src: &[u8], dst: &mut [u8], bpp: usize) {
    let n = dst.len();
    #[cfg(target_arch = "x86_64")]
    if !cfg!(debug_assertions) && !kimg_core::simd::env_off("KIMG_PNG_NO_SIMD") {
        if bpp == 1 {
            // bpp==1 每字节一个像素：块前缀和（读 src 块、跨块 carry 写 dst）。
            if kimg_core::simd::detect("avx2") {
                return unsafe { sse::unfilter_sub_to_bpp1_avx2(src, dst) };
            }
            if kimg_core::simd::detect("sse2") {
                return unsafe { sse::unfilter_sub_to_bpp1_sse2(src, dst) };
            }
        } else if (2..=8).contains(&bpp) && kimg_core::simd::detect("sse2") {
            return unsafe { sse::unfilter_sub_to_sse2(src, dst, bpp) };
        }
    }
    for i in 0..n {
        let left = if i >= bpp { dst[i - bpp] } else { 0 };
        dst[i] = src[i].wrapping_add(left);
    }
}

fn unfilter_average_to(src: &[u8], dst: &mut [u8], above: &[u8], bpp: usize) {
    let n = dst.len();
    #[cfg(target_arch = "x86_64")]
    if (2..=8).contains(&bpp)
        && !cfg!(debug_assertions)
        && kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "sse2")
    {
        return unsafe { sse::unfilter_average_to_sse2(src, dst, above, bpp) };
    }
    for i in 0..n {
        let left = if i >= bpp { dst[i - bpp] } else { 0 };
        dst[i] = src[i].wrapping_add(((u16::from(left) + u16::from(above[i])) >> 1) as u8);
    }
}

fn unfilter_paeth_to(src: &[u8], dst: &mut [u8], above: &[u8], bpp: usize) {
    let n = dst.len();
    #[cfg(target_arch = "x86_64")]
    if (2..=8).contains(&bpp)
        && !cfg!(debug_assertions)
        && !kimg_core::simd::env_off("KIMG_PNG_NO_SIMD")
    {
        if paeth_legacy_forced() {
            if kimg_core::simd::detect("sse2") {
                return unsafe { sse::unfilter_paeth_to_sse2_16lane(src, dst, above, bpp) };
            }
        } else {
            if kimg_core::simd::detect("avx2") {
                return unsafe { sse::unfilter_paeth_to_avx2(src, dst, above, bpp) };
            }
            if kimg_core::simd::detect("sse4.1") {
                return unsafe { sse::unfilter_paeth_to_sse41(src, dst, above, bpp) };
            }
            if kimg_core::simd::detect("sse2") {
                return unsafe { sse::unfilter_paeth_to_sse2_16lane(src, dst, above, bpp) };
            }
        }
    }
    for i in 0..n {
        let left = if i >= bpp { i32::from(dst[i - bpp]) } else { 0 };
        let above_i = i32::from(above[i]);
        let upper_left = if i >= bpp {
            i32::from(above[i - bpp])
        } else {
            0
        };
        dst[i] = src[i].wrapping_add(paeth(left, above_i, upper_left) as u8);
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
                    std::ptr::write_unaligned(
                        d.add(4).cast::<u16>(),
                        _mm_extract_epi16(v, 2) as u16,
                    );
                }
                7 => {
                    std::ptr::write_unaligned(d.cast::<u32>(), lo);
                    std::ptr::write_unaligned(
                        d.add(4).cast::<u16>(),
                        _mm_extract_epi16(v, 2) as u16,
                    );
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

    /// 取 128 位向量字节 15（作为 bpp==1 Sub 的跨块 carry）。
    #[inline(always)]
    unsafe fn byte15(v: __m128i) -> u8 {
        unsafe { _mm_cvtsi128_si32(_mm_srli_si128::<15>(v)) as u8 }
    }

    /// bpp==1 Sub：16 字节块的**块内前缀和 + 跨块 carry 传播**（精确等价标量链 out[i]=f[i]+out[i-1]，
    /// 模 256 回绕天然成立）。`_mm_slli_si128` 是整 128 位字节移位，无跨子块问题。
    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_sub_bpp1_sse2(row: &mut [u8]) {
        unsafe {
            let n = row.len();
            if n == 0 {
                return;
            }
            let r = row.as_mut_ptr();
            let mut i = 1usize; // 首字节原样（无左侧参考）
            let mut carry: u8 = *r;
            while i + 16 <= n {
                let v = _mm_loadu_si128(r.add(i) as *const __m128i);
                // 块内前缀和：p[k] = f[0]+..+f[k]（模 256）。
                let mut p = v;
                p = _mm_add_epi8(p, _mm_slli_si128::<1>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<2>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<4>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<8>(p));
                let out = _mm_add_epi8(p, _mm_set1_epi8(carry as i8));
                _mm_storeu_si128(r.add(i) as *mut __m128i, out);
                carry = byte15(out);
                i += 16;
            }
            while i < n {
                let next = row[i].wrapping_add(row[i - 1]);
                *r.add(i) = next;
                i += 1;
            }
        }
    }

    /// bpp==1 Sub：AVX2 32 字节块。`_mm256_slli_si256` 是**每 128 位道内**移位（实测非跨道），
    /// 故高半道需补上「低半道块内累加和」作跨道 carry：低半输出末字节 = carry+Σ低 16 过滤字节。
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn unfilter_sub_bpp1_avx2(row: &mut [u8]) {
        unsafe {
            let n = row.len();
            if n == 0 {
                return;
            }
            let r = row.as_mut_ptr();
            let mut i = 1usize;
            let mut carry: u8 = *r;
            while i + 32 <= n {
                let v = _mm256_loadu_si256(r.add(i) as *const __m256i);
                // 每道内前缀和（两道独立，缺跨道 carry）。
                let mut p = v;
                p = _mm256_add_epi8(p, _mm256_slli_si256::<1>(p));
                p = _mm256_add_epi8(p, _mm256_slli_si256::<2>(p));
                p = _mm256_add_epi8(p, _mm256_slli_si256::<4>(p));
                p = _mm256_add_epi8(p, _mm256_slli_si256::<8>(p));
                let lo = _mm256_castsi256_si128(p);
                let hi = _mm256_extracti128_si256::<1>(p);
                // 低 16 输出。
                let out_lo = _mm_add_epi8(lo, _mm_set1_epi8(carry as i8));
                carry = byte15(out_lo);
                // 高 16：补上车入（= 低半输出末字节）。
                let out_hi = _mm_add_epi8(hi, _mm_set1_epi8(carry as i8));
                carry = byte15(out_hi);
                let out = _mm256_set_m128i(out_hi, out_lo);
                _mm256_storeu_si256(r.add(i) as *mut __m256i, out);
                i += 32;
            }
            while i + 16 <= n {
                let v = _mm_loadu_si128(r.add(i) as *const __m128i);
                let mut p = v;
                p = _mm_add_epi8(p, _mm_slli_si128::<1>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<2>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<4>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<8>(p));
                let out = _mm_add_epi8(p, _mm_set1_epi8(carry as i8));
                _mm_storeu_si128(r.add(i) as *mut __m128i, out);
                carry = byte15(out);
                i += 16;
            }
            while i < n {
                let next = row[i].wrapping_add(row[i - 1]);
                *r.add(i) = next;
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
                *row.get_unchecked_mut(i) =
                    row[i].wrapping_add(((u16::from(left_b) + u16::from(above[i])) >> 1) as u8);
                i += 1;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Paeth 8 位 lane FPNGE 主径（SSE4.1 128 位 / AVX2 256 位）
    // -----------------------------------------------------------------------
    //
    // 与标准 Paeth 对 2^24 全输入等价的 u8 道公式（image-png #539/#632，见模块头）：
    //   pa = max(b,c) - min(b,c)          // |b-c|（u8 精确差，max-min 免下溢）
    //   pb = max(a,c) - min(a,c)          // |a-c|
    //   absdiff = max(pa,pb) - min(pa,pb) // |pa-pb|
    //   cond = (a<c) == (c<b)             // a、b 在 c 两侧（或其一等于 c）→ |a+b-2c| = |pa-pb|
    //   pc   = cond ? absdiff : 255       // 同侧时真 pc=pa+pb ≥ max(pa,pb)，255 充「无限大」
    //   pred = a if pa<=pb && pa<=pc else b if pb<=pc else c（平局顺序 a→b→c 与 PNG spec 一致）
    // 要点：u8 无符号比较经 0x80 翻转 + 有符号 `_mm_cmpgt_epi8` 表达（低级掩码问题）；`<=` 用
    // `min(x,y)==x`。左像素链串行（一个像素依赖其左邻反滤波结果），故逐像素一步、bpp 字节作道。

    /// 128 位：16 条 u8 道并行 FPNGE Paeth 预测值（高道为垃圾，不影响低道——全部运算是逐道的）。
    #[inline(always)]
    pub(crate) unsafe fn paeth_pred8_128(a: __m128i, b: __m128i, c: __m128i) -> __m128i {
        unsafe {
            // 0x80 翻转保持 u8 域序 → i8 域序单调，无符号 < 即翻转后有符号 <。
            let k80 = _mm_set1_epi8(-128); // i8 -128 == u8 0x80
            let pa = _mm_sub_epi8(_mm_max_epu8(b, c), _mm_min_epu8(b, c));
            let pb = _mm_sub_epi8(_mm_max_epu8(a, c), _mm_min_epu8(a, c));
            let abs_diff = _mm_sub_epi8(_mm_max_epu8(pa, pb), _mm_min_epu8(pa, pb));
            let lt_ac = _mm_cmpgt_epi8(_mm_xor_si128(c, k80), _mm_xor_si128(a, k80)); // (a<c)
            let lt_cb = _mm_cmpgt_epi8(_mm_xor_si128(b, k80), _mm_xor_si128(c, k80)); // (c<b)
            let cond = _mm_cmpeq_epi8(lt_ac, lt_cb); // (a<c) == (c<b)
            // pc = cond ? absdiff : 255（blendv：mask 符号位 1 取第二操作数）。
            let pc = _mm_blendv_epi8(_mm_set1_epi8(-1), abs_diff, cond);
            // x<=y ⟺ min(x,y)==x。
            let le_pa_pb = _mm_cmpeq_epi8(_mm_min_epu8(pa, pb), pa);
            let le_pa_pc = _mm_cmpeq_epi8(_mm_min_epu8(pa, pc), pa);
            let le_pb_pc = _mm_cmpeq_epi8(_mm_min_epu8(pb, pc), pb);
            let take_a = _mm_and_si128(le_pa_pb, le_pa_pc); // pa<=pb && pa<=pc
            // pred = take_a ? a : (le_pb_pc ? b : c)。
            let b_or_c = _mm_blendv_epi8(c, b, le_pb_pc);
            _mm_blendv_epi8(b_or_c, a, take_a)
        }
    }

    /// 256 位：32 条 u8 道，同构于 [`paeth_pred8_128`]（AVX2）。逐像素使用时只消费低 bpp 道。
    #[inline(always)]
    pub(crate) unsafe fn paeth_pred8_256(a: __m256i, b: __m256i, c: __m256i) -> __m256i {
        unsafe {
            let k80 = _mm256_set1_epi8(-128);
            let pa = _mm256_sub_epi8(_mm256_max_epu8(b, c), _mm256_min_epu8(b, c));
            let pb = _mm256_sub_epi8(_mm256_max_epu8(a, c), _mm256_min_epu8(a, c));
            let abs_diff = _mm256_sub_epi8(_mm256_max_epu8(pa, pb), _mm256_min_epu8(pa, pb));
            let lt_ac = _mm256_cmpgt_epi8(_mm256_xor_si256(c, k80), _mm256_xor_si256(a, k80));
            let lt_cb = _mm256_cmpgt_epi8(_mm256_xor_si256(b, k80), _mm256_xor_si256(c, k80));
            let cond = _mm256_cmpeq_epi8(lt_ac, lt_cb);
            let pc = _mm256_blendv_epi8(_mm256_set1_epi8(-1), abs_diff, cond);
            let le_pa_pb = _mm256_cmpeq_epi8(_mm256_min_epu8(pa, pb), pa);
            let le_pa_pc = _mm256_cmpeq_epi8(_mm256_min_epu8(pa, pc), pa);
            let le_pb_pc = _mm256_cmpeq_epi8(_mm256_min_epu8(pb, pc), pb);
            let take_a = _mm256_and_si256(le_pa_pb, le_pa_pc);
            let b_or_c = _mm256_blendv_epi8(c, b, le_pb_pc);
            _mm256_blendv_epi8(b_or_c, a, take_a)
        }
    }

    #[target_feature(enable = "sse4.1")]
    pub(crate) unsafe fn unfilter_paeth_sse41(row: &mut [u8], above: &[u8], bpp: usize) {
        unsafe {
            let n = row.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let r = row.as_mut_ptr();
            let p = above.as_ptr();
            // a=左像素（已反滤波）、c=左上像素；首像素 a=c=0（预测器=0 处理与旧实现一致）。
            let mut a = _mm_setzero_si128();
            let mut c = _mm_setzero_si128();
            let mut i = 0;
            while i + width <= n {
                let filtered = load_pixel(r, i, wide);
                let b = load_pixel(p, i, wide);
                let pred = paeth_pred8_128(a, b, c);
                a = _mm_add_epi8(pred, filtered); // u8 回绕
                c = b;
                store_pixel(r, i, a, bpp);
                i += bpp;
            }
            while i < n {
                let left = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
                let above_i = i32::from(above[i]);
                let upper_left = if i >= bpp {
                    i32::from(above[i - bpp])
                } else {
                    0
                };
                let pred = super::paeth(left, above_i, upper_left);
                *row.get_unchecked_mut(i) = row[i].wrapping_add(pred as u8);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn unfilter_paeth_avx2(row: &mut [u8], above: &[u8], bpp: usize) {
        unsafe {
            let n = row.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let r = row.as_mut_ptr();
            let p = above.as_ptr();
            let mut a = _mm256_setzero_si256();
            let mut c = _mm256_setzero_si256();
            let mut i = 0;
            while i + width <= n {
                // 低 128 位装入像素字节（宽 8/窄 4），上 128 位清零；高道垃圾不影响低 bpp 道。
                let filtered = load_pixel(r, i, wide);
                let b = load_pixel(p, i, wide);
                let b256 = _mm256_castsi128_si256(b);
                let f256 = _mm256_castsi128_si256(filtered);
                let pred = paeth_pred8_256(a, b256, c);
                let out = _mm256_add_epi8(pred, f256);
                a = out;
                c = b256;
                store_pixel(r, i, _mm256_castsi256_si128(out), bpp);
                i += bpp;
            }
            while i < n {
                let left = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
                let above_i = i32::from(above[i]);
                let upper_left = if i >= bpp {
                    i32::from(above[i - bpp])
                } else {
                    0
                };
                let pred = super::paeth(left, above_i, upper_left);
                *row.get_unchecked_mut(i) = row[i].wrapping_add(pred as u8);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse4.1")]
    pub(crate) unsafe fn unfilter_paeth_to_sse41(
        src: &[u8],
        dst: &mut [u8],
        above: &[u8],
        bpp: usize,
    ) {
        unsafe {
            let n = dst.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let p = above.as_ptr();
            let mut a = _mm_setzero_si128();
            let mut c = _mm_setzero_si128();
            let mut i = 0;
            while i + width <= n {
                let filtered = load_pixel(s, i, wide); // a/c 寄存器累加，只读 src
                let b = load_pixel(p, i, wide);
                let pred = paeth_pred8_128(a, b, c);
                a = _mm_add_epi8(pred, filtered);
                c = b;
                store_pixel(d, i, a, bpp);
                i += bpp;
            }
            while i < n {
                let left = if i >= bpp { i32::from(dst[i - bpp]) } else { 0 };
                let above_i = i32::from(above[i]);
                let upper_left = if i >= bpp {
                    i32::from(above[i - bpp])
                } else {
                    0
                };
                let pred = super::paeth(left, above_i, upper_left);
                *dst.get_unchecked_mut(i) = src[i].wrapping_add(pred as u8);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn unfilter_paeth_to_avx2(
        src: &[u8],
        dst: &mut [u8],
        above: &[u8],
        bpp: usize,
    ) {
        unsafe {
            let n = dst.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let p = above.as_ptr();
            let mut a = _mm256_setzero_si256();
            let mut c = _mm256_setzero_si256();
            let mut i = 0;
            while i + width <= n {
                let filtered = load_pixel(s, i, wide);
                let b = load_pixel(p, i, wide);
                let b256 = _mm256_castsi128_si256(b);
                let f256 = _mm256_castsi128_si256(filtered);
                let pred = paeth_pred8_256(a, b256, c);
                let out = _mm256_add_epi8(pred, f256);
                a = out;
                c = b256;
                store_pixel(d, i, _mm256_castsi256_si128(out), bpp);
                i += bpp;
            }
            while i < n {
                let left = if i >= bpp { i32::from(dst[i - bpp]) } else { 0 };
                let above_i = i32::from(above[i]);
                let upper_left = if i >= bpp {
                    i32::from(above[i - bpp])
                } else {
                    0
                };
                let pred = super::paeth(left, above_i, upper_left);
                *dst.get_unchecked_mut(i) = src[i].wrapping_add(pred as u8);
                i += 1;
            }
        }
    }

    /// 旧 Paeth 实现（16 位 lane 拓宽，SSE2 通用）——保留作回退：新 8 位 lane 核若在某平台
    /// parity 不过或无收益，`unfilter_paeth*` 派发可切回本实现（见模块头「回退保护」）。
    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_paeth_sse2_16lane(row: &mut [u8], above: &[u8], bpp: usize) {
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
                let upper_left = if i >= bpp {
                    i32::from(above[i - bpp])
                } else {
                    0
                };
                let pred = super::paeth(left, above_i, upper_left);
                *row.get_unchecked_mut(i) = row[i].wrapping_add(pred as u8);
                i += 1;
            }
        }
    }

    // -----------------------------------------------------------------------
    // unfilter_to（src≠dst）向量主径：与就地版同算法，仅「读 src、写 dst」。
    // 非像素的低道垃圾字节只参与掩码选择、绝不写入 dst（同就地版车道推理）。
    // -----------------------------------------------------------------------

    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn unfilter_up_to_avx2(src: &[u8], dst: &mut [u8], above: &[u8]) {
        unsafe {
            let n = dst.len();
            let mut i = 0;
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let p = above.as_ptr();
            while i + 32 <= n {
                let a = _mm256_loadu_si256(s.add(i) as *const __m256i);
                let b = _mm256_loadu_si256(p.add(i) as *const __m256i);
                _mm256_storeu_si256(d.add(i) as *mut __m256i, _mm256_add_epi8(a, b));
                i += 32;
            }
            while i + 16 <= n {
                let a = _mm_loadu_si128(s.add(i) as *const __m128i);
                let b = _mm_loadu_si128(p.add(i) as *const __m128i);
                _mm_storeu_si128(d.add(i) as *mut __m128i, _mm_add_epi8(a, b));
                i += 16;
            }
            while i < n {
                *d.add(i) = src[i].wrapping_add(above[i]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_up_to_sse2(src: &[u8], dst: &mut [u8], above: &[u8]) {
        unsafe {
            let n = dst.len();
            let mut i = 0;
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let p = above.as_ptr();
            while i + 16 <= n {
                let a = _mm_loadu_si128(s.add(i) as *const __m128i);
                let b = _mm_loadu_si128(p.add(i) as *const __m128i);
                _mm_storeu_si128(d.add(i) as *mut __m128i, _mm_add_epi8(a, b));
                i += 16;
            }
            while i < n {
                *d.add(i) = src[i].wrapping_add(above[i]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_sub_to_sse2(src: &[u8], dst: &mut [u8], bpp: usize) {
        unsafe {
            let n = dst.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            // 首像素无左侧参考：原样拷入（dst[0..bpp) 即左邻初始值）。
            std::ptr::copy_nonoverlapping(s, d, bpp.min(n));
            let mut i = bpp;
            if width <= n && i < n {
                let mut left = load_pixel(d, 0, wide);
                while i + width <= n {
                    let filtered = load_pixel(s, i, wide);
                    left = _mm_add_epi8(left, filtered);
                    store_pixel(d, i, left, bpp);
                    i += bpp;
                }
            }
            while i < n {
                *dst.get_unchecked_mut(i) = src[i].wrapping_add(dst[i - bpp]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_sub_to_bpp1_sse2(src: &[u8], dst: &mut [u8]) {
        unsafe {
            let n = dst.len();
            if n == 0 {
                return;
            }
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            *d = *s; // dst[0] = src[0]（首字节原样）
            let mut i = 1usize;
            let mut carry: u8 = *s;
            while i + 16 <= n {
                let v = _mm_loadu_si128(s.add(i) as *const __m128i);
                let mut p = v;
                p = _mm_add_epi8(p, _mm_slli_si128::<1>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<2>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<4>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<8>(p));
                let out = _mm_add_epi8(p, _mm_set1_epi8(carry as i8));
                _mm_storeu_si128(d.add(i) as *mut __m128i, out);
                carry = byte15(out);
                i += 16;
            }
            while i < n {
                *d.add(i) = src[i].wrapping_add(dst[i - 1]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn unfilter_sub_to_bpp1_avx2(src: &[u8], dst: &mut [u8]) {
        unsafe {
            let n = dst.len();
            if n == 0 {
                return;
            }
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            *d = *s;
            let mut i = 1usize;
            let mut carry: u8 = *s;
            while i + 32 <= n {
                let v = _mm256_loadu_si256(s.add(i) as *const __m256i);
                let mut p = v;
                p = _mm256_add_epi8(p, _mm256_slli_si256::<1>(p));
                p = _mm256_add_epi8(p, _mm256_slli_si256::<2>(p));
                p = _mm256_add_epi8(p, _mm256_slli_si256::<4>(p));
                p = _mm256_add_epi8(p, _mm256_slli_si256::<8>(p));
                let lo = _mm256_castsi256_si128(p);
                let hi = _mm256_extracti128_si256::<1>(p);
                let out_lo = _mm_add_epi8(lo, _mm_set1_epi8(carry as i8));
                carry = byte15(out_lo);
                let out_hi = _mm_add_epi8(hi, _mm_set1_epi8(carry as i8));
                carry = byte15(out_hi);
                let out = _mm256_set_m128i(out_hi, out_lo);
                _mm256_storeu_si256(d.add(i) as *mut __m256i, out);
                i += 32;
            }
            while i + 16 <= n {
                let v = _mm_loadu_si128(s.add(i) as *const __m128i);
                let mut p = v;
                p = _mm_add_epi8(p, _mm_slli_si128::<1>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<2>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<4>(p));
                p = _mm_add_epi8(p, _mm_slli_si128::<8>(p));
                let out = _mm_add_epi8(p, _mm_set1_epi8(carry as i8));
                _mm_storeu_si128(d.add(i) as *mut __m128i, out);
                carry = byte15(out);
                i += 16;
            }
            while i < n {
                *d.add(i) = src[i].wrapping_add(dst[i - 1]);
                i += 1;
            }
        }
    }

    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_average_to_sse2(
        src: &[u8],
        dst: &mut [u8],
        above: &[u8],
        bpp: usize,
    ) {
        unsafe {
            let n = dst.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let p = above.as_ptr();
            let mut i = 0;
            let mut left = _mm_setzero_si128();
            while i + width <= n {
                let filtered = load_pixel(s, i, wide);
                let above_px = load_pixel(p, i, wide);
                left = _mm_add_epi8(filtered, floor_average(left, above_px));
                store_pixel(d, i, left, bpp);
                i += bpp;
            }
            while i < n {
                let left_b = if i >= bpp { dst[i - bpp] } else { 0 };
                *dst.get_unchecked_mut(i) =
                    src[i].wrapping_add(((u16::from(left_b) + u16::from(above[i])) >> 1) as u8);
                i += 1;
            }
        }
    }

    /// 旧 Paeth src≠dst 实现（16 位 lane 拓宽）——新 8 位 lane 核的回退。
    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn unfilter_paeth_to_sse2_16lane(
        src: &[u8],
        dst: &mut [u8],
        above: &[u8],
        bpp: usize,
    ) {
        unsafe {
            let n = dst.len();
            let wide = bpp > 4;
            let width = if wide { 8 } else { 4 };
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let p = above.as_ptr();
            let zero = _mm_setzero_si128();
            let low = _mm_set1_epi16(0x00FF);
            let all_ones = _mm_set1_epi16(-1);
            let mut i = 0;
            let mut a = zero;
            let mut c = zero;
            while i + width <= n {
                let filtered = load_pixel(s, i, wide);
                let b = _mm_unpacklo_epi8(load_pixel(p, i, wide), zero);
                let wf = _mm_unpacklo_epi8(filtered, zero);
                let bc = _mm_sub_epi16(b, c);
                let ac = _mm_sub_epi16(a, c);
                let pa = abs_epi16(bc);
                let pb = abs_epi16(ac);
                let pc = abs_epi16(_mm_add_epi16(ac, bc));
                let take_a = _mm_and_si128(le_epi16(pa, pb), le_epi16(pa, pc));
                let take_b = le_epi16(pb, pc);
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
                store_pixel(d, i, _mm_packus_epi16(a, a), bpp);
                i += bpp;
            }
            while i < n {
                let left = if i >= bpp { i32::from(dst[i - bpp]) } else { 0 };
                let above_i = i32::from(above[i]);
                let upper_left = if i >= bpp {
                    i32::from(above[i - bpp])
                } else {
                    0
                };
                let pred = super::paeth(left, above_i, upper_left);
                *dst.get_unchecked_mut(i) = src[i].wrapping_add(pred as u8);
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
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn scalar_and_simd_unfilter_agree_every_filter_every_bpp() {
        for bpp in 1..=8usize {
            for filter in 0..=4u8 {
                for len in [1, 2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33, 47, 64, 65, 128] {
                    let raw =
                        random_row(len, 0x9E3779B97F4A7C15 ^ (bpp as u64 * 131 + filter as u64));
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
                    unfilter_row(filter, &mut dispatched, &above, bpp);
                    assert_eq!(
                        scalar, raw,
                        "标量参考不符 (filter={filter} bpp={bpp} len={len})"
                    );
                    assert_eq!(
                        dispatched, raw,
                        "派发路径不符 (filter={filter} bpp={bpp} len={len})"
                    );
                }
            }
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
            let c = if i >= bpp {
                i32::from(above[i - bpp])
            } else {
                0
            };
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

    /// 直接调 x86_64 SSE2 向量反滤波，与标量逐字节对齐（debug 下 `unfilter_row` 的
    /// `!cfg!(debug_assertions)` 门禁会跳过 Sub/Average/Paeth 向量主径——本测试在宿主支持
    /// SSE2 时强制走向量路径，补上默认测试管线的 SIMD 覆盖缺口）。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sse_unfilter_matches_scalar() {
        if !kimg_core::simd::detect("sse2") {
            return;
        }
        for bpp in 2..=8usize {
            for filter in [1u8, 3, 4] {
                for len in [
                    1, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33, 47, 60, 64, 65, 96, 128,
                ] {
                    let raw = random_row(len, 0xDEADBEEF_CAFE ^ (bpp as u64 * 131 + filter as u64));
                    let above = random_row(len, 0x1234_5678_9ABC_DEF0 ^ len as u64);
                    let filtered = apply_filter(filter, &raw, &above, bpp);
                    let mut scalar = filtered.clone();
                    match filter {
                        1 => scal_sub(&mut scalar, bpp),
                        3 => scal_avg(&mut scalar, &above, bpp),
                        _ => scal_paeth(&mut scalar, &above, bpp),
                    }
                    let mut simd = filtered.clone();
                    unsafe {
                        match filter {
                            1 => super::sse::unfilter_sub_sse2(&mut simd, bpp),
                            3 => super::sse::unfilter_average_sse2(&mut simd, &above, bpp),
                            _ => super::sse::unfilter_paeth_sse2_16lane(&mut simd, &above, bpp),
                        }
                    }
                    assert_eq!(scalar, raw, "标量 (filter={filter} bpp={bpp} len={len})");
                    assert_eq!(simd, raw, "SIMD (filter={filter} bpp={bpp} len={len})");
                }
            }
        }
    }

    /// bpp==1 Sub 的块前缀和向量（SSE2 16B / AVX2 32B）与标量链逐字节一致。
    /// 行长覆盖：<16（纯标量尾）、恰 16/32 边界、跨块 carry 传播的超长行。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn bpp1_sub_prefix_simd_matches_scalar() {
        let lens = [
            1, 2, 3, 4, 15, 16, 17, 31, 32, 33, 47, 48, 62, 63, 64, 65, 95, 96, 97, 127, 128, 129,
            255, 256, 257, 511, 1000, 1024, 4097,
        ];
        for len in lens {
            for seed in [0x0123_4567_89ABu64, 0xFEDC_BA98_7654u64] {
                let raw = random_row(len, seed ^ len as u64);
                // 正向 Sub 滤波（逆序遍历，raw[i-1] 未被改写）。
                let mut filtered = raw.to_vec();
                for i in (1..len).rev() {
                    filtered[i] = filtered[i].wrapping_sub(filtered[i - 1]);
                }
                let mut scalar = filtered.clone();
                scal_sub(&mut scalar, 1);
                assert_eq!(scalar, raw, "标量 len={len}");
                if kimg_core::simd::detect("sse2") {
                    let mut simd = filtered.clone();
                    unsafe { super::sse::unfilter_sub_bpp1_sse2(&mut simd) };
                    assert_eq!(simd, raw, "SSE2 bpp1 Sub len={len}");
                }
                if kimg_core::simd::detect("avx2") {
                    let mut simd = filtered.clone();
                    unsafe { super::sse::unfilter_sub_bpp1_avx2(&mut simd) };
                    assert_eq!(simd, raw, "AVX2 bpp1 Sub len={len}");
                }
            }
        }
    }

    /// A2 src≠dst 全矩阵回归：`unfilter_to(filter, src, dst, above, bpp)` 与标量参考逐字节一致，
    /// 且与就地 `unfilter_row` 等价。覆盖 8/16 位 bpp（1..=8，含 bpp=3/6/8 的 16bit 语义）、
    /// 5 种 filter、边界行长（行首=1..bpp、行尾非 bpp 倍数、跨 SIMD 块），以及「dst 预先污染」
    /// （确保整行被重新写出、无陈旧字节泄漏）。
    #[test]
    fn unfilter_to_matches_scalar_reference() {
        for bpp in 1..=8usize {
            for filter in 0..=4u8 {
                for len in [
                    1usize, 2, 3, 4, 5, 7, 8, 9, 13, 15, 16, 17, 31, 32, 33, 47, 48, 64, 65, 96,
                    128, 129, 255, 256, 257, 1024,
                ] {
                    let raw = random_row(len, 0x51C0FFEE ^ (bpp as u64 * 131 + filter as u64));
                    let above = random_row(len, 0x123456789ABCDEF0 ^ len as u64);
                    let filtered = apply_filter(filter, &raw, &above, bpp);

                    // 标量参考（读 src、写 dst）。
                    let mut expected = vec![0xA5u8; len];
                    match filter {
                        1 => {
                            expected[..bpp.min(len)].copy_from_slice(&filtered[..bpp.min(len)]);
                            for i in bpp..len {
                                expected[i] = filtered[i].wrapping_add(expected[i - bpp]);
                            }
                        }
                        2 => {
                            for i in 0..len {
                                expected[i] = filtered[i].wrapping_add(above[i]);
                            }
                        }
                        3 => {
                            for i in 0..len {
                                let l = if i >= bpp { expected[i - bpp] } else { 0 };
                                expected[i] = filtered[i].wrapping_add(
                                    ((u16::from(l) + u16::from(above[i])) >> 1) as u8,
                                );
                            }
                        }
                        4 => {
                            for i in 0..len {
                                let l = if i >= bpp {
                                    i32::from(expected[i - bpp])
                                } else {
                                    0
                                };
                                let b = i32::from(above[i]);
                                let c = if i >= bpp {
                                    i32::from(above[i - bpp])
                                } else {
                                    0
                                };
                                expected[i] = filtered[i].wrapping_add(super::paeth(l, b, c) as u8);
                            }
                        }
                        _ => expected.copy_from_slice(&filtered),
                    }

                    // 走完整派发（标量回退 / SIMD 见机行事），用污染值初始化 dst 验证整行重写。
                    let mut dispatched = vec![0x5A; len];
                    unfilter_to(filter, &filtered, &mut dispatched, &above, bpp);
                    assert_eq!(
                        dispatched, expected,
                        "unfilter_to 不符 (filter={filter} bpp={bpp} len={len})"
                    );

                    // 与就地 unfilter_row 等价（同一输入行、同一 above）。
                    let mut inplace = filtered.clone();
                    unfilter_row(filter, &mut inplace, &above, bpp);
                    assert_eq!(
                        dispatched, inplace,
                        "unfilter_to 与就地 unfilter_row 不等价 (filter={filter} bpp={bpp} len={len})"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Paeth 8 位 lane FPNGE 专项 parity（R4 门禁）
    // -----------------------------------------------------------------------

    /// 对给定「未滤波行」`raw_dst` 与 `above`：正向 Paeth 滤波得到解码输入，再用标量与各
    /// 8 位 lane 核（SSE4.1 / AVX2，及 to 变体）反滤波，断言各方与 `raw_dst` 逐字节一致。
    #[cfg(target_arch = "x86_64")]
    fn assert_paeth_kernels_match(
        _rng: &mut impl FnMut() -> u64,
        raw_dst: &[u8],
        above: &[u8],
        bpp: usize,
        tag: &str,
    ) {
        let len = raw_dst.len();
        let filtered = if len == 0 {
            Vec::new()
        } else {
            let mut out = raw_dst.to_vec();
            for i in 0..len {
                let a = if i >= bpp { raw_dst[i - bpp] } else { 0 };
                let b = above[i];
                let c = if i >= bpp { above[i - bpp] } else { 0 };
                let pred = super::paeth(i32::from(a), i32::from(b), i32::from(c)) as u8;
                out[i] = raw_dst[i].wrapping_sub(pred);
            }
            out
        };

        let mut scalar = filtered.clone();
        scal_paeth(&mut scalar, above, bpp);
        assert_eq!(scalar, raw_dst, "标量不符 {tag} bpp={bpp} len={len}");

        if bpp >= 2 {
            if kimg_core::simd::detect("sse4.1") {
                let mut simd = filtered.clone();
                unsafe { super::sse::unfilter_paeth_sse41(&mut simd, above, bpp) };
                assert_eq!(
                    simd, raw_dst,
                    "SSE4.1 8位lane不符 {tag} bpp={bpp} len={len}"
                );
                // to 变体（读 src/写 dst）与标量一致。
                let mut to = vec![0x5Au8; len];
                unsafe { super::sse::unfilter_paeth_to_sse41(&filtered, &mut to, above, bpp) };
                assert_eq!(to, raw_dst, "SSE4.1 to 不符 {tag} bpp={bpp} len={len}");
            }
            if kimg_core::simd::detect("avx2") {
                let mut simd = filtered.clone();
                unsafe { super::sse::unfilter_paeth_avx2(&mut simd, above, bpp) };
                assert_eq!(simd, raw_dst, "AVX2 8位lane不符 {tag} bpp={bpp} len={len}");
                let mut to = vec![0x5Au8; len];
                unsafe { super::sse::unfilter_paeth_to_avx2(&filtered, &mut to, above, bpp) };
                assert_eq!(to, raw_dst, "AVX2 to 不符 {tag} bpp={bpp} len={len}");
            }
        }
    }

    /// Paeth 8 位 lane 核 ↔ 标量逐字节 parity：大样本随机行（bpp∈{1,2,3,4,6,8}，行长随机
    /// 含非 bpp 倍数/行首/行尾）＋ 刻意构造的边界（全 0/全 255、0·255 交替、a=b=c、a=c、b=c）。
    /// 总数 ≥ 10^5 行。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn paeth_fpn8_lane_random_parity() {
        let mut s: u64 = 0x2545_F491_4F6C_DD1Du64;
        let mut rng = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s >> 32
        };

        let mut rows: u64 = 0;
        const ROWS_PER_BPP: u64 = 30_000; // 6×3 万 = 18 万 ≥ 10^5
        for bpp in [1usize, 2, 3, 4, 6, 8] {
            for k in 0..ROWS_PER_BPP {
                rows += 1;
                // 行长：正常随机 1..=72，另混入批次固定值（bpp 倍数/非倍数/短行首尾/跨 SIMD 块）。
                let len = match k % 16 {
                    0 => bpp,
                    1 => bpp.saturating_sub(1).max(1),
                    2 => bpp + 1,
                    3 => 15,
                    4 => 16,
                    5 => 17,
                    6 => 31,
                    7 => 32,
                    8 => 33,
                    9 => 4 * bpp + 3,
                    10 => 8 * bpp + 1,
                    _ => 1 + (rng() % 72) as usize,
                };
                let raw = random_row(len, rng() ^ (bpp as u64 * 131 + k));
                let above = random_row(len, rng() ^ len as u64);
                assert_paeth_kernels_match(&mut rng, &raw, &above, bpp, "random");
            }
        }

        // 刻意边界行：全 0 / 全 255（u8 wrap）/ 0·255 交替（a=b=c、b=c、a=c 与极值交织）。
        for bpp in [1usize, 2, 3, 4, 6, 8] {
            for len in [1usize, bpp, bpp + 1, 15, 16, 17, 31, 32, 33, 63, 64] {
                let zeros = vec![0u8; len];
                let ov = vec![0u8; len];
                assert_paeth_kernels_match(&mut rng, &zeros, &ov, bpp, "all-zero(a=b=c=0)");

                let ones = vec![255u8; len];
                let ov255 = vec![255u8; len];
                assert_paeth_kernels_match(&mut rng, &ones, &ov255, bpp, "all-255(wrap)");

                // a=c（dst==above）、b=c（constant above）构造行。
                let mut alt_dst = vec![0u8; len];
                let mut alt_abv = vec![0u8; len];
                for (i, v) in alt_dst.iter_mut().enumerate() {
                    let val = if i % 3 == 0 {
                        0
                    } else if i % 3 == 1 {
                        255
                    } else {
                        rng() as u8
                    };
                    *v = val;
                    alt_abv[i] = val;
                }
                assert_paeth_kernels_match(&mut rng, &alt_dst, &alt_abv, bpp, "a=c(dst==above)");

                let const_above = vec![rng() as u8; len];
                let mut arg_dst = vec![0u8; len];
                for v in arg_dst.iter_mut() {
                    *v = rng() as u8;
                }
                assert_paeth_kernels_match(
                    &mut rng,
                    &arg_dst,
                    &const_above,
                    bpp,
                    "b=c(const above)",
                );

                // 仅 {0,255} 取值的高频 wrap 行。
                let bw: Vec<u8> = (0..len).map(|k| if k % 2 == 0 { 0 } else { 255 }).collect();
                let bw_a: Vec<u8> = (0..len).map(|k| if k % 5 < 2 { 0 } else { 255 }).collect();
                assert_paeth_kernels_match(&mut rng, &bw, &bw_a, bpp, "binary-wrap");
            }
        }

        assert!(rows >= 100_000, "请求至少 10^5 行，实际 {rows}");
    }

    /// 预测器核级 parity：`paeth_pred8_128` / `paeth_pred8_256`（8 位 lane FPNGE 公式）对随机
    /// (a,b,c)∈[0,255]^3 的每道结果 == 标量 `paeth` 逐字节一致（含恰在 a=b=c / a=c / b=c /
    /// 0·255 边界上的三元组，由随机采样覆盖）。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn paeth_fpn8_predictor_core_parity() {
        let mut s: u64 = 0xA076_1D64_78BD_642Fu64;
        let mut rng = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 32) as u8
        };
        // 同时掺入 {0,1,2,254,255} 附近的小集合，强化相等/边界命中。
        let pick = |rng: &mut dyn FnMut() -> u8| match rng() % 10 {
            0..=2 => rng() % 4,       // 0..=3
            3..=5 => 252 + rng() % 4, // 252..=255
            _ => rng(),
        };

        let mut iter = 0u64;
        let n_calls_128 = 60_000u64; // 60k×16 道 = 96 万三元组
        if kimg_core::simd::detect("sse4.1") {
            for _ in 0..n_calls_128 {
                let mut av = [0u8; 16];
                let mut bv = [0u8; 16];
                let mut cv = [0u8; 16];
                for i in 0..16 {
                    av[i] = pick(&mut rng);
                    bv[i] = pick(&mut rng);
                    cv[i] = pick(&mut rng);
                    iter += 1;
                }
                let aa = unsafe {
                    super::sse::paeth_pred8_128(raw_into128(av), raw_into128(bv), raw_into128(cv))
                };
                let preds: [u8; 16] = unsafe { store128_into(aa) };
                for i in 0..16 {
                    let expect =
                        super::paeth(i32::from(av[i]), i32::from(bv[i]), i32::from(cv[i])) as u8;
                    assert_eq!(
                        preds[i], expect,
                        "128 核 (a={} b={} c={}) 期望 {expect}",
                        av[i], bv[i], cv[i]
                    );
                }
            }
        }
        if kimg_core::simd::detect("avx2") {
            for _ in 0..n_calls_128 {
                let mut av = [0u8; 32];
                let mut bv = [0u8; 32];
                let mut cv = [0u8; 32];
                for i in 0..32 {
                    av[i] = pick(&mut rng);
                    bv[i] = pick(&mut rng);
                    cv[i] = pick(&mut rng);
                    iter += 1;
                }
                let aa = unsafe {
                    super::sse::paeth_pred8_256(raw_into256(av), raw_into256(bv), raw_into256(cv))
                };
                let preds: [u8; 32] = unsafe { store256_into(aa) };
                for i in 0..32 {
                    let expect =
                        super::paeth(i32::from(av[i]), i32::from(bv[i]), i32::from(cv[i])) as u8;
                    assert_eq!(
                        preds[i], expect,
                        "256 核 (a={} b={} c={}) 期望 {expect}",
                        av[i], bv[i], cv[i]
                    );
                }
            }
        }
        assert!(iter >= 1_000_000, "至少百万级三元组，实际 {iter}");
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn raw_into128(x: [u8; 16]) -> std::arch::x86_64::__m128i {
        unsafe {
            std::arch::x86_64::_mm_loadu_si128(x.as_ptr() as *const std::arch::x86_64::__m128i)
        }
    }
    #[cfg(target_arch = "x86_64")]
    unsafe fn store128_into(v: std::arch::x86_64::__m128i) -> [u8; 16] {
        let mut out = [0u8; 16];
        unsafe {
            std::arch::x86_64::_mm_storeu_si128(
                out.as_mut_ptr() as *mut std::arch::x86_64::__m128i,
                v,
            )
        };
        out
    }
    #[cfg(target_arch = "x86_64")]
    unsafe fn raw_into256(x: [u8; 32]) -> std::arch::x86_64::__m256i {
        unsafe {
            std::arch::x86_64::_mm256_loadu_si256(x.as_ptr() as *const std::arch::x86_64::__m256i)
        }
    }
    #[cfg(target_arch = "x86_64")]
    unsafe fn store256_into(v: std::arch::x86_64::__m256i) -> [u8; 32] {
        let mut out = [0u8; 32];
        unsafe {
            std::arch::x86_64::_mm256_storeu_si256(
                out.as_mut_ptr() as *mut std::arch::x86_64::__m256i,
                v,
            )
        };
        out
    }

    /// 内核级 A/B 微基准（`#[ignore]` 手动跑，不进常规 CI）：同一批「纯 Paeth 滤波行」上对比
    /// 标量 / 旧 16 位 lane / 新 8 位 lane SSE4.1 / 新 8 位 lane AVX2 的每字节耗时（ns/B）。
    /// 由「左邻链串行」决定每个实现都是延迟受限循环，指令数少不一定更快——本探针直接量化。
    /// 运行：`cargo test -p kimg --release paeth_kernel_micro -- --ignored --nocapture`
    #[cfg(target_arch = "x86_64")]
    #[test]
    #[ignore = "手动内核 A/B 微基准"]
    #[allow(clippy::type_complexity)] // 诊断基准的 &dyn Fn 形参
    fn paeth_kernel_micro_bench() {
        use std::time::Instant;

        let mut s: u64 = 0xF1EA_5EED_C0FF_EE01u64;
        let mut rng = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) as u8
        };

        const ROWS: usize = 3000;
        const ROWLEN: usize = 1027; // 非 16/32 倍数，制造尾部标量
        for bpp in [3usize, 4, 6, 8] {
            let mut filtered: Vec<Vec<u8>> = Vec::with_capacity(ROWS);
            let mut abv: Vec<Vec<u8>> = Vec::with_capacity(ROWS);
            for _ in 0..ROWS {
                let raw: Vec<u8> = (0..ROWLEN).map(|_| rng()).collect();
                let a_row: Vec<u8> = (0..ROWLEN).map(|_| rng()).collect();
                let mut filt = raw.clone();
                for i in 0..ROWLEN {
                    let a = if i >= bpp { raw[i - bpp] as i32 } else { 0 };
                    let b = a_row[i] as i32;
                    let c = if i >= bpp { a_row[i - bpp] as i32 } else { 0 };
                    filt[i] = raw[i].wrapping_sub(super::paeth(a, b, c) as u8);
                }
                filtered.push(filt);
                abv.push(a_row);
            }
            let passes = 50usize;
            // 每遍从 pristine filtered 克隆再就地反滤波（克隆开销各实现相同，A/B 公平）。
            let time = |f: &dyn Fn(&mut [u8], &[u8], usize)| {
                let t = Instant::now();
                for _ in 0..passes {
                    for (row, above) in filtered.iter().zip(abv.iter()) {
                        let mut work = row.clone();
                        f(&mut work, above, bpp);
                        std::hint::black_box(&work);
                    }
                }
                t.elapsed().as_nanos() as f64 / (passes as f64 * ROWS as f64 * ROWLEN as f64)
            };
            let scal = |row: &mut [u8], above: &[u8], bpp: usize| {
                for i in 0..row.len() {
                    let left = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    let b = above[i] as i32;
                    let c = if i >= bpp { above[i - bpp] as i32 } else { 0 };
                    row[i] = row[i].wrapping_add(super::paeth(left, b, c) as u8);
                }
            };
            let t_scalar = time(&scal);
            let mut line = format!("PRED_KERNEL_BENCH bpp={bpp} scalar={t_scalar:.2}ns/B");
            if kimg_core::simd::detect("sse2") {
                let t16 =
                    time(&|r, a, bpp| unsafe { super::sse::unfilter_paeth_sse2_16lane(r, a, bpp) });
                line.push_str(&format!(
                    " sse2_16lane={t16:.2}ns/B (x{:.2})",
                    t16 / t_scalar
                ));
            }
            if kimg_core::simd::detect("sse4.1") {
                let t41 = time(&|r, a, bpp| unsafe { super::sse::unfilter_paeth_sse41(r, a, bpp) });
                line.push_str(&format!(
                    " sse41_8lane={t41:.2}ns/B (x{:.2})",
                    t41 / t_scalar
                ));
            }
            if kimg_core::simd::detect("avx2") {
                let tavx = time(&|r, a, bpp| unsafe { super::sse::unfilter_paeth_avx2(r, a, bpp) });
                line.push_str(&format!(
                    " avx2_8lane={tavx:.2}ns/B (x{:.2})",
                    tavx / t_scalar
                ));
            }
            println!("{line}");
        }
    }
}
