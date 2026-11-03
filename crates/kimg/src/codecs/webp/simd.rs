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
            // RC1 熵核双路径：`bmi2` ⇒ `bmi1` ⇒ `lzcnt`（语义安全链，`simd_on` 判定一次）。
            "bmi2" => is_x86_feature_detected!("bmi2"),
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
    static ENT: AtomicBool = AtomicBool::new(false);
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        PRED.store(env_flag("KIMG_WEBP_NO_SIMD_PRED"), Relaxed);
        FLT.store(env_flag("KIMG_WEBP_NO_SIMD_FLT"), Relaxed);
        DCT.store(env_flag("KIMG_WEBP_NO_SIMD_DCT"), Relaxed);
        // 熵核双路径（RC1）：`KIMG_WEBP_ENT_NO_BMI=1`（本功能专用名，brief 定）或部件一致性
        // 名 `KIMG_WEBP_NO_SIMD_ENT=1`（同 PRED/FLT/DCT）任一 → 强制熵走基线（bsr+xor/mov→cl）。
        ENT.store(
            env_flag("KIMG_WEBP_ENT_NO_BMI") || env_flag("KIMG_WEBP_NO_SIMD_ENT"),
            Relaxed,
        );
    });
    match part {
        "pred" => PRED.load(Relaxed),
        "flt" => FLT.load(Relaxed),
        "dct" => DCT.load(Relaxed),
        "ent" => ENT.load(Relaxed),
        _ => false,
    }
}

#[cfg(test)]
mod part_override {
    use std::sync::atomic::{AtomicI8, Ordering::Relaxed};
    static PRED: AtomicI8 = AtomicI8::new(-1);
    static FLT: AtomicI8 = AtomicI8::new(-1);
    static DCT: AtomicI8 = AtomicI8::new(-1);
    static ENT: AtomicI8 = AtomicI8::new(-1);
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
            "ent" => ENT.store(v, Relaxed),
            _ => {}
        }
    }
    pub fn get(part: &str) -> Option<bool> {
        let v = match part {
            "pred" => PRED.load(Relaxed),
            "flt" => FLT.load(Relaxed),
            "dct" => DCT.load(Relaxed),
            "ent" => ENT.load(Relaxed),
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

/// 熵核 BMI1/2+lzcnt 快道判定（x86_64；RC1 熵/token 指令选择双路径）：`KIMG_WEBP_ENT_NO_BMI=1`
/// / `KIMG_WEBP_NO_SIMD_ENT=1` / `KIMG_WEBP_NO_SIMD=1` → 强制基线（bsr+xor/mov→cl，
/// 「bmi ≡ baseline」字节恒等对拍的强制侧）；缺省按运行时 `bmi2` 判定（bmi2 ⇒ bmi1 ⇒ lzcnt
/// 语义安全链，老 CPU 自动回落基线）。非 x86_64 恒 false。`#[cfg(test)]` 可经
/// `set_simd_part_override("ent", ...)` 进程内翻转（优先于 env，同 pred/flt/dct 姿势）。
/// 首次访问缓存一次 env+特性检测，之后仅 Relaxed 原子读——正常热循环开销可忽略。
#[inline]
pub(crate) fn ent_bmi_enabled() -> bool {
    part_simd_on("ent", "bmi2")
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

/// 预判定后的 4×4 预测核直调：`part_simd_on("pred","ssse3")` 已由调用方提升出子块循环
/// （该判定含 CPU 特性探测与多个 OnceLock，逐块调用在汇编上是真函数 call；判定结果在整帧
/// 解码内恒定，`reconstruct_macroblock` 每宏块取一次）。**Safety**：调用方须保证 ssse3
/// 可用（如先判定返回 true），且 `row0/col0/pitch` 在平面内——其余语义同
/// `predict_subblock` 的 SSSE3 分支，输出逐字节一致。
#[inline]
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
unsafe fn idct_add_two_sse2(coeff_a: *const i16, coeff_b: *const i16, dst: *mut u8, stride: isize) {
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
        // 第二遍（(x+4)>>3 舍入并入）：以 i32 中间量计算，杜绝 `_mm_add_epi16` 模 2^16 回绕后
        // `srai_epi16` 与标量 i32 `((a+4)>>3)` 的偏出（满幅系数 ±32767/-32768 单系数即可触发，
        // a2+d2、b2±c2 等和超出 i16 域；真实解量化系数量级远小于此，此改动只让内核与标量全输入域一致）。
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
        let s0 = pack_ab(sa0, sb0);
        let s1 = pack_ab(sa1, sb1);
        let s2 = pack_ab(sa2, sb2);
        let s3 = pack_ab(sa3, sb3);
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
/// 与 `idct_add_two_sse2` 共用同一套两遍 butterfly + i32 pass2 算术（A 半边），但 B 块不拼装
/// （无配对/合批）：i4x4 块间存在重建像素串行依赖（块 b 的左上下文读块 a 的「重建后」像素，
/// 必须先 predict b 再逐块 add，故禁止 2 块合批——S7/K15 同版约束）。末段加回像素并 clamp
/// [0,255] 只落低 4 像素/行（严格 4 字节读写，右缘块不越出平面右端，同 `dc_add_two_sse2` 姿势）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn idct_add_one_sse2(coeff: *const i16, dst: *mut u8, stride: isize) {
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
        // 第二遍：只对低 64（A 半边）符号扩展做 i32 pass2（高位乘项同理，高半恒 0）。
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
        // 打包回 i16（列布局）→ 转置回行布局（r_i = 行 i 残差 4 i16，高 64 恒 0）。
        let s0 = _mm_packs_epi32(s0, z);
        let s1 = _mm_packs_epi32(s1, z);
        let s2 = _mm_packs_epi32(s2, z);
        let s3 = _mm_packs_epi32(s3, z);
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
            idct_add_two_sse2(coeff_a.as_ptr(), coeff_b.as_ptr(), dst, pitch as isize);
        }
        return;
    }
    super::vp8::inverse_dct_add(coeff_a, plane, pitch, row0, col0, true);
    super::vp8::inverse_dct_add(coeff_b, plane, pitch, row0, col0 + 4, true);
}

/// 单个 4×4 块的逆 DCT + 残差写回（i4x4 用，字节==既有标量）。
/// `coeff` 为 [i16;16]。SIMD 走单块核 `idct_add_one_sse2`（与 i16 的 DCT 部件同门控
/// `part_simd_on("dct","sse2")`）；标量回退恒 has_ac=true（对仅 DC 块与既有捷径逐字节一致，
/// 见 `inverse_dct_has_ac_shortcut_equivalence`）。调用方在 AC 位为准时进入本入口，DC 捷径仍走标量。
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
            idct_add_one_sse2(coeff.as_ptr(), dst, pitch as isize);
        }
        return;
    }
    super::vp8::inverse_dct_add(coeff, plane, pitch, row0, col0, true);
}

