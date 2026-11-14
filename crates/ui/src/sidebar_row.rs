//! 侧栏列表行（`SidebarRow`）：目录 / 搜索 / 文件树等侧栏单行的统一行壳。
//!
//! 行壳固定 `flex_row + gap_1 + px_2 + py_1 + mx4 + rounded_md`（对齐 `shell_sidebar`
//! 搜索/目录行真实观感），hover 背景经 [`ui::motion::hoverable`] 淡入 `fill`（悬停态键控
//! + flip-only notify + 弹簧淡入，曲线全站统一，`reduce_motion` 自动尊重）。可选前导
//! 元素（图标等）+ 主标题（**文本样式由调用方烘进 title**，本组件不硬编码字号/颜色——
//! 现调用点散落的 text_size/text_color 迁移时随 title 一并上移）+ 可选点击。
//!
//! 交互性**由是否挂 `on_click` 决定**：挂 → 手型 cursor + click 上抛；不挂 → 仅 hover 高亮。

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, CursorStyle, ElementId, Hsla, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, StatefulInteractiveElement, Styled, ViewElement, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::ui::motion::{element_id_label, hoverable};

/// 侧栏列表行：`flex_row` 行壳 + 可选前导元素 + 主标题 + 可选点击；hover 背景淡入
/// 由 [`hoverable`] 接线（弹簧键/悬停态键由 id 派生，与元素自身 id 不撞键）。
pub struct SidebarRow {
    id: ElementId,
    leading: Option<AnyElement>,
    title: AnyElement,
    /// hover 背景 target 色（调用方从 theme 取；自身 alpha 保留，透明度由弹簧 0..1 调制）。
    fill: Hsla,
    on_click: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>,
}

impl SidebarRow {
    /// 以调用方指定的 id 构建（debug_selector 随 id 派生，供 `debug_bounds` 定位）。
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            leading: None,
            title: div().into_any_element(),
            fill: gpui::transparent_black(),
            on_click: None,
        }
    }

    /// 前导元素（图标等）；不设则不渲染。
    pub fn leading(mut self, leading: impl IntoElement) -> Self {
        self.leading = Some(leading.into_any_element());
        self
    }

    /// 主标题（任意 `IntoElement`；文本尺寸/颜色由调用方自行烘入）。
    pub fn title(mut self, title: impl IntoElement) -> Self {
        self.title = title.into_any_element();
        self
    }

    /// hover 背景 target 色（theme 语义色；透明度由 spring 调制）。
    pub fn fill(mut self, fill: Hsla) -> Self {
        self.fill = fill;
        self
    }

    /// 注册点击回调（可交互：手型 cursor + click 上抛）；不调则仅 hover 高亮、无监听。
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for SidebarRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let label = element_id_label(&id);
        let hover_id = id.clone();
        let fill = self.fill;
        let on_click = self.on_click;
        let mut root = div()
            .id(id)
            .debug_selector(move || label.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .mx(px(4.))
            .rounded_md();
        // 前导元素直挂（对齐 `shell_sidebar` 搜索行：图标非 shrink 包装，随行收缩取
        // 决于具体图标——行宽由标题 `flex_1` 弹性吸收）。
        if let Some(leading) = self.leading {
            root = root.child(leading);
        }
        // 主标题包装：`flex_1 + min_w0 + overflow_hidden + whitespace_nowrap`（搜索/目录
        // 行同款弹性截断；文本样式在 title 元素上，这里不加 text_*）。
        root = root.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(self.title),
        );
        // 可交互才给手型 cursor + 点击监听（与 `ui::Pill` 同一门控语义）。
        root = root.when_some(on_click, |this, cb| {
            this.cursor(CursorStyle::PointingHand)
                .on_click(move |event, window, cx| cb(event, window, cx))
        });
        // hover 背景弹簧淡入（悬停态 + flip-only notify + 弹簧键都由 hoverable 内部经
        // id 派生，宿主持有 `{label}-hover` / `{label}-hover-bg` 键控状态）。
        hoverable(root, hover_id, fill, window, cx)
    }
}

impl IntoElement for SidebarRow {
    type Element = ViewElement<Self>;

