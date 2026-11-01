//! WebP 解码 SIMD 加速（对标 libwebp dsp SIMD；**字节精确**：每条向量路径都配标量回退，
//! 且向量结果与标量逐字节一致的断言见模块底部单测）。
//!
//! 开关：`KIMG_WEBP_NO_SIMD=1` 强制 WebP 全部 SIMD 走标量（量化收益对照，与 pngsimd 的
//! `KIMG_PNG_NO_SIMD` 同款姿势）；x86_64 下按运行时特性检测（SSSE3 / SSE4.1 / AVX2）选核，
//! 非 x86_64 一律标量回退。注释中文。
//!
//! 本模块实现 C 项：
//! - `yuv_row_to_rgba`：AVX2 每 16 像素一组（8×i32 lane ×2 段 interleave；同源算术
//!   `mullo_epi32`+`srai_epi32` 保持 `(x*c)>>8`、bias 减后再 `>>6` 严格同序，无重结合），
//!   否则 SSE4.1 每 8 像素一组；饱和 `packus` 即 `clamp(0,255)`。u16-mulhi 变体
//!   （vpmulhuw 系数+max-then-subtract，对标 C# `ConvertRowAvx2`）为 AVX2 默认生产路径，
//!   `KIMG_WEBP_YUV_U16=0` 回退 i32 定点方案（两方案逐字节一致）。
//! - `upsample_fancy_row`：SSE2 16-bit lane 算 fancy 上采样中间列对（`3*t[i]±邻居+8)>>4`），
//!   首/末列因边界算术特殊保留标量，尾列标量收尾。
//! - D1 `predict_subblock`/`predict_large`（SSSE3）：4×4 十模式 + 16×16/8×8 整块，经
//!   `KIMG_WEBP_NO_SIMD`/`KIMG_WEBP_NO_SIMD_PRED` 门控接入（实测内核 1.6×~30× 提速，启用）。
//! - D2 环路滤波：`filter_edge`（SSE2 per-edge）与 `lf_filter_macroblock`（SSE2 宏块熔合，
//!   U/V 打包、宏块一次全 12 核，C# `Vp8Decoder.FilterMacroblock` 同款；字节精确 vs 标量见
//!   模块底部断言）——**熔合版默认接入生产**（vp8.rs `filter_macroblock_row` 经
//!   `part_simd_on("flt","sse2")` 路由；`KIMG_WEBP_NO_SIMD_FLT=1` / `KIMG_WEBP_SIMD_FLT=0` 回退）。

use std::arch::is_x86_feature_detected;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// `KIMG_WEBP_NO_SIMD=1` 强制关 WebP 全部 SIMD（对标量回退性能对照；量化 SIMD 收益）。
/// 首次访问缓存环境变量，之后仅一次 Relaxed 原子读——正常热循环开销可忽略。
pub(crate) fn webp_no_simd() -> bool {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static OFF: AtomicBool = AtomicBool::new(false);
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let off = std::env::var("KIMG_WEBP_NO_SIMD")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        OFF.store(off, Relaxed);
    });
    OFF.load(Relaxed)
}

/// `KIMG_WEBP_YUV_U16`（默认开，u16-mulhi 变体为生产路径）：`0`/`false`/`off` → 关，
/// 走 i32 定点 AVX2 方案（A/B 对照保留；两个方案逐字节一致，分别对标 C# `ConvertRowAvx2`
/// 与既有 i32 标量源）。首次访问缓存。
#[inline]
fn yuv_u16_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let t = std::env::var("KIMG_WEBP_YUV_U16").unwrap_or_default().trim().to_string();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// 运行时 CPU 特性查询（x86_64；非 x86_64 一律 false 走标量回退）。已并入 `webp_no_simd` 闸。
#[inline]
pub(crate) fn simd_on(feature: &str) -> bool {
    if webp_no_simd() {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    {
        match feature {
            "sse2" => is_x86_feature_detected!("sse2"),
            "ssse3" => is_x86_feature_detected!("ssse3"),
            "sse4.1" => is_x86_feature_detected!("sse4.1"),
            "avx2" => is_x86_feature_detected!("avx2"),
            _ => false,
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = feature;
        false
    }
}

// ---------------------------------------------------------------------------
// 部件级开关（D1 帧内预测 / D2 环路滤波）：独立 env `KIMG_WEBP_NO_SIMD_PRED`/`KIMG_WEBP_NO_SIMD_FLT`
// 各部件单独关闭（量化 D1/D2 各自增益）；全局 `KIMG_WEBP_NO_SIMD` 一票否决两者。
// `#[cfg(test)]` 探针另有 `set_simd_part_override` 动态覆盖（优先于 env，便于进程内 A/B）。
// ---------------------------------------------------------------------------

/// 读布尔 env（'1'/'true' → true），首次访问后缓存。
fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn part_env_off(part: &str) -> bool {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static PRED: AtomicBool = AtomicBool::new(false);
    static FLT: AtomicBool = AtomicBool::new(false);
    static DCT: AtomicBool = AtomicBool::new(false);
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        PRED.store(env_flag("KIMG_WEBP_NO_SIMD_PRED"), Relaxed);
        FLT.store(env_flag("KIMG_WEBP_NO_SIMD_FLT"), Relaxed);
        DCT.store(env_flag("KIMG_WEBP_NO_SIMD_DCT"), Relaxed);
    });
    match part {
        "pred" => PRED.load(Relaxed),
        "flt" => FLT.load(Relaxed),
        "dct" => DCT.load(Relaxed),
        _ => false,
    }
}

#[cfg(test)]
mod part_override {
    use std::sync::atomic::{AtomicI8, Ordering::Relaxed};
    static PRED: AtomicI8 = AtomicI8::new(-1);
    static FLT: AtomicI8 = AtomicI8::new(-1);
    static DCT: AtomicI8 = AtomicI8::new(-1);
    pub fn set(part: &str, on: Option<bool>) {
        let v = match on {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        };
        match part {
            "pred" => PRED.store(v, Relaxed),
            "flt" => FLT.store(v, Relaxed),
            "dct" => DCT.store(v, Relaxed),
            _ => {}
        }
    }
    pub fn get(part: &str) -> Option<bool> {
        let v = match part {
            "pred" => PRED.load(Relaxed),
            "flt" => FLT.load(Relaxed),
            "dct" => DCT.load(Relaxed),
            _ => return None,
        };
        match v {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

/// 探针/一致性测试用动态覆盖（`cfg(test)` 下优先于 env 部件开关；全局 `KIMG_WEBP_NO_SIMD`
/// 仍作最终否决——用 `KIMG_WEBP_NO_SIMD=1` 跑即整体关闭，探针进程内 A/B 不设该 env）。
#[cfg(test)]
pub(crate) fn set_simd_part_override(part: &str, on: Option<bool>) {
    part_override::set(part, on);
}

/// 部件级 SIMD 判定（`part`="pred" | "flt" | "dct"）。
#[inline]
pub(crate) fn part_simd_on(part: &str, feature: &str) -> bool {
    #[cfg(test)]
    if let Some(on) = part_override::get(part) {
        return on && simd_on(feature);
    }
    if webp_no_simd() || part_env_off(part) {
        return false;
    }
    // D2 环路滤波：宏块熔合 SIMD（`lf_filter_macroblock`，字节不变）默认接入生产；
    // `KIMG_WEBP_SIMD_FLT=0` / `KIMG_WEBP_NO_SIMD_FLT=1` / `KIMG_WEBP_NO_SIMD=1` 一票否决回退标量。
    if part == "flt" && !flt_enabled() {
        return false;
    }
    simd_on(feature)
}

/// `KIMG_WEBP_SIMD_FLT`（默认开）：`0`/`false`/`off` → 关（A/B 对照保留）；其余/缺省 → 开。
fn flt_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_WEBP_SIMD_FLT").unwrap_or_default();
        let t = v.trim();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

// ---------------------------------------------------------------------------
// D1：帧内预测向量化（SSSE3；C# `Vp8Decoder.PredictSubblockVector`/`PredictLarge*` 同款：
// 4×4 十模式经一个 16 字节 edge 向量 E=[L3,L3,L2,L1,L0,P,A0..A7,A7,A7] + avg2/avg3 向量 +
// 逐模式 pshufb 重排；16×16/8×8 整块按 V/H/TM/DC 分派）。向量与 vp8.rs 标量逐字节一致断言见模块底部。
// ---------------------------------------------------------------------------

/// 每 4×4 模式（2..=9）的 32 字节 shuffle 表：前 16 个索引取 `avg3` 向量、后 16 个取 `avg2`
/// （255 = 该位不取，pshufb 对高位索引输出 0，OR 合并两种取源）。与 C# `SubblockShuffles` 逐字节同表。
const SUBBLOCK_SHUFFLES: [u8; 8 * 32] = [
    // 2 VE
    5, 6, 7, 8, 5, 6, 7, 8, 5, 6, 7, 8, 5, 6, 7, 8, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    // 3 HE
    3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    // 4 LD
    6, 7, 8, 9, 7, 8, 9, 10, 8, 9, 10, 11, 9, 10, 11, 12, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    // 5 RD
    4, 5, 6, 7, 3, 4, 5, 6, 2, 3, 4, 5, 1, 2, 3, 4, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    // 6 VR
    255, 255, 255, 255, 4, 5, 6, 7, 3, 255, 255, 255, 2, 4, 5, 6, 5, 6, 7, 8, 255, 255, 255, 255, 255, 5, 6, 7, 255, 255, 255, 255,
    // 7 VL
    255, 255, 255, 255, 6, 7, 8, 9, 255, 255, 255, 10, 7, 8, 9, 11, 6, 7, 8, 9, 255, 255, 255, 255, 7, 8, 9, 255, 255, 255, 255, 255,
    // 8 HD
    255, 4, 5, 6, 255, 3, 255, 4, 255, 2, 255, 3, 255, 1, 255, 2, 4, 255, 255, 255, 3, 255, 4, 255, 2, 255, 3, 255, 1, 255, 2, 255,
    // 9 HU
    255, 2, 255, 1, 255, 1, 255, 0, 255, 0, 255, 255, 255, 255, 255, 255, 3, 255, 2, 255, 2, 255, 1, 255, 1, 255, 0, 0, 0, 0, 0, 0,
];

/// 4×4 预测分派：SSSE3 向量核；无 SIMD → vp8.rs 标量（单一语义源）。
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

#[cfg(target_arch = "x86_64")]
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

// ---------------------------------------------------------------------------
// D2：环路滤波向量化（SSE2；C# `Vp8LoopFilter.cs` 同款：无符号域算掩码、`x^0x80` 偏差符号域
// 做饱和加减；`common_adjust` 的 tap 先钳后移；垂直边经 16×8 / 8×8 转置梯）。逐字节与 vp8.rs 标量
// 一致断言见模块底部。
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    Simple,
    Inner,
    Macroblock,
}

/// 单边滤波分派：`length` 16（luma）/ 8（chroma）且 flt-SIMD 开 → SSE2 向量核；否则 vp8.rs 标量。
/// **默认未接入生产**（D2 实测持平/微负，vp8.rs 走标量；本入口仅测试/探针/复测用），
/// 非 test 构建允许 dead_code。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn filter_edge(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    #[cfg(target_arch = "x86_64")]
    if part_simd_on("flt", "sse2") {
        // D2v2：libwebp 风格 16/8-lane（本机字节一致的更快实现）。
        unsafe {
            lf_filter_edge_sse2(
                plane, stride, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev,
            )
        }
        return;
    }
    super::vp8::filter_edge(plane, stride, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev);
}

/// 逐字节逻辑右移 k（k<8，u8 域）：psrlw 的 16-bit 移位会用高字节低位污染低字节，故
/// 低字节经掩码独立移、高字节取 psrlw 的高半，再拼回。`k` 须为编译期常量（psrlw 立即数）。
macro_rules! shr8u {
    ($v:expr, $k:expr) => {{
        let vec = $v;
        let hi = _mm_srli_epi16(vec, $k);
        let lo = _mm_srli_epi16(
            _mm_and_si128(vec, _mm_set1_epi16(((0xFFu16 << $k) & 0xFF) as i16)),
            $k,
        );
        _mm_or_si128(_mm_and_si128(hi, _mm_set1_epi16(0xFF00u16 as i16)), lo)
    }};
}

/// `|a-b|`（u8 饱和差 + OR）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn abs_diff_u8(a: __m128i, b: __m128i) -> __m128i {
    _mm_or_si128(_mm_subs_epu8(a, b), _mm_subs_epu8(b, a))
}

