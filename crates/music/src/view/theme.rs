//! 音乐播放器 UI 深紫子主题：调色板 + 常用样式构造助手（自含于本 crate）。
//!
//! 调色板十六进制值**逐字取自** UI 原型 `prototype-orbitrum-player-v1.html` 的
//! `:root` 变量（`--bg-deep` / `--accent` / `--border` …）。GPUI 没有 CSS：
//! - 颜色统一由 [`c()`]/[`c_a()`] 转成运行期 [`Hsla`] 供 `.bg()`/`.text_color()` 消费；
//! - 渐变封面不存在 CSS 渐变，用 `canvas` + `window.paint_quad` + [`linear_gradient`]
//!   自绘（见 [`gradient_cover`]），[`grad_css`] 仅产出原型同款 CSS 字符串供测试/日志。
//!
//! 所有视图只从本模块取色，禁止散落裸 hex。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{LazyLock, RwLock};

use gpui::{
    AnyElement, App, Bounds, Hsla, IntoElement, Pixels, Rgba, Styled as _, canvas, fill,
    linear_color_stop, linear_gradient, px,
};

// ---- 原型 `:root` 调色板常量（逐字，十六进制） ----
/// 主背景（极深紫黑）：`--bg-deep: #0d0b1a`
pub const BG_DEEP: u32 = 0x0d0b1a;
/// 侧栏/右栏/底栏底色：`--bg-primary: #13112a`
pub const BG_PRIMARY: u32 = 0x13112a;
/// 次级表面（搜索框/tab 条/按钮）：`--bg-secondary: #1a1738`
pub const BG_SECONDARY: u32 = 0x1a1738;
/// 卡片/行 hover 底：`--bg-card: #201d40`
pub const BG_CARD: u32 = 0x201d40;
/// 更深一档 hover：`--bg-hover: #282452`
pub const BG_HOVER: u32 = 0x282452;
/// 主题强调色（主按钮/进度条）：`--accent: #7c5cfc`
pub const ACCENT: u32 = 0x7c5cfc;
/// 强调亮色（active 文字/徽标）：`--accent-light: #a78bfa`
pub const ACCENT_LIGHT: u32 = 0xa78bfa;
/// 主文字：`--text-primary: #f0edff`
pub const TEXT: u32 = 0xf0edff;
/// 次要文字：`--text-secondary: #9892b8`
pub const TEXT2: u32 = 0x9892b8;
/// 三级/弱化文字：`--text-muted: #5e5880`
pub const TEXT3: u32 = 0x5e5880;
/// 分隔线/边框：`--border: rgba(255,255,255,0.06)`（0.06×255≈15=0x0f，RRGGBBAA 编码）
pub const BORDER: u32 = 0xffffff0f;
/// 粉色（收藏/渐变/强调）：`--pink: #f472b6`
pub const PINK: u32 = 0xf472b6;
/// 绿色（在线点/状态）：`--green: #34d399`
pub const GREEN: u32 = 0x34d399;
/// 橙色（歌单点等）：`--orange: #fb923c`
pub const ORANGE: u32 = 0xfb923c;
/// 蓝色（歌单点等）：`--blue: #60a5fa`
pub const BLUE: u32 = 0x60a5fa;

/// 十六进制 RGB → 运行期 [`Hsla`]（GPUI 无编译期 hex→HSL，运行时转换即可）。
pub fn c(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

/// 十六进制 RGBA（RRGGBBAA）→ 运行期 [`Hsla`]。
pub fn c_a(hex: u32) -> Hsla {
    gpui::rgba(hex).into()
}

// ==================== 语义调色板：按当前音乐皮肤派生（阶段③-1） ====================
//
// 重构：音乐窗口配色不再写死在固定深紫常量，而是由「当前音乐皮肤（music_skin）」派生。
// 皮肤 = `ui::theme::PRESETS` 的人格令牌（accent/accent_light/srf_* 坡/srf_border/状态色），
// `MusicPalette`（纯数据，**一次构建**、渲染只读）承载解析结果。
// - **base = 音乐原型常量**（深紫基调 + text/pink/… 原文值）：PRESETS 只列人格令牌，
//   **不列** fg/fg_muted/fg_disabled/favorite/success/warning/info/overlay_white——
//   对预设皮肤这些字段保持音乐原值（与重构前逐字节一致）；仅 `"follow"` 分支从全局主题取。
// - **"follow"**：跟随全局 `ui::Theme`（明暗随全局 mode，深浅档皆成立）。
// - **未知/缺省 id → music 预设**（现状逐字节）；零配置（未 `set`）同样 = music 预设。
// - 性能：`MusicSkin::set` 在启动/换肤事件**一次**构建 `MusicPalette`，渲染期无参函数
//   只读缓存字段（不查 PRESETS、不重排）。`danger_close_*`/`glass` 带 alpha、不进全局，
//   仍为音乐局部常量（评审 P0-2）。

/// 音乐语义调色板（纯数据，全 [`Hsla`]；一次构建、渲染只读）。
///
/// 字段 = 现行全部可派生色；来源映射见 [`MusicPalette::from_skin`]。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MusicPalette {
    /// 主题强调色（主按钮/进度条/选中）。
    pub accent: Hsla,
    /// 强调亮色（active 文字/徽标）。
    pub accent_light: Hsla,
    /// 主背景（窗口最深层）。
    pub bg_deep: Hsla,
    /// 侧栏/右栏/底栏底色。
    pub bg_primary: Hsla,
    /// 次级表面（搜索框/tab/按钮）。
    pub bg_secondary: Hsla,
    /// 卡片/行 hover 底。
    pub bg_card: Hsla,
    /// 更深一档 hover。
    pub bg_hover: Hsla,
    /// 主文字。
    pub text: Hsla,
    /// 次要文字。
    pub text2: Hsla,
    /// 三级/弱化文字。
    pub text3: Hsla,
    /// 分隔线/边框。
    pub border: Hsla,
    /// 粉色（收藏/渐变/强调）。
    pub pink: Hsla,
    /// 绿色（在线点/状态）。
    pub green: Hsla,
    /// 橙色（歌单点等）。
    pub orange: Hsla,
    /// 蓝色（歌单点等）。
    pub blue: Hsla,
    /// 覆盖层纯白（`white_fade_α` = `overlay_white.opacity(α)`）。
    pub overlay_white: Hsla,
}

