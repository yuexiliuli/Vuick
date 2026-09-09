//! EPUB 阅读的左侧目录面板（PDF 书签目录同款样式）。
//!
//! 实现与 [`crate::pdf::toc`] 对齐——左侧面板占宽挤压预览主体、可折叠树、
//! 右缘拖拽调宽、纵向滚动条、右上角「☰ 目录/收起」开关；数据来自
//! [`MarkdownPreview::toc`]（行号 / 标题级别 / 标题文本），点击条目跳转到
//! 对应标题行。
//!
//! 通过 [`MarkdownPreview::set_toc_panel`] 开启（EPUB 打开时由应用设置）；
//! 普通 Markdown 预览仍用右上角下拉目录，不受影响。

use gpui::{
    div, px, rgb, App, Context, CursorStyle, Hsla, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement, Styled,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{Icon, IconName};

use super::MarkdownPreview;
use crate::pdf::toc_visibility;

/// 目录面板宽度下限（px）。
pub(crate) const TOC_PANEL_WIDTH_MIN: f32 = 160.0;
/// 目录面板宽度上限（px）。
pub(crate) const TOC_PANEL_WIDTH_MAX: f32 = 480.0;
/// 目录面板默认宽度（px）。
pub(crate) const TOC_PANEL_WIDTH_DEFAULT: f32 = 280.0;

/// 断言条目是否有下级（可折叠）：pre-order 下紧邻下一项层级更深即有子项。
/// （与 [`crate::pdf::toc::toc_node_has_child`] 同逻辑，这里按标题层级数组实现。）
fn toc_node_has_child(levels: &[usize], idx: usize) -> bool {
    levels.get(idx + 1).is_some_and(|&next| next > levels[idx])
}

impl MarkdownPreview {
    /// 目录面板配色：与 PDF 书签面板一致（`pdf::toc::toc_panel_colors`）。
    /// 返回 (面板底, 分隔边框, 主文字, 次要文字)。
    pub(crate) fn toc_panel_colors(cx: &App) -> (Hsla, Hsla, Hsla, Hsla) {
        if gpui_component::Theme::global(cx).is_dark() {
            (
                rgb(0x1f2937).into(),
                rgb(0x374151).into(),
                rgb(0xe5e7eb).into(),
                rgb(0x9ca3af).into(),
            )
        } else {
            (
                gpui::white(),
                rgb(0xe5e7eb).into(),
                rgb(0x111827).into(),
                rgb(0x6b7280).into(),
            )
        }
    }

    /// 切换左侧目录面板开关（右上角按钮）。
    pub(crate) fn toggle_toc_panel(&mut self, cx: &mut Context<Self>) {
        self.toc_open = !self.toc_open;
        cx.notify();
    }

    /// 关闭左侧目录面板（点击 ✕ 时）。
    pub(crate) fn close_toc_panel(&mut self, cx: &mut Context<Self>) {
        self.toc_open = false;
        cx.notify();
    }

    /// 切换某个目录父项的折叠/展开。只有「有下级」的条目可折叠。
    pub(crate) fn toggle_toc_collapse(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.toc.len() {
            return;
        }
        let levels: Vec<usize> = self.toc.iter().map(|(_, l, _)| *l as usize).collect();
        if toc_node_has_child(&levels, index) {
            if !self.toc_collapsed.remove(&index) {
                self.toc_collapsed.insert(index);
            }
            cx.notify();
        }
    }

    /// 开始拖拽目录右缘：记录 (起始 x, 起始宽)。
    pub(crate) fn start_toc_resize(&mut self, start_x: f32, cx: &mut Context<Self>) {
        self.toc_drag = Some((start_x, self.toc_width));
        cx.notify();
    }

    /// 拖拽中：按鼠标位移更新面板宽（自变量窗口坐标 x）。宽度 clamp 在 [MIN, MAX]。
    pub(crate) fn update_toc_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_w)) = self.toc_drag {
            self.toc_width =
                (start_w + (x - start_x)).clamp(TOC_PANEL_WIDTH_MIN, TOC_PANEL_WIDTH_MAX);
            cx.notify();
        }
    }

    /// 结束拖拽。
    pub(crate) fn end_toc_resize(&mut self, cx: &mut Context<Self>) {
        if self.toc_drag.is_some() {
            self.toc_drag = None;
            cx.notify();
        }
    }

    /// 浮在预览主体上的目录开关按钮（面板模式、有标题时始终可见）。
    pub(crate) fn render_toc_button(&self, cx: &Context<Self>) -> Vec<gpui::AnyElement> {
        if !self.toc_panel || self.toc.is_empty() {
            return Vec::new();
        }
        let theme = gpui_component::Theme::global(cx);
        let colors = theme.semantic_tokens().colors;
        let fg = colors.foreground;
        let weak = cx.weak_entity();
        let toggle = {
            let weak = weak.clone();
            Button::new("epub-toc-toggle")
                .label(if self.toc_open { "☰ 收起" } else { "☰ 目录" })
                .ghost()
                .h(px(28.))
                .rounded_full()
                .text_size(px(12.5))
                .text_color(fg)
                .on_click(move |_, _w, cx| {
                    let _ = weak.update(cx, |this, cx| this.toggle_toc_panel(cx));
                })
        };
        vec![
            div()
                .id("epub-toc-open-btn")
                .debug_selector(|| "epub-toc-open-btn".into())
                .absolute()
                .top(px(8.))
                .right(px(10.))
                .child(toggle)
                .into_any_element(),
        ]
    }

    /// 左侧目录面板：作为 flex 行布局的**同级兄弟**，展开时占 `toc_width`，
    /// 右侧预览主体被挤压缩小（而非浮层遮挡）。关闭/无标题时返回 `None`。
    pub(crate) fn render_toc_panel(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.toc_panel || !self.toc_open {
            return None;
        }
        let toc = self.toc.clone();
        if toc.is_empty() {
            return None;
        }
        let levels: Vec<usize> = toc.iter().map(|(_, l, _)| *l as usize).collect();
        let visible = toc_visibility(&levels, &self.toc_collapsed);
        let theme = gpui_component::Theme::global(cx);
        let colors = theme.semantic_tokens().colors;
        let (panel_bg, border, fg, muted) = Self::toc_panel_colors(cx);
        let weak = cx.weak_entity();

        let mut items: Vec<gpui::AnyElement> = Vec::with_capacity(toc.len());
        for (idx, &(row_ix, lvl, ref text)) in toc.iter().enumerate() {
            if !visible[idx] {
                continue;
            }
            let level = lvl as usize;
            let title = text.clone();
            let has_child = toc_node_has_child(&levels, idx);
            let is_collapsed = has_child && self.toc_collapsed.contains(&idx);
            let weak = weak.clone();

            let mut row = div()
                .id(SharedString::from(format!("epub-toc-item-{idx}")))
                .debug_selector(move || format!("epub-toc-item-{idx}"))
                .pl(px(6. + level as f32 * 14.))
                .pr(px(8.))
                .py(px(5.))
                .w_full()
                .relative()
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .group("toc-item")
                .rounded(px(4.))
                .hover(|style| style.bg(colors.secondary).text_color(fg))
                .active(|style| style.bg(colors.secondary).opacity(0.85));

            // 左侧当前项指示条（hover 时显现，统一强调蓝）
            let indicator = div()
                .id(SharedString::from(format!("epub-toc-ind-{idx}")))
                .debug_selector(move || format!("epub-toc-ind-{idx}"))
                .absolute()
                .top(px(5.))
                .bottom(px(5.))
                .left(px(2.))
                .w(px(2.))
                .rounded_full()
                .bg(rgb(crate::scrollbar::RESIZE_ACCENT))
                .opacity(0.0)
                .group_hover("toc-item", |style| style.opacity(1.0));
            row = row.child(indicator);

            // 折叠箭头（有下级时）：点击只折叠/展开，不跳转
            if has_child {
                let weak = weak.clone();
                let chevron = if is_collapsed {
                    Icon::new(IconName::ChevronRight)
                } else {
                    Icon::new(IconName::ChevronDown)
                }
                .size(px(12.))
                .text_color(muted);
                let chevron_btn = div()
                    .id(SharedString::from(format!("epub-toc-caret-{idx}")))
                    .debug_selector(move || format!("epub-toc-caret-{idx}"))
                    .w(px(16.))
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .text_color(muted)
                    .group_hover("toc-item", |style| style.text_color(colors.foreground))
                    .hover(|style| style.bg(colors.secondary))
                    .on_click(move |_, _w, cx| {
                        cx.stop_propagation();
                        let _ = weak.update(cx, |this, cx| this.toggle_toc_collapse(idx, cx));
                    })
                    .child(chevron);
                row = row.child(chevron_btn);
            } else {
                // 无子项：占位宽度对齐箭头列
                row = row.child(div().w(px(16.)));
            }

            row = row
                .child(
                    div()
                        .w(px(5.))
                        .h(px(5.))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(if level == 1 {
                            rgb(crate::scrollbar::RESIZE_ACCENT).into()
                        } else {
                            muted
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(13.))
                        .text_color(fg)
                        .group_hover("toc-item", |style| style.text_color(fg))
                        .child(title),
                );

            let weak_row = weak;
            row = row.on_click(move |_, _w, cx| {
                let _ = weak_row.update(cx, |this, cx| {
                    this.scroll_to_row(row_ix);
                    cx.notify();
                });
            });
            items.push(row.into_any_element());
        }

        let close_btn = {
            let weak = weak.clone();
            // 关闭按钮：图标按钮（IconName::Close 矢量 SVG），替代文本 "✕"，
            // 避免不同字体下字形粗细/偏移不一；带 hover/按下反馈。
            div()
                .id("epub-toc-close")
                .debug_selector(|| "epub-toc-close".into())
                .w(px(22.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(colors.secondary))
                .active(|style| style.bg(colors.secondary).opacity(0.85))
                .on_click(move |_, _w, cx| {
                    let _ = weak.update(cx, |this, cx| this.close_toc_panel(cx));
                })
                .child(Icon::new(IconName::Close).size(px(12.)).text_color(muted))
        };

        // 右缘拖拽手柄：宽 6px 命中区，视觉 2px 细线；按下开始拖拽。
        let resize_handle = {
            let weak = weak.clone();
            div()
                .id("epub-toc-resize")
                .debug_selector(|| "epub-toc-resize".into())
                .absolute()
                .right(px(0.))
                .top(px(0.))
                .bottom(px(0.))
                .w(px(6.))
                .flex()
                .items_stretch()
                .justify_center()
                .cursor(CursorStyle::ResizeColumn)
                .on_hover(cx.listener(
                    |this: &mut MarkdownPreview, hovered: &bool, _window, cx| {
                        this.toc_resize_hovered = *hovered;
                        cx.notify();
                    },
                ))
                .on_mouse_down(MouseButton::Left, move |ev, _window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.start_toc_resize(ev.position.x.as_f32(), cx)
                    });
                })
                .child(
                    div()
                        .w(px(2.))
                        .h_full()
                        .flex_none()
                        .rounded_full()
                        .opacity(if self.toc_drag.is_some() || self.toc_resize_hovered {
                            1.0
                        } else {
                            0.0
                        })
                        .bg(rgb(crate::scrollbar::RESIZE_ACCENT)),
                )
        };

        Some(
            div()
                .id("epub-toc-panel")
                .debug_selector(|| "epub-toc-panel".into())
                .h_full()
                .w(px(self.toc_width))
                .flex_shrink_0()
                .relative()
                .bg(panel_bg)
                .border_r_1()
                .border_color(border)
                .child(
                    div()
                        .id("epub-toc-header")
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(12.))
                        .h(px(40.))
                        .border_b_1()
                        .border_color(border)
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(fg)
                                .child("目录"),
                        )
                        .child(close_btn),
                )
                .child(
                    div()
                        .id("epub-toc-scroll-wrap")
                        .debug_selector(|| "epub-toc-scroll-wrap".into())
                        .absolute()
                        .top(px(46.))
                        .bottom(px(0.))
                        .left(px(0.))
                        .right(px(6.))
                        .child(
                            div()
                                .id("epub-toc-list")
                                .debug_selector(|| "epub-toc-list".into())
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(&self.toc_scroll)
                                .child(
                                    div()
                                        .id("epub-toc-list-inner")
                                        .flex()
                                        .flex_col()
                                        .children(items),
                                ),
                        )
                        // 纵向滚动条：跟随 toc_scroll，滚动时显示（Scrolling 模式）
                        .child(
                            div()
                                .id("epub-toc-vscroll")
                                .debug_selector(|| "epub-toc-vscroll".into())
                                .absolute()
                                .inset_0()
                                .child(
                                    Scrollbar::vertical(&self.toc_scroll)
                                        .mode(ScrollbarMode::Scrolling)
                                        .viewport_from_layout(),
                                ),
                        ),
                )
                .child(resize_handle)
                .into_any_element(),
        )
    }
}
