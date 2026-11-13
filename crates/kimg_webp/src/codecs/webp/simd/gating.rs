/// `KIMG_WEBP_YUV_U16`（默认开，u16-mulhi 变体为生产路径）：`0`/`false`/`off` → 关，
/// 走 i32 定点 AVX2 方案（A/B 对照保留；两个方案逐字节一致，分别对标 C# `ConvertRowAvx2`
/// 与既有 i32 标量源）。首次访问缓存。
#[inline]
pub(crate) fn yuv_u16_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let t = std::env::var("KIMG_WEBP_YUV_U16").unwrap_or_default().trim().to_string();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// `KIMG_WEBP_NO_SIMD=1` 强制关 WebP 全部 SIMD（对标量回退性能对照；量化 SIMD 收益）。
/// 判定收敛到共享模块 `kimg_core::simd::gate`（CPU 特性检测 + env 主闸；env 惰性读一次 +
/// 进程级缓存），语义与迁移前逐位一致。非 x86_64 恒 false 走标量回退。
#[inline]
pub(crate) fn simd_on(feature: &str) -> bool {
    kimg_core::simd::gate("KIMG_WEBP_NO_SIMD", feature)
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
    if part_env_off(part) {
        return false;
    }
    // D2 环路滤波：宏块熔合 SIMD（`lf_filter_macroblock`，字节不变）默认接入生产；
    // `KIMG_WEBP_SIMD_FLT=0` / `KIMG_WEBP_NO_SIMD_FLT=1` / `KIMG_WEBP_NO_SIMD=1` 一票否决回退标量。
    if part == "flt" && !flt_enabled() {
        return false;
    }
    // `KIMG_WEBP_NO_SIMD` 主闸（迁移前 `webp_no_simd()`）已由 `simd_on`（＝
    // `kimg_core::simd::gate("KIMG_WEBP_NO_SIMD", …)`）作最终一票否决，此处无需重复判定。
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

/// 熵分支形开关（RC3）：高偏斜决策点（tokens 的 ZERO p[1] / G5 p[6]，及顺带的
/// CAT8/9/10 p[8..10]）的位读出用「预测分支形」（默认开）还是「cmov 全算」——纯控制流重构、
/// 输出逐字节一致（`branch ≡ cmov` 硬闸对拍），本开关仅为同二进制的 A/B 决策默认值（K55）。
/// `KIMG_WEBP_ENT_BRANCH=0`/`false`/`off` → 强制旧 cmov（A/B 对照侧）；其余/缺省 → 开（分支形）。
/// 与 `ent_bmi_enabled()` 正交：分支形同时作用于 bmi 与 base 两熵变体（共享 tokens 宏体）。
/// `#[cfg(test)]` 可经 `set_ent_branch_override` 进程内翻转（优先于 env，同 renorm_skip 姿势）。
/// 首次访问读一次 env 缓存，之后仅 Relaxed 原子读——热环开销可忽略。
#[inline]
pub(crate) fn ent_branch_enabled() -> bool {
    #[cfg(test)]
    if let Some(v) = ent_branch_override::get() {
        return v;
    }
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_WEBP_ENT_BRANCH").unwrap_or_default();
        let t = v.trim();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// `#[cfg(test)]` 熵分支形进程内覆盖（优先于 env；RC3 同窗 A/B 用，同 `renorm_skip_override` 姿势）。
#[cfg(test)]
mod ent_branch_override {
    use std::sync::atomic::{AtomicI8, Ordering::Relaxed};
    static V: AtomicI8 = AtomicI8::new(-1);
    pub fn set(v: Option<bool>) {
        let val = match v {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        };
        V.store(val, Relaxed);
    }
    pub fn get() -> Option<bool> {
        match V.load(Relaxed) {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn set_ent_branch_override(v: Option<bool>) {
    ent_branch_override::set(v);
}

/// IDCT 第二遍 i16 域开关（K93 双路径）：`KIMG_WEBP_IDCT_I16=0`/`false`/`off` → 强制旧 i32
/// pass2（基线，全输入域与 i32 标量逐字节恒等）；其余/缺省 → i16 域 pass2（与 libwebp
/// `Transform_SSE2` 水平遍逐 op 对齐，可达域 ∥c∥≤2212 内与标量恒等；∥c∥>2212 码流可达时
/// i16 回绕与 i32 相位差——显式接受，见 `idct_pass2_shift16_sse2` 语义注释）。
/// `#[cfg(test)]` 可经 `set_idct_i16_override` 进程内翻转（优先于 env，同 `set_ent_branch_override` 姿势）。
/// 首次访问读一次 env 缓存，之后仅 Relaxed 原子读——热环开销可忽略（同 `ent_branch_enabled`）。
#[inline]
pub(crate) fn idct_i16_enabled() -> bool {
    #[cfg(test)]
    if let Some(v) = idct_i16_override::get() {
        return v;
    }
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_WEBP_IDCT_I16").unwrap_or_default();
        let t = v.trim();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// `#[cfg(test)]` IDCT i16 域进程内覆盖（优先于 env；K93 同窗 A/B 用，同 `ent_branch_override` 姿势）。
#[cfg(test)]
mod idct_i16_override {
    use std::sync::atomic::{AtomicI8, Ordering::Relaxed};
    static V: AtomicI8 = AtomicI8::new(-1);
    pub fn set(v: Option<bool>) {
        let val = match v {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        };
        V.store(val, Relaxed);
    }
    pub fn get() -> Option<bool> {
        match V.load(Relaxed) {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn set_idct_i16_override(v: Option<bool>) {
    idct_i16_override::set(v);
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

/// `KIMG_WEBP_I4_SIMD`（默认开）：i4 段（4×4 B_PRED 子块预测 + IDCT＋add）专用运行时间控
/// （K93 双路径）。`0`/`false`/`off` → 强制 i4 段走标量（`vp8` 的 `predict_subblock`/
/// `inverse_dct_add`，字节恒等基线，供同窗 A/B 与安全回退）；其余/缺省 → 开（i4 仍随
/// `part_simd_on("pred"/"dct")` 既有部件门控取舍，行为取向不变）。只影响 i4 子块段，
/// 不动 i16/UV 逆变换与 16×16/8×8 大块预测。`#[cfg(test)]` 可经 `set_i4_simd_override`
/// 进程内翻转（优先于 env，同 `set_idct_i16_override` 姿势）。
#[inline]
pub(crate) fn i4_simd_enabled() -> bool {
    #[cfg(test)]
    if let Some(v) = i4_simd_override::get() {
        return v;
    }
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_WEBP_I4_SIMD").unwrap_or_default();
        let t = v.trim();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// `#[cfg(test)]` i4 段 SIMD 进程内覆盖（优先于 env；同窗 A/B 与字节恒等对拍用，
/// 同 `idct_i16_override` 姿势）。
#[cfg(test)]
mod i4_simd_override {
    use std::sync::atomic::{AtomicI8, Ordering::Relaxed};
    static V: AtomicI8 = AtomicI8::new(-1);
    pub fn set(v: Option<bool>) {
        let val = match v {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        };
        V.store(val, Relaxed);
    }
    pub fn get() -> Option<bool> {
        match V.load(Relaxed) {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn set_i4_simd_override(v: Option<bool>) {
    i4_simd_override::set(v);
}


/// 每 4×4 模式（2..=9）的 32 字节 shuffle 表：前 16 个索引取 `avg3` 向量、后 16 个取 `avg2`
/// （255 = 该位不取，pshufb 对高位索引输出 0，OR 合并两种取源）。与 C# `SubblockShuffles` 逐字节同表。
pub(crate) const SUBBLOCK_SHUFFLES: [u8; 8 * 32] = [
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