impl MusicPalette {
    /// 音乐预设（原型常量逐字节，零配置基准）。
    pub fn music() -> MusicPalette {
        MusicPalette {
            accent: c(ACCENT),
            accent_light: c(ACCENT_LIGHT),
            bg_deep: c(BG_DEEP),
            bg_primary: c(BG_PRIMARY),
            bg_secondary: c(BG_SECONDARY),
            bg_card: c(BG_CARD),
            bg_hover: c(BG_HOVER),
            text: c(TEXT),
            text2: c(TEXT2),
            text3: c(TEXT3),
            border: c_a(BORDER),
            pink: c(PINK),
            green: c(GREEN),
            orange: c(ORANGE),
            blue: c(BLUE),
            overlay_white: gpui::white(),
        }
    }

    /// 从预设皮肤 id 构建（取 PRESETS 对应预设暗档人格令牌，覆盖到音乐 base 上）。
    ///
    /// 音乐常量 ← 预设 token 映射：`accent`←Accent、`accent_light`←AccentLight、
    /// `bg_deep`←SrfDeep、`bg_primary`←SrfPrimary、`bg_secondary`←SrfSecondary、
    /// `bg_card`←SrfCard、`bg_hover`←SrfHover、`text`←Fg、`text2`←FgMuted、
    /// `text3`←FgDisabled、`border`←SrfBorder、`pink`←Favorite、`green`←Success、
    /// `orange`←Warning、`blue`←Info、`overlay_white`←OverlayWhite。
    ///
    /// 未知/缺省 id → 回落 music 预设（现状逐字节）；`"follow"` 不属预设 → `None`
    /// （跟随全局走 [`MusicPalette::follow_global`] / [`MusicPalette::resolve`]）。
    pub fn from_skin(skin_id: &str) -> Option<MusicPalette> {
        if skin_id == "follow" {
            return None;
        }
        let preset = ui::theme::PRESETS
            .iter()
            .find(|p| p.id == skin_id)
            .or_else(|| ui::theme::PRESETS.iter().find(|p| p.id == "music"))
            .expect("PRESETS 至少含 music");
        Some(Self::from_preset_dark(preset))
    }

    /// 沿预设暗档表应用人格令牌覆盖到音乐 base 上。
    fn from_preset_dark(preset: &ui::theme::PresetSkin) -> MusicPalette {
        let mut p = Self::music();
        for &(token, hex) in preset.dark {
            let Some(v) = ui::theme::ColorToken::parse_hex(hex) else {
                continue; // 仅 6 位 #rrggbb（带 alpha 的人格令牌如 srf_border 走 base/原值）
            };
            let h: Hsla = v.into();
            match token {
                ui::theme::ColorToken::Accent => p.accent = h,
                ui::theme::ColorToken::AccentLight => p.accent_light = h,
                ui::theme::ColorToken::SrfDeep => p.bg_deep = h,
                ui::theme::ColorToken::SrfPrimary => p.bg_primary = h,
                ui::theme::ColorToken::SrfSecondary => p.bg_secondary = h,
                ui::theme::ColorToken::SrfCard => p.bg_card = h,
                ui::theme::ColorToken::SrfHover => p.bg_hover = h,
                ui::theme::ColorToken::SrfBorder => p.border = h,
                ui::theme::ColorToken::Fg => p.text = h,
                ui::theme::ColorToken::FgMuted => p.text2 = h,
                ui::theme::ColorToken::FgDisabled => p.text3 = h,
                ui::theme::ColorToken::Favorite => p.pink = h,
                ui::theme::ColorToken::Success => p.green = h,
                ui::theme::ColorToken::Warning => p.orange = h,
                ui::theme::ColorToken::Info => p.blue = h,
                ui::theme::ColorToken::OverlayWhite => p.overlay_white = h,
                _ => {} // 其余令牌（bg/surface 等）不映射音乐字段
            }
        }
        p
    }

