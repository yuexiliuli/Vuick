//! 用户偏好持久化：主题三态（跟随系统 / 强制亮 / 强制暗）、正文字号、侧边栏宽度。
//!
//! 存储为运行时 JSON 文件（非源码），默认路径：
//! - `%APPDATA%\vuick\config.json`（可用环境变量 `VUICK_CONFIG` 覆盖到指定文件）；
//! - Windows 无 `APPDATA` 时回退到当前目录 `vuick-config.json`。
//!
//! 读写策略：
//! - 只复用已有依赖 `serde_json`，手写 `Value` 解析（不引入 serde derive）；
//! - 文件缺失 / 字段缺省 / JSON 损坏一律回退默认，绝不 panic；
//! - 写入用「临时文件 + rename」原子替换，避免中途断电/杀进程留下半截配置；
//! - `cfg!(test)` 下读写为空操作，避免 `cargo test` 污染真实用户配置。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock as StdLazyLock, Mutex as StdMutex};
use std::time::SystemTime;

use gpui::WindowAppearance;
use serde_json::{json, Map, Value};

use crate::limits::RECENT_MAX;

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

/// PDF 阅读器画布背景预设（字符串标识写入 JSON，见 [`PdfBgPref::as_str`]）。
/// 只改**画布**（页面四周壁纸色），绝不动 PDF 位图/文件本身。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PdfBgPref {
    /// 跟随主题（默认）：画布取当前主题 `window_bg`，切主题随之变化。
    #[default]
    Default,
    /// 夜间深色（#202020）。
    Night,
    /// 护眼豆沙绿（#C7EDCC）。
    Eye,
    /// 羊皮纸（#FBF0D9）。
    Parchment,
    /// 自定义（颜色取 [`UserPrefs::pdf_bg_custom`] 的 `#rrggbb`；非法值应用层回退默认）。
    Custom,
}

impl PdfBgPref {
    /// 写盘用的紧凑标识。
    pub fn as_str(self) -> &'static str {
        match self {
            PdfBgPref::Default => "default",
            PdfBgPref::Night => "night",
            PdfBgPref::Eye => "eye",
            PdfBgPref::Parchment => "parchment",
            PdfBgPref::Custom => "custom",
        }
    }

    /// 从字符串解析（未知值 / 缺省回退跟随主题）。
    pub fn from_str(s: &str) -> Self {
        match s {
            "night" => PdfBgPref::Night,
            "eye" => PdfBgPref::Eye,
            "parchment" => PdfBgPref::Parchment,
            "custom" => PdfBgPref::Custom,
            _ => PdfBgPref::Default,
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
    /// 「关闭到系统托盘」开关。`None` = 未设置过，默认关（点标题栏 × 直接关闭退出）；
    /// `Some(true)` = 开启（点 × / Alt+F4 隐藏到系统托盘图标，托盘退出才真正退出）。
    pub close_to_tray: Option<bool>,
    /// 「记住阅读进度」开关。`None` = 未设置，默认开（重开文件回到上次阅读位置）；
    /// `Some(false)` = 显式关闭（重开回到顶部）。
    pub restore_reading_progress: Option<bool>,
    /// 「媒体在新窗口打开」开关。`None` = 未设置过，默认开（打开音乐/视频进同进程新窗口
    /// 或复用已有媒体窗口）；`Some(false)` = 显式关闭，保持现行行为（当前窗口打开）。
    pub media_new_window: Option<bool>,
    /// 「复用已有媒体窗口」开关（仅 `media_new_window` 开启时生效）。
    /// `None` = 未设置过，默认开（复用最近的媒体窗口）；`Some(false)` = 每次新开独立窗口。
    pub reuse_media_window: Option<bool>,
    /// 音乐窗口的已记录尺寸（逻辑像素 w×h；[`UserPrefs::music_window_size`]）。
    /// `Some((w, h))` = 用户把专用媒体窗口（音乐模式）resize 后自动记录，下次新开音乐
    /// 窗口按该尺寸（居中）；`None` = 从未记录，新开跟随发起窗口大小。
    /// 非法值（非数组 / 非两个数 / NaN / 负 / 零 / ∞）一律回退 `None`。
    pub music_window_size: Option<(f32, f32)>,
    /// 视频窗口的已记录尺寸（与 [`UserPrefs::music_window_size`] 对称，音乐/视频分开）。
    pub video_window_size: Option<(f32, f32)>,
    /// 打开文件夹的根路径（侧边栏「文件」视图）。跨重启恢复文件树；`None` = 未打开过。
    pub opened_folder: Option<String>,
    /// 最近打开的文件夹路径（新→旧，至多 [`RECENT_MAX`] 条）。
    pub recent_folders: Vec<String>,
    /// 最近打开的文件路径（新→旧，至多 [`RECENT_MAX`] 条）。
    pub recent_files: Vec<String>,
    /// 音乐播放器音乐目录。`None` = 未设置过，读默认 [`crate::limits::DEFAULT_MUSIC_DIR`]。
    pub music_dir: Option<String>,
    /// 音乐收藏（歌曲路径字符串，新→旧）。
    pub music_liked: Vec<String>,
    /// 音乐播放器音量（0..=1）。`None` = 未设置过，用播放器默认 1.0。
    pub music_volume: Option<f32>,
    /// 上次播放/选中的曲目绝对路径（跨重启恢复选中定位；`None` = 从未播放过）。
    pub music_last_track: Option<String>,
    /// 上次播放位置（秒；随 [`UserPrefs::music_last_track`] 一起恢复；`None` = 从头起）。
    pub music_progress_secs: Option<f64>,
    /// PDF 阅读画布背景预设（`default` 跟随主题；显式预设/自定义后主题切换不再覆盖）。
    pub pdf_bg: PdfBgPref,
    /// PDF 自定义画布底色（`#rrggbb`；仅 `pdf_bg == Custom` 时消费；非法值应用层回退默认）。
    pub pdf_bg_custom: Option<String>,
}

/// A3 进程级偏好缓存：键 = 配置路径 + 文件元数据（mtime+len）。`load` 首访读盘并缓存；
/// 后续**同一路径且文件未变**直接克隆缓存（启动 `main.rs` 与 `new_impl` 两处不再重复
/// 读盘解析 JSON；`media_window` 打开动作、`collect_prefs` 持久化口径同享缓存）——
/// 只做一次 `fs::metadata`（微秒级 stat，非内容读）。文件被改写（mtime/len 变化，含
/// 测试内同一路径换内容、外部另存）或路径切换（`VUICK_CONFIG` 变）→ 自动重读，
/// 不串缓存。`save` 写盘成功后同步回写缓存。测试构建（`cfg!(test)`）不触缓存。
static PREFS_CACHE: StdLazyLock<StdMutex<Option<(PathBuf, Option<SystemTime>, u64, UserPrefs)>>> =
    StdLazyLock::new(|| StdMutex::new(None));

/// 配置文件的「改版指纹」：(mtime, len)；缺失 → `(None, 0)`。
fn config_meta(path: &Path) -> (Option<SystemTime>, u64) {
    match std::fs::metadata(path) {
        Ok(m) => (m.modified().ok(), m.len()),
        Err(_) => (None, 0),
    }
}

/// 递归复制目录内容（不含正在写的文件锁占用问题：仅复制后读，源保持只读不删）。
/// 迁移专用：源目录仅作「一次性搬家」，复制失败静默忽略（下次启动重试）。
/// `test-mode`（shell 等消费方测试构建把 config 以该 feature 编译）下迁移不触发、
/// 单测又不编 → 允许 dead_code（生产 + 纯 cfg(test) 均真实使用）。
#[cfg_attr(any(test, feature = "test-mode"), allow(dead_code))]
fn copy_dir(src: &Path, dst: &Path) {
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return,
    };
    if let Err(_) = std::fs::create_dir_all(dst) {
        return;
    }
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to);
        } else if let Err(_) = std::fs::copy(&from, &to) {
            // 单个文件拷贝失败（如被占用/只读）不中断整目录，数据以新路径新建为准。
        }
    }
}

