//! 空状态占位：无标签页 / 空目录 / 无搜索命中时的微软风自绘占位（插图 + 文案 + 入口）。
//!
//! 插图采用「变色龙吉祥物」App 图标：圆角方形浅蓝渐变底 + 青蓝圆身变色龙（巨大圆形
//! 单眼 + 螺旋卷尾），来自入库 288×288 PNG（调色板 8bpp，见 [`crate::assets`]），解码成
//! `gpui::RenderImage` 后 `img()` 上屏；固定品牌色板（亮暗主题一致），青蓝色调呼应
//! 「代码/文档/媒体多格式阅读器」的清新气质。
//!
//! 两个入口：
//! - [`empty_state_hero`]：正文区整屏空态（零选项卡欢迎页）——160×120 固定
//!   插图 + 主/副文案 + 可选提示行 + 可选入口按钮（「打开文件…」/「新建文件」）+ 可选
//!   **双击**回调；随窗口自适应居中，仅一次透明度淡入。
//! - [`compact_empty_state`]：侧栏窄面板空态（空目录 / 无搜索命中）——小号灰图标 +
//!   弱化文案，与正文区空态同款灰阶视觉。

use gpui::{
    AnimationExt, App, Hsla, ImageSource, InteractiveElement, IntoElement, ParentElement, Styled,
    div, img, px,
};

use crate::ui::icon::{Icon, IconName};
use crate::ui::motion::hover_spring;
use crate::ui::theme::Theme;

// 变色龙吉祥物图：内嵌 128×128 ICO（PNG 编码）一次性解码成 `gpui::RenderImage`
// （见 [`crate::assets::app_icon_render`]），与自绘标题栏最左图标同源同图。