/// 有符号字节 `v` 算术右移 3：`(v^0x80)>>3 - 16`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn shift_right3(v: __m128i, e80: __m128i) -> __m128i {
    _mm_sub_epi8(shr8u!(_mm_xor_si128(v, e80), 3), _mm_set1_epi8(16))
}

/// 有符号字节算术右移 1：`(v^0x80)>>1 - 64`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn shift_right1(v: __m128i, e80: __m128i) -> __m128i {
    _mm_sub_epi8(shr8u!(_mm_xor_si128(v, e80), 1), _mm_set1_epi8(64))
}

/// 边缘掩码：`2|p0-q0|+|p1-q1|/2 <= edge`（饱和和即便越 255 也恒大于 edge 阈值，见 C# 注释）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn edge_mask(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, edge: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let d0 = unsafe { abs_diff_u8(p0, q0) };
    let d1 = shr8u!(unsafe { abs_diff_u8(p1, q1) }, 1);
    let sum = _mm_adds_epu8(_mm_adds_epu8(d0, d0), d1);
    _mm_cmpeq_epi8(_mm_subs_epu8(sum, _mm_set1_epi8(edge as i8)), zero)
}

/// common_adjust：`a=clamp(outer+3*delta)`（饱和三次），`qa=(a+4)>>3`、`pa=(a+3)>>3`
/// （先钳后移），只更新 `amask` 覆盖的 lane，返回 (p0, q0, qAdjust)。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn common_adjust(
    outer: __m128i,
    delta: __m128i,
    sp0: __m128i,
    sq0: __m128i,
    amask: __m128i,
    e80: __m128i,
) -> (__m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    unsafe {
    let a = _mm_and_si128(_mm_adds_epi8(_mm_adds_epi8(_mm_adds_epi8(outer, delta), delta), delta), amask);
    let q_adj = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(4)), e80);
    let p_adj = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(3)), e80);
    let q0 = _mm_subs_epi8(sq0, q_adj);
    let p0 = _mm_adds_epi8(sp0, p_adj);
    (p0, q0, q_adj)
    }
}

/// 宏块权重 `(w*27/18/9 + 63) >> 7`（w 有符号字节 → i16 符号扩展计算 → 饱和折回）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn tap_weights(w: __m128i) -> (__m128i, __m128i, __m128i) {
    let wlo = _mm_srai_epi16(_mm_unpacklo_epi8(w, w), 8);
    let whi = _mm_srai_epi16(_mm_unpackhi_epi8(w, w), 8);
    let r63 = _mm_set1_epi16(63);
    let c27 = _mm_set1_epi16(27);
    let c18 = _mm_set1_epi16(18);
    let c9 = _mm_set1_epi16(9);
    let a27 = _mm_packs_epi16(
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(wlo, c27), r63), 7),
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(whi, c27), r63), 7),
    );
    let a18 = _mm_packs_epi16(
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(wlo, c18), r63), 7),
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(whi, c18), r63), 7),
    );
    let a9 = _mm_packs_epi16(
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(wlo, c9), r63), 7),
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(whi, c9), r63), 7),
    );
    (a27, a18, a9)
}

/// 16 lane 滤波核心：p3..q3（每 lane 一“样本列”），返回可变的 (p2,p1,p0,q0,q1,q2)，其余原样。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn filter_lanes(
    p3: __m128i,
    p2: __m128i,
    p1: __m128i,
    p0: __m128i,
    q0: __m128i,
    q1: __m128i,
    q2: __m128i,
    q3: __m128i,
    kind: FilterKind,
    interior: i32,
    edge: i32,
    hev: i32,
) -> (__m128i, __m128i, __m128i, __m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    unsafe {
    let e80 = _mm_set1_epi8(-128);
    let zero = _mm_setzero_si128();
    let mask_edge = edge_mask(p1, p0, q0, q1, edge);
    if kind == FilterKind::Simple {
        let sp1 = _mm_xor_si128(p1, e80);
        let sp0 = _mm_xor_si128(p0, e80);
        let sq0 = _mm_xor_si128(q0, e80);
        let sq1 = _mm_xor_si128(q1, e80);
        let (p0n, q0n, _) = common_adjust(
            _mm_subs_epi8(sp1, sq1),
            _mm_subs_epi8(sq0, sp0),
            sp0,
            sq0,
            mask_edge,
            e80,
        );
        return (p2, p1, _mm_xor_si128(p0n, e80), _mm_xor_si128(q0n, e80), q1, q2);
    }
    // Normal（Inner / Macroblock）：额外 interior 上限掩码 + 高方差判定。
    let dp1p0 = abs_diff_u8(p1, p0);
    let dq1q0 = abs_diff_u8(q1, q0);
    let imax = _mm_max_epu8(
        _mm_max_epu8(_mm_max_epu8(abs_diff_u8(p3, p2), abs_diff_u8(p2, p1)), dp1p0),
        _mm_max_epu8(_mm_max_epu8(abs_diff_u8(q3, q2), abs_diff_u8(q2, q1)), dq1q0),
    );
    let mask = _mm_and_si128(
        mask_edge,
        _mm_cmpeq_epi8(_mm_subs_epu8(imax, _mm_set1_epi8(interior as i8)), zero),
    );
    // hv = (|p1-p0|>hev 或 |q1-q0|>hev)：`NOT eq(sat(max-hev), 0)`。
    let hv = _mm_xor_si128(
        _mm_cmpeq_epi8(
            _mm_subs_epu8(_mm_max_epu8(dp1p0, dq1q0), _mm_set1_epi8(hev as i8)),
            zero,
        ),
        _mm_cmpeq_epi8(zero, zero),
    );
    let sp2 = _mm_xor_si128(p2, e80);
    let sp1 = _mm_xor_si128(p1, e80);
    let sp0 = _mm_xor_si128(p0, e80);
    let sq0 = _mm_xor_si128(q0, e80);
    let sq1 = _mm_xor_si128(q1, e80);
    let sq2 = _mm_xor_si128(q2, e80);
    let outer = _mm_subs_epi8(sp1, sq1);
    let delta = _mm_subs_epi8(sq0, sp0);
    if kind == FilterKind::Macroblock {
        // hv lane：common adjust（outer 全取）；!hv lane：27/18/9 加权铺到 p2..q2。
        let (ph, qh, _qa) = common_adjust(outer, delta, sp0, sq0, _mm_and_si128(mask, hv), e80);
        let wmask = _mm_andnot_si128(hv, mask);
        let w = _mm_and_si128(_mm_adds_epi8(_mm_adds_epi8(_mm_adds_epi8(outer, delta), delta), delta), wmask);
        let (a27, a18, a9) = tap_weights(w);
        let p0n = _mm_adds_epi8(ph, a27);
        let q0n = _mm_subs_epi8(qh, a27);
        let p1n = _mm_adds_epi8(sp1, a18);
        let q1n = _mm_subs_epi8(sq1, a18);
        let p2n = _mm_adds_epi8(sp2, a9);
        let q2n = _mm_subs_epi8(sq2, a9);
        return (
            _mm_xor_si128(p2n, e80),
            _mm_xor_si128(p1n, e80),
            _mm_xor_si128(p0n, e80),
            _mm_xor_si128(q0n, e80),
            _mm_xor_si128(q1n, e80),
            _mm_xor_si128(q2n, e80),
        );
    }
    // Inner：outer 仅 hv lane 生效；!hv lane 用 `(qAdj+1)>>1` 动 p1/q1。
    let outer_hv = _mm_and_si128(outer, hv);
    let (p0n, q0n, q_adj) = common_adjust(outer_hv, delta, sp0, sq0, mask, e80);
    let half = _mm_and_si128(
        shift_right1(_mm_add_epi8(q_adj, _mm_set1_epi8(1)), e80),
        _mm_andnot_si128(hv, mask),
    );
    let p1n = _mm_adds_epi8(sp1, half);
    let q1n = _mm_subs_epi8(sq1, half);
    (
        p2,
        _mm_xor_si128(p1n, e80),
        _mm_xor_si128(p0n, e80),
        _mm_xor_si128(q0n, e80),
        _mm_xor_si128(q1n, e80),
        q2,
    )
    }
}

/// 16×8 转置梯（C# `Load16x8` 同款）：`a` 行 y..y+7、`b` 行 y+8..y+15 各 8 列，输出
/// c0..c7 = 各列 16 行的值（低 8 = a 行、高 8 = b 行）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn load16x8(a: *const u8, a_stride: isize, b: *const u8, b_stride: isize) -> [__m128i; 8] {
    use std::arch::x86_64::*;
    unsafe {
    let mut r = [_mm_setzero_si128(); 8];
    for i in 0..8 {
        let is = i as isize;
        let alo = _mm_loadl_epi64(a.offset(is * a_stride) as *const __m128i);
        let bhi = _mm_loadl_epi64(b.offset(is * b_stride) as *const __m128i);
        r[i] = _mm_unpacklo_epi64(alo, bhi);
    }
    let t0 = _mm_unpacklo_epi8(r[0], r[1]);
    let t1 = _mm_unpackhi_epi8(r[0], r[1]);
    let t2 = _mm_unpacklo_epi8(r[2], r[3]);
    let t3 = _mm_unpackhi_epi8(r[2], r[3]);
    let t4 = _mm_unpacklo_epi8(r[4], r[5]);
    let t5 = _mm_unpackhi_epi8(r[4], r[5]);
    let t6 = _mm_unpacklo_epi8(r[6], r[7]);
    let t7 = _mm_unpackhi_epi8(r[6], r[7]);
    let u0 = _mm_unpacklo_epi16(t0, t2);
    let u1 = _mm_unpackhi_epi16(t0, t2);
    let u2 = _mm_unpacklo_epi16(t4, t6);
    let u3 = _mm_unpackhi_epi16(t4, t6);
    let u4 = _mm_unpacklo_epi16(t1, t3);
    let u5 = _mm_unpackhi_epi16(t1, t3);
    let u6 = _mm_unpacklo_epi16(t5, t7);
    let u7 = _mm_unpackhi_epi16(t5, t7);
    let v0 = _mm_unpacklo_epi32(u0, u2);
    let v1 = _mm_unpackhi_epi32(u0, u2);
    let v2 = _mm_unpacklo_epi32(u1, u3);
    let v3 = _mm_unpackhi_epi32(u1, u3);
    let v4 = _mm_unpacklo_epi32(u4, u6);
    let v5 = _mm_unpackhi_epi32(u4, u6);
    let v6 = _mm_unpacklo_epi32(u5, u7);
    let v7 = _mm_unpackhi_epi32(u5, u7);
    let c0 = _mm_unpacklo_epi64(v0, v4);
    let c1 = _mm_unpackhi_epi64(v0, v4);
    let c2 = _mm_unpacklo_epi64(v1, v5);
    let c3 = _mm_unpackhi_epi64(v1, v5);
    let c4 = _mm_unpacklo_epi64(v2, v6);
    let c5 = _mm_unpackhi_epi64(v2, v6);
    let c6 = _mm_unpacklo_epi64(v3, v7);
    let c7 = _mm_unpackhi_epi64(v3, v7);
    [c0, c1, c2, c3, c4, c5, c6, c7]
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn store_pairs(dst: *mut u8, stride: isize, v: [__m128i; 4]) {
    use std::arch::x86_64::*;
    unsafe {
    for i in 0..4 {
        _mm_storel_epi64(dst.offset((2 * i) as isize * stride) as *mut __m128i, v[i]);
        _mm_storel_epi64(
            dst.offset(((2 * i + 1) as isize) * stride) as *mut __m128i,
            _mm_srli_si128(v[i], 8),
        );
    }
    }
}

/// 16×8 转置逆（C# `Store16x8` 同款）：c0..c7 写回 a 行 y..y+7 / b 行 y+8..y+15。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn store16x8(
    a: *mut u8,
    a_stride: isize,
    b: *mut u8,
    b_stride: isize,
    c: [__m128i; 8],
) {
    use std::arch::x86_64::*;
    unsafe {
    let t0 = _mm_unpacklo_epi8(c[0], c[1]);
    let t1 = _mm_unpackhi_epi8(c[0], c[1]);
    let t2 = _mm_unpacklo_epi8(c[2], c[3]);
    let t3 = _mm_unpackhi_epi8(c[2], c[3]);
    let t4 = _mm_unpacklo_epi8(c[4], c[5]);
    let t5 = _mm_unpackhi_epi8(c[4], c[5]);
    let t6 = _mm_unpacklo_epi8(c[6], c[7]);
    let t7 = _mm_unpackhi_epi8(c[6], c[7]);
    let u0 = _mm_unpacklo_epi16(t0, t2);
    let u1 = _mm_unpackhi_epi16(t0, t2);
    let u2 = _mm_unpacklo_epi16(t4, t6);
    let u3 = _mm_unpackhi_epi16(t4, t6);
    let u4 = _mm_unpacklo_epi16(t1, t3);
    let u5 = _mm_unpackhi_epi16(t1, t3);
    let u6 = _mm_unpacklo_epi16(t5, t7);
    let u7 = _mm_unpackhi_epi16(t5, t7);
    let rows01 = _mm_unpacklo_epi32(u0, u2);
    let rows23 = _mm_unpackhi_epi32(u0, u2);
    let rows45 = _mm_unpacklo_epi32(u1, u3);
    let rows67 = _mm_unpackhi_epi32(u1, u3);
    let rows89 = _mm_unpacklo_epi32(u4, u6);
    let rows1011 = _mm_unpackhi_epi32(u4, u6);
    let rows1213 = _mm_unpacklo_epi32(u5, u7);
    let rows1415 = _mm_unpackhi_epi32(u5, u7);
    store_pairs(a, a_stride, [rows01, rows23, rows45, rows67]);
    store_pairs(b, b_stride, [rows89, rows1011, rows1213, rows1415]);
    }
}