/// 预判定后的单块逆变换核直调：`part_simd_on("dct","sse2")` 已由调用方提升出 i4 子块循环
/// （同 `predict_subblock_forced` 姿势，避免逐块特性判定 call）。**Safety**：调用方须保证
/// sse2 可用（如先判定返回 true），越界语义同 `inverse_dct_add_one` 的 SSE2 分支（`row0/col0`
/// 落在平面内）；仅 DC（无 AC）块亦走本核——`idct_add_one_sse2` 对仅 DC 块与标量 DC 捷径
/// 逐字节一致（`inverse_dct_has_ac_shortcut_equivalence`）。
#[inline]
pub(crate) unsafe fn idct_add_one_forced(
    coeff: &[i16; 16],
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
            idct_add_one_sse2(coeff.as_ptr(), dst, pitch as isize);
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
    unsafe {
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
            // 主批量：32 像素/步（YUV 加宽内核），余数 16px / 8px(SSE4.1) / 标量收尾。
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

/// AVX2 u16 16px 核心（已载入并左移 8 的 16-u16 y/u/v）：mulhi → max-sub → srli6 → packus →
/// 逐 lane 交错 RGBA → 两次 256-bit 存储。算术与标量 `(x*c)>>8` 逐位同截断（见
/// [`yuv16_to_rgba_avx2_u16`] 文档）。`out` 写出 64 字节（16 像素 RGBA）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn yuv16_core_avx2_u16(yv: __m256i, uv: __m256i, vv: __m256i, a: Option<*const u8>, out: *mut u8) {
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
        let a8 = match a {
            Some(p) => _mm256_set_m128i(
                _mm_loadl_epi64(p.add(8) as *const __m128i),
                _mm_loadl_epi64(p as *const __m128i),
            ),
            None => _mm256_set1_epi8(-1),
        };

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

/// AVX2 u16-mulhi 变体：32 像素/步（Req: 16→32px 加宽）。载 32 字节 → 拆低/高 16 像素两组，
/// 每组经 [`yuv16_core_avx2_u16`]（算术逐位同 16px 内核）→ 128 字节 RGBA。循环步长减半、
/// 载入并宽 3×32B 替代 6×16B，依赖链交错提 ILP；逐字节恒等于两个相邻 16px 内核调用。
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
        // 32 字节（32 像素）一次载入，拆低/高 16 字节两组。
        let yb = _mm256_loadu_si256(y as *const __m256i);
        let ub = _mm256_loadu_si256(u as *const __m256i);
        let vb = _mm256_loadu_si256(v as *const __m256i);
        let yl = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(yb));
        let yh = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(yb, 1));
        let ul = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(ub));
        let uh = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(ub, 1));
        let vl = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(vb));
        let vh = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(vb, 1));
        // 低 16 像素 → out[0..64]，高 16 像素 → out[64..128]。
        yuv16_core_avx2_u16(_mm256_slli_epi16(yl, 8), _mm256_slli_epi16(ul, 8), _mm256_slli_epi16(vl, 8), a, out);
        yuv16_core_avx2_u16(_mm256_slli_epi16(yh, 8), _mm256_slli_epi16(uh, 8), _mm256_slli_epi16(vh, 8), a.map(|p| p.add(16)), out.add(64));
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
/// 生产热路径改走 [`upsample_fancy_into_uv`]（U/V 合并）；本函数保留为标量对拍参考/测试路径。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn upsample_fancy_into(current: &[u8], adjacent: &[u8], out: &mut [u8], t_ws: &mut [u16]) {
    debug_assert_eq!(current.len(), adjacent.len());
    let cw = current.len();
    debug_assert!(out.len() >= 2 * cw - 1);
    let width = out.len();
    let t = &mut t_ws[..cw + 8];
    // 仅清零 8 个读安全 padding（`t[0..cw]` 紧随其后整体覆写，无需先零）；padding 值恒零、
    // 只作末组 SSE 越界读的保护，不参与任何输出字节（逐字节与全场清零恒等）。
    t[cw..].fill(0);
    // t 行 `3*cur + adj`：AVX2 每 32 列一组、SSE2 每 16 列一组（u16 16-lane），不足尾标量；
    // 经 unpack 零扩展后逐值等于标量（3*255+255=1020 < 2^16 无溢出），`t[0..cw]` 写满语义不变。
    let mut col = 0usize;
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") {
        let cp = current.as_ptr();
        let ap = adjacent.as_ptr();
        let tp = t.as_mut_ptr();
        unsafe {
            while col + 32 <= cw {
                upsample_fancy_trow_avx2(cp.add(col), ap.add(col), tp.add(col));
                col += 32;
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse2") {
        let cp = current.as_ptr();
        let ap = adjacent.as_ptr();
        let tp = t.as_mut_ptr();
        unsafe {
            while col + 16 <= cw {
                upsample_fancy_trow_sse2(cp.add(col), ap.add(col), tp.add(col));
                col += 16;
            }
        }
    }
    while col < cw {
        t[col] = 3 * u16::from(current[col]) + u16::from(adjacent[col]);
        col += 1;
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

    // 中间列对（col=1..cw-2）：AVX2 16 对/步 主批量 → 共享收尾；无 AVX2 直接 ssse3/标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && cw >= 18 {
        let mut col = 1usize;
        unsafe {
            // 16 对/步（C# `ChromaPairShuffleIndices` 同款）：current=t[col..col+16]、
            // 左邻 t[col-1..col+15]、右邻 t[col+1..col+17]，数学右读 ≤ t[cw-1]。
            while col + 17 <= cw {
                upsample_fancy_avx2(t.as_ptr().add(col), out.as_mut_ptr().add(col * 2));
                col += 16;
            }
        }
        if col < cw.saturating_sub(1) {
            upsample_fancy_tail(t, out, col, cw);
        }
        return;
    }
    // 中间列对（col=1..cw-2）：每 4 列一组 SSSE3 推 8 个输出字节；尾列标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        let mut col = 1usize;
        unsafe {
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

/// SSE2：一次载入 `current`/`adjacent` 的 16 字节 → unpack 零扩展成 16 u16 → 算
/// `t = 3*current + adjacent` 写 16 个 u16。与标量 `t[i] = 3*cur[i] + adj[i]` 逐值相等
/// （3*255+255=1020 < u16::MAX 无溢出；调用方保证 `col+16 ≤ cw` 不越界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn upsample_fancy_trow_sse2(cur: *const u8, adj: *const u8, t: *mut u16) {
    use std::arch::x86_64::*;
    unsafe {
        let c = _mm_loadu_si128(cur as *const __m128i);
        let a = _mm_loadu_si128(adj as *const __m128i);
        let three = _mm_set1_epi16(3);
        let zero = _mm_setzero_si128();
        let clo = _mm_unpacklo_epi8(c, zero);
        let chi = _mm_unpackhi_epi8(c, zero);
        let alo = _mm_unpacklo_epi8(a, zero);
        let ahi = _mm_unpackhi_epi8(a, zero);
        let tlo = _mm_add_epi16(_mm_mullo_epi16(clo, three), alo);
        let thi = _mm_add_epi16(_mm_mullo_epi16(chi, three), ahi);
        _mm_storeu_si128(t as *mut __m128i, tlo);
        _mm_storeu_si128((t as *mut __m128i).add(1), thi);
    }
}

/// AVX2：一次载入 `current`/`adjacent` 的 32 字节 → 拆低/高 16 字节并宽成 32 u16 → 算
/// `t = 3*current + adjacent` 写 32 个 u16（t[col..col+32]）。与标量逐值相等，逐字节等于两次
/// [`upsample_fancy_trow_sse2`]（每 16 列组算术同源；调用方保证 `col+32 ≤ cw` 不越界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn upsample_fancy_trow_avx2(cur: *const u8, adj: *const u8, t: *mut u16) {
    use std::arch::x86_64::*;
    unsafe {
        let c = _mm256_loadu_si256(cur as *const __m256i);
        let a = _mm256_loadu_si256(adj as *const __m256i);
        let three = _mm256_set1_epi16(3);
        let zero = _mm256_setzero_si256();
        // 低 16 列 → t[col..col+16]，高 16 列 → t[col+16..col+32]。
        let clo = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(c));
        let chi = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(c, 1));
        let alo = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(a));
        let ahi = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(a, 1));
        let tlo = _mm256_add_epi16(_mm256_mullo_epi16(clo, three), alo);
        let thi = _mm256_add_epi16(_mm256_mullo_epi16(chi, three), ahi);
        _mm256_storeu_si256(t as *mut __m256i, tlo);
        _mm256_storeu_si256((t as *mut __m256i).add(1), thi);
    }
}

/// 中间列对收尾：从 `col` 起 4 对/步 SSSE3 + 标量尾（与 `upsample_fancy` 标量同算术）。
/// 供 AVX2 16 对/步主批量推进后复用（SSSE3 未启用时全标量）。
#[cfg(target_arch = "x86_64")]
fn upsample_fancy_tail(t: &[u16], out: &mut [u8], mut col: usize, cw: usize) {
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        unsafe {
            // 窗口 t[col-1..col+5]（6 个 u16）+ 8 填充保证 8 u16 读安全：col+5 ≤ cw。
            while col + 4 < cw {
                upsample_fancy_sse2(t.as_ptr().add(col - 1), out.as_mut_ptr().add(col * 2));
                col += 4;
            }
        }
    }
    while col < cw.saturating_sub(1) {
        let c = t[col];
        let oc = col * 2;
        out[oc] = ((3 * c + t[col - 1] + 8) >> 4) as u8;
        out[oc + 1] = ((3 * c + t[col + 1] + 8) >> 4) as u8;
        col += 1;
    }
}