    /// 跟随全局主题：从 `ui::Theme::global` **已合并** colors 取对应用（明暗由全局
    /// mode 决定，深浅档皆成立）。`danger_close_*`/`glass` 仍为音乐局部常量。
    pub fn follow_global(cx: &App) -> MusicPalette {
        let colors = &ui::theme::Theme::global(cx).colors;
        let t = |v: &Rgba| Hsla::from(*v);
        MusicPalette {
            accent: t(&colors.accent),
            accent_light: t(&colors.accent_light),
            bg_deep: t(&colors.srf_deep),
            bg_primary: t(&colors.srf_primary),
            bg_secondary: t(&colors.srf_secondary),
            bg_card: t(&colors.srf_card),
            bg_hover: t(&colors.srf_hover),
            text: t(&colors.fg),
            text2: t(&colors.fg_muted),
            text3: t(&colors.fg_disabled),
            border: t(&colors.srf_border),
            pink: t(&colors.favorite),
            green: t(&colors.success),
            orange: t(&colors.warning),
            blue: t(&colors.info),
            overlay_white: t(&colors.overlay_white),
        }
    }

    /// 统一解析：`"follow"` → [`Self::follow_global`]；预设 id → [`Self::from_skin`]；
    /// 未知 → music 预设（现状逐字节）。
    pub fn resolve(skin_id: &str, cx: &App) -> MusicPalette {
        if skin_id == "follow" {
            Self::follow_global(cx)
        } else {
            Self::from_skin(skin_id).expect("非 follow 恒 Some（未知回落 music 预设）")
        }
    }
}

/// 全 crate 测试级皮肤缓存串行锁：触碰进程级 `CURRENT` 的并行测试（theme.rs 的
/// `SkinTestGuard` 与 view/tests.rs 的 `SkinViewGuard`）都经这一把锁互斥，等价于
/// `--test-threads=1` 的 CURRENT 语义、并兼容并行跑。守卫 RAII 持锁并在进入/离开时
/// `MusicSkin::reset`。（仅测试构建需要，生产不随附）
#[cfg(test)]
pub(crate) static MUSIC_SKIN_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 当前音乐皮肤注册（进程级静态缓存）。
///
/// 选型说明：无参取色函数（`bg_deep()`/`accent()`…）在渲染期被大量调用，签名不能带
/// `&App`（否则 ③-2 要改全部调用点）；因此不走 gpui App-Global（读需 `&App`），改用
/// 进程级缓存存「已解析 palette + skin id」：`set` 时一次构建、渲染只读字段。测试隔离：
/// `reset()` 清缓存回音乐预设缺省（戳全局的测试统一用 `MUSIC_SKIN_TEST_SERIAL` 串行化，
/// 见 theme::tests 与 view::tests）。
pub struct MusicSkin;

/// 已登记状态（`id` 仅供读值 API；渲染只读 `palette`）。
struct MusicSkinState {
    id: String,
    palette: MusicPalette,
}

/// 当前生效调色板缓存：`None` = 未配置 → 音乐预设缺省（零配置 = 现状逐字节）。
static CURRENT: RwLock<Option<MusicSkinState>> = RwLock::new(None);

/// 音乐预设缺省调色板（一次构建、渲染只读；零配置基准）。
static DEFAULT_MUSIC: LazyLock<MusicPalette> =
    LazyLock::new(|| MusicPalette::from_skin("music").expect("music 预设恒可解析"));

impl MusicSkin {
    /// 登记当前皮肤并重建调色板（启动/换肤事件时调用一次；渲染只读缓存）。
    /// `skin_id`: `"follow"`（跟随全局）/ 预设 id / 未知（→ music 预设）。
    pub fn set(skin_id: &str, cx: &mut App) {
        let palette = MusicPalette::resolve(skin_id, cx);
        *CURRENT.write().unwrap() = Some(MusicSkinState {
            id: skin_id.to_string(),
            palette,
        });
    }

    /// 仅预设登记（无 App 环境 / 测试）：`"follow"` 降级为 music 预设。
    pub fn set_preset(skin_id: &str) {
        let palette = MusicPalette::from_skin(skin_id).unwrap_or_else(MusicPalette::music);
        *CURRENT.write().unwrap() = Some(MusicSkinState {
            id: skin_id.to_string(),
            palette,
        });
    }

    /// 清除登记（测试隔离）：回到未配置 = 音乐预设缺省。
    pub fn reset() {
        *CURRENT.write().unwrap() = None;
    }

    /// 是否处于「跟随全局主题」皮肤（`"follow"`）。**渲染热路径判定专用**：read guard 内
    /// 直接比较，不克隆 owned String（`current_skin_id` 返回 owned String、每帧调用有克隆
    /// 开销，非 follow 皮肤整帧重绘也不该背上）。
    pub fn is_follow() -> bool {
        CURRENT
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.id == "follow")
    }

    /// 当前皮肤 id（未配置 → `"music"`）。供 UI/日志等**字符串消费**；渲染热路径仅需
    /// 「是否 follow」时请用 [`MusicSkin::is_follow`]（本函数返回 owned String）。
    pub fn current_skin_id() -> String {
        CURRENT
            .read()
            .unwrap()
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "music".into())
    }
}

