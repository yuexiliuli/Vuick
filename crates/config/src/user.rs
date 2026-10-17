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
/// 注意：不再派生 `Copy`——`opened_folder` 为 `Option<String>` 非 Copy 类型；
/// 全仓已核对无代码依赖 `UserPrefs` 的复制语义（只有 `load`/构造/按字段读取）。
#[derive(Clone, Debug, Default)]
pub struct UserPrefs {
    /// 主题三态。
    pub theme: ThemePref,
    /// 正文字号（px）。`None` = 未设置过，用 `layout::DEFAULT_FONT_SIZE`。
    pub font_size: Option<f32>,
    /// 侧边栏宽度（px）。`None` = 从未手动拖拽过，保持随内容自适应。
    pub sidebar_width: Option<f32>,
    /// 「恢复上次会话」开关。`None` = 未设置过，默认开启；`Some(false)` = 显式关闭。
    pub session_restore: Option<bool>,
    /// 打开文件夹的根路径（侧边栏「文件」视图）。跨重启恢复文件树；`None` = 未打开过。
    pub opened_folder: Option<String>,
}

impl UserPrefs {
    /// 配置文件完整路径：环境变量覆盖 → APPDATA → 当前目录回退。
    pub fn config_path() -> PathBuf {
        if let Ok(over) = std::env::var("GPUI_EDITOR_CONFIG")
            && !over.trim().is_empty()
        {
            return PathBuf::from(over);
        }
        if let Ok(appdata) = std::env::var("APPDATA")
            && !appdata.trim().is_empty()
        {
            return PathBuf::from(appdata).join("gpui-editor").join("config.json");
        }
        PathBuf::from("gpui-editor-config.json")
    }

    /// 会话临时文件目录 = 配置文件所在目录下的 `session/` 子目录
    /// （如 `%APPDATA%\gpui-editor\session\`；GPUI_EDITOR_CONFIG 覆盖时随其路径）。
    pub fn session_dir() -> PathBuf {
        match Self::config_path().parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join("session"),
            _ => PathBuf::from("session"),
        }
    }

    /// 从 JSON 对象解析（纯函数，供 `load` 与测试使用）：字段缺省 / 类型不符 /
    /// 值非法一律回退默认，绝不 panic。
    fn from_json_value(v: &Value) -> Self {
        let mut prefs = Self::default();
        let Some(map) = v.as_object() else {
            return prefs;
        };
        if let Some(Value::String(s)) = map.get("theme") {
            prefs.theme = ThemePref::from_str(s);
        }
        if let Some(Value::Number(n)) = map.get("font_size")
            && let Some(v) = n.as_f64()
        {
            prefs.font_size = Some(v as f32);
        }
        if let Some(Value::Number(n)) = map.get("sidebar_width")
            && let Some(v) = n.as_f64()
        {
            prefs.sidebar_width = Some(v as f32);
        }
        if let Some(Value::Bool(b)) = map.get("session_restore") {
            prefs.session_restore = Some(*b);
        }
        if let Some(Value::String(s)) = map.get("opened_folder") {
            if !s.is_empty() {
                prefs.opened_folder = Some(s.clone());
            }
        }
        prefs
    }

    /// 序列化成 JSON 对象（纯函数，供 `save` 与测试使用）。
    fn to_json_value(&self) -> Value {
        json!({
            "theme": self.theme.as_str(),
            "font_size": self.font_size,
            "sidebar_width": self.sidebar_width,
            "session_restore": self.session_restore,
            "opened_folder": self.opened_folder,
        })
    }

    /// 加载用户偏好：缺失 / 损坏 / 字段缺省均回退默认。
    pub fn load() -> Self {
        if cfg!(test) {
            return Self::default();
        }
        let Ok(text) = fs::read_to_string(Self::config_path()) else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        Self::from_json_value(&v)
    }

    /// 保存：临时文件 + rename 原子替换（Windows rename 不覆盖 → 先删旧文件）。
    pub fn save(&self) {
        if cfg!(test) {
            return;
        }
        let path = Self::config_path();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::create_dir_all(parent);
        }
        let body = self.to_json_value().to_string();
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

    /// 「恢复上次会话」开关字段 roundtrip：None = 默认开；Some(false/true) 显式保留；
    /// 缺字段 / 非法值回退默认。走纯函数（`to_json_value` / `from_json_value`），
    /// 不触碰真实配置路径（cfg!(test) 下 load/save 为空操作）。
    #[test]
    fn session_restore_json_roundtrip() {
        // None（从未设置）= 默认开：不写入 JSON、读回为 None。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["session_restore"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.session_restore, None);
        assert_eq!(back.theme, ThemePref::Follow);

        // Some(false/true) 显式持久化并读回。
        let p = UserPrefs {
            theme: ThemePref::Dark,
            font_size: Some(15.0),
            sidebar_width: None,
            session_restore: Some(false),
            opened_folder: None,
        };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(back.session_restore, Some(false));
        assert_eq!(back.theme, ThemePref::Dark);
        assert_eq!(back.font_size, Some(15.0));
        let p2 = UserPrefs { session_restore: Some(true), ..UserPrefs::default() };
        assert_eq!(UserPrefs::from_json_value(&p2.to_json_value()).session_restore, Some(true));

        // 非法值（非布尔）→ 回退 None。
        let v = serde_json::json!({ "session_restore": "yes" });
        assert_eq!(UserPrefs::from_json_value(&v).session_restore, None);
        // 非对象 → 全默认。
        let back = UserPrefs::from_json_value(&Value::Null);
        assert_eq!(back.session_restore, None);
    }

    /// 「打开文件夹」根路径 roundtrip：None = 未打开（JSON 值为 null、读回 None）；
    /// Some(路径) 写入并读回；空串 / 非字符串 / 缺字段一律回退 None。
    #[test]
    fn opened_folder_json_roundtrip() {
        // None：不写入有效路径（null）、读回 None。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["opened_folder"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.opened_folder, None);

        // Some(路径)：显式持久化并读回（连同用 `..UserPrefs::default()` 的结构体更新）。
        let p = UserPrefs {
            opened_folder: Some("C:\\Data\\docs".to_string()),
            ..UserPrefs::default()
        };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(
            back.opened_folder.as_deref(),
            Some("C:\\Data\\docs"),
            "打开文件夹路径应 roundtrip"
        );
        assert_eq!(back.theme, ThemePref::Follow, "其余字段不受影响");

        // 非法值：空串（读回 None）、非字符串（回退 None）、缺字段（None）。
        let v = serde_json::json!({ "opened_folder": "" });
        assert_eq!(UserPrefs::from_json_value(&v).opened_folder, None);
        let v = serde_json::json!({ "opened_folder": 42 });
        assert_eq!(UserPrefs::from_json_value(&v).opened_folder, None);
        let back = UserPrefs::from_json_value(&Value::Null);
        assert_eq!(back.opened_folder, None);
    }
}