/// 8×8 转置梯：rows[i] 低 8 字节 = 行 i 的 8 列；输出 cols[k] 低 8 字节 = 列 k 的 8 行。
/// 矩阵转置是自身的逆，故往返即还原（单测有 round-trip 断言）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn transpose8x8(rows: [__m128i; 8]) -> [__m128i; 8] {
    let zero = _mm_setzero_si128();
    let a0 = _mm_unpacklo_epi8(rows[0], rows[1]);
    let a1 = _mm_unpacklo_epi8(rows[2], rows[3]);
    let a2 = _mm_unpacklo_epi8(rows[4], rows[5]);
    let a3 = _mm_unpacklo_epi8(rows[6], rows[7]);
    let b0 = _mm_unpacklo_epi16(a0, a1);
    let b1 = _mm_unpackhi_epi16(a0, a1);
    let b2 = _mm_unpacklo_epi16(a2, a3);
    let b3 = _mm_unpackhi_epi16(a2, a3);
    let c01 = _mm_unpacklo_epi32(b0, b2);
    let c23 = _mm_unpackhi_epi32(b0, b2);
    let c45 = _mm_unpacklo_epi32(b1, b3);
    let c67 = _mm_unpackhi_epi32(b1, b3);
    [
        _mm_unpacklo_epi64(c01, zero),
        _mm_unpackhi_epi64(c01, zero),
        _mm_unpacklo_epi64(c23, zero),
        _mm_unpackhi_epi64(c23, zero),
        _mm_unpacklo_epi64(c45, zero),
        _mm_unpackhi_epi64(c45, zero),
        _mm_unpacklo_epi64(c67, zero),
        _mm_unpackhi_epi64(c67, zero),
    ]
}