/// fancy 上采样 U/V 两行一次完成（参照 D2 `lf_uv_pair` 的 U/V 双平面打包先例，把两次
/// `upsample_fancy_into` 合并成同一循环构建两 t 行 + 同一循环处理两输出平面，省散布往返）。
/// 语义与逐平面调用 [`upsample_fancy_into`] 完全一致（逐字节）：t 行 `3*cur+adj`、边界列、
/// 中间列对 `(3*t±邻居+8)>>4` 全同算术，仅调用形态合并。`t_ws` 须 ≥ `2*(cw+8)`（低半 U、高半 V）。
pub(crate) fn upsample_fancy_into_uv(
    u_cur: &[u8],
    u_adj: &[u8],
    v_cur: &[u8],
    v_adj: &[u8],
    u_out: &mut [u8],
    v_out: &mut [u8],
    t_ws: &mut [u16],
) {
    let cw = u_cur.len();
    debug_assert_eq!(u_cur.len(), u_adj.len());
    debug_assert_eq!(v_cur.len(), v_adj.len());
    debug_assert_eq!(u_cur.len(), v_cur.len());
    debug_assert!(u_out.len() >= 2 * cw - 1);
    debug_assert_eq!(u_out.len(), v_out.len());
    let width = u_out.len();
    debug_assert!(t_ws.len() >= 2 * (cw + 8));
    let (tu, tv) = t_ws.split_at_mut(cw + 8);
    // padding 值恒零、只作末组 SSE 越界读保护（语义同单平面版）。
    tu[cw..].fill(0);
    tv[cw..].fill(0);
    // t 行合并构建：同一循环内向量化两平面（AVX2 32 列 / SSE2 16 列每组），尾部标量。
    let mut col = 0usize;
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") {
        let ucp = u_cur.as_ptr();
        let uap = u_adj.as_ptr();
        let vcp = v_cur.as_ptr();
        let vap = v_adj.as_ptr();
        let tup = tu.as_mut_ptr();
        let tvp = tv.as_mut_ptr();
        unsafe {
            while col + 32 <= cw {
                upsample_fancy_trow_avx2(ucp.add(col), uap.add(col), tup.add(col));
                upsample_fancy_trow_avx2(vcp.add(col), vap.add(col), tvp.add(col));
                col += 32;
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse2") {
        let ucp = u_cur.as_ptr();
        let uap = u_adj.as_ptr();
        let vcp = v_cur.as_ptr();
        let vap = v_adj.as_ptr();
        let tup = tu.as_mut_ptr();
        let tvp = tv.as_mut_ptr();
        unsafe {
            while col + 16 <= cw {
                upsample_fancy_trow_sse2(ucp.add(col), uap.add(col), tup.add(col));
                upsample_fancy_trow_sse2(vcp.add(col), vap.add(col), tvp.add(col));
                col += 16;
            }
        }
    }
    while col < cw {
        tu[col] = 3 * u16::from(u_cur[col]) + u16::from(u_adj[col]);
        tv[col] = 3 * u16::from(v_cur[col]) + u16::from(v_adj[col]);
        col += 1;
    }
    let uo = &mut u_out[..width];
    let vo = &mut v_out[..width];

    // 边界列（U/V 同款算术，两平面并算）。
    let (fu, fv) = (tu[0], tv[0]);
    uo[0] = ((4 * fu + 8) >> 4) as u8;
    vo[0] = ((4 * fv + 8) >> 4) as u8;
    if width > 1 {
        let (nu, nv) = if cw > 1 { (tu[1], tv[1]) } else { (fu, fv) };
        uo[1] = ((3 * fu + nu + 8) >> 4) as u8;
        vo[1] = ((3 * fv + nv + 8) >> 4) as u8;
    }
    if cw > 1 {
        let last_col = cw - 1;
        let (cu, cv) = (tu[last_col], tv[last_col]);
        let oc = last_col * 2;
        uo[oc] = ((3 * cu + tu[last_col - 1] + 8) >> 4) as u8;
        vo[oc] = ((3 * cv + tv[last_col - 1] + 8) >> 4) as u8;
        if oc + 1 < width {
            uo[oc + 1] = ((4 * cu + 8) >> 4) as u8;
            vo[oc + 1] = ((4 * cv + 8) >> 4) as u8;
        }
    }

    // 中间列对（col=1..cw-2）：AVX2 16 对/步 主批量（U/V 同一迭代）→ 共享收尾；否则 ssse3/标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && cw >= 18 {
        let mut col = 1usize;
        unsafe {
            // 16 对/步（C# `ChromaPairShuffleIndices` 同款）：右读 ≤ t[cw-1]。
            while col + 17 <= cw {
                upsample_fancy_avx2(tu.as_ptr().add(col), uo.as_mut_ptr().add(col * 2));
                upsample_fancy_avx2(tv.as_ptr().add(col), vo.as_mut_ptr().add(col * 2));
                col += 16;
            }
        }
        if col < cw.saturating_sub(1) {
            upsample_fancy_tail(tu, uo, col, cw);
            upsample_fancy_tail(tv, vo, col, cw);
        }
        return;
    }
    // 中间列对（col=1..cw-2）：每 4 列一组 SSSE3 各推 8 字节（同一迭代内 U/V），尾列标量。
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && cw >= 6 {
        let mut col = 1usize;
        unsafe {
            // 窗口 t[col-1..col+5]（6 个 u16）+ 8 填充保证 8 u16 读安全：col+5 ≤ cw。
            while col + 4 < cw {
                upsample_fancy_sse2(tu.as_ptr().add(col - 1), uo.as_mut_ptr().add(col * 2));
                upsample_fancy_sse2(tv.as_ptr().add(col - 1), vo.as_mut_ptr().add(col * 2));
                col += 4;
            }
        }
        while col < cw.saturating_sub(1) {
            let oc = col * 2;
            let cu = tu[col];
            let cv = tv[col];
            uo[oc] = ((3 * cu + tu[col - 1] + 8) >> 4) as u8;
            uo[oc + 1] = ((3 * cu + tu[col + 1] + 8) >> 4) as u8;
            vo[oc] = ((3 * cv + tv[col - 1] + 8) >> 4) as u8;
            vo[oc + 1] = ((3 * cv + tv[col + 1] + 8) >> 4) as u8;
            col += 1;
        }
        return;
    }

    // 无 SIMD / 未满 6 列：直接复用 vp8 标量（单一算术源，边界语义零漂移）。
    uo.copy_from_slice(&super::vp8::upsample_fancy(u_cur, u_adj, width));
    vo.copy_from_slice(&super::vp8::upsample_fancy(v_cur, v_adj, width));
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

/// AVX2：16 对/步（C# `UpsampleFancyVector128Core` 的 Avx2 段同款）。`t_base=&t[pair]`，
/// 载 16 个 u16 current（t[pair..pair+16]）+ 左邻（pair-1）+ 右邻（pair+1）各 16 u16 →
/// `even=(3*cur+left+8)>>4`、`odd=(3*cur+right+8)>>4` → packus 分开偶/奇 → 逐 128-bit lane
/// shuffle 交错（每 lane 内 8 对、lanes 各持 pairs 0-7/8-15，天然按输出序）→ 32 字节输出。
/// 算术同 [`upsample_fancy_sse2`]（`mid*3+左/右邻+8 >>4`），逐字节等于两次 4 对 SSE2 十分之一
/// 组（调用方保证 `pair+17 ≤ cw`：右读 t[pair+16] ≤ t[cw-1]，数学恒正不越界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn upsample_fancy_avx2(t_base: *const u16, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let w = _mm256_loadu_si256(t_base as *const __m256i);        // t[pair..pair+16]
    let left = _mm256_loadu_si256(t_base.sub(1) as *const __m256i); // t[pair-1..pair+15]
    let right = _mm256_loadu_si256(t_base.add(1) as *const __m256i); // t[pair+1..pair+17]
    let three = _mm256_set1_epi16(3);
    let bias8 = _mm256_set1_epi16(8);

    let even = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(_mm256_mullo_epi16(w, three), left), bias8), 4);
    let odd = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(_mm256_mullo_epi16(w, three), right), bias8), 4);

    // packus：偶 16 列 → 低/高 128 位各 8 字节，奇 16 列 → 同样布局；每 lane 内 shuffle 把该
    // lane 的偶/奇字节交错成 [e0,o0,e1,o1,...]（每 lane 全 16 字节：0..15 完整逐对交错）。
    let p = _mm256_packus_epi16(even, odd);
    let shuf = _mm256_set_epi8(
        15, 7, 14, 6, 13, 5, 12, 4, 11, 3, 10, 2, 9, 1, 8, 0,
        15, 7, 14, 6, 13, 5, 12, 4, 11, 3, 10, 2, 9, 1, 8, 0,
    );
    let interleaved = _mm256_shuffle_epi8(p, shuf);
    _mm256_storeu_si256(out as *mut __m256i, interleaved);
    }
}

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

// ---------------------------------------------------------------------------
// RGBA8 ↔ RGB8 通道重排（4→3 抽条 / 3→4 扩写，alpha 恒 255）——双转换去成本
// （K53 测量：to_rgba8 的 Rgb8 分支像素循环 22.7ms/8.6MP 是大头；SSSE3 扩写 ~6-13ms）。
// 字节恒等：ssse3 只做逐字节重排 + OR 常量 0xFF，不经任何算术。
// ---------------------------------------------------------------------------

/// RGBA8 → RGB8 抽条（4→3）：SSSE3 每 4 像素（16B 入 → 12B 出 = 8B 存 + 4B 存），
/// 尾像素标量（同现有 `chunks_exact` 语义）。无 SSSE3 → 全标量。输入长度须为 4 的倍数。
pub(crate) fn rgba8_to_rgb8(src: &[u8], dst: &mut [u8]) {
    debug_assert!(src.len() % 4 == 0);
    debug_assert_eq!(dst.len(), src.len() / 4 * 3);
    let n = src.len() / 4;
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    {
        if simd_on("ssse3") {
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            unsafe {
                while i + 4 <= n {
                    let v = _mm_loadu_si128(s.add(i * 4) as *const __m128i);
                    let out = _mm_shuffle_epi8(
                        v,
                        _mm_setr_epi8(0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 0, 0, 0, 0),
                    );
                    let o = d.add(i * 3);
                    _mm_storel_epi64(o as *mut __m128i, out);
                    let hi =
                        _mm_cvtsi128_si32(_mm_srli_si128(out, 8)) as u32;
                    std::ptr::write_unaligned(o.add(8) as *mut u32, hi);
                    i += 4;
                }
            }
        }
    }
    // 尾像素 + 无 SIMD 标量兜底（与既有 `chunks_exact(4) → copy_from_slice(&px[..3])` 同字节）。
    for (px, out) in src[i * 4..].chunks_exact(4).zip(dst[i * 3..].chunks_exact_mut(3)) {
        out.copy_from_slice(&px[..3]);
    }
}

/// RGB8 → RGBA8 扩写（3→4，alpha 恒 255）：SSSE3 每 4 像素（12B 读 → 16B 写；12B 读
/// 不越界：offset=(n-4)*3 恰 12B ≤ n*3），尾像素标量。无 SSSE3 → 全标量。
/// 与 `Pixel::to_rgba` 的 `[r,g,b,255]` 逐字节一致。
pub(crate) fn rgb8_to_rgba8(src: &[u8], dst: &mut [u8]) {
    debug_assert!(src.len() % 3 == 0);
    debug_assert_eq!(dst.len(), src.len() / 3 * 4);
    let n = src.len() / 3;
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    {
        if simd_on("ssse3") {
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            unsafe {
                while i + 4 <= n {
                    let lo = _mm_loadl_epi64(s.add(i * 3) as *const __m128i);
                    let hi32 = std::ptr::read_unaligned(s.add(i * 3 + 8) as *const u32) as i32;
                    let hi = _mm_cvtsi32_si128(hi32);
                    let data = _mm_unpacklo_epi64(lo, hi);
                    // pshufb 索引 0xFF（位 7 置位）→ 输出字节 0；OR 常量 0xFF 得 alpha=255。
                    let shuf = _mm_setr_epi8(0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1);
                    let alpha = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
                    let out = _mm_shuffle_epi8(data, shuf);
                    _mm_storeu_si128(
                        d.add(i * 4) as *mut __m128i,
                        _mm_or_si128(out, alpha),
                    );
                    i += 4;
                }
            }
        }
    }
    // 尾像素 + 无 SIMD 标量兜底（与既有 `[r,g,b,255]` 逐字节一致）。
    for (px, out) in src[i * 3..].chunks_exact(3).zip(dst[i * 4..].chunks_exact_mut(4)) {
        out[0] = px[0];
        out[1] = px[1];
        out[2] = px[2];
        out[3] = 255;
    }
}

