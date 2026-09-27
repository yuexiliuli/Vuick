//! 自研主题系统：替换 `gpui_component::Theme` 作为本应用 UI 配色/明暗的唯一来源。
//!
//! # 获取
//! - [`Theme::global(cx)`] / [`Theme::global_mut(cx)`]：未初始化时回落到内置 Light 主题
//!   （`try_global` 兜底——测试里不显式初始化也能零成本使用）。
//! - 启动时调用 [`Theme::init(cx)`] 显式种入默认主题；切换走 [`Theme::set`]。
//!
//! # 迁移期双轨
//! [`Theme::set`] 在写入本主题的同时驱动 `gpui_component::Theme::change`，让仍残留的
//! gpui-component 组件（Input/菜单/语法高亮）配色与自绘 UI 保持一致；彻底移除
//! gpui-component 依赖后删除兼容桥。
//!
//! # 语义色板
//! 颜色统一从这里取语义字段，禁止在组件里新写硬编码 `rgba()`。第 3 波迁移后，
//! Markdown 预览 / WYSIWYG / 编辑器的排版与配色也已接入本主题（`markdown/view/style.rs`、
//! `editor/view` 的 typography、`editor/markdown.rs`、`syntax` 深耦合点全部收编，
//! 见 `docs/ui-migration.md`）。残留的 gpui-component 消费还剩编辑器 Input 本体（#12）。

use std::sync::LazyLock;

use gpui::{App, Global, Rgba, SharedString, rgb};

/// 明暗模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// 排版参数（与色板同属主题、跨明暗的字体家族与字号体系）。
///
/// 「正文/标题字号体系」由 `config::layout::DEFAULT_FONT_SIZE` 驱动，本结构固化缺省
/// 字号与缩放比例，编辑器/预览/WYSIWYG 各自从这里读默认值：
/// `editor_default_size` / `editor_line_height_ratio` 是编辑器的等宽缺省字号与行高
/// 系数（无 `font_override` 时用；保留 gpui-component `mono_md` 的 13/20 既有值，
/// 视觉无回归——应用侧恒有 override=14，此值仅供无覆盖/测试路径）；
/// `default_body_size` 是预览 / WYSIWYG / 编辑区覆盖路径的统一缩放基准（与
/// `config::layout::DEFAULT_FONT_SIZE` 同值）。正文行高/代码字号/标题字号的推导
/// 函数集中在 `markdown::view::style::{typography_for_size, heading_metrics_for_size}`，
/// 即统一缩放架构（见 `docs/ui-migration.md`）。
#[derive(Clone, Debug)]
pub struct Typography {
    /// 等宽字体族（代码块/编辑器/行内代码/语法高亮共用；平台默认，Windows=Consolas）。
    pub mono: SharedString,
    /// 编辑器等宽缺省字号（px，`font_override=None` 时用；原 `mono_md` 默认 13）。
    pub editor_default_size: f32,
    /// 编辑器等宽行高系数（× 字号）；原 `mono_md` 20/13 比例。
    pub editor_line_height_ratio: f32,
    /// 缺省正文字号（px，预览/WYSIWYG 默认与编辑区覆盖路径的统一缩放基准，
    /// 与 `config::layout::DEFAULT_FONT_SIZE` 同值）。
    pub default_body_size: f32,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            mono: default_mono_font_family(),
            editor_default_size: 13.0,
            editor_line_height_ratio: 20.0 / 13.0,
            default_body_size: crate::config::layout::DEFAULT_FONT_SIZE,
        }
    }
}

/// 平台默认等宽字体（Windows=`Consolas`，macOS=`Menlo`，其余=`DejaVu Sans Mono`），
/// 与 gpui-component 默认一致，避免换主题后代码字形跳变。
fn default_mono_font_family() -> SharedString {
    if cfg!(target_os = "macos") {
        "Menlo".into()
    } else if cfg!(target_os = "windows") {
        "Consolas".into()
    } else {
        "DejaVu Sans Mono".into()
    }
}

