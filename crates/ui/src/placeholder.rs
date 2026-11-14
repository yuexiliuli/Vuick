//! 居中占位文本组件（加载中 / 加载失败共用）：铺满可用空间后 flex 居中一行 muted 文本。
//!
//! 收敛各媒体查看器（image_viewer / video / pdf）原先各自手写的同构占位构建器
//! （Pattern 1.1）。可选能力：
//! - `max_w`：给定则文本限定宽度内**自动换行**（长错误文案不被窗口横向截断）；
//! - `glass`：包 `ui::glass_card` 玻璃卡壳（对齐 video 占位的观感）。
//!
//! 纯展示、零自身状态 → `RenderOnce` 组件；颜色/字号由调用方经 builder 注入
//! （默认取 `theme.colors.fg_muted` / 14px，与既有占位一致），见组件约定「样式单一来源」。

use gpui::{
    App, Hsla, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div, px,
};

use crate::ui::theme::Theme;
use crate::ui::{
    CardShellPad, CardShellRadius, CardShellShadow, GlassCardBg, glass_card,
};

/// 默认占位字号（px）：与 image/pdf/video 既有占位一致。
pub const PLACEHOLDER_TEXT_SIZE: f32 = 14.0;

/// 居中占位文本组件（纯展示、零自身状态）。用法：
/// `ui::centered_placeholder("正在加载…").color(muted).max_w(px(240.).0).into_any_element()`。
pub struct CenteredPlaceholder {
    text: SharedString,
    color: Option<Hsla>,
    text_size: f32,
    max_w: Option<f32>,
    glass: bool,
}

/// 构建一个居中占位文本（默认 14px / `fg_muted`；可经 builder 覆写）。
pub fn centered_placeholder(text: impl Into<SharedString>) -> CenteredPlaceholder {
    CenteredPlaceholder {
        text: text.into(),
        color: None,
        text_size: PLACEHOLDER_TEXT_SIZE,
        max_w: None,
        glass: false,
    }
}

impl CenteredPlaceholder {
    /// 文本颜色（默认 `theme.colors.fg_muted`）。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// 字号（默认 14px）。
    pub fn text_size(mut self, size: f32) -> Self {
        self.text_size = size;
        self
    }

    /// 限宽（px）：文本限定到该宽度内自动换行（长错误文案不被横向截断）。
    pub fn max_w(mut self, w: f32) -> Self {
        self.max_w = Some(w);
        self
    }

    /// 包 `ui::glass_card` 玻璃卡壳（对齐 video 占位观感）。
    pub fn glass(mut self, glass: bool) -> Self {
        self.glass = glass;
        self
    }
}

impl RenderOnce for CenteredPlaceholder {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::global(cx);
        let fg = self.color.unwrap_or_else(|| theme.colors.fg_muted.into());
        if self.glass {
            // 玻璃占位壳：走 ui::glass_card 纯壳——Acrylic 默认 α（主题亮 0.80/暗 0.72）、
            // NearDouble 双影 + PxPy(16,8)，与 video 原占位逐值一致。
            let inner = glass_card(
                &theme.colors,
                theme.is_dark(),
                GlassCardBg::Acrylic,
                CardShellShadow::NearDouble,
                CardShellRadius::Lg,
                CardShellPad::PxPy(px(16.), px(8.)),
                None,
            )
            .text_size(px(self.text_size))
            .text_color(fg)
            .child(self.text.to_string());
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(inner)
                .into_any_element();
        }
        let mut txt = div().text_size(px(self.text_size)).text_color(fg);
        if let Some(w) = self.max_w {
            txt = txt.max_w(px(w));
        }
        let txt = txt.child(self.text.to_string());
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(txt)
            .into_any_element()
    }
}

impl IntoElement for CenteredPlaceholder {
    type Element = gpui::ViewElement<Self>;

    #[track_caller]
    fn into_element(self) -> Self::Element {
        gpui::ViewElement::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, Hsla, InteractiveElement, Render, TestAppContext, VisualTestContext,
    };

    struct Harness {
        glass: bool,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let color: Hsla = gpui::hsla(0.60, 0.60, 0.50, 1.0);
            let mut p = centered_placeholder("正在加载…").color(color);
            if self.glass {
                p = p.glass(true);
            }
            div()
                .id("place-wrap")
                .debug_selector(|| "place-wrap".into())
                .w(px(600.))
                .h(px(400.))
                .child(p)
        }
    }

    fn harness(cx: &mut TestAppContext, glass: bool) -> &mut VisualTestContext {
        let (_, cx) = cx.add_window_view(move |_, _| Harness { glass });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx
    }

    /// 普通占位铺满宿主（size_full），绘制不 panic。
    #[gpui::test]
    fn plain_placeholder_fills_host(cx: &mut TestAppContext) {
        let cx = harness(cx, false);
        let wrap = cx.debug_bounds("place-wrap").expect("宿主应在场");
        assert_eq!(wrap.size.width, px(600.), "占位应铺满宿主宽");
        assert_eq!(wrap.size.height, px(400.), "占位应铺满宿主高");
    }

    /// 玻璃卡占位同样铺满宿主且绘制不 panic。
    #[gpui::test]
    fn glass_placeholder_fills_host(cx: &mut TestAppContext) {
        let cx = harness(cx, true);
        let wrap = cx.debug_bounds("place-wrap").expect("宿主应在场");
        assert_eq!(wrap.size.width, px(600.), "玻璃占位应铺满宿主宽");
        assert_eq!(wrap.size.height, px(400.), "玻璃占位应铺满宿主高");
    }
}