/// 当前生效调色板（渲染只读；未配置 → 音乐预设缺省）。
fn current() -> MusicPalette {
    CURRENT
        .read()
        .unwrap()
        .as_ref()
        .map(|s| s.palette)
        .unwrap_or(*DEFAULT_MUSIC)
}

/// 主背景色。
pub fn bg_deep() -> Hsla {
    current().bg_deep
}
/// 侧栏/右栏/底栏底色。
pub fn bg_primary() -> Hsla {
    current().bg_primary
}
/// 次级表面（搜索框/tab/按钮）。
pub fn bg_secondary() -> Hsla {
    current().bg_secondary
}
/// 卡片/行 hover 底。
pub fn bg_card() -> Hsla {
    current().bg_card
}
/// 更深一档 hover。
pub fn bg_hover() -> Hsla {
    current().bg_hover
}
/// 主题强调色。
pub fn accent() -> Hsla {
    current().accent
}
/// 强调亮色。
pub fn accent_light() -> Hsla {
    current().accent_light
}
/// 主文字。
pub fn text() -> Hsla {
    current().text
}
/// 次要文字。
pub fn text2() -> Hsla {
    current().text2
}
/// 三级/弱化文字。
pub fn text3() -> Hsla {
    current().text3
}
/// 分隔线/边框。
pub fn border() -> Hsla {
    current().border
}
/// 粉色。
pub fn pink() -> Hsla {
    current().pink
}
/// 绿色。
pub fn green() -> Hsla {
    current().green
}
/// 橙色。
pub fn orange() -> Hsla {
    current().orange
}
/// 蓝色。
pub fn blue() -> Hsla {
    current().blue
}

// ---- 白弱化 token（2026-11-14 收编：生产视图裸 `white().opacity(x)` 迁移） ----
// 与 text2/text3 的关系：text2/text3 是「不透明灰紫」，与「白 × 透明度」各档位色值
// 不严格相等，直接替换会引入观感偏移；故逐档收编为固定 token，值与原裸白完全相同
// （零观感变化是硬约束）。③-1：值源改为 `MusicPalette.overlay_white`（预设/全局覆盖
// 白均 = 纯白，follow 也随全局），行为与裸白逐档一致。

/// 白 × 6%（全屏控制钮常态底 / 唱片环线）。
pub fn white_fade_06() -> Hsla {
    current().overlay_white.opacity(0.06)
}
/// 白 × 8%（全屏控制钮描边 / 关闭钮常态底）。
pub fn white_fade_08() -> Hsla {
    current().overlay_white.opacity(0.08)
}
/// 白 × 12%（全屏控制钮 hover 底 / 封面占位音符）。
pub fn white_fade_12() -> Hsla {
    current().overlay_white.opacity(0.12)
}
/// 白 × 13%（歌词已播句）。
pub fn white_fade_13() -> Hsla {
    current().overlay_white.opacity(0.13)
}
/// 白 × 24%（歌词未来句）。
pub fn white_fade_24() -> Hsla {
    current().overlay_white.opacity(0.24)
}
/// 白 × 40%（歌曲封面占位音符）。
pub fn white_fade_40() -> Hsla {
    current().overlay_white.opacity(0.4)
}
/// 白 × 50%（列表缩略图占位音符）。
pub fn white_fade_50() -> Hsla {
    current().overlay_white.opacity(0.5)
}

// ---- 全屏关闭钮危险红（2026-11-14 收编：裸 `hsla(5,…)` 迁移，值相同） ----
/// 关闭钮 hover 底：`hsla(5, 0.6, 0.55, 0.22)`。
pub fn danger_close_bg() -> Hsla {
    gpui::hsla(5., 0.6, 0.55, 0.22)
}
/// 关闭钮 hover 描边：`hsla(5, 0.6, 0.6, 0.3)`。
pub fn danger_close_border() -> Hsla {
    gpui::hsla(5., 0.6, 0.6, 0.3)
}
/// 关闭钮 hover 图标：`hsla(5, 0.7, 0.7, 1.)`（不透明危险红）。
pub fn danger_close_icon() -> Hsla {
    gpui::hsla(5., 0.7, 0.7, 1.)
}

/// 深紫玻璃遮罩（toast 底）：原型 `.toast { background: rgba(20,17,50,0.92) }`
/// → 0.92×255≈235=0xeb，编码 `0x141132eb`。
pub fn glass() -> Hsla {
    c_a(0x141132eb)
}

// ---- Windows 风格标题栏窗口控制钮取色（对齐项目 shell 标题栏 `ui::theme::ChromeColors`） ----

