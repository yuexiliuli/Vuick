//! 统一 SIMD 门控（kimg 各格式 codec 共享）。
//!
//! - [`detect`]：x86_64 运行时 CPU 特性检测；非 x86_64 恒 false → 标量回退。
//! - [`gate`]：`<env_key>=1|true`（不区分大小写）的 env 总闸（惰性读一次 + 进程级缓存）
//!   → 回落到 [`detect`]。
//!
//! 语义与各 codec 迁移前逐位一致（K93：SIMD 门控错 = 可能出错像素/字节），只把
//! 「特性检测 + env 主闸」两段重复实现收敛到本模块；各 crate 保留各自 env 键名
//! （如 WebP 的 `KIMG_WEBP_NO_SIMD`）。零 unsafe（`is_x86_feature_detected` 为 std 宏）。
//! 注释中文。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 运行时 CPU 特性检测（x86_64；非 x86_64 一律 false → 标量回退）。
///
/// 与迁移前 WebP `simd_on` 逐分支等价：`sse2`/`ssse3`/`sse4.1`/`avx2`，外加熵核双路径
/// 的 `bmi2`（RC1 语义安全链 `bmi2 ⇒ bmi1 ⇒ lzcnt`，判定一次）。其余特性名 → false。
#[inline]
pub fn detect(feature: &str) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected;
        match feature {
            "sse2" => is_x86_feature_detected!("sse2"),
            "ssse3" => is_x86_feature_detected!("ssse3"),
            "sse4.1" => is_x86_feature_detected!("sse4.1"),
            "avx2" => is_x86_feature_detected!("avx2"),
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

/// `<env_key>=1|true`（不区分大小写）→ off 的布尔 env 标志；首次访问读一次并进程级缓存，
/// 之后仅锁 + HashMap 读（调用方应在帧/宏块级提升判定，与迁移前 OnceLock+Relaxed 原子同语义）。
///
/// 公开提供纯粹「env 是否强制关」的判定（不掺特性检测），供 `gate` 之外的少数结构复用：
/// 如 PNG 反滤波的「整块关闭」前置门 `!env_off("KIMG_PNG_NO_SIMD")`，避免把 `gate(key, "sse2")`
/// 这类「借 x86_64 恒有 SSE2 当纯 env 闸」的隐式技巧散落在各 codec（K93 语义逐位等价的前提是
/// 每个 codec 的 env 语义与 `env_off` 一致；HEIF 的 `KIMG_HEIF_TRANSFORM_SIMD=0` 反转极性
/// 不适用本判定，见其本地实现）。
pub fn env_off(key: &str) -> bool {
    fn flags() -> &'static Mutex<HashMap<String, bool>> {
        static FLAGS: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
        FLAGS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    let mut g = flags().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(&v) = g.get(key) {
        return v;
    }
    let v = std::env::var(key)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    g.insert(key.to_string(), v);
    v
}

/// env 总闸 → [`detect`]。`env_key` 置为 `1`/`true`（不区分大小写）→ 一律 false（标量回退）；
/// 否则回落 [`detect`]（按 CPU 特性判定）。env 惰性读一次（进程级缓存），各 crate 用各自键名
/// （如 `KIMG_WEBP_NO_SIMD`），进热路径前请提升到帧/宏块级判定。
#[inline]
pub fn gate(env_key: &str, feature: &str) -> bool {
    if env_off(env_key) {
        return false;
    }
    detect(feature)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未知特性名一律 false。
    #[test]
    fn detect_unknown_feature_false() {
        assert!(!detect("bogus_feature"));
    }

    /// x86_64 恒有 SSE2（架构保证）；非 x86_64 全部特性恒 false（标量回退）。
    #[test]
    fn detect_platform_semantics() {
        #[cfg(target_arch = "x86_64")]
        assert!(detect("sse2"));
        #[cfg(not(target_arch = "x86_64"))]
        {
            for f in ["sse2", "ssse3", "sse4.1", "avx2", "bmi2"] {
                assert!(!detect(f));
            }
        }
    }

    /// `<key>=1` → 总闸关闭（即使 CPU 支持该特性）。
    #[test]
    fn gate_env_one_forces_off() {
        let key = "KIMG_CORE_TEST_GATE_OFF_1";
        unsafe { std::env::set_var(key, "1") };
        assert!(!gate(key, "sse2"));
        unsafe { std::env::remove_var(key) };
    }

    /// `<key>=true`（不区分大小写）→ 总闸关闭。
    #[test]
    fn gate_env_true_forces_off() {
        let key = "KIMG_CORE_TEST_GATE_TRUE";
        unsafe { std::env::set_var(key, "true") };
        assert!(!gate(key, "sse2"));
        unsafe { std::env::remove_var(key) };
    }

    /// env 缺省 → gate 与 detect 完全一致。
    #[test]
    fn gate_env_absent_equals_detect() {
        let key = "KIMG_CORE_TEST_GATE_ABSENT";
        unsafe { std::env::remove_var(key) };
        assert_eq!(gate(key, "avx2"), detect("avx2"));
        assert_eq!(gate(key, "ssse3"), detect("ssse3"));
        assert_eq!(gate(key, "bmi2"), detect("bmi2"));
    }

    /// 惰性只读一次：首访后再改 env 不影响已缓存结果（同迁移前 OnceLock 语义）。
    #[test]
    fn gate_env_lazy_cached_once() {
        let key = "KIMG_CORE_TEST_GATE_CACHE";
        unsafe { std::env::remove_var(key) };
        let first = gate(key, "sse2");
        unsafe { std::env::set_var(key, "1") };
        assert_eq!(gate(key, "sse2"), first);
        unsafe { std::env::remove_var(key) };
    }
}
