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
//! 颜色统一从这里取语义字段，禁止在组件里新写硬编码 `rgba()`。当前只迁移了
//! 明暗判断与滚动条等少量消费；`markdown/view/style.rs`、`editor/view` 的 typography/
//! 语义色深耦合仍在组件主题上（见 `docs/ui-migration.md` 路线图）。

use std::sync::LazyLock;

use gpui::{App, Global, Rgba, rgb};

/// 明暗模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// 语义色板（亮/暗各一套）。字段名是「用途」而非具体色值。
///
/// 当前只消费了 `fg`/`mode`（滚动条、明暗分支）；`bg`/`surface`/`border`/`accent`/`link`
/// 等字段是给下一波（`markdown/view/style.rs`、`editor/view` 的 typography/语义色迁移）预留的
/// 语义色出口，届时逐字段接入，勿在组件里新写硬编码 `rgba()`。
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // 语义色部分字段待下一波迁移消费，先固化地基
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
    /// 强调色（拖拽分栏/激活高亮共用，原 `config::colors::RESIZE_ACCENT`）
    pub accent: Rgba,
    /// 链接（原 `config::colors::LINK_LIGHT/DARK`）
    pub link: Rgba,
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
        }
    }
}

/// 全局主题：当前模式 + 对应语义色板。
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub mode: ThemeMode,
    pub colors: ColorScheme,
}

impl Global for Theme {}

/// 未初始化时的兜底主题（Light，与应用默认一致）。
static DEFAULT_LIGHT: LazyLock<Theme> = LazyLock::new(Theme::light);

impl Theme {
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            colors: ColorScheme::light(),
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            colors: ColorScheme::dark(),
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
}