    #[track_caller]
    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, Modifiers, Render, TestAppContext, VisualTestContext, point, px,
    };
    use std::{cell::Cell, rc::Rc};

    /// 测试宿主：计数宿主 render 次数（`hoverable` 的 `on_hover → notify(view_id)`
    /// 会把每次 hover 切换反映成一次宿主重绘，见 `motion.rs` 同款测试）。
    struct Harness {
        with_leading: bool,
        interactive: bool,
        clicks: Rc<Cell<usize>>,
        render_count: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.render_count.set(self.render_count.get() + 1);
            let clicks = self.clicks.clone();
            let mut row = SidebarRow::new("sidebar-row")
                .fill(gpui::hsla(0.60, 0.60, 0.60, 1.0))
                .title(div().text_size(px(14.)).child("Sidebar row"));
            if self.with_leading {
                row = row.leading(
                    div()
                        .debug_selector(|| "sidebar-row-leading".into())
                        .flex_shrink_0()
                        .size(px(13.))
                        .child("◆"),
                );
            }
            if self.interactive {
                row = row.on_click(move |_, _, _| clicks.set(clicks.get() + 1));
            }
            div()
                .id("wrap")
                .w(px(300.))
                .h(px(200.))
                .child(row)
        }
    }

    fn harness(
        cx: &mut TestAppContext,
        with_leading: bool,
        interactive: bool,
    ) -> (&mut VisualTestContext, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0));
        let render_count = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let render_count = render_count.clone();
            move |_, _| Harness {
                with_leading,
                interactive,
                clicks,
                render_count,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        (cx, clicks, render_count)
    }

    /// 渲染：标题文本参与横向布局（宽>0），前导元素渲出 13×13 独立子元素；整链路无
    /// panic。（行宽 = 父容器满宽、标题 `flex_1` 吸收富余，故不断「带前导更宽」。）
    #[gpui::test]
    fn renders_with_title_and_leading(cx: &mut TestAppContext) {
        let (cx0, _, _) = harness(cx, false, false);
        let plain = cx0.debug_bounds("sidebar-row").expect("行应渲染");
        assert!(
            plain.size.width.as_f32() > 0.,
            "title 文本应参与布局（宽>0）"
        );
        let (cx1, _, _) = harness(cx, true, false);
        let row = cx1.debug_bounds("sidebar-row").expect("带前导行应渲染");
        assert!(row.size.width.as_f32() > 0., "行应在场");
        let leading = cx1
            .debug_bounds("sidebar-row-leading")
            .expect("前导元素应渲出独立子元素");
        assert_eq!(leading.size.width, px(13.), "前导图标宽 = 13px");
    }

    /// 交互（挂 on_click）：点击命中区触发回调一次。
    #[gpui::test]
    fn click_fires_on_click(cx: &mut TestAppContext) {
        let (cx, clicks, _) = harness(cx, false, true);
        let row = cx.debug_bounds("sidebar-row").expect("行应在场");
        cx.simulate_click(row.center(), Modifiers::default());
        assert_eq!(clicks.get(), 1, "点击行应触发 on_click");
    }

    /// hover：移入目标 → `hoverable` 置悬停态并 notify 宿主（重绘一次）；移出 → 再
    /// 重绘一次。两段都接线、整链路无 panic（镜像 `motion.rs` hover 测试姿势）。
    #[gpui::test]
    fn hover_toggles_notify_host_on_enter_and_leave(cx: &mut TestAppContext) {
        let (cx, _, render_count) = harness(cx, false, false);
        let row = cx.debug_bounds("sidebar-row").expect("行应在场");
        let base = render_count.get();

        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            render_count.get() > base,
            "悬停进入应触发宿主重绘，base={base} now={}",
            render_count.get()
        );
        let after_enter = render_count.get();

        // 移出到行外（仍在窗口内）：指针下无该行，悬停过渡靠下一拍重绘触发
        // on_hover(false) → notify。
        cx.simulate_mouse_move(point(px(150.), px(100.)), None, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            render_count.get() > after_enter,
            "悬停移出应再次触发宿主重绘，after_enter={after_enter} now={}",
            render_count.get()
        );
    }
}
