//! 平台相关的应用目录解析（运行期路径平台化，B2 切片）。
//!
//! 单点提供「应用配置目录」，供下列消费方以**同一口径**解析（依赖方向：base 为叶子 crate，
//! config 依赖 base，故目录口径放本层、config/editor/shell_core 消费之）：
//! - `config::user::UserPrefs::config_path()`（非 Windows 分支）→ `<配置目录>/config.json`
//! - `base::diag::diag_log_path(filename)` → `<配置目录>/<filename>`
//!
//! Windows 行为与既有实现**逐字节一致**：配置目录 = `%APPDATA%\vuick`。
//! 非 Windows 遵循 XDG Base Directory：`$XDG_CONFIG_HOME/vuick`，未设则
//! `$HOME/.config/vuick`；两者皆缺 → 返回空路径（调用方回落当前目录，保持既有回退语义）。

use std::path::PathBuf;

/// 平台应用配置目录（不含文件名），空路径 = 无可用基目录（调用方回落当前目录）：
/// - Windows：`%APPDATA%\vuick`（既有语义原样）
/// - 非 Windows：XDG —— `$XDG_CONFIG_HOME/vuick` 或 `$HOME/.config/vuick`。
pub fn app_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        match std::env::var_os("APPDATA").map(PathBuf::from) {
            Some(dir) => dir.join("vuick"),
            None => PathBuf::new(),
        }
    }
    #[cfg(not(windows))]
    {
        xdg_config_dir(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
        .unwrap_or_else(PathBuf::new)
    }
}

/// XDG 配置目录纯函数（跨平台可单测；仅非 Windows 生产分支调用，Windows 下 test 引用）：
/// `$XDG_CONFIG_HOME` 设且非空 → `Some(<XDG>/vuick)`；否则 `$HOME` 设且非空 →
/// `Some(<HOME>/.config/vuick)`；两者皆缺 → `None`（调用方回落当前目录）。
#[cfg_attr(windows, allow(dead_code))]
fn xdg_config_dir(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(base) = xdg_config_home
        && !base.is_empty()
    {
        return Some(PathBuf::from(base).join("vuick"));
    }
    if let Some(home) = home
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join(".config").join("vuick"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // 纯路径拼接测试：两侧同一运算，平台无关（Windows 上 "/xdg" 亦为合法前缀路径）。

    #[test]
    fn xdg_prefers_config_home_over_home() {
        assert_eq!(
            xdg_config_dir(Some(std::ffi::OsStr::new("/xdg")), Some(std::ffi::OsStr::new("/home"))),
            Some(PathBuf::from("/xdg").join("vuick"))
        );
    }

    #[test]
    fn xdg_falls_back_to_home_config_when_config_home_unset() {
        assert_eq!(
            xdg_config_dir(None, Some(std::ffi::OsStr::new("/home"))),
            Some(PathBuf::from("/home").join(".config").join("vuick"))
        );
    }

    #[test]
    fn xdg_empty_config_home_treated_as_unset() {
        // 与 `config_path` 既有「空串视同未设」语义一致。
        assert_eq!(
            xdg_config_dir(Some(std::ffi::OsStr::new("")), Some(std::ffi::OsStr::new("/home"))),
            Some(PathBuf::from("/home").join(".config").join("vuick"))
        );
    }

    #[test]
    fn xdg_no_home_falls_back_to_none() {
        assert_eq!(
            xdg_config_dir(None, None),
            None,
            "无 XDG_CONFIG_HOME 与 HOME → None（调用方回落当前目录）"
        );
    }
}