/// SSE2 单边滤波：按 vertical + length 分四条（luma 16 × 横/纵、chroma 8 × 横/纵）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn filter_edge_sse2(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    use std::arch::x86_64::*;
    unsafe {
    let kind = if simple {
        FilterKind::Simple
    } else if macroblock_edge {
        FilterKind::Macroblock
    } else {
        FilterKind::Inner
    };
    let base = plane.as_mut_ptr();
    let center = y * stride + 1 + x;
    if !vertical {
        // 水平边：每 lane 一批的 p3..q3 是整行（16/8 连续字节）。
        if length == 16 {
            let q0p = base.add(center);
            let p3 = _mm_loadu_si128(q0p.sub(4 * stride) as *const __m128i);
            let p2 = _mm_loadu_si128(q0p.sub(3 * stride) as *const __m128i);
            let p1 = _mm_loadu_si128(q0p.sub(2 * stride) as *const __m128i);
            let p0 = _mm_loadu_si128(q0p.sub(stride) as *const __m128i);
            let q0 = _mm_loadu_si128(q0p as *const __m128i);
            let q1 = _mm_loadu_si128(q0p.add(stride) as *const __m128i);
            let q2 = _mm_loadu_si128(q0p.add(2 * stride) as *const __m128i);
            let q3 = _mm_loadu_si128(q0p.add(3 * stride) as *const __m128i);
            let (p2n, p1n, p0n, q0n, q1n, q2n) =
                filter_lanes(p3, p2, p1, p0, q0, q1, q2, q3, kind, interior, edge, hev);
            macroblock_store16(q0p, stride, kind, p2n, p1n, p0n, q0n, q1n, q2n);
        } else {
            let q0p = base.add(center);
            let p3 = _mm_loadl_epi64(q0p.sub(4 * stride) as *const __m128i);
            let p2 = _mm_loadl_epi64(q0p.sub(3 * stride) as *const __m128i);
            let p1 = _mm_loadl_epi64(q0p.sub(2 * stride) as *const __m128i);
            let p0 = _mm_loadl_epi64(q0p.sub(stride) as *const __m128i);
            let q0 = _mm_loadl_epi64(q0p as *const __m128i);
            let q1 = _mm_loadl_epi64(q0p.add(stride) as *const __m128i);
            let q2 = _mm_loadl_epi64(q0p.add(2 * stride) as *const __m128i);
            let q3 = _mm_loadl_epi64(q0p.add(3 * stride) as *const __m128i);
            let (p2n, p1n, p0n, q0n, q1n, q2n) =
                filter_lanes(p3, p2, p1, p0, q0, q1, q2, q3, kind, interior, edge, hev);
            macroblock_store8(q0p, stride, kind, p2n, p1n, p0n, q0n, q1n, q2n);
        }
        return;
    }
    // 垂直边：lane = 行方向 → 转置梯。
    if length == 16 {
        let a = base.add(center - 4);
        let b = a.add(8 * stride);
        let c = load16x8(a, stride as isize, b, stride as isize);
        let (p2n, p1n, p0n, q0n, q1n, q2n) =
            filter_lanes(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], kind, interior, edge, hev);
        let out = [c[0], p2n, p1n, p0n, q0n, q1n, q2n, c[7]];
        store16x8(a, stride as isize, b, stride as isize, out);
    } else {
        let mut rows = [_mm_setzero_si128(); 8];
        for i in 0..8 {
            rows[i] = _mm_loadl_epi64(base.add(center - 4 + i * stride) as *const __m128i);
        }
        let cols0 = transpose8x8(rows);
        let (p2n, p1n, p0n, q0n, q1n, q2n) =
            filter_lanes(
                cols0[0], cols0[1], cols0[2], cols0[3], cols0[4], cols0[5], cols0[6], cols0[7],
                kind, interior, edge, hev,
            );
        let cols1 = [cols0[0], p2n, p1n, p0n, q0n, q1n, q2n, cols0[7]];
        let out = transpose8x8(cols1);
        for i in 0..8 {
            _mm_storel_epi64(base.add(center - 4 + i * stride) as *mut __m128i, out[i]);
        }
    }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn macroblock_store16(
    q0p: *mut u8,
    stride: usize,
    kind: FilterKind,
    p2: __m128i,
    p1: __m128i,
    p0: __m128i,
    q0: __m128i,
    q1: __m128i,
    q2: __m128i,
) {
    use std::arch::x86_64::*;
    unsafe {
    match kind {
        FilterKind::Simple => {
            _mm_storeu_si128(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storeu_si128(q0p as *mut __m128i, q0);
        }
        FilterKind::Macroblock => {
            _mm_storeu_si128(q0p.sub(3 * stride) as *mut __m128i, p2);
            _mm_storeu_si128(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storeu_si128(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storeu_si128(q0p as *mut __m128i, q0);
            _mm_storeu_si128(q0p.add(stride) as *mut __m128i, q1);
            _mm_storeu_si128(q0p.add(2 * stride) as *mut __m128i, q2);
        }
        FilterKind::Inner => {
            _mm_storeu_si128(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storeu_si128(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storeu_si128(q0p as *mut __m128i, q0);
            _mm_storeu_si128(q0p.add(stride) as *mut __m128i, q1);
        }
    }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn macroblock_store8(
    q0p: *mut u8,
    stride: usize,
    kind: FilterKind,
    p2: __m128i,
    p1: __m128i,
    p0: __m128i,
    q0: __m128i,
    q1: __m128i,
    q2: __m128i,
) {
    use std::arch::x86_64::*;
    unsafe {
    match kind {
        FilterKind::Simple => {
            _mm_storel_epi64(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storel_epi64(q0p as *mut __m128i, q0);
        }
        FilterKind::Macroblock => {
            _mm_storel_epi64(q0p.sub(3 * stride) as *mut __m128i, p2);
            _mm_storel_epi64(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storel_epi64(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storel_epi64(q0p as *mut __m128i, q0);
            _mm_storel_epi64(q0p.add(stride) as *mut __m128i, q1);
            _mm_storel_epi64(q0p.add(2 * stride) as *mut __m128i, q2);
        }
        FilterKind::Inner => {
            _mm_storel_epi64(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storel_epi64(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storel_epi64(q0p as *mut __m128i, q0);
            _mm_storel_epi64(q0p.add(stride) as *mut __m128i, q1);
        }
    }
    }
}

// ---------------------------------------------------------------------------
// D2v2（第三弹）：环路滤波 libwebp 风格 SSE2（16/8-lane，弃旧 16×8 全转置慢实现）
// ---------------------------------------------------------------------------
// 端口自 libwebp dec_sse2.c：同一算术（thresh 掩码、flatness、hev、2/4/6-tap），逐字节对齐
// vp8.rs 三个标量滤波器。水平边（our vertical=false）连续字节行直接装入；竖直边
// （our vertical=true）用 Load16x4/Store16x4 紧凑转置。delta 在翻转符号域做饱和算术
// （减法翻转不变 → delta 为原始有符号值）；p0/q0 加/减在翻转域 saturating => 等价 [0,255] clamp。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_abs(a: __m128i, b: __m128i) -> __m128i {
    _mm_or_si128(_mm_subs_epu8(a, b), _mm_subs_epu8(b, a))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_needs_filter(mask: __m128i, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, thresh: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let t1 = lf_abs(p1, q1);
    let t2 = _mm_and_si128(t1, _mm_set1_epi8(0xFEu8 as i8));
    let t3 = _mm_srli_epi16(t2, 1);
    let t4 = lf_abs(p0, q0);
    let t5 = _mm_adds_epu8(_mm_adds_epu8(t4, t4), t3);
    let t6 = _mm_subs_epu8(t5, _mm_set1_epi8(thresh as i8));
    _mm_and_si128(_mm_cmpeq_epi8(t6, zero), mask)
}

/// 平坦度：`mask` 与 `max(|p3-p2|,|p2-p1|,|p1-p0|)` 再与 `max(...,|q1-q0|,|q2-q1|,|q3-q2|)` <= ithresh。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_flat(mask: __m128i, max_diff: __m128i, ithresh: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let t = _mm_subs_epu8(max_diff, _mm_set1_epi8(ithresh as i8));
    _mm_and_si128(_mm_cmpeq_epi8(t, zero), mask)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_not_hev(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, hev: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let mx = _mm_max_epu8(lf_abs(p1, p0), lf_abs(q1, q0));
    let t = _mm_subs_epu8(mx, _mm_set1_epi8(hev as i8));
    _mm_cmpeq_epi8(t, zero)
}

/// base delta（输入为翻转符号域）：p1-q1 + 3(q0-p0) 饱和 = 标量 clamp(clamp(p1-q1)+3(q0-p0))。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_base_delta(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i) -> __m128i {
    let p1_q1 = _mm_subs_epi8(p1, q1);
    let q0_p0 = _mm_subs_epi8(q0, p0);
    let s1 = _mm_adds_epi8(p1_q1, q0_p0);
    let s2 = _mm_adds_epi8(q0_p0, s1);
    let s3 = _mm_adds_epi8(q0_p0, s2);
    s3
}

/// 翻转符号域饱和加/减（等价标量 [0,255] clamp 后的 p/q 更新）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_add_byte(v: __m128i, d: __m128i, sb: __m128i) -> __m128i {
    _mm_xor_si128(_mm_adds_epi8(_mm_xor_si128(v, sb), d), sb)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_sub_byte(v: __m128i, d: __m128i, sb: __m128i) -> __m128i {
    _mm_xor_si128(_mm_subs_epi8(_mm_xor_si128(v, sb), d), sb)
}

/// 翻转符号域饱和加/减（等价标量 [0,255] clamp 后的 p/q 写回）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_flip_add(v: __m128i, d: __m128i, sb: __m128i) -> __m128i {
    lf_add_byte(v, d, sb)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_flip_sub(v: __m128i, d: __m128i, sb: __m128i) -> __m128i {
    lf_sub_byte(v, d, sb)
}

/// 水平边行向量：读指针 `p` 处 `n` 连续字节（n=16 或 8）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_row(b: *const u8, n: usize) -> __m128i {
    if n == 16 {
        _mm_loadu_si128(b as *const __m128i)
    } else {
        _mm_loadl_epi64(b as *const __m128i)
    }
}

/// 简单 2-tap（mask lane）：p0 += (a+3)>>3、q0 -= (a+4)>>3。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_simple2(a: __m128i, p0: __m128i, q0: __m128i) -> (__m128i, __m128i) {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let pa = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(3)), sb);
    let qa = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(4)), sb);
    (lf_add_byte(p0, pa, sb), lf_sub_byte(q0, qa, sb))
}

/// Load8x4：读 8 行 × 4 列（围绕竖直边），返回 (col0 的 8 行 + col1 的 8 行) 与 (col2+col3)。
/// 端口 libwebp `Load8x4_SSE2`（WebPMemToInt32 等价 read_unaligned<i32>）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_load8x4(b: *const u8, stride: isize) -> (__m128i, __m128i) {
    use std::arch::x86_64::*;
    let r = |i: isize| -> i32 {
        let p = b.offset(i * stride);
        let mut v = [0u8; 4];
        std::ptr::copy_nonoverlapping(p, v.as_mut_ptr(), 4);
        i32::from_le_bytes(v)
    };
    let a0 = _mm_set_epi32(r(6), r(2), r(4), r(0));
    let a1 = _mm_set_epi32(r(7), r(3), r(5), r(1));
    let b0 = _mm_unpacklo_epi8(a0, a1);
    let b1 = _mm_unpackhi_epi8(a0, a1);
    let c0 = _mm_unpacklo_epi16(b0, b1);
    let c1 = _mm_unpackhi_epi16(b0, b1);
    (
        _mm_unpacklo_epi32(c0, c1),
        _mm_unpackhi_epi32(c0, c1),
    )
}

/// Load16x4：读 16 行 × 4 列（竖直边 p1/p0/q0/q1 四列），返回四列各 16 字节 lane。
/// 端口 libwebp `Load16x4_SSE2`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_load16x4(b: *const u8, stride: isize) -> (__m128i, __m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    // r0=行0..7 的四列，r8=行8..15。
    let (p1lo, q0lo) = lf_load8x4(b, stride);
    let (p0hi, q1hi) = lf_load8x4(b.offset(8 * stride), stride);
    let t1 = p1lo;
    let t2 = q0lo;
    (
        _mm_unpacklo_epi64(t1, p0hi),
        _mm_unpackhi_epi64(t1, p0hi),
        _mm_unpacklo_epi64(t2, q1hi),
        _mm_unpackhi_epi64(t2, q1hi),
    )
}

/// Store4x4：把 4 个 lane 写回 4 行 × 4 列（每行一个 i32）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_store4x4(x: &mut __m128i, dst: *mut u8, stride: isize) {
    use std::arch::x86_64::*;
    for i in 0..4isize {
        let v = _mm_cvtsi128_si32(*x);
        std::ptr::write_unaligned(dst.offset(i * stride) as *mut i32, v);
        *x = _mm_srli_si128(*x, 4);
    }
}

/// Store16x4：4 个列向量（c0..c3 = col0..col3，各 16 行字节）写回 16 行。端口 libwebp `Store16x4_SSE2`。
/// 注意 unpack 顺序与 C 一致：先 (col0,col1) 与 (col2,col3) 按字节交错，再按 i16 交错还原 4×4。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_store16x4(
    c0: &mut __m128i, c1: &mut __m128i, c2: &mut __m128i, c3: &mut __m128i,
    r0: *mut u8, r8: *mut u8, stride: isize,
) {
    let t1 = *c1;
    let a = _mm_unpacklo_epi8(*c0, t1);
    let b = _mm_unpackhi_epi8(*c0, t1);
    let t1 = *c3;
    let c = _mm_unpacklo_epi8(*c2, t1);
    let d = _mm_unpackhi_epi8(*c2, t1);
    let t1 = a;
    let mut e = _mm_unpacklo_epi16(t1, c);
    let mut f = _mm_unpackhi_epi16(t1, c);
    let t1 = b;
    let mut g = _mm_unpacklo_epi16(t1, d);
    let mut h = _mm_unpackhi_epi16(t1, d);
    lf_store4x4(&mut e, r0, stride);
    lf_store4x4(&mut f, r0.offset(4 * stride), stride);
    lf_store4x4(&mut g, r8, stride);
    lf_store4x4(&mut h, r8.offset(4 * stride), stride);
}

/// DoFilter2（简单边）：NeedsFilter 掩码 lane 只更新 p0/q0（thresh=`edge`）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_do_filter2(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, edge: i32) -> (__m128i, __m128i) {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let mask = lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge);
    let a = _mm_and_si128(
        lf_base_delta(_mm_xor_si128(p1, sb), _mm_xor_si128(p0, sb), _mm_xor_si128(q0, sb), _mm_xor_si128(q1, sb)),
        mask,
    );
    lf_simple2(a, p0, q0)
}

/// DoFilter4（内部边）：delta=hev(p1-q1)+3(q0-p0)；p0/q0 2-tap；!hev 时 p1/q1 (qa+1)>>1。mask 已复合。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_do_filter4(
    p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, mask: __m128i, hev: i32,
) -> (__m128i, __m128i, __m128i, __m128i) {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let not_hev = lf_not_hev(p1, p0, q0, q1, hev);
    let p1s = _mm_xor_si128(p1, sb);
    let p0s = _mm_xor_si128(p0, sb);
    let q0s = _mm_xor_si128(q0, sb);
    let q1s = _mm_xor_si128(q1, sb);
    let mut t1 = _mm_subs_epi8(p1s, q1s);
    t1 = _mm_andnot_si128(not_hev, t1);
    let t2 = _mm_subs_epi8(q0s, p0s);
    t1 = _mm_adds_epi8(_mm_adds_epi8(_mm_adds_epi8(t1, t2), t2), t2);
    t1 = _mm_and_si128(t1, mask);
    let (p0n, q0n) = lf_simple2(t1, p0, q0);
    // !hev lane：inn = (qa + 1) >> 1（qa = (a+4)>>3 raw；标量同名恒等式）。
    let qa = shift_right3(_mm_adds_epi8(t1, _mm_set1_epi8(4)), sb);
    let inn = _mm_and_si128(shift_right1(_mm_adds_epi8(qa, _mm_set1_epi8(1)), sb), not_hev);
    let p1n = lf_flip_add(p1, inn, sb);
    let q1n = lf_flip_sub(q1, inn, sb);
    (p1n, p0n, q0n, q1n)
}

/// DoFilter6（宏块边）：hev lane 2-tap；!hev lane 27/18/9 权重（f×9/18/27+63）>>7 铺 p2..q2。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_do_filter6(
    p2: __m128i, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, q2: __m128i,
    mask: __m128i, hev: i32,
) -> (__m128i, __m128i, __m128i, __m128i, __m128i, __m128i) {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let zero = _mm_setzero_si128();
    let not_hev = lf_not_hev(p1, p0, q0, q1, hev);
    let a = lf_base_delta(
        _mm_xor_si128(p1, sb), _mm_xor_si128(p0, sb),
        _mm_xor_si128(q0, sb), _mm_xor_si128(q1, sb),
    );
    let m_hv = _mm_andnot_si128(not_hev, mask);
    let a_hv = _mm_and_si128(a, m_hv);
    let pa_hv = shift_right3(_mm_adds_epi8(a_hv, _mm_set1_epi8(3)), sb);
    let qa_hv = shift_right3(_mm_adds_epi8(a_hv, _mm_set1_epi8(4)), sb);
    let m_not = _mm_and_si128(not_hev, mask);
    let f = _mm_and_si128(a, m_not);
    // f（i8）放 i16 高字节（unpacklo_epi8(zero, f)）→ mulhi ×0x0900 = f×9 精确。
    let k9 = _mm_set1_epi16(0x0900i16);
    let k63 = _mm_set1_epi16(63);
    let f_lo = _mm_unpacklo_epi8(zero, f);
    let f_hi = _mm_unpackhi_epi8(zero, f);
    let f9_lo = _mm_mulhi_epi16(f_lo, k9);
    let f9_hi = _mm_mulhi_epi16(f_hi, k9);
    let a9_lo = _mm_add_epi16(f9_lo, k63);
    let a9_hi = _mm_add_epi16(f9_hi, k63);
    let a18_lo = _mm_add_epi16(a9_lo, f9_lo);
    let a18_hi = _mm_add_epi16(a9_hi, f9_hi);
    let a27_lo = _mm_add_epi16(a18_lo, f9_lo);
    let a27_hi = _mm_add_epi16(a18_hi, f9_hi);
    let s9 = _mm_packs_epi16(_mm_srai_epi16(a9_lo, 7), _mm_srai_epi16(a9_hi, 7));
    let s18 = _mm_packs_epi16(_mm_srai_epi16(a18_lo, 7), _mm_srai_epi16(a18_hi, 7));
    let s27 = _mm_packs_epi16(_mm_srai_epi16(a27_lo, 7), _mm_srai_epi16(a27_hi, 7));
    let p2n = lf_flip_add(p2, s9, sb);
    let p1n = lf_flip_add(p1, s18, sb);
    let q2n = lf_flip_sub(q2, s9, sb);
    let q1n = lf_flip_sub(q1, s18, sb);
    // p0/q0：hev 增量（pa/qa_hv，仅 hev∧mask lane 非零）与 6-tap 增量（s27，仅 !hev∧mask lane 非零）
    // 逐 lane 互斥 → 直接相加（非掩码 lane 两者皆 0 → 原值不变）。杜绝 OR 合并把非掩码 lane 清零。
    let p0n = lf_flip_add(p0, _mm_add_epi8(pa_hv, s27), sb);
    let q0n = lf_flip_sub(q0, _mm_add_epi8(qa_hv, s27), sb);
    (p2n, p1n, p0n, q0n, q1n, q2n)
}

