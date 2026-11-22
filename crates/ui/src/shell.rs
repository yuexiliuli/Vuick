//! 外壳共享几何与小部件（无缝直连形态）。
//!
//! 标题栏 / 选项卡栏 / 侧栏 / 状态栏使用同一 chrome mica 底色直连成片：矩形直角、
//! 区域间以发丝线分隔，不设漂浮圆角卡与缝隙。本模块提供共享色值
//! （`card_surface` / `body_card_face`）供各挂载点取用。
//!
//! 颜色一律取 [`crate::ui::theme`] 语义 token（`surface`/`bg`/`border`/`drag_hover_border`），
//! 不散落 `rgba`。

use gpui::{App, Hsla};

use crate::ui::theme::Theme;

/// chrome 区域底色（状态栏等）：读**合并后** theme `chrome_bg`（原 config `CHROME_BG_DARK`
/// #191A1B，常量已删，与标题栏/选项卡栏/侧栏同色直连）；亮 #FFFFFF（同 token 亮档）。
/// P1-2：不再从 `for_mode` base 现取，自定义/预设经 apply 合并后这里直接可见。
pub fn card_surface(cx: &App) -> Hsla {
    Theme::global(cx).chrome.chrome_bg.into()
}

/// 正文区底色：亮色 = **合并后** theme `panel_bg`（编辑器同一来源）；**暗色 = 合并后
/// theme `editor_bg`**（原 config `EDITOR_BG_DARK` #121314，常量已删），与编辑区/预览
/// 面板同色。
pub fn body_card_face(cx: &App) -> Hsla {
    let theme = Theme::global(cx);
    content_bg_theme(cx, theme.is_dark(), theme.chrome.panel_bg.into())
}

/// 内容区（编辑区/预览/欢迎页）底色统一入口（**合并后 theme 版**）：暗色取合并后
/// [`Theme`] `chrome.editor_bg`（原 config `EDITOR_BG_DARK` #121314，常量已删），亮色用
/// 调用方给的既有底色（theme `bg` / `panel_bg`）。ui 内生产消费（`body_card_face` /
/// `empty_state`）走本函数；无 `&App` 的未迁移调用方仍走 [`content_bg`]（P1-2b 收口）。
pub fn content_bg_theme(cx: &App, dark: bool, light: Hsla) -> Hsla {
    if dark {
        Theme::global(cx).chrome.editor_bg.into()
    } else {
        light
    }
}

/// 内容区底色旧签名（无 `&App` 上下文 / 未迁移调用方用）：暗色回退 `ChromeColors` 暗档
/// `editor_bg`——`editor_bg` 无 UI 覆盖面（非 UI_EXPOSABLE_TOKENS、预设不列），默认/定制
/// 下输出恒等于 [`content_bg_theme`]；crates/editor 与 crates/shell 迁移到
/// [`content_bg_theme`] 后本函数仅作测试/回退（保留 `for_mode` 仅此回退，见 P1-2）。
pub fn content_bg(dark: bool, light: Hsla) -> Hsla {
    if dark {
        crate::ui::theme::ChromeColors::for_mode(true).editor_bg.into()
    } else {
        light
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::ThemeMode;
    use gpui::TestAppContext;

    #[gpui::test]
    fn theme_tokens_produce_distinct_light_dark(cx: &mut TestAppContext) {
        cx.update(|cx| {
            Theme::set(ThemeMode::Light, cx);
            let (s_light, b_light) = (card_surface(cx), body_card_face(cx));
            Theme::set(ThemeMode::Dark, cx);
            let (s_dark, b_dark) = (card_surface(cx), body_card_face(cx));
            assert_ne!(s_light, s_dark, "浮动表面底色随明暗切换");
            assert_ne!(b_light, b_dark, "正文区底色随明暗");
            // 颜色收编专项：暗档逐字节 = 原 config 常量（#191A1B / #121314），亮档 = 白——
            // 消费点取值钉死，防「收编后屏幕变色」回归。
            assert_eq!(s_dark, gpui::rgb(0x191a1b).into(), "暗 chrome 底=原 CHROME_BG_DARK");
            assert_eq!(b_dark, gpui::rgb(0x121314).into(), "暗正文底=原 EDITOR_BG_DARK");
            assert_eq!(s_light, gpui::rgb(0xffffff).into(), "亮 chrome 底=白");
            assert_eq!(b_light, gpui::rgb(0xffffff).into(), "亮正文底=白");
        });
    }
}