// ---------------------------------------------------------------------------
// VP8L 预测器 / 色变换逆变换：块段分派 + SSE2/AVX2（字节==标量）
// 端口 libwebp `lossless_sse2.c` / `lossless_avx2.c` 批处理核（Average2 改 floor、
// ClampedAddSubtractHalf/Full、选择器 SAD 逐字节同）；行/段/末列边界语义严格对齐
// vp8l.rs 标量（标量版即字节正确蓝本，生成本文所有数值预期）。输入 u32 ARGB 小端序
// 内存 [B,G,R,A]×4 → 16 字节向量，`_mm_add_epi8` 即 `add_argb`（逐字节 mod 256）。
// 门控：`KIMG_WEBP_NO_SIMD=1` 一票否决（回退 vp8l.rs 整段标量）；预测器走 SSE2 核，
// 色变换优先 AVX2 8-px、回退 SSE4.1 4-px。
// ---------------------------------------------------------------------------

/// VP8L 预测器 SIMD 就绪（x86_64 且 SSE2 可用且 `KIMG_WEBP_NO_SIMD` 未关）。
pub(crate) fn vp8l_predictor_ready() -> bool {
    simd_on("sse2")
}

/// 段内恒定 mode 的单像素标量预测（内部像素：x>0 且 y>0，无边缘分支）。
/// 返回 `None` = mode 非法（>13，调用方整段回退 → vp8l.rs 产出结构化 Err）。
#[inline(always)]
fn pred_scalar_one(mode: u32, left: u32, top: u32, top_left: u32, top_right: u32) -> Option<u32> {
    use super::vp8l::{average, clamp_add, clamp_half, select, BLACK};
    Some(match mode {
        0 => BLACK,
        1 => left,
        2 => top,
        3 => top_right,
        4 => top_left,
        5 => average(average(left, top_right), top),
        6 => average(left, top_left),
        7 => average(left, top),
        8 => average(top_left, top),
        9 => average(top, top_right),
        10 => average(average(left, top_left), average(top, top_right)),
        11 => select(left, top, top_left),
        12 => clamp_add(left, top, top_left),
        13 => clamp_half(average(left, top), top_left),
        _ => return None,
    })
}

/// 整段恒定 mode 标量批处理（与 vp8l.rs 逐像素语义逐字节一致；含段首左侧读取）。
/// mode 非法（>13）时无操作（调用方已预检，不出现）。
#[inline]
fn pred_add_scalar(mode: u32, dst: *mut u32, upper: *const u32, n: usize) {
    if n == 0 {
        return;
    }
    unsafe {
        let mut left = *dst.sub(1);
        for i in 0..n {
            let s = *dst.add(i);
            let t = *upper.add(i);
            let tl = *upper.add(i).sub(1);
            let tr = *upper.add(i).add(1);
            let pred = pred_scalar_one(mode, left, t, tl, tr).expect("mode 已预检合法");
            let r = super::vp8l::add_argb(s, pred);
            *dst.add(i) = r;
            left = r;
        }
    }
}

/// 逐字节 floor 均值 `(a+b)>>1`（16 通道）：`pavgb - ((a^b)&1)`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn avg2_epu8(a: __m128i, b: __m128i) -> __m128i {
    let avg1 = _mm_avg_epu8(a, b);
    let one = _mm_and_si128(_mm_xor_si128(a, b), _mm_set1_epi8(1));
    _mm_sub_epi8(avg1, one)
}

/// 单像素 `select(a=top, b=left, c=top_left)`（模式 11，libwebp `Select_SSE2` 同款）：
/// pa=|T-TL|、pb=|L-TL|，pb<=pa → 取 a（top），否则取 b（left）——与 vp8l 标量
/// `dl<dt 取 left 否则 top` 逐位一致（相等时同取 top）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn select_u32(a: u32, b: u32, c: u32) -> u32 {
    let zero = _mm_setzero_si128();
    let va = _mm_cvtsi32_si128(a as i32);
    let vb = _mm_cvtsi32_si128(b as i32);
    let vc = _mm_cvtsi32_si128(c as i32);
    let ac = _mm_or_si128(_mm_subs_epu8(va, vc), _mm_subs_epu8(vc, va));
    let bc = _mm_or_si128(_mm_subs_epu8(vb, vc), _mm_subs_epu8(vc, vb));
    let pa = _mm_unpacklo_epi8(ac, zero);
    let pb = _mm_unpacklo_epi8(bc, zero);
    let diff = _mm_sub_epi16(pb, pa);
    let mut out = [0i16; 8];
    _mm_storeu_si128(out.as_mut_ptr() as *mut __m128i, diff);
    let pa_minus_pb = out[0] + out[1] + out[2] + out[3];
    if pa_minus_pb <= 0 {
        a
    } else {
        b
    }
}

/// 预测器段分派（mode 恒定，n 像素；`dst` 段首、`upper` 顶行、`left` 段首左侧已变换值）。
/// 运行时按 CPU 选核（AVX2 8 宽并行 → SSE2 4 宽 → 标量）；调用方已确保 sse2 在位。
/// 返回 false = mode 非法（>13，调用方整段回退标量）。
#[cfg(target_arch = "x86_64")]
fn predictor_segment(mode: u32, dst: *mut u32, upper: *const u32, n: usize, left: u32) -> bool {
    if mode > 13 {
        return false;
    }
    match mode {
        // 无 left 依赖：优先 AVX2 8 宽，回退 SSE2 4 宽
        // （0 黑 / 2 顶 / 3 顶右 / 4 顶左 / 8 均值(TL,T) / 9 均值(T,TR)）。
        0 | 2 | 3 | 4 | 8 | 9 => {
            if simd_on("avx2") {
                unsafe {
                    pred_add_parallel_avx2(mode, dst, upper, n);
                }
            } else {
                unsafe {
                    pred_add_parallel(mode, dst, upper, n);
                }
            }
        }
        // mode 1：left 前缀和（4 宽 log-step 扫描）。
        1 => unsafe {
            pred_add_prefix(dst, n, left);
        },
        // left 串行链：5/6/7/11/12/13 用「left 寄存器断代 + 整向量算」核
        // （5 avg3、6/7 avg2、11 select、12 clamp-add、13 clamp-half；add=1 条 `_mm_add_epi8`）。
        5 => unsafe {
            pred_add_avg3_serial(dst, upper, n, left);
        },
        6 | 7 => unsafe {
            pred_add_avg2_serial(mode, dst, upper, n, left);
        },
        11 => unsafe {
            pred_add_select_serial(dst, upper, n, left);
        },
        12 => unsafe {
            pred_add_clamp_add_serial(dst, upper, n, left);
        },
        13 => unsafe {
            pred_add_clamp_half_serial(dst, upper, n, left);
        },
        // 10 保持整段标量（命中占比低时可后补核）。
        _ => pred_add_scalar(mode, dst, upper, n),
    }
    true
}

/// AVX2 8 宽并行预测-加（模式 0/2/3/4/8/9；长剩余交回 SSE2 4 宽）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse2")]
unsafe fn pred_add_parallel_avx2(mode: u32, dst: *mut u32, upper: *const u32, n: usize) {
    let black = _mm256_set1_epi32(0xff00_0000u32 as i32);
    let mut i = 0usize;
    while i + 8 <= n {
        let src = _mm256_loadu_si256(dst.add(i) as *const __m256i);
        let pred = match mode {
            0 => black,
            2 => _mm256_loadu_si256(upper.add(i) as *const __m256i),
            3 => _mm256_loadu_si256(upper.add(i).add(1) as *const __m256i),
            4 => _mm256_loadu_si256(upper.add(i).sub(1) as *const __m256i),
            8 => avg2_epu8_256(
                _mm256_loadu_si256(upper.add(i).sub(1) as *const __m256i),
                _mm256_loadu_si256(upper.add(i) as *const __m256i),
            ),
            9 => avg2_epu8_256(
                _mm256_loadu_si256(upper.add(i) as *const __m256i),
                _mm256_loadu_si256(upper.add(i).add(1) as *const __m256i),
            ),
            _ => unreachable!(),
        };
        _mm256_storeu_si256(
            dst.add(i) as *mut __m256i,
            _mm256_add_epi8(src, pred),
        );
        i += 8;
    }
    if i < n {
        pred_add_parallel(mode, dst.add(i), upper.add(i), n - i);
    }
}

/// 256 位逐字节 floor 均值（AVX2）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse2")]
unsafe fn avg2_epu8_256(a: __m256i, b: __m256i) -> __m256i {
    let avg1 = _mm256_avg_epu8(a, b);
    let one = _mm256_and_si256(_mm256_xor_si256(a, b), _mm256_set1_epi8(1));
    _mm256_sub_epi8(avg1, one)
}

/// 4 宽并行预测-加（模式 0/2/3/4/8/9；n≥1，段首左侧已变换值 `left` 不参与预测）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_parallel(mode: u32, dst: *mut u32, upper: *const u32, n: usize) {
    let black = _mm_set1_epi32(0xff00_0000u32 as i32);
    let mut i = 0usize;
    while i + 4 <= n {
        let src = _mm_loadu_si128(dst.add(i) as *const __m128i);
        let pred = match mode {
            0 => black,
            2 => _mm_loadu_si128(upper.add(i) as *const __m128i),
            3 => _mm_loadu_si128(upper.add(i).add(1) as *const __m128i),
            4 => _mm_loadu_si128(upper.add(i).sub(1) as *const __m128i),
            8 => avg2_epu8(
                _mm_loadu_si128(upper.add(i).sub(1) as *const __m128i),
                _mm_loadu_si128(upper.add(i) as *const __m128i),
            ),
            9 => avg2_epu8(
                _mm_loadu_si128(upper.add(i) as *const __m128i),
                _mm_loadu_si128(upper.add(i).add(1) as *const __m128i),
            ),
            _ => unreachable!(),
        };
        _mm_storeu_si128(dst.add(i) as *mut __m128i, _mm_add_epi8(src, pred));
        i += 4;
    }
    if i < n {
        // 标量尾（段首左侧自取 *dst.sub(1)；不越界：段在行内且 x≥1）。
        pred_add_scalar(mode, dst.add(i), upper.add(i), n - i);
    }
}