/// 旧目录 → 新目录一次性迁移（判定 + 递归复制；独立于 `cfg` 门控，可单测）。
/// 旧目录不存在或新目录已存在 → 无操作；否则整目录递归复制、保留源。
/// `test-mode` 下同 [`copy_dir`] 一样可能 dead（见其 cfg_attr 注释）。
#[cfg_attr(any(test, feature = "test-mode"), allow(dead_code))]
fn migrate_legacy_dir(old: &Path, new: &Path) {
    if old.is_dir() && !new.exists() {
        copy_dir(old, new);
    }
}

impl UserPrefs {
    /// 配置文件完整路径：环境变量覆盖 → 测试隔离临时目录 → APPDATA → 当前目录回退。
    pub fn config_path() -> PathBuf {
        if let Ok(over) = std::env::var("VUICK_CONFIG")
            && !over.trim().is_empty()
        {
            return PathBuf::from(over);
        }
        // 测试构建隔离（K8 根因根治）。注意：config 作依赖被 shell/music 测试编译时，其
        // 自身 `cfg!(test)` **不生效**（编译期仅对「被测试的 crate」置位）→ 由消费方
        // dev-dependencies 打开 `test-mode` feature 补上，等效于 cfg!(test)。两路都落
        // 「一次性临时目录」，绝不触碰真实 `%APPDATA%`；显式 `VUICK_CONFIG` 恒优先。
        #[cfg(any(test, feature = "test-mode"))]
        {
            return Self::test_config_path();
        }
        if let Ok(appdata) = std::env::var("APPDATA")
            && !appdata.trim().is_empty()
        {
            let appdata_dir = PathBuf::from(appdata);
            Self::migrate_legacy_appdata(&appdata_dir);
            return appdata_dir.join("vuick").join("config.json");
        }
        PathBuf::from("vuick-config.json")
    }

    /// 旧版本目录 `%APPDATA%\gpui-editor` 一次性迁移到新目录 `%APPDATA%\vuick`：
    /// 首次启动检测到旧目录存在且新目录不存在时，整目录递归复制（config.json /
    /// `session/` / `pdf_bookmarks.json` / `reading_progress.json` / 日志等一并带走），
    /// 用户数据零丢失；复制成功后才读新路径，此后旧目录内容不再被消费。仅生产路径
    /// 触发（测试隔离目录不经此函数）；旧目录不存在 / 新目录已存在 → 无操作。
    fn migrate_legacy_appdata(appdata_dir: &Path) {
        #[cfg(any(test, feature = "test-mode"))]
        {
            let _ = appdata_dir;
        }
        #[cfg(not(any(test, feature = "test-mode")))]
        {
            use std::sync::OnceLock;
            static MIGRATED: OnceLock<()> = OnceLock::new();
            let _ = MIGRATED.get_or_init(|| {
                migrate_legacy_dir(&appdata_dir.join("gpui-editor"), &appdata_dir.join("vuick"));
            });
        }
    }

