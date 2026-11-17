//! 进程级 env 门控诊断开关 / 测试构建判定 / 日志路径（各 crate 手写 `static ON: OnceLock<bool>`
//! 探针的**收敛站点**，Pattern 7）。零生产语义：仅显式设 env 才开启；关闭 = 一次缓存查即返回，
//! 零分配零 IO。
//!
//! Pattern 7 收敛站点：`tab_diag` / `editor::diag` / `shell_core::rp_diag` / `image_viewer::diag`。
//! （`shell_preview::wheel_probe` / `pdf::view` 的 `VUICK_PDF_TIMING` + `VUICK_DIAG` /
//! `video::seek_diag` 判断谓词 ≠ 本模块严格「`"1"`」口径——`"1"`||`"true"` / 存在即开，未收敛。）

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// 进程级 env「`XXX=1` 布尔开关」缓存（按环境变量名各存一份；首个进程内查询读一次 env 即冻结）。
static CACHE: OnceLock<Mutex<HashMap<&'static str, bool>>> = OnceLock::new();

/// 进程级 env 布尔开关（按名字缓存；默认只认 `"1"`，其余值 / 缺失 → 关）。
///
/// 与原各 crate 手写 `static ON: OnceLock<bool>` 语义一致：**首读冻结**——首个进程内对该名字
/// 的查询读 env 一次并缓存，此后同名查询不再触 env。并发安全：`Mutex<HashMap>` 内建一次缓存。
pub fn env_flag(name: &'static str) -> bool {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap_or_else(|p| p.into_inner());
    *map.entry(name)
        .or_insert_with(|| matches!(std::env::var(name).as_deref(), Ok("1")))
}

/// 测试构建判定（与原各站点同口径：`cfg!(test)` 或 env `VUICK_TEST=1`）。
pub fn is_test_build() -> bool {
    cfg!(test) || matches!(std::env::var("VUICK_TEST").as_deref(), Ok("1"))
}

/// 诊断日志路径：测试构建 → `%TEMP%\{filename}`；生产 → `%APPDATA%\vuick\{filename}`。
/// 与原 `tab_diag::log_path` / `editor::diag::log_path` 逐字等价（文件名换参）。
pub fn diag_log_path(filename: &str) -> PathBuf {
    if is_test_build() {
        return std::env::temp_dir().join(filename);
    }
    match std::env::var_os("APPDATA").map(PathBuf::from) {
        Some(base) => base.join("vuick").join(filename),
        _ => PathBuf::from(filename),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // env 为进程全局；本批测试串行跑（--test-threads=1），各用例用独立名字避免冻结串扰。

    #[test]
    fn env_flag_default_false() {
        // SAFETY: 测试独占进程；该名字仅本用例使用，无并发读者。
        unsafe { std::env::remove_var("P7_DIAG_UNSET") };
        assert!(!env_flag("P7_DIAG_UNSET"));
    }

    #[test]
    fn env_flag_1_enables_other_values_disable() {
        // SAFETY: 测试独占进程；两名字仅本用例使用，无并发读者。
        unsafe {
            std::env::set_var("P7_DIAG_ONE", "1");
            std::env::set_var("P7_DIAG_TWO", "2");
        }
        assert!(env_flag("P7_DIAG_ONE"));
        assert!(!env_flag("P7_DIAG_TWO"));
    }

    #[test]
    fn env_flag_freezes_on_first_read() {
        // SAFETY: 测试独占进程；该名字仅本用例使用，无并发读者。
        unsafe { std::env::set_var("P7_DIAG_FREEZE", "1") };
        assert!(env_flag("P7_DIAG_FREEZE"));
        // 首读冻结：后续再改 env 不影响已缓存结果。
        unsafe { std::env::remove_var("P7_DIAG_FREEZE") };
        assert!(env_flag("P7_DIAG_FREEZE"));
    }

    #[test]
    fn is_test_build_true_in_cfg_test() {
        assert!(is_test_build());
    }

    #[test]
    fn diag_log_path_test_build_under_temp() {
        // 测试构建下 `%APPDATA%` 分支不可达，`%TEMP%` 分支恒生效。
        let p = diag_log_path("x.log");
        assert!(
            p.starts_with(std::env::temp_dir()),
            "测试构建下日志必须以 %TEMP% 为前缀，实际 path={p:?}"
        );
    }
}