/// mode 1 前缀和（log-step 扫描，4 宽；`left` = 段首左侧已变换值）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_prefix(dst: *mut u32, n: usize, left_init: u32) {
    let mut i = 0usize;
    let mut prev = _mm_set1_epi32(left_init as i32);
    while i + 4 <= n {
        let src = _mm_loadu_si128(dst.add(i) as *const __m128i);
        let s0 = _mm_add_epi8(src, _mm_slli_si128(src, 4));
        let s1 = _mm_add_epi8(s0, _mm_slli_si128(s0, 8));
        let res = _mm_add_epi8(s1, prev);
        _mm_storeu_si128(dst.add(i) as *mut __m128i, res);
        prev = _mm_shuffle_epi32(res, 0xff);
        i += 4;
    }
    if i < n {
        let mut left = if i >= 4 { *dst.add(i - 1) } else { left_init };
        while i < n {
            let s = *dst.add(i);
            let r = super::vp8l::add_argb(s, left);
            *dst.add(i) = r;
            left = r;
            i += 1;
        }
    }
}

/// left 串行链模式 6/7（avg2）：`left` 全程驻留向量寄存器，`add_argb` = `_mm_add_epi8`（1 条）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_avg2_serial(mode: u32, dst: *mut u32, upper: *const u32, n: usize, left_init: u32) {
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        // mode 7 只读 T；mode 6 只读 TL（不必读 TR）。
        let o = if mode == 7 { *upper.add(i) } else { *upper.add(i).sub(1) };
        let pred = avg2_epu8(lv, _mm_cvtsi32_si128(o as i32));
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}

/// left 串行链模式 5（avg3：avg(avg(L,TR),T)）：向量驻留。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_avg3_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) {
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        let tv = _mm_cvtsi32_si128(*upper.add(i) as i32);
        let trv = _mm_cvtsi32_si128(*upper.add(i).add(1) as i32);
        let a1 = avg2_epu8(lv, trv);
        let pred = avg2_epu8(a1, tv);
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}

/// left 串行链模式 11（select）：SAD 判定需横向求和 → 每像素提取标量做比较。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_select_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) {
    let mut left = left_init;
    for i in 0..n {
        let s = *dst.add(i);
        let t = *upper.add(i);
        let tl = *upper.add(i).sub(1);
        let pred = select_u32(t, left, tl);
        let res = _mm_add_epi8(
            _mm_cvtsi32_si128(s as i32),
            _mm_cvtsi32_si128(pred as i32),
        );
        let r = _mm_cvtsi128_si32(res) as u32;
        *dst.add(i) = r;
        left = r;
    }
}

/// left 串行链模式 12（clamp-add）：i16 lane `clamp(L+T-TL)`，`add`=1 条。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_clamp_add_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) {
    let zero = _mm_setzero_si128();
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        let l16 = _mm_unpacklo_epi8(lv, zero);
        let t16 = _mm_unpacklo_epi8(_mm_cvtsi32_si128(*upper.add(i) as i32), zero);
        let tl16 = _mm_unpacklo_epi8(_mm_cvtsi32_si128(*upper.add(i).sub(1) as i32), zero);
        let all = _mm_sub_epi16(_mm_add_epi16(l16, t16), tl16);
        let pred = _mm_packus_epi16(all, zero);
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}

/// left 串行链模式 13（clamp-half：clamp(avg(L,T) + (avg-TL)/2)）：向量驻留。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pred_add_clamp_half_serial(dst: *mut u32, upper: *const u32, n: usize, left_init: u32) {
    let zero = _mm_setzero_si128();
    let mut lv = _mm_cvtsi32_si128(left_init as i32);
    for i in 0..n {
        let s = *dst.add(i);
        let tv = _mm_cvtsi32_si128(*upper.add(i) as i32);
        let tlv = _mm_cvtsi32_si128(*upper.add(i).sub(1) as i32);
        let avgv = avg2_epu8(lv, tv);
        let am16 = _mm_unpacklo_epi8(avgv, zero);
        let t16 = _mm_unpacklo_epi8(tlv, zero);
        let a1 = _mm_sub_epi16(am16, t16);
        let bgt = _mm_srli_epi16(_mm_cmpgt_epi16(t16, am16), 15);
        let a2 = _mm_add_epi16(a1, bgt);
        let a3 = _mm_srai_epi16(a2, 1);
        let a4 = _mm_add_epi16(am16, a3);
        let pred = _mm_packus_epi16(a4, zero);
        let res = _mm_add_epi8(_mm_cvtsi32_si128(s as i32), pred);
        *dst.add(i) = _mm_cvtsi128_si32(res) as u32;
        lv = res;
    }
}

/// VP8L 预测器逆变换（整个图像；段分派见 `predictor_segment`）。`None` = 模式非法
/// 或 SIMD 未就绪 → 调用方回退 vp8l.rs 标量整段（非法模式产出结构化 Err，绝不半途改动像素）。
pub(crate) fn predictor_inverse(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) -> Option<()> {
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse2") {
        // 预检：全部 mode 合法（>13 不动像素直接回退）。
        for &d in data {
            if ((d >> 8) & 0xff) > 13 {
                return None;
            }
        }
        unsafe {
            predictor_inverse_sse2(pixels, width, height, bits, data);
        }
        return Some(());
    }
    None
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn predictor_inverse_sse2(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) {
    if width == 0 || height == 0 {
        return;
    }
    let block_w = (width + (1usize << bits) - 1) >> bits;
    let p = pixels.as_mut_ptr();
    let bitsu = bits as usize;

    // y=0：首像素 = in+BLACK，其余 = left 前缀和（模式无关）。
    {
        let residual = *p;
        let r0 = super::vp8l::add_argb(residual, super::vp8l::BLACK);
        *p = r0;
        if width > 1 {
            predictor_segment(1, p.add(1), p, width - 1, r0);
        }
    }

    for y in 1..height {
        let row = y * width;
        let top = p.add(row - width);
        // x==0：预测 = top（模式无关）。
        let r0 = super::vp8l::add_argb(*p.add(row), *top);
        *p.add(row) = r0;
        if width <= 1 {
            continue;
        }
        // 块段分派 x∈[1, width-1)（末列单独标量处理，TR 卷绕语义同标量）。
        let yb = y >> bitsu;
        let mut x = 1usize;
        while x < width - 1 {
            let bx = x >> bitsu;
            let mode = (*data.get_unchecked(yb * block_w + bx) >> 8) & 0xff;
            let seg_end = (((bx + 1) << bitsu)).min(width);
            let run_end = seg_end.min(width - 1);
            if run_end <= x {
                break;
            }
            predictor_segment(mode, p.add(row + x), top.add(x), run_end - x, *p.add(row + x - 1));
            x = run_end;
        }
        // 末列（width-1）：TR 卷绕 = 本行首像素（已变换）。
        let i = row + width - 1;
        let bx = (width - 1) >> bitsu;
        let mode = (*data.get_unchecked(yb * block_w + bx) >> 8) & 0xff;
        let left = *p.add(i - 1);
        let t = *p.add(i - width);
        let tl = *p.add(i - width - 1);
        let tr = *p.add(row); // 本行首像素
        if let Some(pred) = pred_scalar_one(mode, left, t, tl, tr) {
            *p.add(i) = super::vp8l::add_argb(*p.add(i), pred);
        }
    }
}

// ---------------------------------------------------------------------------
// VP8L 色变换逆变换（`Vector<int>` lane 算术：绿→红→蓝→α 顺序与 `color_delta` 标量
// `(i8*t * i8*g)>>5` 逐位一致）。AVX2 8 宽主、SSE4.1 4 宽回退、尾列标量。
// ---------------------------------------------------------------------------

/// 块内恒定 cte 的整段标量色变换（与 vp8l.rs 标量逐字节一致）。
#[inline]
unsafe fn color_run_scalar(dst: *mut u32, n: usize, cte: u32) {
    for i in 0..n {
        let p = *dst.add(i);
        let green = (p >> 8) & 0xff;
        let red = (p >> 16) & 0xff;
        let blue = p & 0xff;
        let red_delta = super::vp8l::color_delta(cte & 0xff, green);
        let blue_delta = super::vp8l::color_delta((cte >> 8) & 0xff, green);
        let red_for_blue = (red as i32 + red_delta) & 0xff;
        let new_blue = (blue as i32 + blue_delta
            + super::vp8l::color_delta((cte >> 16) & 0xff, red_for_blue as u32))
            & 0xff;
        *dst.add(i) = (p & 0xff00_ff00) | ((red_for_blue as u32) << 16) | (new_blue as u32);
    }
}

/// SSE4.1 4 宽色变换核。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn color_run_sse2(dst: *mut u32, n: usize, cte: u32) {
    let cr = _mm_set1_epi32((cte as u8 as i8) as i32);
    let cb = _mm_set1_epi32(((cte >> 8) as u8 as i8) as i32);
    let cb2 = _mm_set1_epi32(((cte >> 16) as u8 as i8) as i32);
    let m255 = _mm_set1_epi32(0xff);
    let m_ag = _mm_set1_epi32(0xff00_ff00u32 as i32);
    let mut i = 0usize;
    while i + 4 <= n {
        let vp = _mm_loadu_si128(dst.add(i) as *const __m128i);
        let g = _mm_and_si128(_mm_srai_epi32(vp, 8), m255);
        let gs = _mm_srai_epi32(_mm_slli_epi32(g, 24), 24);
        let r8 = _mm_and_si128(_mm_srai_epi32(vp, 16), m255);
        let b8 = _mm_and_si128(vp, m255);
        let rd = _mm_srai_epi32(_mm_mullo_epi32(gs, cr), 5);
        let rp = _mm_and_si128(_mm_add_epi32(r8, rd), m255);
        let rs = _mm_srai_epi32(_mm_slli_epi32(rp, 24), 24);
        let bd1 = _mm_srai_epi32(_mm_mullo_epi32(gs, cb), 5);
        let bd2 = _mm_srai_epi32(_mm_mullo_epi32(rs, cb2), 5);
        let bp = _mm_and_si128(_mm_add_epi32(_mm_add_epi32(b8, bd1), bd2), m255);
        let out = _mm_or_si128(
            _mm_and_si128(vp, m_ag),
            _mm_or_si128(_mm_slli_epi32(rp, 16), bp),
        );
        _mm_storeu_si128(dst.add(i) as *mut __m128i, out);
        i += 4;
    }
    if i < n {
        color_run_scalar(dst.add(i), n - i, cte);
    }
}