/// 复合掩码：`filter`（thresh 判定）∧ 平坦度（max_diff <= ithresh）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_mask_composite(filter: __m128i, max_diff: __m128i, ithresh: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let t = _mm_subs_epu8(max_diff, _mm_set1_epi8(ithresh as i8));
    _mm_and_si128(filter, _mm_cmpeq_epi8(t, zero))
}

/// 平面内平坦度（MB/inner 共用六邻绝对差的最大值）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_max_diff(p3: __m128i, p2: __m128i, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, q2: __m128i, q3: __m128i) -> __m128i {
    let m = _mm_max_epu8(
        _mm_max_epu8(_mm_max_epu8(lf_abs(p3, p2), lf_abs(p2, p1)), lf_abs(p1, p0)),
        _mm_max_epu8(_mm_max_epu8(lf_abs(q0, q1), lf_abs(q1, q2)), lf_abs(q2, q3)),
    );
    m
}

/// 竖直边（our vertical=true）16-wide。`pt` 指向 (行 y, 帧列 x-4)（p3 列；simple 时自动 +2 至 p1）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_v16(
    pt: *mut u8, stride: isize,
    interior: i32, edge: i32, hev: i32, macroblock_edge: bool, simple: bool,
) {
    if simple {
        let bp = pt.offset(2);
        let (p1, p0, q0, q1) = lf_load16x4(bp, stride);
        let (p0n, q0n) = lf_do_filter2(p1, p0, q0, q1, edge);
        let (mut a0, mut a1, mut a2, mut a3) = (p1, p0n, q0n, q1);
        lf_store16x4(&mut a0, &mut a1, &mut a2, &mut a3, bp, bp.offset(8 * stride), stride);
        return;
    }
    let (p3, p2, p1, p0) = lf_load16x4(pt, stride);
    let (q0, q1, q2, q3) = lf_load16x4(pt.offset(4), stride);
    let mx = lf_max_diff(p3, p2, p1, p0, q0, q1, q2, q3);
    let mask = lf_mask_composite(lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge), mx, interior);
    if macroblock_edge {
        let (p2n, p1n, p0n, q0n, q1n, q2n) = lf_do_filter6(p2, p1, p0, q0, q1, q2, mask, hev);
        let (mut a0, mut a1, mut a2, mut a3) = (p3, p2n, p1n, p0n);
        let (mut b0, mut b1, mut b2, mut b3) = (q0n, q1n, q2n, q3);
        lf_store16x4(&mut a0, &mut a1, &mut a2, &mut a3, pt, pt.offset(8 * stride), stride);
        lf_store16x4(&mut b0, &mut b1, &mut b2, &mut b3, pt.offset(4), pt.offset(4 + 8 * stride), stride);
    } else {
        let (p1n, p0n, q0n, q1n) = lf_do_filter4(p1, p0, q0, q1, mask, hev);
        let (mut a0, mut a1, mut a2, mut a3) = (p3, p2, p1n, p0n);
        let (mut b0, mut b1, mut b2, mut b3) = (q0n, q1n, q2, q3);
        lf_store16x4(&mut a0, &mut a1, &mut a2, &mut a3, pt, pt.offset(8 * stride), stride);
        lf_store16x4(&mut b0, &mut b1, &mut b2, &mut b3, pt.offset(4), pt.offset(4 + 8 * stride), stride);
    }
}

/// 水平边（our vertical=false）16/8-wide。`pt` 指向 (行 y, 帧列 x)（q0 行首）；n=16/8。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_h(
    pt: *mut u8, stride: isize, n: usize,
    interior: i32, edge: i32, hev: i32, macroblock_edge: bool, simple: bool,
) {
    let p3 = lf_row(pt.offset(-4 * stride), n);
    let p2 = lf_row(pt.offset(-3 * stride), n);
    let p1 = lf_row(pt.offset(-2 * stride), n);
    let p0 = lf_row(pt.offset(-1 * stride), n);
    let q0 = lf_row(pt.offset(0), n);
    let q1 = lf_row(pt.offset(1 * stride), n);
    let q2 = lf_row(pt.offset(2 * stride), n);
    let q3 = lf_row(pt.offset(3 * stride), n);
    if simple {
        let (p0n, q0n) = lf_do_filter2(p1, p0, q0, q1, edge);
        std::ptr::copy_nonoverlapping(&p0n as *const __m128i as *const u8, pt.offset(-1 * stride), n);
        std::ptr::copy_nonoverlapping(&q0n as *const __m128i as *const u8, pt.offset(0), n);
        return;
    }
    let mx = lf_max_diff(p3, p2, p1, p0, q0, q1, q2, q3);
    let mask = lf_mask_composite(lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge), mx, interior);
    if macroblock_edge {
        let (p2n, p1n, p0n, q0n, q1n, q2n) = lf_do_filter6(p2, p1, p0, q0, q1, q2, mask, hev);
        let rows = [(p2n, -3isize), (p1n, -2), (p0n, -1), (q0n, 0), (q1n, 1), (q2n, 2)];
        for (v, off) in rows {
            std::ptr::copy_nonoverlapping(&v as *const __m128i as *const u8, pt.offset(off * stride), n);
        }
    } else {
        let (p1n, p0n, q0n, q1n) = lf_do_filter4(p1, p0, q0, q1, mask, hev);
        let rows = [(p1n, -2isize), (p0n, -1), (q0n, 0), (q1n, 1)];
        for (v, off) in rows {
            std::ptr::copy_nonoverlapping(&v as *const __m128i as *const u8, pt.offset(off * stride), n);
        }
    }
}

/// 新环路滤波入口（libwebp 风格 SSE2）：语义与 vp8::filter_edge 相同。8-wide 竖直边暂保留标量。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_filter_edge_sse2(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    let base = plane.as_mut_ptr();
    let center = (y * stride + 1 + x) as isize;
    let s = stride as isize;
    if vertical {
        if length == 16 {
            lf_edge_v16(base.offset(center - 4), s, interior, edge, hev, macroblock_edge, simple);
        } else {
            super::vp8::filter_edge(plane, stride, x, y, 8, true, macroblock_edge, simple, interior, edge, hev);
        }
    } else {
        lf_edge_h(base.offset(center), s, length, interior, edge, hev, macroblock_edge, simple);
    }
}

// ---------------------------------------------------------------------------
// D2v3：宏块级熔合环路滤波（C# `Vp8Decoder.FilterMacroblock` + `Vp8LoopFilter` 同款）
// ---------------------------------------------------------------------------
// 一个宏块把全 12 个核熔合成一次调用：luma 竖直×4、chroma 竖直×2（U/V 打包成 16-lane）、
// luma 水平×4、chroma 水平×2（U/V 打包）。边次序与 vp8.rs `filter_macroblock_row` 标量完全
// 一致（左 MB 竖 → 内竖×3 → 上 MB 横 → 内横×3），熔合只改调用形态、不改任何算术/读序 →
// 逐字节相同。U/V 打包把两平面拼一个向量：lane 逐样本独立，不做任何跨 U/V 混合，纯省调用与
// 向量粒度；掩码 lane 结果为原值，故「从不对 lane 跳过」也保证未滤 lane 字节不变。

/// 读一条 16-lane 色度行（低 8 = U、高 8 = V，同偏移行各 8 字节）—— C# `LoadPair` 同款。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_uv_pair_row(u: *const u8, v: *const u8, off: isize) -> __m128i {
    let ul = _mm_loadl_epi64(u.offset(off) as *const __m128i);
    let vl = _mm_loadl_epi64(v.offset(off) as *const __m128i);
    _mm_unpacklo_epi64(ul, vl)
}

/// 写回一条 16-lane 色度行（低 8 → U、高 8 → V）—— C# `StorePair` 同款。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_uv_pair_store(u: *mut u8, v: *mut u8, off: isize, val: __m128i) {
    _mm_storel_epi64(u.offset(off) as *mut __m128i, val);
    _mm_storel_epi64(v.offset(off) as *mut __m128i, _mm_srli_si128(val, 8));
}

