//! 外壳卡片化（VS Code 新式「浮层卡片分组」）共享几何与小部件。
//!
//! 每个功能区域 = 圆角矩形卡片，卡间 ~6px 间距，浮在自绘材质层（chrome/mica 分带）
//! 之上；**贴窗边缘的边保持直角贴边**（正文区右缘、侧栏左缘、状态栏底缘、标题栏顶缘），
//! 其余边圆角（8px，= 仓库既有 `rounded_md`）。
//!
//! 颜色一律取 [`crate::ui::theme`] 语义 token（`surface`/`bg`/`border`/`drag_hover_border`），
//! 不散落 `rgba`。卡面由各挂载点自选 `card_surface`（浮层表面）或 `panel_bg`（正文）。

use gpui::{
    AnimationExt, App, Hsla, InteractiveElement, IntoElement, ParentElement, Styled, div, px,
};

use crate::ui::motion::hover_spring;
use crate::ui::theme::{ChromeColors, Theme};

/// 卡片间距（px）：卡间 / 卡与 chrome 之间露出的自绘材质层留白。
pub const CARD_GAP: f32 = 6.0;
/// 卡片圆角（px）：与仓库既有 `rounded_md` 一致。
pub const CARD_RADIUS: f32 = 8.0;
/// 三点手柄：单个圆点直径（px）。
pub const HANDLE_DOT_D: f32 = 4.0;
/// 三点手柄：相邻圆点间距（px），中心距 ≈ `HANDLE_DOT_D + HANDLE_DOT_GAP` ≈ 8。
pub const HANDLE_DOT_GAP: f32 = 4.0;

/// 卡片「浮层表面」底色（theme `surface`：亮白 / 暗 #333，比窗口 chrome/材质体
/// 亮一档，浮在自绘材质层之上；与 `popover_bg` 同源）。
pub fn card_surface(cx: &App) -> Hsla {
    Theme::global(cx).colors.surface.into()
}

/// 卡片描边（theme `border`，与 `bar_border` 同源）。
pub fn card_border(cx: &App) -> Hsla {
    Theme::global(cx).colors.border.into()
}

/// 三点手柄常态弱化色（theme `border` / divider 系）。
pub fn handle_rest_color(cx: &App) -> Hsla {
    Theme::global(cx).colors.border.into()
}

/// 三点手柄 hover / 拖拽高亮色（theme `drag_hover_border`：分栏 splitter / 侧栏右缘
/// 调宽的拖拽强调线，亮暗两档都等于对应 `accent`）。
pub fn handle_accent_color(cx: &App) -> Hsla {
    Theme::global(cx).colors.drag_hover_border.into()
}

/// 正文卡底色（theme `bg`，= 编辑器/预览 `panel_bg` 同一来源），保证卡面与正文无缝。
pub fn body_card_face(cx: &App) -> Hsla {
    let dark = Theme::global(cx).is_dark();
    ChromeColors::for_mode(dark).panel_bg.into()
}

/// 「三点分隔手柄」：三个水平圆点（`HANDLE_DOT_D` 直径、`HANDLE_DOT_GAP` 间距），
/// 在分隔条 / 调宽条中垂直居中。
///
/// 纯视觉指示：常态画弱化色（border/divider），`active`（hover / 拖拽）时上层
/// accent 圆点经统一 hover 弹簧淡入（P2-10，与侧栏右缘调宽细线同款）；**不参与
/// 命中测试**——命中区 / 拖拽逻辑由外层 splitter / resize 条负责，语义完全不变。
pub fn three_dot_grip(grip_key: &str, active: bool, cx: &App) -> impl IntoElement {
    let rest = handle_rest_color(cx);
    let accent = handle_accent_color(cx);
    let grip_key = grip_key.to_string();
    let dot = |color: Hsla| {
        div()
            .w(px(HANDLE_DOT_D))
            .h(px(HANDLE_DOT_D))
            .rounded_full()
            .bg(color)
    };
    let rest_sel = format!("{grip_key}-rest");
    let accent_sel = format!("{grip_key}-accent");
    let rest_row = div()
        .debug_selector(move || rest_sel.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(HANDLE_DOT_GAP))
        .child(dot(rest))
        .child(dot(rest))
        .child(dot(rest));
    let accent_row = div()
        .debug_selector(move || accent_sel.clone())
        .absolute()
        .inset_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(HANDLE_DOT_GAP))
        .child(dot(accent))
        .child(dot(accent))
        .child(dot(accent))
        .with_spring(
            grip_key,
            hover_spring(if active { 1.0 } else { 0.0 }),
            move |this, v| this.opacity(v),
        );
    div()
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .child(rest_row)
        .child(accent_row)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::ThemeMode;
    use gpui::TestAppContext;

    #[gpui::test]
    fn card_and_handle_constants_are_in_spec(_cx: &mut TestAppContext) {
        // 设计规约钉死：卡间距 ~6px、圆角 8px（= rounded_md）、圆点 4px、中心距 ≈8px。
        assert_eq!(CARD_GAP, 6.0, "卡间间距 ~6px");
        assert_eq!(CARD_RADIUS, 8.0, "卡圆角 = rounded_md(8px)");
        assert_eq!(HANDLE_DOT_D, 4.0, "三点手柄圆点直径 ~4px");
        assert!((HANDLE_DOT_D + HANDLE_DOT_GAP - 8.0).abs() < 1e-6, "圆点中心距 ≈8px");
    }

    #[gpui::test]
    fn theme_tokens_produce_distinct_light_dark(cx: &mut TestAppContext) {
        cx.update(|cx| {
            Theme::set(ThemeMode::Light, cx);
            let (s_light, r_light, a_light, b_light) = (
                card_surface(cx),
                handle_rest_color(cx),
                handle_accent_color(cx),
                body_card_face(cx),
            );
            Theme::set(ThemeMode::Dark, cx);
            let (s_dark, r_dark, a_dark, b_dark) = (
                card_surface(cx),
                handle_rest_color(cx),
                handle_accent_color(cx),
                body_card_face(cx),
            );
            assert_ne!(s_light, s_dark, "卡片表面随明暗切换");
            assert_ne!(r_light, r_dark, "手柄常态色随明暗");
            assert_ne!(a_light, a_dark, "手柄高亮色随明暗（亮深蓝/暗亮蓝）");
            assert_ne!(b_light, b_dark, "正文卡底色随明暗");
        });
    }
}