/// 语义色板（亮/暗各一套）。字段名是「用途」而非具体色值。
///
/// 第 3 波迁移后已全量消费：`bg`/`surface`/`fg`/`fg_muted`/`border`/`accent`/`link`
/// 供通用控件；`code_*`/`quote_bg`/`table_head_bg`/`heading`/`popover_bg` 供
/// Markdown 预览 / WYSIWYG / 编辑器（原 `markdown/view/style.rs`、`note_card`、
/// `editor/markdown.rs` 的硬编码 `rgb()` 收编于此，见 `docs/ui-migration.md`）。
#[derive(Clone, Copy, Debug)]
pub struct ColorScheme {
    /// 主背景（画布/正文底色）
    pub bg: Rgba,
    /// 面板/状态栏等次级表面
    pub surface: Rgba,
    /// 主文字
    pub fg: Rgba,
    /// 次要文字
    pub fg_muted: Rgba,
    /// 分隔线/边框
    pub border: Rgba,
    /// 强调色（拖拽分栏/激活高亮/引用标记共用，原 `config::colors` 的 RESIZE_ACCENT）
    pub accent: Rgba,
    /// 链接（原 `config::colors` 的 LINK_LIGHT/DARK）
    pub link: Rgba,
    /// 错误/危险色（打开失败通知等）
    pub error: Rgba,
    /// 代码块背景（预览 / WYSIWYG 围栏底，明 #f3f4f4 / 暗 #282936）
    pub code_block_bg: Rgba,
    /// 行内代码（反引号）文字色（明 #222222 / 暗 #dadada，近正文，靠底色+等宽区分）
    pub code_inline_fg: Rgba,
    /// 行内代码圆角 pill 底色（预览 / WYSIWYG，明 #f3f4f4 / 暗 #3e3f40）
    pub code_inline_bg: Rgba,
    /// 编辑器 Markdown 滤镜行内代码底色（明 #eff1f3 / 暗 #2b3340，与预览略异，保留原值）
    pub editor_code_bg: Rgba,
    /// 公司块背景（明 #f9fafb / 暗 #1b2431）
    pub quote_bg: Rgba,
    /// 表头底色（明 #f3f4f6 / 暗 #1f2937）
    pub table_head_bg: Rgba,
    /// Markdown 标题文字色（编辑器滤镜用，明 #0550ae / 暗 #79c0ff）
    pub heading: Rgba,
    /// 悬浮小卡片/弹层背景（EPUB 脚注卡片等，明 #ffffff / 暗 #1e2430）
    pub popover_bg: Rgba,
}

impl ColorScheme {
    pub fn light() -> Self {
        Self {
            bg: rgb(0xffffff),
            surface: rgb(0xffffff),
            fg: rgb(0x1f2328),
            fg_muted: rgb(0x6b7280),
            border: rgb(0xd1d5db),
            accent: rgb(0x0078d4),
            link: rgb(0x0969da),
            error: rgb(0xd1242f),
            code_block_bg: rgb(0xf3f4f4),
            code_inline_fg: rgb(0x222222),
            code_inline_bg: rgb(0xf3f4f4),
            editor_code_bg: rgb(0xeff1f3),
            quote_bg: rgb(0xf9fafb),
            table_head_bg: rgb(0xf3f4f6),
            heading: rgb(0x0550ae),
            popover_bg: rgb(0xffffff),
        }
    }

    pub fn dark() -> Self {
        Self {
            bg: rgb(0x0f172a),
            surface: rgb(0x111b2a),
            fg: rgb(0xe5e7eb),
            fg_muted: rgb(0x94a3b8),
            border: rgb(0x1f2937),
            accent: rgb(0x0078d4),
            link: rgb(0x79c0ff),
            error: rgb(0xff7b72),
            code_block_bg: rgb(0x282936),
            code_inline_fg: rgb(0xdadada),
            code_inline_bg: rgb(0x3e3f40),
            editor_code_bg: rgb(0x2b3340),
            quote_bg: rgb(0x1b2431),
            table_head_bg: rgb(0x1f2937),
            heading: rgb(0x79c0ff),
            popover_bg: rgb(0x1e2430),
        }
    }
}

/// 全局主题：当前模式 + 对应语义色板 + 排版参数。
#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: ThemeMode,
    pub colors: ColorScheme,
    pub typography: Typography,
}

impl Global for Theme {}

/// 未初始化时的兜底主题（Light，与应用默认一致）。
static DEFAULT_LIGHT: LazyLock<Theme> = LazyLock::new(Theme::light);