/// 空态插图（固定 160×120）：变色龙吉祥物 App 图标（自含圆角浅蓝渐变底），
/// 固定品牌色板双主题一致；ICO 解码渲染后 `img()` 上屏。
pub(crate) fn empty_illustration() -> gpui::AnyElement {
    div()
        .id("empty-illustration")
        .debug_selector(|| "empty-illustration".into())
        .relative()
        .w(px(160.))
        .h(px(120.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .id("chameleon-icon")
                .debug_selector(|| "chameleon-icon".into())
                .w(px(112.))
                .h(px(112.))
                .child(
                    img(ImageSource::Render(crate::assets::app_icon_render()))
                        .w(px(112.))
                        .h(px(112.)),
                ),
        )
        .into_any_element()
}

/// 正文区整屏空态：插图 + 主文案 + 副文案（快捷键提示）+ 可选提示行 + 可选入口按钮。
/// 铺满宿主并自适应居中；仅一次透明度淡入（克制——只动绘制、不碰布局）。
/// - `hint`：主/副文案下方一行弱色小字提示（如「双击新建文件」），`None` 不渲染。
/// - `on_double_click`：欢迎页根元素（`empty-state`）的鼠标左键**双击**回调
///   （以 `MouseDownEvent.click_count >= 2` 判定，与编辑区双击同一套语义）。回调
///   由旁路 `Entity::update` 驱动宿主状态，不应 `stop_propagation`（欢迎页是整屏
///   覆盖元素，单击/按钮互不干扰）。
#[allow(clippy::type_complexity)] // 双击回调签名较长（宿主无关回调），拆别名反添一层
pub fn empty_state_hero(
    main: &str,
    sub: &str,
    action: Option<gpui::AnyElement>,
    hint: Option<&str>,
    on_double_click: Option<Box<dyn Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App)>>,
    cx: &mut App,
) -> gpui::AnyElement {
    let c = Theme::global(cx).colors;
    let dark = Theme::global(cx).is_dark();
    let fg: Hsla = c.fg.into();
    let muted: Hsla = c.fg_muted.into();
    // 提示行取最弱一档灰（插图正文线的同款语义 token），字号再小一档。
    let hint_color: Hsla = c.fg_disabled.into();

    let mut root = div()
        .id("empty-state")
        .debug_selector(|| "empty-state".into())
        .size_full()
        .overflow_hidden()
        // 欢迎页底色与编辑区同色（暗色 = theme `editor_bg` #121314，原 config
        // EDITOR_BG_DARK 常量已删）；走合并后 theme（`content_bg_theme`，P1-2）。
        .bg(crate::ui::shell::content_bg_theme(cx, dark, c.bg.into()))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(14.))
        .child(empty_illustration())
        .child(
            div()
                .text_size(px(16.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(fg)
                .child(main.to_string()),
        )
        .child(
            div()
                .text_size(px(12.5))
                .text_color(muted)
                .child(sub.to_string()),
        );
    if let Some(h) = hint {
        root = root.child(
            div()
                .id("empty-hint")
                .debug_selector(|| "empty-hint".into())
                .text_size(px(11.))
                .text_color(hint_color)
                .child(h.to_string()),
        );
    }
    if let Some(a) = action {
        root = root.child(a);
    }
    if let Some(dbl) = on_double_click {
        root = root.on_mouse_down(gpui::MouseButton::Left, dbl);
    }
    root.with_spring(
        "empty-state-fade",
        hover_spring(1.0).from(0.0),
        move |this, v| this.opacity(v),
    )
    .into_any_element()
}

/// 侧栏窄面板的空态占位：小号灰色图标 + 弱化文案（与正文区空态同款灰蓝视觉）。
/// `id` 供测试与样式定位；图标复用 [`IconName`]（Fluent 1.5 线性描边，P2 统一风格）。
/// 纯提示型空态的便捷封装（toc/搜索等调用方保持原签名零改动，见
/// [`compact_empty_state_with_action`]）。
pub fn compact_empty_state(
    id: &'static str,
    icon: IconName,
    text: &str,
    cx: &mut App,
) -> gpui::AnyElement {
    compact_empty_state_with_action(id, icon, text, None, cx)
}

/// 侧栏窄面板的空态占位（带可选操作按钮）：小号灰色图标 + 弱化文案 + 可选追加的
/// 操作按钮（如文件树「打开文件夹…」）。样式与 [`compact_empty_state`] 完全一致，
/// 仅多渲染 `action`（追加在文案之后）；`action` 由调用方自建（含宿主回调，行为留在
/// 宿主），传 `None` 退化为纯提示型空态（= [`compact_empty_state`]）。
/// `id` 供测试与样式定位；图标复用 [`IconName`]（Fluent 1.5 线性描边，P2 统一风格）。
pub fn compact_empty_state_with_action(
    id: &'static str,
    icon: IconName,
    text: &str,
    action: Option<gpui::AnyElement>,
    cx: &mut App,
) -> gpui::AnyElement {
    let c = Theme::global(cx).colors;
    let muted: Hsla = c.fg_muted.into();
    let mut root = div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .px_3()
        .py_4()
        .child(Icon::new(icon).size(px(20.)).text_color(muted))
        .child(
            div()
                .text_size(px(12.))
                .text_color(muted)
                .child(text.to_string()),
        );
    if let Some(a) = action {
        root = root.child(a);
    }
    root.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, VisualTestContext, Window, px};

    /// 空态插图 / 英雄 / 紧凑占位在两种主题下都能渲染出非零尺寸（零 panic、零失败光栅）。
    struct Harness;

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(400.))
                .h(px(400.))
                .child(empty_illustration())
                .child(empty_state_hero("主", "副", None, Some("双击新建文件"), None, cx))
                .child(compact_empty_state("compact-test", IconName::Search, "无结果", cx))
                .child(compact_empty_state_with_action(
                    "compact-action-test",
                    IconName::Folder,
                    "未打开文件夹",
                    Some(
                        div()
                            .id("compact-action-btn")
                            .debug_selector(|| "compact-action-btn".into())
                            .child("打开文件夹…")
                            .into_any_element(),
                    ),
                    cx,
                ))
        }
    }

    fn harness(cx: &mut TestAppContext, dark: bool) -> &mut VisualTestContext {
        // 主题在窗口外一次性种入（不能在 render 内 Theme::set——refresh_windows 会让
        // 测试窗口陷入无限重绘循环）。
        cx.update(|cx| {
            crate::ui::theme::Theme::set(
                if dark {
                    crate::ui::theme::ThemeMode::Dark
                } else {
                    crate::ui::theme::ThemeMode::Light
                },
                cx,
            );
        });
        let (_, cx) = cx.add_window_view(|_, _| Harness);
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx
    }

    #[gpui::test]
    fn empty_state_renders_in_both_themes(cx: &mut TestAppContext) {
        for dark in [false, true] {
            let cx = harness(cx, dark);
            let illus = cx.debug_bounds("empty-illustration").expect("插图应渲染");
            assert!(
                illus.size.width > px(0.) && illus.size.height > px(0.),
                "插图应有尺寸（{dark}）：{illus:?}"
            );
            assert!(
                cx.debug_bounds("chameleon-icon").is_some(),
                "变色龙图标应渲染（{dark}）"
            );
            assert!(
                cx.debug_bounds("empty-state").is_some(),
                "英雄空态应渲染（{dark}）"
            );
            assert!(
                cx.debug_bounds("empty-hint").is_some(),
                "提示行应渲染（{dark}）"
            );
            assert!(
                cx.debug_bounds("compact-test").is_some(),
                "紧凑空态应渲染（{dark}）"
            );
            assert!(
                cx.debug_bounds("compact-action-test").is_some(),
                "带 action 的紧凑空态应渲染（{dark}）"
            );
            assert!(
                cx.debug_bounds("compact-action-btn").is_some(),
                "action 按钮应在紧凑空态中渲染（{dark}）"
            );
        }
    }
}