/// 竖直色度边（U/V 打包，8+8=16 lane）：`u`/`v` 指向 q0 列（行 uv_row、列 uvx），每侧 8 行 8 列
/// 经 Load16x8/Store16x8 转置成 8 列、每列 16 lane（低 8 = U 行、高 8 = V 行）一次滤完 ——
/// C# `ChromaVertical` 同款。补齐 D2v2 `lf_filter_edge_sse2` 的 8 宽竖直标量回退。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_v8_pair(
    u: *mut u8,
    v: *mut u8,
    stride: isize,
    interior: i32,
    edge: i32,
    hev: i32,
    macroblock_edge: bool,
    simple: bool,
) {
    let ul = u.offset(-4);
    let vl = v.offset(-4);
    let mut c = load16x8(ul, stride, vl, stride);
    unsafe {
        if simple {
            let (p0n, q0n) = lf_do_filter2(c[2], c[3], c[4], c[5], edge);
            c[3] = p0n;
            c[4] = q0n;
        } else {
            let mx = lf_max_diff(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
            let mask = lf_mask_composite(
                lf_needs_filter(_mm_set1_epi8(-1), c[2], c[3], c[4], c[5], edge),
                mx,
                interior,
            );
            if macroblock_edge {
                let (p2n, p1n, p0n, q0n, q1n, q2n) =
                    lf_do_filter6(c[1], c[2], c[3], c[4], c[5], c[6], mask, hev);
                c[1] = p2n;
                c[2] = p1n;
                c[3] = p0n;
                c[4] = q0n;
                c[5] = q1n;
                c[6] = q2n;
            } else {
                let (p1n, p0n, q0n, q1n) = lf_do_filter4(c[2], c[3], c[4], c[5], mask, hev);
                c[2] = p1n;
                c[3] = p0n;
                c[4] = q0n;
                c[5] = q1n;
            }
        }
    }
    store16x8(ul, stride, vl, stride, c);
}

/// 水平色度边（U/V 打包）：`u`/`v` 指向 q0 行（行 uv_row±k、列 uvx），同偏移行各 8 字节拼一个
/// 16-lane 向量一次滤完 —— C# `ChromaHorizontal` 同款 LoadPair/StorePair。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_h_pair(
    u: *mut u8,
    v: *mut u8,
    stride: isize,
    interior: i32,
    edge: i32,
    hev: i32,
    macroblock_edge: bool,
    simple: bool,
) {
    unsafe {
        if simple {
            let (p0n, q0n) = lf_do_filter2(
                lf_uv_pair_row(u, v, -2 * stride),
                lf_uv_pair_row(u, v, -1 * stride),
                lf_uv_pair_row(u, v, 0),
                lf_uv_pair_row(u, v, stride),
                edge,
            );
            lf_uv_pair_store(u, v, -1 * stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            return;
        }
        let p3 = lf_uv_pair_row(u, v, -4 * stride);
        let p2 = lf_uv_pair_row(u, v, -3 * stride);
        let p1 = lf_uv_pair_row(u, v, -2 * stride);
        let p0 = lf_uv_pair_row(u, v, -1 * stride);
        let q0 = lf_uv_pair_row(u, v, 0);
        let q1 = lf_uv_pair_row(u, v, stride);
        let q2 = lf_uv_pair_row(u, v, 2 * stride);
        let q3 = lf_uv_pair_row(u, v, 3 * stride);
        let mx = lf_max_diff(p3, p2, p1, p0, q0, q1, q2, q3);
        let mask = lf_mask_composite(
            lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge),
            mx,
            interior,
        );
        if macroblock_edge {
            let (p2n, p1n, p0n, q0n, q1n, q2n) = lf_do_filter6(p2, p1, p0, q0, q1, q2, mask, hev);
            lf_uv_pair_store(u, v, -3 * stride, p2n);
            lf_uv_pair_store(u, v, -2 * stride, p1n);
            lf_uv_pair_store(u, v, -1 * stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            lf_uv_pair_store(u, v, stride, q1n);
            lf_uv_pair_store(u, v, 2 * stride, q2n);
        } else {
            let (p1n, p0n, q0n, q1n) = lf_do_filter4(p1, p0, q0, q1, mask, hev);
            lf_uv_pair_store(u, v, -2 * stride, p1n);
            lf_uv_pair_store(u, v, -1 * stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            lf_uv_pair_store(u, v, stride, q1n);
        }
    }
}

/// 宏块级熔合环路滤波入口（生产；C# `Vp8Decoder.FilterMacroblock` 同款）。一次宏块完成
/// 全 12 个 16-lane 核调用，边次序与标量 `vp8::filter_macroblock_row` 完全一致，逐字节相同。
/// 参数为滤波强度实值（调用方已算好 interior/edge_limit/sub_limit/hev）。
/// 指针语义：`yc` 指向 (行 mby*16、列 mbx*16) 的 q0（= center = y*pitch+1+x），`uc`/`vc` 同理
/// 指向色度 q0；跨块读（左列 -4 / 上行 -4）落在解码平面 4 像素边距内（与标量同界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn lf_filter_macroblock(
    yc: *mut u8,
    y_pitch: isize,
    uc: *mut u8,
    vc: *mut u8,
    uv_pitch: isize,
    left: bool,
    top: bool,
    inner: bool,
    simple: bool,
    interior: i32,
    edge_limit: i32,
    sub_limit: i32,
    hev: i32,
) {
    // 竖直段：左 MB 边 → 内竖×3（与标量同序：后边读前边已滤像素）。
    if left {
        lf_edge_v16(yc.offset(-4), y_pitch, interior, edge_limit, hev, true, simple);
        if !simple {
            lf_edge_v8_pair(uc, vc, uv_pitch, interior, edge_limit, hev, true, false);
        }
    }
    if inner {
        lf_edge_v16(yc, y_pitch, interior, sub_limit, hev, false, simple);
        lf_edge_v16(yc.offset(4), y_pitch, interior, sub_limit, hev, false, simple);
        lf_edge_v16(yc.offset(8), y_pitch, interior, sub_limit, hev, false, simple);
        if !simple {
            lf_edge_v8_pair(uc.offset(4), vc.offset(4), uv_pitch, interior, sub_limit, hev, false, false);
        }
    }
    // 水平段：上 MB 边 → 内横×3。
    if top {
        lf_edge_h(yc, y_pitch, 16, interior, edge_limit, hev, true, simple);
        if !simple {
            lf_edge_h_pair(uc, vc, uv_pitch, interior, edge_limit, hev, true, false);
        }
    }
    if inner {
        lf_edge_h(yc.offset(4 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        lf_edge_h(yc.offset(8 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        lf_edge_h(yc.offset(12 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        if !simple {
            lf_edge_h_pair(
                uc.offset(4 * uv_pitch),
                vc.offset(4 * uv_pitch),
                uv_pitch,
                interior,
                sub_limit,
                hev,
                false,
                false,
            );
        }
    }
}



// 算术：RFC 6386 §14.4 与 vp8.rs `inverse_dct_short` 完全同序——用 k1=20091 / k2=-30068 的
// i16-mulhi 技巧（K = k + 65536，故 (x*K)>>16 = (x*k)>>16 + x，见 libwebp dec_sse2.c 注释），
// 两个 4×4 块（水平相邻）各一行装进一个 128-bit 寄存器做并行 butterfly；中间 i16 回绕与标量
// `as i16` 逐字节一致；末段 (x+4)>>3 用先加 4 再 srai_epi16（=算术右移，等价标量整除方向）。
// 两个块同行处理，dest 为 8 像素行首。字节==标量唯一标准；随机 parity 断言见模块底部。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn idct_add_two_sse2(src: *const i16, dst: *mut u8, stride: isize) {
    use std::arch::x86_64::*;
    unsafe {
        let k1 = _mm_set1_epi16(20091);
        let k2 = _mm_set1_epi16(-30068);
        // 各读 4 个 i16（低 64 位），与第二块同排拼成 8 个 i16。
        let la = |o: usize| _mm_loadl_epi64(src.add(o) as *const __m128i);
        let in0 = _mm_unpacklo_epi64(la(0), la(16));
        let in1 = _mm_unpacklo_epi64(la(4), la(20));
        let in2 = _mm_unpacklo_epi64(la(8), la(24));
        let in3 = _mm_unpacklo_epi64(la(12), la(28));
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
        // 第二遍（(x+4)>>3 舍入并入）。
        let four = _mm_set1_epi16(4);
        let dc = _mm_add_epi16(t0, four);
        let a2 = _mm_add_epi16(dc, t2);
        let b2 = _mm_sub_epi16(dc, t2);
        let c2 = _mm_add_epi16(
            _mm_sub_epi16(_mm_mulhi_epi16(t1, k2), _mm_mulhi_epi16(t3, k1)),
            _mm_sub_epi16(t1, t3),
        );
        let d2 = _mm_add_epi16(
            _mm_add_epi16(_mm_mulhi_epi16(t1, k1), _mm_mulhi_epi16(t3, k2)),
            _mm_add_epi16(t1, t3),
        );
        let s0 = _mm_srai_epi16(_mm_add_epi16(a2, d2), 3);
        let s1 = _mm_srai_epi16(_mm_add_epi16(b2, c2), 3);
        let s2 = _mm_srai_epi16(_mm_sub_epi16(b2, c2), 3);
        let s3 = _mm_srai_epi16(_mm_sub_epi16(a2, d2), 3);
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

/// 两个水平相邻 4×4 块的逆 DCT + 残差写回（字节==既有标量）。
/// `coeff_a`/`coeff_b` 为 [i16;16]，`col0` 为块 `a` 的左上列（帧坐标），块 `b` 在 `col0+4`。
/// 标量回退：逐块走 `vp8::inverse_dct_add`（恒以 has_ac=true —— 对全零块/仅 DC 块结果与
/// 既有短路逐字节一致，见 K 注释：零块残差为 0、DC-only 全 16 像素 (dc+4)>>3 同恒等式）。
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
        // libwebp 内核按「两 4×4 块在内存中紧邻（32 个 i16）」取数；两个入参是分离数组，
        // 必须拷进连续缓冲（绝不能依赖栈上巧合相邻——那是不定行为，real 管线曾因 UB 出错）。
        let mut buf = [0i16; 32];
        buf[..16].copy_from_slice(coeff_a);
        buf[16..].copy_from_slice(coeff_b);
        let dst = unsafe { plane.as_mut_ptr().add(row0 * pitch + 1 + col0) };
        unsafe {
            idct_add_two_sse2(buf.as_ptr(), dst, pitch as isize);
        }
        return;
    }
    super::vp8::inverse_dct_add(coeff_a, plane, pitch, row0, col0, true);
    super::vp8::inverse_dct_add(coeff_b, plane, pitch, row0, col0 + 4, true);
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
    unsafe {
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
}

// ---------------------------------------------------------------------------
// YUV→RGB 定点转换（对齐 vp8.rs `yuv_to_rgb`：`(s*k)>>8` 各项、bias 减后再 `>>6`，无重结合）
// ---------------------------------------------------------------------------

/// 标量单像素转换（直接复用 `vp8::yuv_to_rgb` 的算术源，保证 SIMD 与标量同序同 truncation）。
#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    super::vp8::yuv_to_rgb(y, u, v)
}

/// 一行 YUV→RGBA8（alpha 恒 255）：AVX2 每 16 像素批量（SSE4.1 段收尾）+ 标量尾；
/// 无 AVX2 则 SSE4.1 每 8 像素批量 + 标量尾；无 SIMD 全标量。逐字节恒等于标量
/// `vp8::yuv_to_rgb`（各路径同一算术源，见内核注释）。
/// 输入三平面各 `n` 字节，输出 `4*n` 字节。
pub(crate) fn yuv_row_to_rgba(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8]) {
    debug_assert_eq!(y.len(), u.len());
    debug_assert_eq!(y.len(), v.len());
    debug_assert_eq!(out.len(), y.len() * 4);
    let n = y.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 16 {
        // 默认 u16-mulhi 变体（`KIMG_WEBP_YUV_U16=0` 回退 i32 定点；两者逐字节一致）。
        let u16 = yuv_u16_enabled();
        let mut col = 0usize;
        unsafe {
            while col + 16 <= n {
                if u16 {
                    yuv16_to_rgba_avx2_u16(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
                        out.as_mut_ptr().add(col * 4),
                    );
                } else {
                    yuv16_to_rgba_avx2(
                        y.as_ptr().add(col),
                        u.as_ptr().add(col),
                        v.as_ptr().add(col),
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
                    out.as_mut_ptr().add(col * 4),
                );
                col += 8;
            }
            // 尾列（0..8 像素）逐像素标量。
            for i in col..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                let o = i * 4;
                *out.get_unchecked_mut(o) = r;
                *out.get_unchecked_mut(o + 1) = g;
                *out.get_unchecked_mut(o + 2) = b;
                *out.get_unchecked_mut(o + 3) = 255;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse4.1") && n >= 8 {
        let mut col = 0usize;
        unsafe {
            while col + 8 <= n {
                yuv8_to_rgba_sse41(
                    y.as_ptr().add(col),
                    u.as_ptr().add(col),
                    v.as_ptr().add(col),
                    out.as_mut_ptr().add(col * 4),
                );
                col += 8;
            }
            // 尾列（0..8 像素）逐像素标量。
            for i in col..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                let o = i * 4;
                *out.get_unchecked_mut(o) = r;
                *out.get_unchecked_mut(o + 1) = g;
                *out.get_unchecked_mut(o + 2) = b;
                *out.get_unchecked_mut(o + 3) = 255;
            }
        }
        return;
    }
    for i in 0..n {
        let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = 255;
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
    let a8 = _mm_set1_epi8(-1);

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

/// AVX2：16 像素（8×i32 lane × 2 组）定点 YUV→RGBA。算术与 [`yuv8_to_rgba_sse41`] 严格同源
/// （`(x*c)>>8` → bias 减 → `>>6` → min/max clamp [0,255]，`mullo_epi32`+`srai_epi32` 语义一致），
/// 只扩宽到 256-bit：`vpmovzxbd` 每 8 字节段提升成 8 个 i32 lane，低 8 像素/高 8 像素各一遍通道，
/// 再按 128-bit 段 pack 成 16 字节平面、256-bit 逐 lane unpack 交错成 RGBA——逐字节等于两个
/// 独立的 8px SSE4.1 调用（每 128-bit lane 的内层 unpack/pack 与 SSE4.1 完全同款）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_to_rgba_avx2(
    y: *const u8,
    u: *const u8,
    v: *const u8,
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
        let a8 = _mm256_set1_epi8(-1);

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
    out: *mut u8,
) {
    use std::arch::x86_64::*;
    unsafe {
        // 16 像素载入 → 16 i16（低位=像素）→ 左移 8 让像素落高字节（=×256）。
        let yv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(y as *const __m128i)), 8);
        let uv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(u as *const __m128i)), 8);
        let vv = _mm256_slli_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(v as *const __m128i)), 8);

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
        let a8 = _mm256_set1_epi8(-1);

        // 与 i32 变体同款交错+重排。
        let rg = _mm256_unpacklo_epi8(r8, g8);
        let ba = _mm256_unpacklo_epi8(b8, a8);
        let out_lo = _mm256_unpacklo_epi16(rg, ba);
        let out_hi = _mm256_unpackhi_epi16(rg, ba);
        let p01 = _mm256_permute2x128_si256(out_lo, out_hi, 0x20);
        let p23 = _mm256_permute2x128_si256(out_lo, out_hi, 0x31);
        _mm256_storeu_si256(out as *mut __m256i, p01);
        _mm256_storeu_si256((out as *mut __m256i).add(1), p23);
    }
}

// ---------------------------------------------------------------------------
// fancy 上采样（SSE2 16-bit lane；语义对齐 vp8.rs `upsample_fancy`：`3*cur+adj` 的 t 行 +
// 偶数/奇数对的 `(3t±邻居+8)>>4`，无重结合）
// ---------------------------------------------------------------------------

/// fancy 上采样一行 chroma：`current`/`adjacent` 各 `cw` 字节 → 输出 `width` 字节。
/// SIMD（SSSE3）处理中间列对（col=1..cw-2），首/末列与 `cw<=1` 走标量（边界算术特殊）。
/// 便捷包装（生产热路径用 [`upsample_fancy_into`] 复用缓冲；本包装供测试/独立调用）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn upsample_fancy_row(current: &[u8], adjacent: &[u8], width: usize) -> Vec<u8> {
    let mut out = vec![0u8; width];
    let mut t = vec![0u16; current.len() + 8];
    upsample_fancy_into(current, adjacent, &mut out, &mut t);
    out
}

