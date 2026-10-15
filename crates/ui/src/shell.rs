//! 外壳共享几何与小部件（无缝直连形态）。
//!
//! 标题栏 / 选项卡栏 / 侧栏 / 状态栏使用同一 chrome mica 底色直连成片：矩形直角、
//! 区域间以发丝线分隔，不设漂浮圆角卡与缝隙。本模块提供共享色值
//! （`card_surface` / `body_card_face`）供各挂载点取用。
//!
//! 颜色一律取 [`crate::ui::theme`] 语义 token（`surface`/`bg`/`border`/`drag_hover_border`），
//! 不散落 `rgba`。

use gpui::{App, Hsla};

use crate::ui::theme::{ChromeColors, Theme};

/// chrome 区域底色（状态栏等）：暗色 = theme `chrome_bg`（原 config `CHROME_BG_DARK`
/// #191A1B，常量已删，与标题栏/选项卡栏/侧栏同色直连）；亮色 = 同 token 亮档 #FFFFFF
/// （原 theme `surface`，同值）。
pub fn card_surface(cx: &App) -> Hsla {
    ChromeColors::for_mode(Theme::global(cx).is_dark())
        .chrome_bg
        .into()
}

/// 正文区底色：亮色 = theme `panel_bg`（编辑器同一来源）；**暗色 = theme `editor_bg`**
/// （原 config `EDITOR_BG_DARK` #121314，常量已删），与编辑区/预览面板同色。
pub fn body_card_face(cx: &App) -> Hsla {
    let dark = Theme::global(cx).is_dark();
    content_bg(dark, ChromeColors::for_mode(false).panel_bg.into())
}

/// 内容区（编辑区/预览/欢迎页）底色统一入口：暗色恒取 theme `editor_bg`（原 config
/// `EDITOR_BG_DARK` #121314，常量已删），亮色用调用方给的既有底色（theme `bg` /
/// `panel_bg`）。
pub fn content_bg(dark: bool, light: Hsla) -> Hsla {
    if dark {
        ChromeColors::for_mode(true).editor_bg.into()
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