/// AVX2 8 宽色变换核（长剩余交回 4 宽 SSE4.1）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse4.1")]
unsafe fn color_run_avx2(dst: *mut u32, n: usize, cte: u32) {
    let cr = _mm256_set1_epi32((cte as u8 as i8) as i32);
    let cb = _mm256_set1_epi32(((cte >> 8) as u8 as i8) as i32);
    let cb2 = _mm256_set1_epi32(((cte >> 16) as u8 as i8) as i32);
    let m255 = _mm256_set1_epi32(0xff);
    let m_ag = _mm256_set1_epi32(0xff00_ff00u32 as i32);
    let mut i = 0usize;
    while i + 8 <= n {
        let vp = _mm256_loadu_si256(dst.add(i) as *const __m256i);
        let g = _mm256_and_si256(_mm256_srai_epi32(vp, 8), m255);
        let gs = _mm256_srai_epi32(_mm256_slli_epi32(g, 24), 24);
        let r8 = _mm256_and_si256(_mm256_srai_epi32(vp, 16), m255);
        let b8 = _mm256_and_si256(vp, m255);
        let rd = _mm256_srai_epi32(_mm256_mullo_epi32(gs, cr), 5);
        let rp = _mm256_and_si256(_mm256_add_epi32(r8, rd), m255);
        let rs = _mm256_srai_epi32(_mm256_slli_epi32(rp, 24), 24);
        let bd1 = _mm256_srai_epi32(_mm256_mullo_epi32(gs, cb), 5);
        let bd2 = _mm256_srai_epi32(_mm256_mullo_epi32(rs, cb2), 5);
        let bp = _mm256_and_si256(_mm256_add_epi32(_mm256_add_epi32(b8, bd1), bd2), m255);
        let out = _mm256_or_si256(
            _mm256_and_si256(vp, m_ag),
            _mm256_or_si256(_mm256_slli_epi32(rp, 16), bp),
        );
        _mm256_storeu_si256(dst.add(i) as *mut __m256i, out);
        i += 8;
    }
    if i < n {
        color_run_sse2(dst.add(i), n - i, cte);
    }
}