/// fancy 上采样写入调用方缓冲（复用 `out`/`t_ws`，避免逐行分配；`t_ws` 须 ≥ `cw+8`）。
pub(crate) fn upsample_fancy_into(current: &[u8], adjacent: &[u8], out: &mut [u8], t_ws: &mut [u16]) {
    debug_assert_eq!(current.len(), adjacent.len());
    let cw = current.len();
    debug_assert!(out.len() >= 2 * cw - 1);
    let width = out.len();
    let t = &mut t_ws[..cw + 8];
    t.fill(0);
    for col in 0..cw {
        t[col] = 3 * u16::from(current[col]) + u16::from(adjacent[col]);
    }
    let out = &mut out[..width];

    // 边界列（0/1 与末列）标量（`upsample_fancy` 同款）。
    let first = t[0];
    out[0] = ((4 * first + 8) >> 4) as u8;
    if width > 1 {
        let next = if cw > 1 { t[1] } else { first };
        out[1] = ((3 * first + next + 8) >> 4) as u8;
    }
    if cw > 1 {
        let last_col = cw - 1;
        let c = t[last_col];
        let oc = last_col * 2;
        out[oc] = ((3 * c + t[last_col - 1] + 8) >> 4) as u8;
        if oc + 1 < width {
            out[oc + 1] = ((4 * c + 8) >> 4) as u8;
        }
    }

    // 中间列对（col=1..cw-2）：每 4 列一组 SSSE3 推 8 个输出字节；尾列标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        let mut col = 1usize;
        unsafe {
            // 窗口 t[col-1..col+5]（6 个 u16）+ 8 填充保证 8 u16 读安全：col+5 ≤ cw。
            while col + 4 < cw {
                upsample_fancy_sse2(t.as_ptr().add(col - 1), out.as_mut_ptr().add(col * 2));
                col += 4;
            }
        }
        while col < cw.saturating_sub(1) {
            let c = t[col];
            let oc = col * 2;
            out[oc] = ((3 * c + t[col - 1] + 8) >> 4) as u8;
            out[oc + 1] = ((3 * c + t[col + 1] + 8) >> 4) as u8;
            col += 1;
        }
        return;
    }

    // 无 SIMD / 未满 6 列：直接复用 vp8 标量（单一算术源，边界语义零漂移）。
    out.copy_from_slice(&super::vp8::upsample_fancy(current, adjacent, width));
}

/// SSSE3：处理 `t_base` 处的 8 个 u16（t[col-1..col+6]，后者可有填充），对 4 个连续
/// 中间列（col..col+3）各算一对输出 `out[(col)*2 .. col*2+8]`：
/// `even=(3*t[i]+t[i-1]+8)>>4`、`odd=(3*t[i]+t[i+1]+8)>>4`（全部正数，逻辑右移即对）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn upsample_fancy_sse2(t_base: *const u16, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    // t_base = &t[col-1]；载 8 个 u16（调用方保证 col+4<cw 且 t 尾填充 ≥8）。
    let w = _mm_loadu_si128(t_base as *const __m128i); // lanes: t[col-1..col+6]
    let three = _mm_set1_epi16(3);
    let bias8 = _mm_set1_epi16(8);

    // 四个中间列 mid = t[col..col+3]（lanes 1..4）。
    let mid = _mm_srli_si128(w, 2);
    // 左邻 left = t[col-1..col+2]（lanes 0..3）。
    let left = w;
    // 右邻 right = t[col+1..col+4]（lanes 2..5）。
    let right = _mm_srli_si128(w, 4);

    let even = _mm_srli_epi16(_mm_add_epi16(_mm_add_epi16(_mm_mullo_epi16(mid, three), left), bias8), 4);
    let odd = _mm_srli_epi16(_mm_add_epi16(_mm_add_epi16(_mm_mullo_epi16(mid, three), right), bias8), 4);

    // packus：u16（≤255）→ u8。偶数 in 字节 0..7、奇数 in 字节 8..15；shuffle 取每 16 位对的
    // 偶/奇字节交错成 [e0,o0,e1,o1,e2,o2,e3,o3]（mask 各输出位 p[0/8]、p[1/9]…）。
    let p = _mm_packus_epi16(even, odd);
    let shuf = _mm_set_epi8(-1, -1, -1, -1, -1, -1, -1, -1, 11, 3, 10, 2, 9, 1, 8, 0);
    let interleaved = _mm_shuffle_epi8(p, shuf);
    _mm_storel_epi64(out as *mut __m128i, interleaved);
    }
}

// ---------------------------------------------------------------------------
// VP8L ARGB→RGBA8 输出（SSSE3 字节重排）与 Subtract-Green 逆变换（SSE4.1）——
// 对齐 vp8l.rs `to_rgba` / `apply_subtract_green`（mod 256 逐通道加，无进位）
// ---------------------------------------------------------------------------