/// 窗口控制钮 hover 填充（8% 白）：**别名** `ui::theme::ChromeColors::dark().hover_fill`
/// `0xffffff14`（与原型 `.win-btn:hover { background: rgba(255,255,255,0.08) }` 同值）。
pub const CTRL_HOVER_FILL: Rgba = ui::theme::CHROME_HOVER_FILL_DARK;
/// 窗口控制钮按压填充（约 14% 白）：**别名** shell `pressed_fill` 暗色档 `0xffffff24`。
pub const CTRL_PRESSED_FILL: Rgba = ui::theme::CHROME_PRESSED_FILL_DARK;
/// 关闭钮 hover 底红（Win11 危险红 `#e81123`）：**别名** shell `close_hover`。
pub const CLOSE_HOVER: Rgba = ui::theme::CHROME_CLOSE_HOVER;
/// 关闭钮按压底红（深一档 `#a70d1a`）：**别名** shell `close_pressed`（危险红亮暗同值）。
pub const CLOSE_PRESSED: Rgba = ui::theme::CHROME_CLOSE_PRESSED;

/// 窗口控制钮 hover 填充色。
pub fn ctrl_hover_fill() -> Hsla {
    CTRL_HOVER_FILL.into()
}
/// 窗口控制钮按压填充色。
pub fn ctrl_pressed_fill() -> Hsla {
    CTRL_PRESSED_FILL.into()
}
/// 关闭钮 hover 底红。
pub fn close_hover() -> Hsla {
    CLOSE_HOVER.into()
}
/// 关闭钮按压底红。
pub fn close_pressed() -> Hsla {
    CLOSE_PRESSED.into()
}

// ---- 全屏 / 圆角图标钮几何（消费方逐字使用，防尺寸漂移） ----

/// 全屏工具条图标钮尺寸（px）：40×40。
pub const FS_CTRL_BTN_SIZE: f32 = 40.0;
/// 全屏工具条图标钮圆角（px）：20（全圆）。
pub const FS_CTRL_BTN_RADIUS: f32 = 20.0;
/// 全屏工具条图标钮图标尺寸（px）：20。
pub const FS_CTRL_BTN_ICON: f32 = 20.0;
/// 圆角图标钮（通知/设置）尺寸（px）：36×36。
pub const ROUND_ICON_BTN_SIZE: f32 = 36.0;
/// 圆角图标钮圆角（px）：18（全圆）。
pub const ROUND_ICON_BTN_RADIUS: f32 = 18.0;
/// 圆角图标钮图标尺寸（px）：17。
pub const ROUND_ICON_BTN_ICON: f32 = 17.0;

/// 由 model `Track.grad`（主色 `(r,g,b)`）派生渐变双色：主色 + 加深 55% 变体
/// （与 `model::gradient_pair` 的加深系数一致，保证封面观感同源）。
pub fn gradient_pair_of((r, g, b): (u32, u32, u32)) -> (Hsla, Hsla) {
    let dark = (
        ((r as f32 * 0.55) as u32).min(255),
        ((g as f32 * 0.55) as u32).min(255),
        ((b as f32 * 0.55) as u32).min(255),
    );
    (c((r << 16) | (g << 8) | b), c((dark.0 << 16) | (dark.1 << 8) | dark.2))
}

/// 解析 `#rrggbb` 渐变字符串（`MixGroup.grad` 两色）→ [`Hsla`]；解析失败回退强调色。
pub fn from_hex_str(s: &str) -> Hsla {
    // 解析失败回退**当前强调色**（③-1：随皮肤走而非固定 music 常量）。
    match u32::from_str_radix(s.trim_start_matches('#'), 16) {
        Ok(hex) => c(hex),
        Err(_) => current().accent,
    }
}

/// Hsla → `#rrggbb`（供 [`grad_css`] 复刻原型 CSS 写法）。
fn hsla_hex(color: Hsla) -> String {
    let rgba: gpui::Rgba = color.into();
    let to8 = |v: f32| ((v * 255.0).round() as u32).min(255);
    format!(
        "#{:02x}{:02x}{:02x}",
        to8(rgba.r),
        to8(rgba.g),
        to8(rgba.b)
    )
}

/// 由 `Track.grad` 生成原型 `linear-gradient(135deg,#..,#..)` 字符串（任务契约要求；
/// GPUI 绘制不走本函数，见 [`gradient_cover`]）。
pub fn grad_css((r, g, b): (u32, u32, u32)) -> String {
    let (light, dark) = gradient_pair_of((r, g, b));
    format!("linear-gradient(135deg,{}, {})", hsla_hex(light), hsla_hex(dark))
}

/// 渐变封面（GPUI 无 CSS 渐变）：`canvas` 绘制 `135deg` 线性渐变圆角方块，
/// 通常 `absolute inset-0` 铺满父容器。`(light, dark)` 为 `[0%, 100%]` 两停靠色。
pub fn gradient_cover((light, dark): (Hsla, Hsla), radius: f32) -> AnyElement {
    gradient_cover_impl((light, dark), radius, None)
}

