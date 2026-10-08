//! 用户偏好持久化：主题三态（跟随系统 / 强制亮 / 强制暗）、正文字号、侧边栏宽度。
//!
//! 存储为运行时 JSON 文件（非源码），默认路径：
//! - `%APPDATA%\gpui-editor\config.json`（可用环境变量 `GPUI_EDITOR_CONFIG` 覆盖到指定文件）；
//! - Windows 无 `APPDATA` 时回退到当前目录 `gpui-editor-config.json`。
//!
//! 读写策略：
//! - 只复用已有依赖 `serde_json`，手写 `Value` 解析（不引入 serde derive）；
//! - 文件缺失 / 字段缺省 / JSON 损坏一律回退默认，绝不 panic；
//! - 写入用「临时文件 + rename」原子替换，避免中途断电/杀进程留下半截配置；
//! - `cfg!(test)` 下读写为空操作，避免 `cargo test` 污染真实用户配置。

use std::fs;
use std::path::PathBuf;

use gpui::WindowAppearance;
use serde_json::{json, Value};

/// 主题三态（字符串标识写入 JSON，见 [`ThemePref::as_str`]）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemePref {
    /// 跟随系统深浅色（首启默认；用户手动切亮/暗后退出跟随）。
    #[default]
    Follow,
    /// 强制浅色。
    Light,
    /// 强制深色。
    Dark,
}

impl ThemePref {
    /// 写盘用的紧凑标识。
    pub fn as_str(self) -> &'static str {
        match self {
            ThemePref::Follow => "follow",
            ThemePref::Light => "light",
            ThemePref::Dark => "dark",
        }
    }

    /// 从字符串解析（未知值 / 缺省回退跟随）。
    pub fn from_str(s: &str) -> Self {
        match s {
            "light" => ThemePref::Light,
            "dark" => ThemePref::Dark,
            _ => ThemePref::Follow,
        }
    }

    /// 解析成「有效明暗」：强制档直出；跟随档取当前系统外观。
    pub fn resolved_dark(self, system: WindowAppearance) -> bool {
        match self {
            ThemePref::Dark => true,
            ThemePref::Light => false,
            ThemePref::Follow => matches!(
                system,
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        }
    }
}

/// 用户偏好（所有字段可缺省，`load` 时逐字段回退）。
#[derive(Clone, Copy, Debug, Default)]
pub struct UserPrefs {
    /// 主题三态。
    pub theme: ThemePref,
    /// 正文字号（px）。`None` = 未设置过，用 `layout::DEFAULT_FONT_SIZE`。
    pub font_size: Option<f32>,
    /// 侧边栏宽度（px）。`None` = 从未手动拖拽过，保持随内容自适应。
    pub sidebar_width: Option<f32>,
}

impl UserPrefs {
    /// 配置文件完整路径：环境变量覆盖 → APPDATA → 当前目录回退。
    pub fn config_path() -> PathBuf {
        if let Ok(over) = std::env::var("GPUI_EDITOR_CONFIG") {
            if !over.trim().is_empty() {
                return PathBuf::from(over);
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata).join("gpui-editor").join("config.json");
            }
        }
        PathBuf::from("gpui-editor-config.json")
    }

    /// 加载用户偏好：缺失 / 损坏 / 字段缺省均回退默认。
    pub fn load() -> Self {
        if cfg!(test) {
            return Self::default();
        }
        let Ok(text) = fs::read_to_string(Self::config_path()) else {
            return Self::default();
        };
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        let mut prefs = Self::default();
        if let Some(Value::String(s)) = map.get("theme") {
            prefs.theme = ThemePref::from_str(s);
        }
        if let Some(Value::Number(n)) = map.get("font_size") {
            if let Some(v) = n.as_f64() {
                prefs.font_size = Some(v as f32);
            }
        }
        if let Some(Value::Number(n)) = map.get("sidebar_width") {
            if let Some(v) = n.as_f64() {
                prefs.sidebar_width = Some(v as f32);
            }
        }
        prefs
    }

    /// 保存：临时文件 + rename 原子替换（Windows rename 不覆盖 → 先删旧文件）。
    pub fn save(&self) {
        if cfg!(test) {
            return;
        }
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        let body = json!({
            "theme": self.theme.as_str(),
            "font_size": self.font_size,
            "sidebar_width": self.sidebar_width,
        })
        .to_string();
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, body).is_err() {
            return;
        }
        let _ = fs::remove_file(&path);
        let _ = fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_pref_roundtrip_and_resolve() {
        assert_eq!(ThemePref::from_str("light"), ThemePref::Light);
        assert_eq!(ThemePref::from_str("dark"), ThemePref::Dark);
        assert_eq!(ThemePref::from_str("follow"), ThemePref::Follow);
        assert_eq!(ThemePref::from_str("garbage"), ThemePref::Follow);
        assert_eq!(ThemePref::Light.as_str(), "light");

        assert!(!ThemePref::Light.resolved_dark(WindowAppearance::Dark));
        assert!(ThemePref::Dark.resolved_dark(WindowAppearance::Light));
        assert!(ThemePref::Follow.resolved_dark(WindowAppearance::VibrantDark));
        assert!(!ThemePref::Follow.resolved_dark(WindowAppearance::Light));
    }
}