/// ARGB 像素（u32：A<<24|R<<16|G<<8|B，内存小端字节序 [B,G,R,A]）→ RGBA8 字节。
/// 即 vp8l.rs `to_rgba` 的字节序交换（B/R 对调）；SSSE3 每 4 像素一次 shuffle，尾列标量。
pub(crate) fn argb_to_rgba(pixels: &[u32], out: &mut [u8]) {
    debug_assert_eq!(out.len(), pixels.len() * 4);
    let n = pixels.len();
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

/// Subtract-Green 逆变换（标量语义 `apply_subtract_green`）：R/G 通道 + G 通道（mod 256）。
/// 像素内存字节 [B,G,R,A]：把 G 字节加到 B 与 R 字节，G/A 原样保留。SSE4.1 每 4 像素，尾列标量。
pub(crate) fn subtract_green(pixels: &mut [u32]) {
    let n = pixels.len();
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

// ---------------------------------------------------------------------------
// 一致性单测：SIMD 路径与标量逐字节相同（bpp 覆盖 / 边界 / 伪随机）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性 LCG（xorshift64*），不依赖 rand。
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 40) as u8
        }
    }

    #[test]
    fn yuv_row_simd_matches_scalar() {
        let mut rng = Lcg(0x123456789abcdef0);
        if !simd_on("sse4.1") {
            eprintln!("SIMD_TEST SKIP (无 sse4.1)");
            return;
        }
        for n in [0usize, 1, 2, 4, 7, 8, 9, 15, 16, 33, 128, 257] {
            let y: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let u: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let v: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let mut simd_out = vec![0u8; n * 4];
            let mut scalar_out = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut simd_out);
            for i in 0..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                scalar_out[i * 4] = r;
                scalar_out[i * 4 + 1] = g;
                scalar_out[i * 4 + 2] = b;
                scalar_out[i * 4 + 3] = 255;
            }
            assert_eq!(simd_out, scalar_out, "n={n}: SIMD vs 标量逐字节");
        }
        // 边界值：全 0 / 全 255 / 常见灰阶（验证负数与 clamp 路径）。
        for val in [0u8, 1, 16, 64, 128, 129, 254, 255] {
            let y = vec![val; 16];
            let u = vec![val; 16];
            let v = vec![val; 16];
            let mut simd_out = vec![0u8; 64];
            let mut scalar_out = vec![0u8; 64];
            yuv_row_to_rgba(&y, &u, &v, &mut simd_out);
            for i in 0..16 {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                scalar_out[i * 4] = r;
                scalar_out[i * 4 + 1] = g;
                scalar_out[i * 4 + 2] = b;
                scalar_out[i * 4 + 3] = 255;
            }
            assert_eq!(simd_out, scalar_out, "flat {val}: SIMD vs 标量");
        }
        eprintln!("YUV_ROW_SIMD_PARITY_OK");
    }

    /// ARGB→RGBA 输出与 Subtract-Green：SIMD vs 标量逐字节（含奇偶边界与全通道路径）。
    #[test]
    fn argb_to_rgba_and_subtract_green_match_scalar() {
        let mut rng = Lcg(0x0123456789abcdef);
        for n in [0usize, 1, 2, 3, 4, 5, 8, 9, 16, 33, 200] {
            let mut px: Vec<u32> = (0..n).map(|_| rng.next() as u32).collect();
            let mut out_simd = vec![0u8; n * 4];
            argb_to_rgba(&px, &mut out_simd);
            let mut out_scalar = vec![0u8; n * 4];
            for (i, &p) in px.iter().enumerate() {
                out_scalar[i * 4] = (p >> 16) as u8;
                out_scalar[i * 4 + 1] = (p >> 8) as u8;
                out_scalar[i * 4 + 2] = p as u8;
                out_scalar[i * 4 + 3] = (p >> 24) as u8;
            }
            assert_eq!(out_simd, out_scalar, "argb_to_rgba n={n}");

            let simd_px = px.clone();
            subtract_green(&mut px);
            // 真实标量语义（v8pl `apply_subtract_green`：`+G & 0xff` 掩 8 位）逐字节对拍。
            let true_ref = simd_px
                .iter()
                .map(|&p| {
                    let g = (p >> 8) & 0xff;
                    let r = (((p >> 16) & 0xff) + g) & 0xff;
                    let b = ((p & 0xff) + g) & 0xff;
                    (p & 0xff00_ff00) | (r << 16) | b
                })
                .collect::<Vec<u32>>();
            assert_eq!(px, true_ref, "subtract_green TRUE-scalar n={n}");
        }
        eprintln!("ARGB_TO_RGBA_SUBTRACT_GREEN_PARITY_OK");
    }

    /// fancy 上采样：SIMD 整行 vs vp8 标量 `upsample_fancy` 逐字节（单一算术源对拍）。
    #[test]
    fn upsample_fancy_row_matches_scalar() {
        let mut rng = Lcg(0xdeadbeefcafef00d);
        // 各宽度（含奇数/偶数与超宽）覆盖中间/首末列路径。
        for cw in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 64, 129] {
            for wmul in [0usize, 1] {
                if cw == 0 {
                    continue;
                }
                // 输出宽 = 2*cw 或 2*cw-1（奇数原宽时末列仅左像素）。
                let width = 2 * cw - wmul;
                if width == 0 {
                    continue;
                }
                let cur: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let adj: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let simd = upsample_fancy_row(&cur, &adj, width);
                let scalar = super::super::vp8::upsample_fancy(&cur, &adj, width);
                assert_eq!(simd, scalar, "cw={cw} width={width}: SIMD vs 标量");
            }
        }
        eprintln!("UPSAMPLE_FANCY_SIMD_PARITY_OK");
    }

    // -----------------------------------------------------------------------
    // D1：4×4 十模式 × 邻居组合 + 16×16/8×8 四模式，SIMD 与标量逐字节一致
    // -----------------------------------------------------------------------

    /// 邻居值抽样（含饱和/边界）。
    fn neighbor_byte(rng: &mut Lcg) -> u8 {
        match rng.next() % 8 {
            0 => 0,
            1 => 1,
            2 => 127,
            3 => 128,
            4 => 254,
            5 => 255,
            _ => rng.byte(),
        }
    }

    #[test]
    fn predict_subblock_simd_matches_scalar() {
        if !simd_on("ssse3") {
            eprintln!("SIMD_TEST SKIP (无 ssse3)");
            return;
        }
        let mut rng = Lcg(0x51d2e3f4a5b6c7d8);
        let (pitch, rows) = (40usize, 40usize);
        let mut plane = vec![0u8; rows * pitch];
        let mut a = vec![0u8; rows * pitch];
        let mut b = vec![0u8; rows * pitch];
        let mut checked = 0usize;
        for trial in 0..400 {
            let mode = (trial % 10) as u8;
            let r0 = 1 + (trial % 3);
            let c0 = 1 + (trial % 3);
            for px in plane.iter_mut() {
                *px = rng.byte();
            }
            // 左列（列 col0-1）与 above 参数分别采样；偶发左列=129 帧边界。
            let mut above = [0u8; 9];
            for i in 0..9 {
                above[i] = neighbor_byte(&mut rng);
            }
            for i in 0..4 {
                let v = if trial % 7 == 0 { 129 } else { neighbor_byte(&mut rng) };
                plane[(r0 + i) * pitch + c0 - 1] = v;
            }
            a.copy_from_slice(&plane);
            b.copy_from_slice(&plane);
            super::predict_subblock(&mut a, pitch, r0, c0, mode, &above);
            super::super::vp8::predict_subblock(&mut b, pitch, r0, c0, mode, &above);
            assert_eq!(a, b, "4×4 mode={mode} r0={r0} c0={c0} trial={trial}");
            checked += 1;
        }
        eprintln!("PREDICT_SUBBLOCK_SIMD_PARITY\t{checked}");
    }

    /// 16×16 / 8×8 四大模式 SIMD vs 标量逐字节。
    #[test]
    fn predict_large_simd_matches_scalar() {
        if !simd_on("ssse3") {
            eprintln!("SIMD_TEST SKIP (无 ssse3)");
            return;
        }
        let mut rng = Lcg(0x0123456789abcdef);
        let pitch = 64usize;
        let mut plane = vec![0u8; 64 * 64];
        let mut a = vec![0u8; 64 * 64];
        let mut b = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for trial in 0..120 {
            let size = if trial % 2 == 0 { 16 } else { 8 };
            let mode = (trial / 3) % 4;
            let has_top = (trial >> 2) & 1 == 0;
            let has_left = (trial >> 3) & 1 == 0;
            let (r0, c0) = (1, 1);
            for px in plane.iter_mut() {
                *px = rng.byte();
            }
            let above: Vec<u8> = (0..=size).map(|_| neighbor_byte(&mut rng)).collect();
            for i in 0..size {
                plane[(r0 + i) * pitch + c0 - 1] = neighbor_byte(&mut rng);
            }
            a.copy_from_slice(&plane);
            b.copy_from_slice(&plane);
            super::predict_large(&mut a, pitch, r0, c0, size, mode, has_top, has_left, &above);
            super::super::vp8::predict_large(&mut b, pitch, r0, c0, size, mode, has_top, has_left, &above);
            assert_eq!(
                a, b,
                "predict_large mode={mode} size={size} has_top={has_top} has_left={has_left} trial={trial}"
            );
            checked += 1;
        }
        eprintln!("PREDICT_LARGE_SIMD_PARITY\t{checked}");
    }

    // -----------------------------------------------------------------------
    // D2：环路滤波 Simple/Normal（Inner/Macroblock）× 竖/横 × 16/8 lane × 参数带
    // -----------------------------------------------------------------------

    #[test]
    fn filter_edge_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        set_simd_part_override("flt", Some(true));
        let mut rng = Lcg(0xfeedfacecafebeef);
        let pitch = 80usize;
        let n = 80usize;
        let mut plane_simd = vec![0u8; n * pitch];
        let mut plane_scalar = vec![0u8; n * pitch];
        let mut checked = 0usize;
        let simple_opts = [true, false];
        let mb_opt = [true, false];
        let vertical_opt = [true, false];
        let len_opt = [16usize, 8];
        // sharpness/level 常见量级：interior/edge/hev 全组合抽样。
        for trial in 0..160 {
            let simple = simple_opts[trial % 2];
            let macroblock_edge = mb_opt[(trial / 2) % 2];
            let vertical = vertical_opt[(trial / 4) % 2];
            let length = len_opt[(trial / 8) % 2];
            let (x, y) = if length == 16 { (16, 16) } else { (8, 8) };
            let interior = match (trial / 16) % 8 { 0 => 1, 1 => 2, 2 => 3, 3 => 5, 4 => 7, 5 => 9, 6 => 15, _ => 63 };
            let edge = match (trial / 16) % 8 { 0 => 2, 1 => 8, 2 => 16, 3 => 24, 4 => 40, 5 => 60, 6 => 130, _ => 190 };
            let hev = ((trial / 17) % 3) as i32;
            for b in plane_simd.iter_mut() {
                *b = rng.byte();
            }
            // 逼真邻域：边界常数值偶发插入（大 flat / 强边）。
            for i in 0..(n * pitch / 8) {
                let off = rng.next() as usize % (n * pitch);
                plane_simd[off] = if i % 3 == 0 { 0 } else { 255 };
            }
            plane_scalar.copy_from_slice(&plane_simd);
            super::filter_edge(&mut plane_simd, pitch, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev);
            super::super::vp8::filter_edge(&mut plane_scalar, pitch, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev);
            assert_eq!(
                plane_simd, plane_scalar,
                "filter simple={simple} mb={macroblock_edge} vertical={vertical} len={length} interior={interior} edge={edge} hev={hev} trial={trial}"
            );
            checked += 1;
        }
        set_simd_part_override("flt", None);
        eprintln!("FILTER_EDGE_SIMD_PARITY\t{checked}");
    }

    /// 8×8 转置梯往返（矩阵转置自身逆）与 16×8 梯（load16x8 ↔ store16x8）自检。
    #[test]
    fn transpose_ladders_roundtrip() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        let mut rng = Lcg(0x8421c8421c842184);
        // 8×8：各行低 8 字节为矩阵行，转两次应还原。
        let mut plane = [0u8; 64];
        for b in plane.iter_mut() {
            *b = rng.byte();
        }
        unsafe {
            let rows: [__m128i; 8] = std::array::from_fn(|i| {
                _mm_loadl_epi64(plane.as_ptr().add(i * 8) as *const __m128i)
            });
            let cols = transpose8x8(rows);
            // 断言低 8 字节 = 期望转置（cols[k] 低 8 字节 = 列 k 的 8 行）。
            for k in 0..8 {
                let mut got = [0u8; 8];
                _mm_storel_epi64(got.as_mut_ptr() as *mut __m128i, cols[k]);
                for r in 0..8 {
                    assert_eq!(got[r], plane[r * 8 + k], "8×8 转置 col{k} row{r}");
                }
            }
            let back = transpose8x8(cols);
            for i in 0..8 {
                let mut got = [0u8; 8];
                _mm_storel_epi64(got.as_mut_ptr() as *mut __m128i, back[i]);
                assert_eq!(&got[..], &plane[i * 8..i * 8 + 8], "8×8 往返 row{i}");
            }
        }
        // 16×8：内存 16 行 × 8 列，load16x8 拆 a/b 两半 → store16x8 应还原。
        let mut plane = vec![0u8; 16 * 8];
        for b in plane.iter_mut() {
            *b = rng.byte();
        }
        let mut copy = plane.clone();
        unsafe {
            let a = copy.as_mut_ptr();
            let b = a.add(8 * 8); // 行 8..15（stride=8）
            let c = load16x8(a, 8, b, 8);
            store16x8(a, 8, b, 8, c);
        }
        assert_eq!(copy, plane, "16×8 转置梯往返");
        eprintln!("TRANSPOSE_LADDERS_ROUNDTRIP_OK");
    }

    // -----------------------------------------------------------------------
    // D3：IDCT 双块并行（SSE2）与标量逐字节一致（系数/量级/DC-only/全零随机对拍）
    // -----------------------------------------------------------------------

    /// 系数抽样：覆盖仅 DC（has_ac=false 语义）、全 AC、大/小量级、符号。
    fn coeff_i16(rng: &mut Lcg) -> i16 {
        match rng.next() % 12 {
            0 => 0,
            1 => 1,
            2 => -1,
            3 => 255,
            4 => -255,
            5 => 1024,
            6 => -1024,
            7 => 8192,
            8 => -8192,
            9 => 32767,
            10 => -32768,
            _ => (rng.next() as i32 as i16),
        }
    }

    #[test]
    fn inverse_dct_has_ac_shortcut_equivalence() {
        // 关键前提验证：has_ac=false（仅 DC 捷径）与 has_ac=true（全变换）对 [dc,0..0] 块
        // 逐字节一致 —— inverse_dct_add_two 标量回退恒传 has_ac=true 依赖此恒等式。
        let mut rng = Lcg(0x1111111122222222);
        let pitch = 32usize;
        let mut a = vec![0u8; 32 * 32];
        let mut b = vec![0u8; 32 * 32];
        let mut checked = 0usize;
        let mut dc_vals: Vec<i16> = vec![0, 1, -1, 15, -15, 255, -255, 2047, -2048,
            32767, -32768, 12345, -12345, 300, -300];
        for _ in 0..200 {
            dc_vals.push((rng.next() as u16) as i16);
        }
        for dc in dc_vals {
            let mut coeff = [0i16; 16];
            coeff[0] = dc;
            for px in a.iter_mut() {
                *px = rng.byte();
            }
            b.copy_from_slice(&a);
            super::super::vp8::inverse_dct_add(&coeff, &mut a, pitch, 4, 4, false);
            super::super::vp8::inverse_dct_add(&coeff, &mut b, pitch, 4, 4, true);
            assert_eq!(a, b, "has_ac 捷径不一致 dc={dc}");
            checked += 1;
        }
        eprintln!("IDCT_HAS_AC_SHORTCUT_EQUIV\t{checked}");
    }

    #[test]
    fn inverse_dct_two_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(false)); // 先关，建立标量行为基线不依赖 override
        let mut rng = Lcg(0xa5a5a5a5deadbeef);
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for trial in 0..400 {
            let mut ca = [0i16; 16];
            let mut cb = [0i16; 16];
            for k in 0..16 {
                ca[k] = coeff_i16(&mut rng);
                cb[k] = coeff_i16(&mut rng);
            }
            // 偶发整块零 / 仅 DC（映射到标量 has_ac 分支差异的最大考验）。
            if trial % 37 == 0 {
                ca = [0i16; 16];
            }
            if trial % 53 == 0 {
                let mut dc_only = [0i16; 16];
                dc_only[0] = coeff_i16(&mut rng);
                cb = dc_only;
            }
            for px in plane_simd.iter_mut() {
                *px = rng.byte();
            }
            let (row0, col0) = (8 + (trial % 8), 8 + (trial % 8));
            plane_scalar.copy_from_slice(&plane_simd);
            // SIMD 双块路径。
            super::inverse_dct_add_two(&ca, &cb, &mut plane_simd, pitch, row0, col0);
            // 标量参考：逐块恒 has_ac=true（已证与既有短路语义一致）。
            super::super::vp8::inverse_dct_add(&ca, &mut plane_scalar, pitch, row0, col0, true);
            super::super::vp8::inverse_dct_add(&cb, &mut plane_scalar, pitch, row0, col0 + 4, true);
            assert_eq!(
                plane_simd, plane_scalar,
                "inverse_dct_two trial={trial} row0={row0} col0={col0}"
            );
            checked += 1;
        }
        set_simd_part_override("dct", None);
        eprintln!("INVERSE_DCT_TWO_SIMD_PARITY\t{checked}");
    }
}