impl Theme {
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            colors: ColorScheme::light(),
            typography: Typography::default(),
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            colors: ColorScheme::dark(),
            typography: Typography::default(),
        }
    }

    pub fn is_dark(&self) -> bool {
        matches!(self.mode, ThemeMode::Dark)
    }

    /// 读取全局主题。未初始化时回落到默认 Light（测试/早期初始化顺序不敏感）。
    pub fn global(cx: &App) -> &Theme {
        cx.try_global::<Theme>().unwrap_or(&DEFAULT_LIGHT)
    }

    /// 切换主题：写本全局 + 驱动 gpui-component 兼容桥 + 刷新窗口。
    ///
    /// 迁移期双轨：残留组件仍读组件主题，这里同步推过去保证配色一致。
    pub fn set(mode: ThemeMode, cx: &mut App) {
        let theme = match mode {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
        };
        cx.set_global(theme);
        // 兼容桥：彻底移除 gpui-component 后删除。
        gpui_component::Theme::change(
            match mode {
                ThemeMode::Dark => gpui_component::ThemeMode::Dark,
                ThemeMode::Light => gpui_component::ThemeMode::Light,
            },
            None,
            cx,
        );
        cx.refresh_windows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn uninitialized_defaults_to_light(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let t = Theme::global(cx);
            assert_eq!(t.mode, ThemeMode::Light);
            assert!(!t.is_dark());
        });
    }

    #[gpui::test]
    fn set_writes_mode_and_drives_component_theme(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| Theme::set(ThemeMode::Dark, cx));
        cx.update(|cx| {
            assert!(Theme::global(cx).is_dark());
            assert!(gpui_component::Theme::global(cx).is_dark());
        });
        cx.update(|cx| Theme::set(ThemeMode::Light, cx));
        cx.update(|cx| {
            assert!(!Theme::global(cx).is_dark());
            assert!(!gpui_component::Theme::global(cx).is_dark());
        });
    }

    #[gpui::test]
    fn palette_has_distinct_light_and_dark(_cx: &mut TestAppContext) {
        let light = Theme::light();
        let dark = Theme::dark();
        assert_ne!(light.colors.bg, dark.colors.bg, "亮/暗主背景应不同");
        assert_ne!(light.colors.fg, dark.colors.fg, "亮/暗主文字应不同");
        assert_ne!(light.colors.fg, light.colors.fg_muted, "主次文字应不同");
        assert_eq!(light.colors.accent, dark.colors.accent, "强调色全局统一");
        assert_ne!(light.colors.link, dark.colors.link, "链接色随明暗切换");
    }

    #[gpui::test]
    fn markdown_semantic_palette_pins_original_values(_cx: &mut TestAppContext) {
        // 回归：第 3 波把 markdown/view/style.rs 硬编码色收编进色板后，**逐字节保持原值**
        //（预览/编辑器/WYSIWYG 依赖这些精确值，见各模块的从 theme 派生 / 单测）。
        let light = Theme::light();
        let dark = Theme::dark();
        // 代码块背景（白 #F3F4F4 / 黑 #282936）
        assert_eq!(light.colors.code_block_bg, rgb(0xf3f4f4), "白主题代码块底");
        assert_eq!(dark.colors.code_block_bg, rgb(0x282936), "黑主题代码块底");
        // 行内代码 pill（白 底 #F3F4F4 + 字 #222222 / 黑 底 #3E3F40 + 字 #DADADA）
        assert_eq!(light.colors.code_inline_bg, rgb(0xf3f4f4));
        assert_eq!(light.colors.code_inline_fg, rgb(0x222222));
        assert_eq!(dark.colors.code_inline_bg, rgb(0x3e3f40));
        assert_eq!(dark.colors.code_inline_fg, rgb(0xdadada));
        // 编辑器滤镜行内代码底（白 #EFF1F3 / 黑 #2B3340，与预览略异，保留原值）
        assert_eq!(light.colors.editor_code_bg, rgb(0xeff1f3));
        assert_eq!(dark.colors.editor_code_bg, rgb(0x2b3340));
        // 引述底 / 表头底随明暗切换
        assert_ne!(light.colors.quote_bg, dark.colors.quote_bg, "引述底色随明暗");
        assert_ne!(light.colors.table_head_bg, dark.colors.table_head_bg, "表头底色随明暗");
        // 标题（编辑器滤镜）与链接（明/暗同源不改蓝）
        assert_ne!(light.colors.heading, dark.colors.heading, "标题色随明暗");
        // 悬浮卡片底随明暗
        assert_ne!(light.colors.popover_bg, dark.colors.popover_bg, "弹层底色随明暗");
    }

    #[gpui::test]
    fn typography_default_has_mono_family_and_unified_ratio(_cx: &mut TestAppContext) {
        let t = Typography::default();
        assert!(!t.mono.is_empty(), "等宽字体族不应为空");
        assert_eq!(t.mono, Typography::default().mono, "字体族跨明暗不变");
        // 行高比例保留 gpui-component mono_md 的 20/13（编辑器缩放无回归）。
        assert!((t.editor_line_height_ratio - 20.0 / 13.0).abs() < 1e-6);
        // 编辑器无覆盖默认保留 mono_md 的 13（应用侧恒有 override，仅供无覆盖/测试路径）。
        assert_eq!(t.editor_default_size, 13.0);
        // 缺省正文字号＝统一缩放架构的全局驱动值。
        assert_eq!(t.default_body_size, crate::config::layout::DEFAULT_FONT_SIZE);
        assert_eq!(
            (t.editor_default_size * t.editor_line_height_ratio).round(),
            20.0,
            "编辑器默认行高保留 20"
        );
    }
}