/// 同 [`gradient_cover`]，但把 canvas **实际绘制盒**写入 `record`：供 fs 进度填充的
/// 「填充随位置增长」回归断言——canvas 布局高度曾塌缩为 0（真机复报五：填充隐形只留轨道
/// 一色），此盒可直接断言绘制面积非零且随 frac 变化（`bounds_canvas` 同款 Cell 姿势）。
/// K24 休眠：已被 `ui::SliderFill::Gradient` 取代（fullscreen 用两色阶），保留防回归，勿删。
pub fn gradient_cover_tracked(
    (light, dark): (Hsla, Hsla),
    radius: f32,
    record: Rc<Cell<Bounds<Pixels>>>,
) -> AnyElement {
    gradient_cover_impl((light, dark), radius, Some(record))
}

/// 渐变绘制公共实现：canvas 自身 `absolute().inset_0()` 填最近定位祖先（gpui 所有 div 默认
/// `position: relative` = 直接父 div）——否则 block 子盒高度塌缩 0（布局只横向拉伸、纵向为
/// content 高 0）→ 渐变面积 0 不可见（真机复报五根因）。
fn gradient_cover_impl(
    (light, dark): (Hsla, Hsla),
    radius: f32,
    record: Option<Rc<Cell<Bounds<Pixels>>>>,
) -> AnyElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _cx| {
            if let Some(r) = &record {
                r.set(bounds);
            }
            window.paint_quad(
                fill(
                    bounds,
                    linear_gradient(
                        135.,
                        linear_color_stop(dark, 0.),
                        linear_color_stop(light, 1.),
                    ),
                )
                .corner_radii(px(radius)),
            );
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// 供可交互条（进度/音量）记录元素窗口坐标盒的 canvas 辅助：prepaint 期把自身布局盒
/// 写入 `bounds`，事件回调据此反算比例（对齐 video 查看器 `strip_element` 姿势）。
/// `absolute().inset_0()`：canvas 脱离布局流、在任何容器上下文（含 flex-row）都记录
/// **父条完整盒**（宽>0）——旧实现无尺寸 canvas 在 flex-row（如 `#fs-vol`）里是 flex
/// item → 宽塌缩 0 → `fraction_at` 拒算 → 全屏音量/进度点拖失效（真机复报）。
pub fn bounds_canvas(bounds: Rc<Cell<Bounds<Pixels>>>) -> AnyElement {
    canvas(
        move |b, _w, _app| bounds.set(b),
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn palette_constants_match_proto_colors() {
        // 逐字钉死原型 `:root` 十六进制，防抄写漂移（proto 锚仍钉真值）。
        assert_eq!(BG_DEEP, 0x0d0b1a);
        assert_eq!(BG_PRIMARY, 0x13112a);
        assert_eq!(BG_SECONDARY, 0x1a1738);
        assert_eq!(BG_CARD, 0x201d40);
        assert_eq!(BG_HOVER, 0x282452);
        assert_eq!(ACCENT, 0x7c5cfc);
        assert_eq!(ACCENT_LIGHT, 0xa78bfa);
        assert_eq!(TEXT, 0xf0edff);
        assert_eq!(TEXT2, 0x9892b8);
        assert_eq!(TEXT3, 0x5e5880);
        assert_eq!(PINK, 0xf472b6);
        assert_eq!(GREEN, 0x34d399);
        assert_eq!(ORANGE, 0xfb923c);
        assert_eq!(BLUE, 0x60a5fa);
        // ③-1：`from_skin("music")` 必须与原型常量**逐字节**一致（判别式：
        // 删派生改回硬编码即 pass）——字段 == 常量换算值。
        let p = MusicPalette::from_skin("music").expect("music 预设可解析");
        assert_eq!(p.accent, c(ACCENT));
        assert_eq!(p.accent_light, c(ACCENT_LIGHT));
        assert_eq!(p.bg_deep, c(BG_DEEP));
        assert_eq!(p.bg_primary, c(BG_PRIMARY));
        assert_eq!(p.bg_secondary, c(BG_SECONDARY));
        assert_eq!(p.bg_card, c(BG_CARD));
        assert_eq!(p.bg_hover, c(BG_HOVER));
        assert_eq!(p.text, c(TEXT));
        assert_eq!(p.text2, c(TEXT2));
        assert_eq!(p.text3, c(TEXT3));
        assert_eq!(p.border, c_a(BORDER));
        assert_eq!(p.pink, c(PINK));
        assert_eq!(p.green, c(GREEN));
        assert_eq!(p.orange, c(ORANGE));
        assert_eq!(p.blue, c(BLUE));
    }

    #[test]
    fn grad_css_produces_proto_css_gradient() {
        let s = grad_css((0x7c, 0x5c, 0xfc));
        assert!(s.starts_with("linear-gradient(135deg,"), "s={s}");
        assert!(s.contains('#'), "应变出 #rrggbb 两色：{s}");
        assert_eq!(grad_css((0x7c, 0x5c, 0xfc)), grad_css((0x7c, 0x5c, 0xfc)));
    }

    #[test]
    fn gradient_pair_darkens_single_color() {
        let (light, dark) = gradient_pair_of((0x7c, 0x5c, 0xfc));
        assert_ne!(light, dark, "两色应不同（主色 vs 加深变体）");
        let d: gpui::Rgba = dark.into();
        let l: gpui::Rgba = light.into();
        assert!(d.r < l.r && d.g < l.g && d.b < l.b, "加深变体各分量更暗");
    }

    #[test]
    fn title_ctrl_colors_match_shell_chrome() {
        // CTRL_* 已改为 `ui::theme` 的 `CHROME_*` 常量**别名**：逐个断言「别名 = ui 常量
        // = 权威十六进制」（hover_fill 暗档 0xffffff14 / pressed_fill 0xffffff24 /
        // close_hover #e81123 / close_pressed #a70d1a），防漂移。
        assert_eq!(CTRL_HOVER_FILL, ui::theme::CHROME_HOVER_FILL_DARK);
        assert_eq!(CTRL_PRESSED_FILL, ui::theme::CHROME_PRESSED_FILL_DARK);
        assert_eq!(CLOSE_HOVER, ui::theme::CHROME_CLOSE_HOVER);
        assert_eq!(CLOSE_PRESSED, ui::theme::CHROME_CLOSE_PRESSED);
        assert_eq!(CTRL_HOVER_FILL, gpui::rgba(0xffffff14));
        assert_eq!(CTRL_PRESSED_FILL, gpui::rgba(0xffffff24));
        assert_eq!(CLOSE_HOVER, gpui::rgb(0xe81123));
        assert_eq!(CLOSE_PRESSED, gpui::rgb(0xa70d1a));
        let hover: gpui::Rgba = close_hover().into();
        let pressed: gpui::Rgba = close_pressed().into();
        assert!(
            pressed.r < hover.r && pressed.g < hover.g && pressed.b < hover.b,
            "关闭按压红应比 hover 红深一档"
        );
    }

    #[test]
    fn white_fade_tokens_keep_exact_alpha() {
        // 收编硬约束：白弱化 token 必须与迁移前的裸 `white().opacity(x)` 逐档同值
        //（零观感变化）；此处钉死透明度，防后续重建 token 时漂移。
        let cases = [
            (white_fade_06(), 0.06),
            (white_fade_08(), 0.08),
            (white_fade_12(), 0.12),
            (white_fade_13(), 0.13),
            (white_fade_24(), 0.24),
            (white_fade_40(), 0.4),
            (white_fade_50(), 0.5),
        ];
        for (hsla, a) in cases {
            assert_eq!(hsla.a, a, "透明度应精确等于 {a}");
        }
    }

    #[test]
    fn danger_close_tokens_keep_claimed_hsl() {
        // 全屏关闭钮危险红三件套：钉死迁移前裸 `hsla(5,…)` 的**解析后实际值**。
        // 注：gpui `hsla()` 的 hue 取 [0,1] 并 clamp（`color.rs:424`），原字面 `5.`
        // 即被 clamp 成 1.0（红）；token 与原字面逐位相同，故断言 1.0（红）而非 5.0。
        let bg = danger_close_bg();
        assert_eq!((bg.h, bg.s, bg.l, bg.a), (1.0, 0.6, 0.55, 0.22), "bg {bg:?}");
        let border = danger_close_border();
        assert_eq!((border.h, border.s, border.l, border.a), (1.0, 0.6, 0.6, 0.3), "border {border:?}");
        let icon = danger_close_icon();
        assert_eq!((icon.h, icon.s, icon.l, icon.a), (1.0, 0.7, 0.7, 1.0), "icon {icon:?}");
    }

    // ---- ③-1：皮肤派生 / 当前皮肤切换 / 零配置判别 ----

    /// 串行化「触碰进程级皮肤缓存」的测试（零配置 + 换肤两个用）：持有全 crate 共享的
    /// [`MUSIC_SKIN_TEST_SERIAL`]（与 view::tests 的 `SkinViewGuard` 同一把锁）并在进入/
    /// 离开时清全局，防并行测试间皮肤缓存串扰（cargo test 同进程多线程）。
    /// 锁可能被「断言失败时仍持有的守卫」毒化（守卫 Drop 在 unwind 中运行）；锁本身
    /// 总会随守卫释放，故遇毒化用 `into_inner` 恢复即可继续。
    #[allow(dead_code)] // 字段仅供 RAII 生命周期持有（不读取），见 `SkinTestGuard` 注释
    struct SkinTestGuard(std::sync::MutexGuard<'static, ()>);
    impl SkinTestGuard {
        fn new() -> Self {
            MusicSkin::reset();
            let lock = MUSIC_SKIN_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            Self(lock)
        }
    }
    impl Drop for SkinTestGuard {
        fn drop(&mut self) {
            MusicSkin::reset();
        }
    }

    #[test]
    fn from_skin_ocean_overrides_personality_tokens() {
        // ocean 预设暗档：accent=#2dd4bf / accent_light=#5eead4 / bg 坡冷蓝灰。
        let p = MusicPalette::from_skin("ocean").expect("ocean 预设存在");
        assert_eq!(p.accent, c(0x2dd4bf), "ocean 海青强调色");
        assert_eq!(p.accent_light, c(0x5eead4));
        assert_eq!(p.bg_deep, c(0x0b111c), "bg 坡冷蓝灰");
        assert_eq!(p.bg_primary, c(0x0e1826));
        assert_eq!(p.bg_secondary, c(0x122033));
        assert_eq!(p.bg_card, c(0x15263a));
        assert_eq!(p.bg_hover, c(0x1a2d44));
        // PRESETS 未列 fg/favorite/… → 非人格令牌保持音乐 base（text/pink/border 不变）。
        assert_eq!(p.text, c(TEXT));
        assert_eq!(p.pink, c(PINK));
        assert_eq!(p.border, c_a(BORDER));
    }

    #[test]
    fn from_skin_follow_is_none_and_unknown_falls_back_to_music() {
        // "follow" 不属预设：from_skin 返回 None（跟随走 follow_global）。
        assert!(MusicPalette::from_skin("follow").is_none());
        // 未知 id → 回落 music 预设（现状逐字节）。
        let unknown = MusicPalette::from_skin("no-such-skin").expect("未知回落 music");
        assert_eq!(unknown, MusicPalette::from_skin("music").unwrap());
    }

    #[gpui::test]
    fn follow_resolve_tracks_global_theme(cx: &mut TestAppContext) {
        use ui::theme::{Theme, ThemeMode, ThemeSpec};
        // 未初始化全局 → 回落 Light：follow 取亮档已合并色（srf_deep=#f4f5f7）。
        cx.update(|cx| {
            let p = MusicPalette::resolve("follow", cx);
            assert_eq!(p.bg_deep, c(0xf4f5f7), "Light 兜底面坡深");
            assert_eq!(p.overlay_white, gpui::white(), "覆盖白随全局");
        });
        // 应用 music 暗档：follow 跟随合并后的 accent/srf 紫坡 + 全局 fg。
        cx.update(|cx| {
            Theme::apply(
                ThemeSpec {
                    mode: ThemeMode::Dark,
                    preset: ui::theme::PRESETS.iter().find(|p| p.id == "music").unwrap(),
                    custom: Default::default(),
                },
                cx,
            );
        });
        cx.update(|cx| {
            let p = MusicPalette::resolve("follow", cx);
            assert_eq!(p.accent, c(0x7c5cfc), "follow music 暗档 accent");
            assert_eq!(p.bg_deep, c(0x0d0b1a));
            assert_eq!(p.text, c(0xe6e6e6), "follow text=全局 fg（ColorScheme::dark().fg）");
        });
    }

    #[test]
    fn zero_config_noarg_functions_match_music_constants() {
        let _guard = SkinTestGuard::new();
        // 未 set：无参取色 = 音乐预设逐字节（判别式：删派生改回硬编码即 pass）。
        assert_eq!(bg_deep(), c(BG_DEEP));
        assert_eq!(bg_primary(), c(BG_PRIMARY));
        assert_eq!(bg_secondary(), c(BG_SECONDARY));
        assert_eq!(bg_card(), c(BG_CARD));
        assert_eq!(bg_hover(), c(BG_HOVER));
        assert_eq!(accent(), c(ACCENT));
        assert_eq!(accent_light(), c(ACCENT_LIGHT));
        assert_eq!(text(), c(TEXT));
        assert_eq!(text2(), c(TEXT2));
        assert_eq!(text3(), c(TEXT3));
        assert_eq!(border(), c_a(BORDER));
        assert_eq!(pink(), c(PINK));
        assert_eq!(green(), c(GREEN));
        assert_eq!(orange(), c(ORANGE));
        assert_eq!(blue(), c(BLUE));
        // 局部常量钉值保持（不受皮肤影响）+ 回退当前强调色。
        assert_eq!(glass(), c_a(0x141132eb));
        assert_eq!(from_hex_str("not-hex"), accent());
        assert_eq!(MusicSkin::current_skin_id(), "music");
    }

    #[gpui::test]
    fn set_skin_switches_noarg_colors(cx: &mut TestAppContext) {
        let _guard = SkinTestGuard::new();
        cx.update(|cx| MusicSkin::set("ocean", cx));
        assert_eq!(accent(), c(0x2dd4bf), "ocean 强调色生效");
        assert_eq!(bg_deep(), c(0x0b111c), "ocean 面坡生效");
        assert_eq!(text(), c(TEXT), "text 非人格令牌保持音乐 base");
        assert_eq!(from_hex_str("not-hex"), accent(), "回退当前强调色随皮肤走");
        assert_eq!(MusicSkin::current_skin_id(), "ocean");
        // 未知 id：set 回落 music，无参取色复原。
        cx.update(|cx| MusicSkin::set("bad-id", cx));
        assert_eq!(accent(), c(ACCENT));
        assert_eq!(bg_deep(), c(BG_DEEP));
        // 显式 set music 与零配置一致。
        cx.update(|cx| MusicSkin::set("music", cx));
        assert_eq!(accent(), c(ACCENT));
        assert_eq!(MusicSkin::current_skin_id(), "music");
    }
}