    /// 测试构建隔离目录：`%TEMP%\vuick-test-<进程ID>\config.json`（进程唯一、惰性
    /// 分配一次），`session_dir()` 随之派生到同目录 `session/` 子目录。平行 `cargo test`
    /// 会话/进程互不共享，天然隔离真实 `%APPDATA%`；测试结束不清理（留在 %TEMP%，无碍）。
    #[cfg(any(test, feature = "test-mode"))]
    fn test_config_path() -> PathBuf {
        use std::sync::OnceLock;
        static TEST_CFG_DIR: OnceLock<PathBuf> = OnceLock::new();
        TEST_CFG_DIR
            .get_or_init(|| {
                std::env::temp_dir().join(format!("vuick-test-{}", std::process::id()))
            })
            .join("config.json")
    }

    /// 会话临时文件目录 = 配置文件所在目录下的 `session/` 子目录
    /// （如 `%APPDATA%\vuick\session\`；VUICK_CONFIG 覆盖时随其路径）。
    pub fn session_dir() -> PathBuf {
        match Self::config_path().parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join("session"),
            _ => PathBuf::from("session"),
        }
    }

    /// 从 JSON 对象解析（纯函数，供 `load`、导入设置与测试使用）：字段缺省 / 类型不符 /
    /// 值非法一律回退默认，绝不 panic。
    pub fn from_json_value(v: &Value) -> Self {
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
        if let Some(Value::Bool(b)) = map.get("close_to_tray") {
            prefs.close_to_tray = Some(*b);
        }
        if let Some(Value::Bool(b)) = map.get("restore_reading_progress") {
            prefs.restore_reading_progress = Some(*b);
        }
        if let Some(Value::Bool(b)) = map.get("media_new_window") {
            prefs.media_new_window = Some(*b);
        }
        if let Some(Value::Bool(b)) = map.get("reuse_media_window") {
            prefs.reuse_media_window = Some(*b);
        }
        if let Some(Value::String(s)) = map.get("opened_folder") {
            if !s.is_empty() {
                prefs.opened_folder = Some(s.clone());
            }
        }
        if let Some(Value::String(s)) = map.get("music_dir") {
            if !s.is_empty() {
                prefs.music_dir = Some(s.clone());
            }
        }
        if let Some(Value::Number(n)) = map.get("music_volume")
            && let Some(v) = n.as_f64()
        {
            prefs.music_volume = Some(v as f32);
        }
        if let Some(Value::String(s)) = map.get("music_last_track") {
            if !s.is_empty() {
                prefs.music_last_track = Some(s.clone());
            }
        }
        if let Some(Value::Number(n)) = map.get("music_progress_secs") {
            prefs.music_progress_secs = n.as_f64();
        }
        // PDF 画布背景：`pdf_bg` 字符串标识（未知 / 缺省回退跟随主题）；
        // `pdf_bg_custom` 仅 `#rrggbb` 合法（格式非法 → None，应用层再兜底回退默认）。
        if let Some(Value::String(s)) = map.get("pdf_bg") {
            prefs.pdf_bg = PdfBgPref::from_str(s);
        }
        prefs.pdf_bg_custom = Self::read_hex_color(map, "pdf_bg_custom");
        // 媒体窗口记录尺寸（音乐/视频分开）：数组 `[w, h]`；缺省 / 非数组 / 非两个数 /
        // NaN / 负 / 零 / ∞ 一律回退 None（安全护栏）。
        prefs.music_window_size = Self::read_size_tuple(map, "music_window_size");
        prefs.video_window_size = Self::read_size_tuple(map, "video_window_size");
        // 最近打开（文件夹 / 文件）：读取字符串数组，非数组 / 缺省 → 空；
        // 元素非字符串跳过；截断至 RECENT_MAX（安全护栏，防手改配置超长）。
        prefs.recent_folders = Self::read_string_array(map, "recent_folders");
        prefs.recent_files = Self::read_string_array(map, "recent_files");
        prefs.music_liked = Self::read_string_array(map, "music_liked");
        prefs
    }

    /// 从 JSON 对象里读一个「尺寸元组」字段（`[w, h]`，见 [`UserPrefs::from_json_value`]）：
    /// 非「恰好两个成员的数字数组」、任一非有限/非正 → 回退 `None`（绝不写 NaN/∞/负尺寸）。
    fn read_size_tuple(map: &Map<String, Value>, key: &str) -> Option<(f32, f32)> {
        let Value::Array(arr) = map.get(key)? else {
            return None;
        };
        if arr.len() != 2 {
            return None;
        }
        let w = arr[0].as_f64()? as f32;
        let h = arr[1].as_f64()? as f32;
        if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
            return None;
        }
        Some((w, h))
    }

    /// 从 JSON 对象里读一个「字符串数组」字段（见 [`UserPrefs::from_json_value`]）。
    fn read_string_array(map: &Map<String, Value>, key: &str) -> Vec<String> {
        let Some(Value::Array(arr)) = map.get(key) else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .take(RECENT_MAX)
            .collect()
    }

    /// 从 JSON 对象里读「自定义画布底色」（`#rrggbb`，见 [`UserPrefs::from_json_value`]）：
    /// 非字符串 / 格式非法（非 `#`+6 个十六进制位）→ `None`（安全护栏，防手改配置）。
    fn read_hex_color(map: &Map<String, Value>, key: &str) -> Option<String> {
        let Value::String(s) = map.get(key)? else {
            return None;
        };
        let s = s.trim();
        if Self::is_valid_hex_color(s) {
            Some(s.to_string())
        } else {
            None
        }
    }

    /// `#rrggbb` 格式校验（`r/g/b` 为十六进制位，大小写均可）。
    fn is_valid_hex_color(s: &str) -> bool {
        s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// 序列化成 JSON 对象（纯函数，供 `save`、「导出设置」与测试使用）。
    pub fn to_json_value(&self) -> Value {
        json!({
            "theme": self.theme.as_str(),
            "font_size": self.font_size,
            "sidebar_width": self.sidebar_width,
            "session_restore": self.session_restore,
            "close_to_tray": self.close_to_tray,
            "restore_reading_progress": self.restore_reading_progress,
            "media_new_window": self.media_new_window,
            "reuse_media_window": self.reuse_media_window,
            "music_window_size": self.music_window_size,
            "video_window_size": self.video_window_size,
            "opened_folder": self.opened_folder,
            "recent_folders": self.recent_folders,
            "recent_files": self.recent_files,
            "music_dir": self.music_dir,
            "music_liked": self.music_liked,
            "music_volume": self.music_volume,
            "music_last_track": self.music_last_track,
            "music_progress_secs": self.music_progress_secs,
            "pdf_bg": self.pdf_bg.as_str(),
            "pdf_bg_custom": self.pdf_bg_custom,
        })
    }

    /// 加载用户偏好：缺失 / 损坏 / 字段缺省均回退默认。
    /// **A3 进程级缓存**：见 [`PREFS_CACHE`]——首访读盘一次，后续同一路径未变直接读
    /// 缓存（只 stat 一次，不再重复读盘解析）；文件变化/路径切换自动重读。
    pub fn load() -> Self {
        if cfg!(test) {
            return Self::default();
        }
        let path = Self::config_path();
        let meta = config_meta(&path);
        if let Ok(cache) = PREFS_CACHE.lock()
            && let Some((p, m, len, v)) = cache.as_ref()
            && *p == path
            && *m == meta.0
            && *len == meta.1
        {
            return v.clone();
        }
        let loaded = Self::load_from_disk();
        if let Ok(mut cache) = PREFS_CACHE.lock() {
            *cache = Some((path, meta.0, meta.1, loaded.clone()));
        }
        loaded
    }

    /// 从磁盘读取并解析（`load` 的底层；缺失 / 损坏 / 字段缺省均回退默认）。
    fn load_from_disk() -> Self {
        let Ok(text) = fs::read_to_string(Self::config_path()) else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        Self::from_json_value(&v)
    }

    /// 保存：同目录临时文件 + rename 原子替换（唯一临时名统一收敛到
    /// `base::util::atomic_write`；Windows `std::fs::rename` 可覆盖既有文件，不再先删旧文件）。
    /// 写失败静默回退：保持内存态、不覆盖磁盘，与既有处置一致。
    /// **A3**：写盘成功后同步进程级缓存（缓存镜像盘面；失败不更新，`load` 仍读旧盘面）。
    pub fn save(&self) {
        if cfg!(test) {
            return;
        }
        let path = Self::config_path();
        let body = self.to_json_value().to_string();
        if base::util::atomic_write(&path, body.as_bytes()).is_err() {
            return;
        }
        let meta = config_meta(&path);
        if let Ok(mut cache) = PREFS_CACHE.lock() {
            *cache = Some((path, meta.0, meta.1, self.clone()));
        }
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
            close_to_tray: None,
            restore_reading_progress: None,
            media_new_window: None,
            reuse_media_window: None,
            music_window_size: None,
            video_window_size: None,
            opened_folder: None,
            recent_folders: Vec::new(),
            recent_files: Vec::new(),
            music_dir: None,
            music_liked: Vec::new(),
            music_volume: None,
            music_last_track: None,
            music_progress_secs: None,
            pdf_bg: PdfBgPref::default(),
            pdf_bg_custom: None,
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

    /// 「关闭到系统托盘」开关字段 roundtrip：None = 默认关（不写入、读回 None）；
    /// Some(false/true) 显式保留；缺字段 / 非法值回退默认。走纯函数
    /// （`to_json_value` / `from_json_value`），不触碰真实配置路径。
    #[test]
    fn close_to_tray_json_roundtrip() {
        // None（从未设置）= 默认关：不写入 JSON、读回为 None。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["close_to_tray"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.close_to_tray, None);

        // Some(false/true) 显式持久化并读回（其余字段不受影响）。
        let p = UserPrefs { close_to_tray: Some(false), ..UserPrefs::default() };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(back.close_to_tray, Some(false));
        assert_eq!(back.theme, ThemePref::Follow);
        assert_eq!(p.to_json_value()["close_to_tray"], serde_json::json!(false));
        let p2 = UserPrefs { close_to_tray: Some(true), ..UserPrefs::default() };
        assert_eq!(
            UserPrefs::from_json_value(&p2.to_json_value()).close_to_tray,
            Some(true)
        );

        // 非法值（非布尔）→ 回退 None。
        let v = serde_json::json!({ "close_to_tray": "yes" });
        assert_eq!(UserPrefs::from_json_value(&v).close_to_tray, None);
        // 非对象 → 全默认。
        let back = UserPrefs::from_json_value(&Value::Null);
        assert_eq!(back.close_to_tray, None);
    }

    /// 「记住阅读进度」开关字段 roundtrip：None = 默认开；Some(false/true) 显式保留；
    /// 缺字段 / 非法值回退默认。走纯函数（`to_json_value` / `from_json_value`），
    /// 不触碰真实配置路径（cfg!(test) 下 load/save 为空操作）。
    #[test]
    fn restore_reading_progress_json_roundtrip() {
        // None（从未设置）= 默认开：不写入 JSON、读回为 None。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["restore_reading_progress"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.restore_reading_progress, None);

        // Some(false/true) 显式持久化并读回。
        let p = UserPrefs { restore_reading_progress: Some(false), ..UserPrefs::default() };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(back.restore_reading_progress, Some(false));
        assert_eq!(p.to_json_value()["restore_reading_progress"], serde_json::json!(false));
        let p2 = UserPrefs { restore_reading_progress: Some(true), ..UserPrefs::default() };
        assert_eq!(
            UserPrefs::from_json_value(&p2.to_json_value()).restore_reading_progress,
            Some(true)
        );

        // 非法值（非布尔）→ 回退 None。
        let v = serde_json::json!({ "restore_reading_progress": "yes" });
        assert_eq!(UserPrefs::from_json_value(&v).restore_reading_progress, None);
        // 非对象 → 全默认。
        let back = UserPrefs::from_json_value(&Value::Null);
        assert_eq!(back.restore_reading_progress, None);
    }

    /// 「媒体打开位置」两个开关字段 roundtrip：media_new_window / reuse_media_window 均
    /// None = 默认开（不写入、读回 None）；Some(false/true) 显式保留；缺字段 / 非法值回退
    /// 默认。走纯函数（`to_json_value` / `from_json_value`），不触碰真实配置路径。
    #[test]
    fn media_window_prefs_json_roundtrip() {
        // None（从未设置）= 默认开：不写入 JSON、读回 None。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["media_new_window"], Value::Null);
        assert_eq!(v["reuse_media_window"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.media_new_window, None);
        assert_eq!(back.reuse_media_window, None);

        // Some(false/true) 显式持久化并读回（其余字段不受影响）。
        let p = UserPrefs {
            media_new_window: Some(false),
            reuse_media_window: Some(true),
            ..UserPrefs::default()
        };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(back.media_new_window, Some(false));
        assert_eq!(back.reuse_media_window, Some(true));
        assert_eq!(
            p.to_json_value()["media_new_window"],
            serde_json::json!(false)
        );
        assert_eq!(
            p.to_json_value()["reuse_media_window"],
            serde_json::json!(true)
        );
        let p2 = UserPrefs {
            media_new_window: Some(true),
            reuse_media_window: Some(false),
            ..UserPrefs::default()
        };
        let back2 = UserPrefs::from_json_value(&p2.to_json_value());
        assert_eq!(back2.media_new_window, Some(true));
        assert_eq!(back2.reuse_media_window, Some(false));

        // 非法值（非布尔）→ 回退默认（None）。
        let v = serde_json::json!({ "media_new_window": "yes" });
        assert_eq!(UserPrefs::from_json_value(&v).media_new_window, None);
        let v = serde_json::json!({ "reuse_media_window": 42 });
        assert_eq!(UserPrefs::from_json_value(&v).reuse_media_window, None);
        // 非对象 → 全默认。
        let back = UserPrefs::from_json_value(&Value::Null);
        assert_eq!(back.media_new_window, None);
        assert_eq!(back.reuse_media_window, None);
    }

    /// 「媒体窗口记录尺寸」两字段 roundtrip：None（从未记录）→ JSON 为 null、读回 None；
    /// Some((w,h)) 写入 `[w, h]` 并读回；音乐/视频两字段互不影响。非法值（非数组 / 长度
    /// 非 2 / 非数字 / NaN / 负 / 零 / ∞）一律回退 None。
    #[test]
    fn media_window_size_json_roundtrip() {
        // None（从未记录）：null / 读回 None。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["music_window_size"], Value::Null);
        assert_eq!(v["video_window_size"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.music_window_size, None);
        assert_eq!(back.video_window_size, None);

        // Some((w,h))：写入 `[w, h]` 并 roundtrip；两字段独立、互不影响。
        let p = UserPrefs {
            music_window_size: Some((1280.0, 720.0)),
            ..UserPrefs::default()
        };
        assert_eq!(
            p.to_json_value()["music_window_size"],
            serde_json::json!([1280.0, 720.0])
        );
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(back.music_window_size, Some((1280.0, 720.0)));
        assert_eq!(back.video_window_size, None, "音乐/视频字段互不影响");
        let p2 = UserPrefs {
            video_window_size: Some((960.0, 540.0)),
            ..UserPrefs::default()
        };
        let back2 = UserPrefs::from_json_value(&p2.to_json_value());
        assert_eq!(back2.video_window_size, Some((960.0, 540.0)));
        assert_eq!(back2.music_window_size, None, "音乐/视频字段互不影响");
        // 两字段同时写齐全都能读回。
        let p3 = UserPrefs {
            music_window_size: Some((800.0, 600.0)),
            video_window_size: Some((1024.0, 768.0)),
            ..UserPrefs::default()
        };
        let back3 = UserPrefs::from_json_value(&p3.to_json_value());
        assert_eq!(back3.music_window_size, Some((800.0, 600.0)));
        assert_eq!(back3.video_window_size, Some((1024.0, 768.0)));

        // 非法值 → 回退 None：非数组 / 长度非 2 / 成员非数字 / 负 / 零 / 缺字段。
        for bad in [
            serde_json::json!({ "music_window_size": "1280x720" }),
            serde_json::json!({ "music_window_size": [1280.0, 720.0, 1.0] }),
            serde_json::json!({ "music_window_size": [1280.0] }),
            serde_json::json!({ "music_window_size": [1280.0, "720"] }),
            serde_json::json!({ "music_window_size": [-1280.0, 720.0] }),
            serde_json::json!({ "music_window_size": [0.0, 720.0] }),
            serde_json::json!({ "video_window_size": [1280.0] }),
        ] {
            assert_eq!(
                UserPrefs::from_json_value(&bad).music_window_size,
                None,
                "非法尺寸应回退 None：{bad}"
            );
            assert_eq!(
                UserPrefs::from_json_value(&bad).video_window_size,
                None,
                "视频尺寸非法同样回退 None：{bad}"
            );
        }
        let back = UserPrefs::from_json_value(&Value::Null);
        assert_eq!(back.music_window_size, None);
        assert_eq!(back.video_window_size, None);
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

    /// 「打开最近」两类数组字段 roundtrip：空 Vec → `[]`、非空读回一致；
    /// 超过 [`RECENT_MAX`] 条截断到 50（安全护栏）；非数组 / 缺字段 → 空。
    #[test]
    fn recent_arrays_json_roundtrip() {
        // 缺省 → 空数组。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["recent_folders"], serde_json::json!([]));
        assert_eq!(v["recent_files"], serde_json::json!([]));
        let back = UserPrefs::from_json_value(&v);
        assert!(back.recent_folders.is_empty());
        assert!(back.recent_files.is_empty());

        // 非空数组 roundtrip（顺序保持新→旧）。
        let p = UserPrefs {
            recent_folders: vec![
                "C:\\Data\\docs".to_string(),
                "C:\\Data\\src".to_string(),
            ],
            recent_files: vec!["C:\\Data\\docs\\a.md".to_string()],
            ..UserPrefs::default()
        };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(
            back.recent_folders,
            vec!["C:\\Data\\docs".to_string(), "C:\\Data\\src".to_string()],
            "最近文件夹应 roundtrip 且保持顺序"
        );
        assert_eq!(back.recent_files, vec!["C:\\Data\\docs\\a.md".to_string()]);
        assert_eq!(back.theme, ThemePref::Follow, "其余字段不受影响");

        // 超 50 条 → 读回截断到 50（安全护栏）。
        let long: Vec<String> = (0..80).map(|i| format!("C:\\Data\\f{i}")).collect();
        let v = serde_json::json!({
            "recent_files": long,
        });
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.recent_files.len(), RECENT_MAX, "超上限应截断到 50");
        assert_eq!(back.recent_files[0], "C:\\Data\\f0", "截断保留前 50 条");

        // 非法值：非数组（对象 / 字符串 / 数字）与缺字段 → 空。
        for bad in [
            serde_json::json!({ "recent_files": { "a": 1 } }),
            serde_json::json!({ "recent_folders": "x" }),
            serde_json::json!({ "recent_folders": 42 }),
        ] {
            let back = UserPrefs::from_json_value(&bad);
            assert!(back.recent_folders.is_empty(), "非法值应回退空：{bad}");
            assert!(back.recent_files.is_empty(), "非法值应回退空：{bad}");
        }
        let back = UserPrefs::from_json_value(&Value::Null);
        assert!(back.recent_folders.is_empty());
        assert!(back.recent_files.is_empty());

        // 数组内含非字符串元素 → 跳过该元素。
        let v = serde_json::json!({ "recent_files": ["a.txt", 42, null, "b.txt"] });
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(
            back.recent_files,
            vec!["a.txt".to_string(), "b.txt".to_string()],
            "非字符串元素应跳过"
        );
    }

    /// 音乐播放器字段 roundtrip：music_dir（None = 未设置 / 空串回退 None；Some 写入并
    /// 读回）、music_liked（字符串数组，非数组/缺省 → 空）、music_volume / music_last_track /
    /// music_progress_secs（None = 未设置；Some 写入并读回；非法值回退 None）。走纯函数，
    /// 不触碰真实配置。
    #[test]
    fn music_prefs_json_roundtrip() {
        // None / 缺省：JSON 值为 null、读回 None；music_liked 空数组。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["music_dir"], Value::Null);
        assert_eq!(v["music_liked"], serde_json::json!([]));
        assert_eq!(v["music_volume"], Value::Null);
        assert_eq!(v["music_last_track"], Value::Null);
        assert_eq!(v["music_progress_secs"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.music_dir, None);
        assert!(back.music_liked.is_empty());
        assert_eq!(back.music_volume, None);
        assert_eq!(back.music_last_track, None);
        assert_eq!(back.music_progress_secs, None);

        // Some(路径) 与收藏 roundtrip（其余字段不受影响）。
        let p = UserPrefs {
            music_dir: Some("D:\\music\\music".to_string()),
            music_liked: vec![
                "D:\\music\\music\\周杰伦 - 晴天.ogg".to_string(),
                "D:\\music\\music\\林俊杰 - 江南.mp3".to_string(),
            ],
            music_volume: Some(0.42),
            music_last_track: Some("D:\\music\\music\\林俊杰 - 江南.mp3".to_string()),
            music_progress_secs: Some(123.25),
            ..UserPrefs::default()
        };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(back.music_dir.as_deref(), Some("D:\\music\\music"));
        assert_eq!(
            back.music_liked,
            p.music_liked,
            "音乐收藏应 roundtrip 且保持顺序"
        );
        assert_eq!(back.music_volume, Some(0.42), "音量应 roundtrip");
        assert_eq!(
            back.music_last_track.as_deref(),
            Some("D:\\music\\music\\林俊杰 - 江南.mp3"),
            "上次曲目应 roundtrip"
        );
        assert_eq!(back.music_progress_secs, Some(123.25), "播放进度应 roundtrip");
        assert_eq!(back.theme, ThemePref::Follow, "其余字段不受影响");

        // 非法值：music_dir 空串 / 非字符串 → None；music_liked 非数组 → 空；
        // music_volume 非数字 → None（数字原样存，越界由播放器 set_volume clamp）；
        // music_last_track 空串 / 非字符串 → None；music_progress_secs 非数字 → None。
        let v = serde_json::json!({ "music_dir": "" });
        assert_eq!(UserPrefs::from_json_value(&v).music_dir, None);
        let v = serde_json::json!({ "music_dir": 42 });
        assert_eq!(UserPrefs::from_json_value(&v).music_dir, None);
        let v = serde_json::json!({ "music_liked": "x" });
        assert!(UserPrefs::from_json_value(&v).music_liked.is_empty());
        let v = serde_json::json!({ "music_volume": "loud" });
        assert_eq!(UserPrefs::from_json_value(&v).music_volume, None);
        let v = serde_json::json!({ "music_volume": 5.0 });
        assert_eq!(
            UserPrefs::from_json_value(&v).music_volume,
            Some(5.0),
            "音量越界数字仍原样存（player 侧 clamp）"
        );
        let v = serde_json::json!({ "music_last_track": "" });
        assert_eq!(UserPrefs::from_json_value(&v).music_last_track, None);
        let v = serde_json::json!({ "music_last_track": 42 });
        assert_eq!(UserPrefs::from_json_value(&v).music_last_track, None);
        let v = serde_json::json!({ "music_progress_secs": "mid" });
        assert_eq!(UserPrefs::from_json_value(&v).music_progress_secs, None);
        let back = UserPrefs::from_json_value(&Value::Null);
        assert_eq!(back.music_dir, None);
        assert!(back.music_liked.is_empty());
    }

    /// PDF 画布背景两字段 roundtrip：pdf_bg（default 缺省；预设/自定义 roundtrip 保留，
    /// 未知值回退 default）+ pdf_bg_custom（`#rrggbb` 保留、大小写均可；格式非法 /
    /// 非字符串 / 缺省 → None）。走纯函数，不触碰真实配置路径。
    #[test]
    fn pdf_bg_json_roundtrip() {
        // 缺省：pdf_bg = default、pdf_bg_custom = null；读回默认。
        let v = UserPrefs::default().to_json_value();
        assert_eq!(v["pdf_bg"], serde_json::json!("default"));
        assert_eq!(v["pdf_bg_custom"], Value::Null);
        let back = UserPrefs::from_json_value(&v);
        assert_eq!(back.pdf_bg, PdfBgPref::Default);
        assert_eq!(back.pdf_bg_custom, None);

        // 预设 / 自定义 roundtrip（其余字段不受影响）。
        for (s, pref) in [
            ("night", PdfBgPref::Night),
            ("eye", PdfBgPref::Eye),
            ("parchment", PdfBgPref::Parchment),
            ("custom", PdfBgPref::Custom),
        ] {
            let p = UserPrefs { pdf_bg: pref, ..UserPrefs::default() };
            assert_eq!(p.to_json_value()["pdf_bg"], serde_json::json!(s));
            let back = UserPrefs::from_json_value(&p.to_json_value());
            assert_eq!(back.pdf_bg, pref, "pdf_bg={s} 应 roundtrip");
            assert_eq!(back.theme, ThemePref::Follow, "其余字段不受影响");
        }
        let p = UserPrefs {
            pdf_bg: PdfBgPref::Custom,
            pdf_bg_custom: Some("#1a2B3c".to_string()),
            ..UserPrefs::default()
        };
        let back = UserPrefs::from_json_value(&p.to_json_value());
        assert_eq!(back.pdf_bg, PdfBgPref::Custom);
        assert_eq!(back.pdf_bg_custom.as_deref(), Some("#1a2B3c"), "自定义色应原样保留（大小写均可）");
        assert_eq!(back.music_volume, None, "其余字段不受影响");

        // 未知 / 非字符串 pdf_bg → 回退 default。
        assert_eq!(PdfBgPref::from_str("garbage"), PdfBgPref::Default);
        assert_eq!(PdfBgPref::from_str(""), PdfBgPref::Default);
        let v = serde_json::json!({ "pdf_bg": 42 });
        assert_eq!(UserPrefs::from_json_value(&v).pdf_bg, PdfBgPref::Default);

        // pdf_bg_custom：格式非法（长度/前缀/非法位/非字符串/缺省）一律 None（安全护栏）。
        for bad in [
            serde_json::json!({ "pdf_bg_custom": "#12345" }),
            serde_json::json!({ "pdf_bg_custom": "#1234567" }),
            serde_json::json!({ "pdf_bg_custom": "123456" }),
            serde_json::json!({ "pdf_bg_custom": "#zzzzzz" }),
            serde_json::json!({ "pdf_bg_custom": "#12 34 56" }),
            serde_json::json!({ "pdf_bg_custom": 0x123456 }),
            serde_json::json!({ "pdf_bg_custom": "" }),
        ] {
            assert_eq!(
                UserPrefs::from_json_value(&bad).pdf_bg_custom,
                None,
                "非法 hex 应回退 None：{bad}"
            );
        }
        // 合法小写与带前后空白（trim 后合法）→ 保留。
        let v = serde_json::json!({ "pdf_bg_custom": "#abcdef" });
        assert_eq!(
            UserPrefs::from_json_value(&v).pdf_bg_custom.as_deref(),
            Some("#abcdef")
        );
        let v = serde_json::json!({ "pdf_bg_custom": "  #ABCDEF " });
        assert_eq!(
            UserPrefs::from_json_value(&v).pdf_bg_custom.as_deref(),
            Some("#ABCDEF"),
            "前后空白应 trim 后校验、去空白存储"
        );
    }

    /// 旧目录 → 新目录迁移：旧存在且新不存在 → 整目录递归复制（含子目录与文件）、
    /// 源保留；旧不存在 → 无操作；新已存在 → 不覆盖。
    #[test]
    fn migrate_legacy_dir_copies_when_old_exists_and_new_missing() {
        let base = std::env::temp_dir().join(format!("vuick-migrate-test-{}", std::process::id()));
        let old = base.join("gpui-editor");
        let new = base.join("vuick");
        let _ = std::fs::remove_dir_all(&base);

        // 旧目录存在、新不存在 → 复制（config.json + session/ 子目录 + 嵌套文件）。
        std::fs::create_dir_all(old.join("session")).unwrap();
        std::fs::write(old.join("config.json"), r#"{"theme":"dark"}"#).unwrap();
        std::fs::write(old.join("session").join("t0.md"), "正文").unwrap();
        migrate_legacy_dir(&old, &new);
        assert!(new.join("config.json").is_file(), "config.json 应迁移");
        assert!(new.join("session").join("t0.md").is_file(), "session 子目录应迁移");
        assert_eq!(
            std::fs::read_to_string(new.join("config.json")).unwrap(),
            r#"{"theme":"dark"}"#,
            "内容应逐字一致"
        );
        assert!(old.join("config.json").is_file(), "源目录应保留（不删不搬）");

        // 新已存在 → 无操作（不覆盖改后的内容）。
        let new_overwrite = base.join("vuick-exists");
        std::fs::create_dir_all(&new_overwrite).unwrap();
        std::fs::write(new_overwrite.join("config.json"), "新内容").unwrap();
        migrate_legacy_dir(&base.join("ghost"), &new_overwrite);
        assert_eq!(
            std::fs::read_to_string(new_overwrite.join("config.json")).unwrap(),
            "新内容",
            "新目录已存在时不得覆盖"
        );

        // 旧不存在 → 无操作、不建新目录。
        let ghost_new = base.join("vuick-ghost");
        migrate_legacy_dir(&base.join("ghost-dir"), &ghost_new);
        assert!(!ghost_new.exists(), "旧目录不存在时不得创建新目录");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// copy_dir 空目录 / 单文件 / 嵌套三层递归路径完整性。
    #[test]
    fn copy_dir_recurses_nested_trees() {
        let base = std::env::temp_dir().join(format!("vuick-copydir-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("a").join("b")).unwrap();
        std::fs::write(src.join("root.txt"), "r").unwrap();
        std::fs::write(src.join("a").join("x.txt"), "x").unwrap();
        std::fs::write(src.join("a").join("b").join("deep.txt"), "d").unwrap();
        copy_dir(&src, &dst);
        assert_eq!(std::fs::read_to_string(dst.join("a").join("b").join("deep.txt")).unwrap(), "d");
        assert_eq!(std::fs::read_to_string(dst.join("a").join("x.txt")).unwrap(), "x");
        assert_eq!(std::fs::read_to_string(dst.join("root.txt")).unwrap(), "r");
        let _ = std::fs::remove_dir_all(&base);
    }
}