/// VP8L 色变换逆变换（整个图像；`bits` 块位宽、`data` 块乘数网格）。返回 false = SIMD
/// 未就绪（调用方回退 vp8l.rs 标量整段）。
pub(crate) fn color_transform_inverse(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) -> bool {
    let (avx2, sse4) = (simd_on("avx2"), simd_on("sse4.1"));
    if !avx2 && !sse4 {
        return false;
    }
    if width == 0 || height == 0 {
        return true;
    }
    let block_w = (width + (1usize << bits) - 1) >> bits;
    let bitsu = bits as usize;
    let d = data.as_ptr();
    let px = pixels.as_mut_ptr();
    for y in 0..height {
        let row = y * width;
        for bx in 0..block_w {
            let cte = unsafe { *d.add((y >> bitsu) * block_w + bx) };
            let start = bx << bitsu;
            let n = (((bx + 1) << bitsu).min(width)) - start;
            if n == 0 {
                continue;
            }
            let dst = unsafe { px.add(row + start) };
            unsafe {
                if avx2 {
                    color_run_avx2(dst, n, cte);
                } else {
                    color_run_sse2(dst, n, cte);
                }
            }
        }
    }
    true
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

    /// RGB3 直出核：`yuv_row_to_rgb3` 每像素前 3 字节恒等于 `yuv_row_to_rgba` 同输入
    /// RGB3 直出核：`yuv_row_to_rgb3` 每像素前 3 字节恒等于 `yuv_row_to_rgba` 同输入
    /// （opaque 有损直出 RGB3 的字节契约硬闸）。覆盖全档（avx2/u16、sse4.1、标量）与边界长
    /// （0/1/2/7/8/9/15/16/17/32/33/128/257 及全 0/全 255 灰阶）。
    #[test]
    fn yuv_row_rgb3_matches_rgba() {
        let mut rng = Lcg(0x9e3779b97f4a7c15);
        // 强制/直走各档长度，验证任一 SIMD 或标量路径 RGB3 与 RGBA 前 3 字节恒等。
        for n in [0usize, 1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 128, 257] {
            let y: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let u: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let v: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let mut rgba = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut rgba);
            let mut rgb3 = vec![0u8; n * 3];
            yuv_row_to_rgb3(&y, &u, &v, &mut rgb3);
            for i in 0..n {
                assert_eq!(
                    rgb3[i * 3..i * 3 + 3],
                    rgba[i * 4..i * 4 + 3],
                    "n={n} px={i}: RGB3 前 3 字节应等于 RGBA 前 3 字节"
                );
            }
        }
        // 边界值灰阶（负数/超限 clamp 路径）。
        for val in [0u8, 1, 16, 64, 128, 129, 254, 255] {
            let y = vec![val; 16];
            let u = vec![val; 16];
            let v = vec![val; 16];
            let mut rgba = vec![0u8; 64];
            let mut rgb3 = vec![0u8; 48];
            yuv_row_to_rgba(&y, &u, &v, &mut rgba);
            yuv_row_to_rgb3(&y, &u, &v, &mut rgb3);
            for i in 0..16 {
                assert_eq!(rgb3[i * 3..i * 3 + 3], rgba[i * 4..i * 4 + 3], "flat {val} px={i}");
            }
        }
        eprintln!("YUV_ROW_RGB3_PARITY_OK");
    }

    /// lossy-alpha 发射融合核：`yuv_row_to_rgba_alpha(Some(&alpha))` 逐字节恒等于
    /// 「旧二趟 `yuv_row_to_rgba` + 事后标量 `rgba[i*4+3]=alpha[i]`」合成（单遍≡两遍字节契约），
    /// 且 `None` 时与 `yuv_row_to_rgba`（α=255）完全一致；直接对标量 RGB 参考（覆盖全档
    /// avx2/i32、avx2/u16、sse4.1、标量尾列与边界长）。
    #[test]
    fn yuv_row_rgba_alpha_matches_two_pass_merge() {
        let mut rng = Lcg(0x51ed270b8c2d5f01);
        if !simd_on("sse4.1") {
            eprintln!("SIMD_TEST SKIP (无 sse4.1)");
            return;
        }
        for n in [0usize, 1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 128, 257] {
            let y: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let u: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let v: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let a: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            // 两遍合成参考：`yuv_row_to_rgba`（α=255）→ 事后标量写 alpha。
            let mut two_pass = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut two_pass);
            for (i, &av) in a.iter().enumerate() {
                two_pass[i * 4 + 3] = av;
            }
            // 单遍融合核（alpha 并入发射内层）。
            let mut fused = vec![0u8; n * 4];
            yuv_row_to_rgba_alpha(&y, &u, &v, Some(&a), &mut fused);
            assert_eq!(fused, two_pass, "n={n}: 单遍融合应逐字节等于两遍合成");
            // 直接对标量 RGB 参考（A 通道=alpha[i]）。
            for i in 0..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                assert_eq!(&fused[i * 4..i * 4 + 3], &[r, g, b], "n={n} px={i} RGB");
                assert_eq!(fused[i * 4 + 3], a[i], "n={n} px={i} A");
            }
            // None（无 ALPH）≡ 旧 `yuv_row_to_rgba`（α=255）。
            let mut none_out = vec![0u8; n * 4];
            yuv_row_to_rgba_alpha(&y, &u, &v, None, &mut none_out);
            let mut plain = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut plain);
            assert_eq!(none_out, plain, "n={n}: None 恒 255 应与 yuv_row_to_rgba 一致");
        }
        // 边界值灰阶 + 全 0/全 255 alpha。
        for val in [0u8, 1, 64, 128, 254, 255] {
            let y = vec![val; 16];
            let u = vec![val; 16];
            let v = vec![val; 16];
            for av in [0u8, 128, 255] {
                let a = vec![av; 16];
                let mut fused = vec![0u8; 64];
                yuv_row_to_rgba_alpha(&y, &u, &v, Some(&a), &mut fused);
                for i in 0..16 {
                    let (r, g, b) = yuv_to_rgb(val, val, val);
                    assert_eq!(&fused[i * 4..i * 4 + 3], &[r, g, b], "flat {val} av={av} px={i}");
                    assert_eq!(fused[i * 4 + 3], av, "flat {val} av={av} A");
                }
            }
        }
        eprintln!("YUV_ROW_RGBA_ALPHA_MERGE_PARITY_OK");
    }

    /// ARGB→RGBA 输出与 Subtract-Green：SIMD vs 标量逐字节（含奇偶边界与全通道路径）。
    #[test]
    fn argb_to_rgba_and_subtract_green_match_scalar() {
        let mut rng = Lcg(0x0123456789abcdef);
        for n in [0usize, 1, 2, 3, 4, 5, 8, 9, 12, 16, 33, 200] {
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

            // 原地版：SIMD 收尾 vs 标量逐字节（B/R 对调；覆盖 AVX2 8px / SSSE3 4px / 尾列长）。
            let mut ip_simd = px.clone();
            argb_to_rgba_inplace(&mut ip_simd);
            let ip_ref: Vec<u32> = px
                .iter()
                .map(|&p| (p & 0xff00_ff00) | ((p & 0xff) << 16) | ((p >> 16) & 0xff))
                .collect();
            assert_eq!(ip_simd, ip_ref, "argb_to_rgba_inplace n={n}");
            // 原地版逐字节 = 非原地版输出（同一输入）。
            let ip_bytes: Vec<u8> = ip_simd
                .iter()
                .flat_map(|&p| p.to_le_bytes())
                .collect();
            assert_eq!(ip_bytes, out_simd, "argb_to_rgba inplace vs out-of-place n={n}");

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

    /// fancy 上采样 U/V 合并路径 vs 单平面路径（单平面已与 vp8 标量逐字节对拍）逐字节一致。
    /// 覆盖 cw∈{1,2,…} 奇偶输出宽与 cw≤1 边界（cw=0 无输出宽，跳过与单平面测试一致）。
    #[test]
    fn upsample_fancy_uv_matches_single() {
        let mut rng = Lcg(0x9e3779b97f4a7c15);
        for cw in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 64, 129] {
            for wmul in [0usize, 1] {
                if cw == 0 {
                    continue;
                }
                // 输出宽 = 2*cw 或 2*cw-1（奇数原宽时末列仅左像素）。
                let width = 2 * cw - wmul;
                let u_cur: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let u_adj: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let v_cur: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let v_adj: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                // 单平面（对照）路径。
                let u_single = upsample_fancy_row(&u_cur, &u_adj, width);
                let v_single = upsample_fancy_row(&v_cur, &v_adj, width);
                // 合并路径。
                let mut u_out = vec![0u8; width];
                let mut v_out = vec![0u8; width];
                let mut t_ws = vec![0u16; 2 * (cw + 8)];
                upsample_fancy_into_uv(
                    &u_cur, &u_adj, &v_cur, &v_adj, &mut u_out, &mut v_out, &mut t_ws,
                );
                assert_eq!(u_out, u_single, "U cw={cw} width={width}: 合并 vs 单平面");
                assert_eq!(v_out, v_single, "V cw={cw} width={width}: 合并 vs 单平面");
            }
        }
        eprintln!("UPSAMPLE_FANCY_UV_PARITY_OK");
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

    /// 单块 i4x4 1-block SSE2 核与标量逐字节一致：满幅 i16 系数 + 随机平面 + 随机 DC，
    /// 跨 0/±4095/±4096/±4097 边界，偶发全零 / 仅 DC 块（映射标量 has_ac=true 路径）。
    #[test]
    fn inverse_dct_one_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP idct_one (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true)); // 强制 SSE2 单块核
        let boundaries: [i16; 16] = [
            0, 1, -1, 4095, -4095, 4096, -4096, 4097, -4097, 2047, -2048, 8192, -8192, 32767, -32768, 16384,
        ];
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for seed in [
            0x1357_9bdf_2468_ace0u64,
            0x0bad_cafe_f00d_3333,
            0xfeed_f00d_abcd_4444,
            0xdead_beef_1234_5555,
        ] {
            let mut rng = Lcg(seed);
            for trial in 0..300 {
                let mut c = [0i16; 16];
                for k in 0..16 {
                    // 满幅均匀随机（全 i16 值域），偶发注入跨 0/±4096 边界值。
                    c[k] = if rng.next() % 7 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        (rng.next() as u16) as i16
                    };
                }
                // 偶发整块零 / 仅 DC。
                if trial % 41 == 0 {
                    c = [0i16; 16];
                }
                if trial % 61 == 0 {
                    let mut dc_only = [0i16; 16];
                    dc_only[0] = boundaries[(rng.next() as usize) % boundaries.len()];
                    c = dc_only;
                }
                for px in plane_simd.iter_mut() {
                    *px = rng.byte();
                }
                // 覆盖右缘/下缘：块 4x4 输出列 col0+1..col0+4、行 row0..row0+3，落 64x64
                // 平面内 col0 顶到 59（尾列 63）、row0 顶到 60（尾行 63），验证 4 字节读写不越界。
                let (row0, col0) = match trial % 23 {
                    0 => (60usize, 59usize),
                    1 => (60, 4 + (trial % 11)),
                    2 => (4 + (trial % 11), 59),
                    _ => (4 + (trial % 11), 4 + (trial % 11)),
                };
                plane_scalar.copy_from_slice(&plane_simd);
                super::inverse_dct_add_one(&c, &mut plane_simd, pitch, row0, col0);
                super::super::vp8::inverse_dct_add(&c, &mut plane_scalar, pitch, row0, col0, true);
                assert_eq!(
                    plane_simd, plane_scalar,
                    "idct_one seed=0x{seed:016x} trial={trial} row0={row0} col0={col0} c={c:?}"
                );
                checked += 1;
            }
        }
        set_simd_part_override("dct", None);
        eprintln!("INVERSE_DCT_ONE_SIMD_PARITY\t{checked}");
    }

    #[test]
    fn dc_add_two_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true)); // 强制 SSE2：DC 捷径内核（仅 DC 块）逐字节==标量
        let mut rng = Lcg(0x76543210fedcba98);
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for trial in 0..400 {
            let dc_a = coeff_i16(&mut rng);
            let dc_b = coeff_i16(&mut rng);
            for px in plane_simd.iter_mut() {
                *px = rng.byte();
            }
            let (row0, col0) = (8 + (trial % 8), 8 + (trial % 8));
            plane_scalar.copy_from_slice(&plane_simd);
            super::dc_add_two(dc_a, dc_b, &mut plane_simd, pitch, row0, col0);
            // 标量参考：逐块恒 has_ac=true（[dc,0..0] 完整变换残差恒 (dc+4)>>3）。
            let mut ca = [0i16; 16];
            let mut cb = [0i16; 16];
            ca[0] = dc_a;
            cb[0] = dc_b;
            super::super::vp8::inverse_dct_add(&ca, &mut plane_scalar, pitch, row0, col0, true);
            super::super::vp8::inverse_dct_add(&cb, &mut plane_scalar, pitch, row0, col0 + 4, true);
            assert_eq!(
                plane_simd, plane_scalar,
                "dc_add_two trial={trial} dc_a={dc_a} dc_b={dc_b} row0={row0} col0={col0}"
            );
            checked += 1;
        }
        set_simd_part_override("dct", None);
        eprintln!("DC_ADD_TWO_SIMD_PARITY\t{checked}");
    }

    // -----------------------------------------------------------------------
    // VP8L 预测器 / 色变换：SIMD（块段分派）与标量逐字节一致
    // -----------------------------------------------------------------------

    /// 预测器标量参考（严格镜像 vp8l.rs `apply_predictor`：y==0 前缀 / x==0 顶行 /
    /// 末列 TR 卷绕 = 本行首像素）。
    fn predictor_reference(pixels: &mut [u32], width: usize, height: usize, bits: u32, data: &[u32]) {
        let block_w = (width + (1usize << bits) - 1) >> bits;
        use super::super::vp8l::{add_argb, BLACK};
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let pred = if x == 0 && y == 0 {
                    BLACK
                } else if y == 0 {
                    pixels[index - 1]
                } else if x == 0 {
                    pixels[index - width]
                } else {
                    let left = pixels[index - 1];
                    let top = pixels[index - width];
                    let tl = pixels[index - width - 1];
                    let tr = if x == width - 1 {
                        pixels[index - x]
                    } else {
                        pixels[index - width + 1]
                    };
                    let mode = (data[(y >> bits) * block_w + (x >> bits)] >> 8) & 0xff;
                    pred_scalar_one(mode, left, top, tl, tr).expect("mode 合法")
                };
                pixels[index] = add_argb(pixels[index], pred);
            }
        }
    }

    /// 色变换标量参考（严格镜像 vp8l.rs `apply_color_transform`）。
    fn color_reference(pixels: &mut [u32], width: usize, height: usize, bits: u32, data: &[u32]) {
        let block_w = (width + (1usize << bits) - 1) >> bits;
        use super::super::vp8l::color_delta;
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let cte = data[(y >> bits) * block_w + (x >> bits)];
                let green = (pixels[index] >> 8) & 0xff;
                let red = (pixels[index] >> 16) & 0xff;
                let blue = pixels[index] & 0xff;
                let red_delta = color_delta(cte & 0xff, green);
                let blue_delta = color_delta((cte >> 8) & 0xff, green);
                let red_for_blue = (red as i32 + red_delta) & 0xff;
                let new_blue = (blue as i32 + blue_delta + color_delta((cte >> 16) & 0xff, red_for_blue as u32)) & 0xff;
                pixels[index] = (pixels[index] & 0xff00_ff00) | ((red_for_blue as u32) << 16) | (new_blue as u32);
            }
        }
    }

    #[test]
    fn vp8l_predictor_inverse_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP vp8l_predictor (无 sse2)");
            return;
        }
        let mut rng = Lcg(0x51ab_c0de_0fae_9001);
        let widths = [1usize, 2, 3, 4, 5, 7, 8, 16, 33, 64];
        let heights = [1usize, 2, 3, 5, 8];
        let bitsets = [2u32, 3];
        let mut checked = 0usize;
        // 全图随机模式 + 逐模式整图（确保每 mode 核与边缘/尾部都被真机上下文覆盖）。
        for &w in &widths {
            for &h in &heights {
                for &b in &bitsets {
                    let block_w = (w + (1usize << b) - 1) >> b;
                    let block_h = (h + (1usize << b) - 1) >> b;
                    let mut data = vec![0u32; block_w * block_h];
                    // 随机模式图。
                    for d in data.iter_mut() {
                        *d = ((rng.next() % 14) as u32) << 8;
                    }
                    for _ in 0..3 {
                        let mut a = vec![0u32; w * h];
                        let mut b2 = vec![0u32; w * h];
                        // 不透明像素（α≥0x81）：标量 clamp_add/clamp_half 的整像素 i32 中间量
                        // 在 debug 下不会溢出（release 回绕逐字节仍一致）；同时保留全 RGB 字节多样性。
                        for px in a.iter_mut() {
                            *px = 0xff00_0000 | ((rng.next() as u32) & 0x00ff_ffff);
                        }
                        b2.copy_from_slice(&a);
                        predictor_inverse(&mut a, w, h, b, &data).expect("SIMD 预测器");
                        predictor_reference(&mut b2, w, h, b, &data);
                        assert_eq!(a, b2, "随机模式图 w={w} h={h} bits={b}");
                        checked += 1;
                    }
                    // 逐模式整图（0..13）。
                    for m in 0..14u32 {
                        data.fill(m << 8);
                        let mut a = vec![0u32; w * h];
                        let mut b2 = vec![0u32; w * h];
                        for px in a.iter_mut() {
                            *px = 0xff00_0000 | ((rng.next() as u32) & 0x00ff_ffff);
                        }
                        b2.copy_from_slice(&a);
                        predictor_inverse(&mut a, w, h, b, &data).expect("SIMD 预测器");
                        predictor_reference(&mut b2, w, h, b, &data);
                        assert_eq!(a, b2, "单模式={m} w={w} h={h} bits={b}");
                        checked += 1;
                    }
                }
            }
        }
        eprintln!("VP8L_PREDICTOR_SIMD_PARITY\t{checked}");
    }

    #[test]
    fn vp8l_predictor_invalid_mode_defers_scalar() {
        // 非法模式（>13）：predictor_inverse 返回 None，绝不半途改动像素。
        if !simd_on("sse2") {
            return;
        }
        let mut rng = Lcg(0x0bad_cafe_f00d_beee);
        for &w in &[4usize, 8] {
            for &h in &[3usize, 5] {
                for &b in &[2u32, 3] {
                    let block_w = (w + (1usize << b) - 1) >> b;
                    let block_h = (h + (1usize << b) - 1) >> b;
                    let mut data = vec![0u32; block_w * block_h];
                    data[block_w * block_h / 2] = 14 << 8; // 一个非法 mode
                    let mut a = vec![0u32; w * h];
                    for px in a.iter_mut() {
                        *px = rng.next() as u32;
                    }
                    let before = a.clone();
                    let r = predictor_inverse(&mut a, w, h, b, &data);
                    assert!(r.is_none(), "非法模式应返回 None");
                    assert_eq!(a, before, "非法模式不得改动像素");
                }
            }
        }
    }

    #[test]
    fn vp8l_color_transform_simd_matches_scalar() {
        if !simd_on("sse4.1") && !simd_on("avx2") {
            eprintln!("SIMD_TEST SKIP vp8l_color (无 sse4.1/avx2)");
            return;
        }
        let mut rng = Lcg(0xfeeb_daad_beef_cafe);
        let widths = [1usize, 2, 3, 4, 5, 7, 8, 16, 33, 64];
        let heights = [1usize, 2, 3, 5, 8];
        let bitsets = [2u32, 3, 4];
        let edge_cte = [0x0000_0000u32, 0x0080_8080, 0x00ff_ffff, 0x007f_7f7f, 0x0080_ff00, 0x007f_0180];
        let mut checked = 0usize;
        for &w in &widths {
            for &h in &heights {
                for &b in &bitsets {
                    let block_w = (w + (1usize << b) - 1) >> b;
                    let block_h = (h + (1usize << b) - 1) >> b;
                    let mut data = vec![0u32; block_w * block_h];
                    for d in data.iter_mut() {
                        // 随机 cte + 常给符号/全值边界（0x00/0x7f/0x80/0xff）。
                        *d = if (rng.next() % 4) == 0 {
                            edge_cte[(rng.next() as usize) % edge_cte.len()]
                        } else {
                            (rng.next() as u32) & 0x00ff_ffff
                        };
                    }
                    for _ in 0..2 {
                        let mut a = vec![0u32; w * h];
                        let mut b2 = vec![0u32; w * h];
                        for px in a.iter_mut() {
                            *px = (rng.next() as u32) & 0xffff_ffff;
                        }
                        b2.copy_from_slice(&a);
                        assert!(color_transform_inverse(&mut a, w, h, b, &data), "SIMD 色变换");
                        color_reference(&mut b2, w, h, b, &data);
                        assert_eq!(a, b2, "色变换 w={w} h={h} bits={b}");
                        checked += 1;
                    }
                }
            }
        }
        eprintln!("VP8L_COLOR_SIMD_PARITY\t{checked}");
    }

    /// RGBA8↔RGB8 通道重排：SIMD 核（有 SSSE3 时走宽道）与标量逐字节一致
    /// （长度覆盖：标量尾分支 / SSSE3 整组 / 奇数右缘 / 大缓冲）。
    #[test]
    fn rgba8_rgb8_repack_matches_scalar() {
        let mut rng = Lcg(0x55aa_55aa_55aa_55aa);
        for n in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 17, 33, 257, 4096] {
            // 4→3 抽条。
            let rgba: Vec<u8> = (0..n * 4).map(|_| rng.byte()).collect();
            let mut rgb_simd = vec![0u8; n * 3];
            let mut rgb_ref = vec![0u8; n * 3];
            rgba8_to_rgb8(&rgba, &mut rgb_simd);
            for (px, out) in rgba.chunks_exact(4).zip(rgb_ref.chunks_exact_mut(3)) {
                out.copy_from_slice(&px[..3]);
            }
            assert_eq!(rgb_simd, rgb_ref, "4→3 n={n}");

            // 3→4 扩写（alpha 恒 255）。
            let rgb: Vec<u8> = (0..n * 3).map(|_| rng.byte()).collect();
            let mut rgba_simd = vec![0u8; n * 4];
            let mut rgba_ref = vec![0u8; n * 4];
            rgb8_to_rgba8(&rgb, &mut rgba_simd);
            for (i, px) in rgb.chunks_exact(3).enumerate() {
                let o = i * 4;
                rgba_ref[o] = px[0];
                rgba_ref[o + 1] = px[1];
                rgba_ref[o + 2] = px[2];
                rgba_ref[o + 3] = 255;
            }
            assert_eq!(rgba_simd, rgba_ref, "3→4 n={n}");
        }
        // 边界值：全 0 / 全 255 / 交替（覆盖 OR 0xFF 与 0 字节路径）。
        for val in [0u8, 1u8, 127, 128, 254, 255] {
            let rgba = vec![val; 16 * 4];
            let mut rgb_simd = vec![0u8; 16 * 3];
            rgba8_to_rgb8(&rgba, &mut rgb_simd);
            assert!(rgb_simd.iter().all(|&b| b == val), "4→3 flat {val}");
            let rgb = vec![val; 16 * 3];
            let mut rgba_simd = vec![0u8; 16 * 4];
            rgb8_to_rgba8(&rgb, &mut rgba_simd);
            assert!(
                rgba_simd
                    .chunks_exact(4)
                    .enumerate()
                    .all(|(_, px)| px[0] == val && px[1] == val && px[2] == val && px[3] == 255),
                "3→4 flat {val}"
            );
        }
        eprintln!("RGBA_RGB_REPACK_PARITY_OK");
    }

    /// P2（B3 评审残留→B5 已修）：`idct_add_two_sse2` 双指针核的对抗随机对拍——满幅随机 dc/ac、
    /// 跨 0/±4095/±4096/±4097 边界、多种子（static override 互踩由调用方 `--test-threads=1` 隔离）。
    /// 该测试曾因第二遍 i16 回绕偏出（2026-11-01 定位），B5 已改 i32 中间量修复并翻绿；
    /// 若再偏出，FAIL 并将 seed/trial/系数打屏定位。
    #[test]
    fn idct_add_two_full_scale_random_parity() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP idct_two (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true)); // 强制 SSE2 双指针核
        let boundaries: [i16; 16] = [
            0, 1, -1, 4095, -4095, 4096, -4096, 4097, -4097, 2047, -2048, 8192, -8192, 32767, -32768, 16384,
        ];
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for seed in [
            0x1234_5678_9abc_def0u64,
            0x0bad_cafe_f00d_1111,
            0xfeed_f00d_abcd_2222,
            0xdead_beef_1234_3333,
        ] {
            let mut rng = Lcg(seed);
            for trial in 0..300 {
                let mut ca = [0i16; 16];
                let mut cb = [0i16; 16];
                for k in 0..16 {
                    // 满幅均匀随机（全 i16 值域），偶发注入跨 0/±4096 边界值。
                    ca[k] = if rng.next() % 7 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        (rng.next() as u16) as i16
                    };
                    cb[k] = if rng.next() % 7 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        (rng.next() as u16) as i16
                    };
                }
                for px in plane_simd.iter_mut() {
                    *px = rng.byte();
                }
                let (row0, col0) = (8 + (trial % 9), 8 + (trial % 9));
                plane_scalar.copy_from_slice(&plane_simd);
                super::inverse_dct_add_two(&ca, &cb, &mut plane_simd, pitch, row0, col0);
                super::super::vp8::inverse_dct_add(&ca, &mut plane_scalar, pitch, row0, col0, true);
                super::super::vp8::inverse_dct_add(&cb, &mut plane_scalar, pitch, row0, col0 + 4, true);
                assert_eq!(
                    plane_simd, plane_scalar,
                    "idct_two seed=0x{seed:016x} trial={trial} row0={row0} col0={col0} ca={ca:?} cb={cb:?}"
                );
                checked += 1;
            }
        }
        set_simd_part_override("dct", None);
        eprintln!("IDCT_TWO_FULLSCALE_RANDOM_PARITY\t{checked}");
    }
}
