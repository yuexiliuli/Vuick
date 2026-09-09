//! PDF 书签目录（TOC）面板：折叠逻辑、可见性计算、面板渲染与拖拽调宽。
//!
//! - 纯函数 [`toc_visibility`] / [`toc_node_has_child`] 负责「哪些条目可见」；
//! - [`impl PdfViewer`](PdfViewer) 上的 TOC 方法负责渲染、toggle 展开/折叠/调宽等。

use std::collections::HashSet;

use gpui::{
    div, px, rgb, App, Context, CursorStyle, Hsla, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement, Styled,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{Icon, IconName};

use crate::pdf::PdfOutlineEntry;
use crate::pdf::view::PdfViewer;

/// 目录面板宽度下限（px）——太窄容纳不下标题文字。
pub(crate) const TOC_WIDTH_MIN: f32 = 160.0;
/// 目录面板宽度上限（px）——避免把右侧 PDF 主体挤没。
pub(crate) const TOC_WIDTH_MAX: f32 = 480.0;
/// 目录面板默认宽度（px）。
pub(crate) const TOC_WIDTH_DEFAULT: f32 = 280.0;

/// 断言某个条目是否有下级（可折叠）：pre-order 下紧邻下一项层级更深即有子项。
pub(crate) fn toc_node_has_child(outlines: &[PdfOutlineEntry], idx: usize) -> bool {
    outlines
        .get(idx + 1)
        .is_some_and(|next| next.level > outlines[idx].level)
}

/// 目录折叠可见性：按 pre-order 条目判断每个下标是否应显示。
///
/// 语义（与 `PdfViewer::toc_collapsed` 匹配）：
/// - 条目本身可见，除非它处于某个**已折叠祖先**之下（祖先 index ∈ `collapsed`）。
///   `collapsed` 只含「有下级的条目」（`level[i] < level[i+1]`）。折叠即隐藏全部后代。
/// - 祖先链判定：pre-order 下，条目 i 的祖先 = 在其之前、层级严格递增且未被其后更浅
///   条目截断的那些条目；采用「层级递减栈」维护。任一祖先折叠 → 本条隐藏。
///
/// 纯函数、无副作用，便于单测。
pub(crate) fn toc_visibility(
    levels: &[usize],
    collapsed: &HashSet<usize>,
) -> Vec<bool> {
    let n = levels.len();
    let mut visible = vec![true; n];
    // 祖先栈：保存“截至当前位置仍处于祖先链上的条目下标”（层级严格递增）。
    let mut stack: Vec<usize> = Vec::with_capacity(n.min(64));
    for i in 0..n {
        let lvl = levels[i];
        // 弹出层级 >= 当前条的祖先（它们不再是 lvl 的后代链成员）
        while let Some(&top) = stack.last() {
            if levels[top] >= lvl {
                stack.pop();
            } else {
                break;
            }
        }
        // 祖先链上任一成员折叠 → 本条被隐藏
        let any_collapsed = stack.iter().any(|&a| collapsed.contains(&a));
        visible[i] = !any_collapsed;
        stack.push(i);
    }
    visible
}

impl PdfViewer {
    /// 目录面板配色：与应用工具栏色板一致（避免组件库 surface 在暗色下近黑，突兀）。
    /// 返回 (面板底, 分隔边框, 主文字, 次要文字)。
    pub(crate) fn toc_panel_colors(cx: &App) -> (Hsla, Hsla, Hsla, Hsla) {
        if gpui_component::Theme::global(cx).is_dark() {
            (
                rgb(0x1f2937).into(), // 面板底（同 bar_bg / 页面卡片）
                rgb(0x374151).into(), // 边框（同 bar_border）
                rgb(0xe5e7eb).into(), // 主文字（同 text_primary）
                rgb(0x9ca3af).into(), // 次要文字（同 text_minor）
            )
        } else {
            (
                gpui::white().into(), // 面板底
                rgb(0xe5e7eb).into(), // 边框
                rgb(0x111827).into(), // 主文字
                rgb(0x6b7280).into(), // 次要文字
            )
        }
    }

    /// 切换左侧目录面板开关。
    pub(crate) fn toggle_toc(&mut self, cx: &mut Context<Self>) {
        self.toc_open = !self.toc_open;
        cx.notify();
    }

    /// 关闭左侧目录面板（点击目录项跳转后 / 点击 ✕ 时）。
    pub(crate) fn close_toc(&mut self, cx: &mut Context<Self>) {
        self.toc_open = false;
        cx.notify();
    }

    /// 切换某个目录父项的折叠/展开。只有「有下级」的条目可折叠。
    pub(crate) fn toggle_toc_collapse(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.outlines().len() {
            return;
        }
        if toc_node_has_child(self.outlines(), index) {
            let collapsed = self.toc_collapsed_mut();
            if !collapsed.remove(&index) {
                collapsed.insert(index);
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
            self.toc_width = (start_w + (x - start_x)).clamp(TOC_WIDTH_MIN, TOC_WIDTH_MAX);
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

    /// 浮在 PDF 主体上的目录开关按钮（始终可见，有书签时）。
    pub(crate) fn render_toc_button(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.outlines().is_empty() {
            return Vec::new();
        }
        let theme = gpui_component::Theme::global(cx);
        let colors = theme.semantic_tokens().colors;
        let fg = colors.foreground;
        let weak = cx.weak_entity();
        let toggle = {
            let weak = weak.clone();
            Button::new("pdf-toc-toggle")
                .label(if self.toc_open { "☰ 收起" } else { "☰ 目录" })
                .ghost()
                .h(px(28.))
                .rounded_full()
                .text_size(px(12.5))
                .text_color(fg)
                .on_click(move |_, _w, cx| {
                    let _ = weak.update(cx, |this, cx| this.toggle_toc(cx));
                })
        };
        vec![
            div()
                .id("pdf-toc-open-btn")
                .debug_selector(|| "pdf-toc-open-btn".into())
                .absolute()
                .top(px(8.))
                .right(px(10.))
                .child(toggle)
                .into_any_element(),
        ]
    }

    /// 左侧目录面板：作为 flex 行布局的**同级兄弟**，展开时占 `toc_width`.
    /// 右侧 PDF 主体被挤压缩小（而非浮层遮挡）。
    ///
    /// 关闭或无书签时返回 `None`（完全不参与布局）。
    pub(crate) fn render_toc_panel(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let outlines = self.outlines().to_vec();
        if outlines.is_empty() || !self.toc_open {
            return None;
        }
        let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
        let visible = toc_visibility(&levels, self.toc_collapsed());
        let theme = gpui_component::Theme::global(cx);
        let colors = theme.semantic_tokens().colors;
        // 目录面板配色：与应用工具栏色板一致（暗色下是 1f2937 而非组件库 surface 的近黑）
        let (panel_bg, border, fg, muted) = Self::toc_panel_colors(cx);
        let weak = cx.weak_entity();

        let mut items: Vec<gpui::AnyElement> = Vec::with_capacity(outlines.len());
        for (idx, entry) in outlines.iter().enumerate() {
            if !visible[idx] {
                continue;
            }
            let title = entry.title.clone();
            let level = entry.level;
            let has_page = entry.page_index != usize::MAX;
            let page = entry.page_index;
            let weak = weak.clone();
            let fg = fg.clone();
            // 有下级 → 可折叠
            let has_child = toc_node_has_child(&outlines, idx);
            let is_collapsed = has_child && self.toc_collapsed().contains(&idx);

            let mut row = div()
                .id(SharedString::from(format!("pdf-toc-item-{page}-{idx}")))
                .debug_selector(move || format!("pdf-toc-item-{page}-{idx}"))
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
                // 悬浮：主题化柔和底 + 左侧指示条 + 交互反馈；按下稍微加深
                .hover(|style| style.bg(colors.secondary).text_color(fg))
                .active(|style| style.bg(colors.secondary).opacity(0.85));

            // 左侧当前项指示条（hover 时显现，统一强调蓝）
            let indicator = div()
                .id(SharedString::from(format!("pdf-toc-ind-{idx}")))
                .debug_selector(move || format!("pdf-toc-ind-{idx}"))
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
                    .id(SharedString::from(format!("pdf-toc-caret-{idx}")))
                    .debug_selector(move || format!("pdf-toc-caret-{idx}"))
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
                        .bg(if level == 0 {
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
                        .text_color(if has_page { fg } else { muted })
                        .group_hover("toc-item", |style| style.text_color(fg.clone()))
                        .child(title),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.5))
                        .text_color(muted)
                        .child(if has_page {
                            (page + 1).to_string()
                        } else {
                            "—".into()
                        }),
                );

            let weak_row = weak;
            row = row.on_click(move |_, _w, cx| {
                if has_page {
                    // 跳转到目标页，但保持目录面板打开（不 close_toc）
                    let _ = weak_row.update(cx, |this, cx| {
                        this.scroll_to_page(page);
                        cx.notify();
                    });
                }
            });
            items.push(row.into_any_element());
        }

        let close_btn = {
            let weak = weak.clone();
            // 关闭按钮：手绘置中图标按钮（IconName::Close 矢量 SVG），替代文本 "✕"，
            // 避免不同字体下字形粗细/偏移不一；带 hover/按下反馈。
            div()
                .id("pdf-toc-close")
                .debug_selector(|| "pdf-toc-close".into())
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
                    let _ = weak.update(cx, |this, cx| this.close_toc(cx));
                })
                .child(Icon::new(IconName::Close).size(px(12.)).text_color(muted))
        };

        // 右缘拖拽手柄：宽 6px 命中区，视觉 2px 细线；按下开始拖拽。
        let resize_handle = {
            let weak = weak.clone();
            div()
                .id("pdf-toc-resize")
                .debug_selector(|| "pdf-toc-resize".into())
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
                    |this: &mut PdfViewer, hovered: &bool, _window, cx| {
                        this.toc_resize_hovered = *hovered;
                        cx.notify();
                    },
                ))
                .on_mouse_down(MouseButton::Left, move |ev, _window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.start_toc_resize(ev.position.x.as_f32(), cx)
                    });
                })
                // 命中区透明；平时不画第三条线（面板右缘已有 border 边线），
                // 仅悬浮/拖拽时在命中区中心显示一条强调色竖线作为手势反馈。
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
                .id("pdf-toc-panel")
                .debug_selector(|| "pdf-toc-panel".into())
                .h_full()
                .w(px(self.toc_width))
                .flex_shrink_0()
                .relative()
                .bg(panel_bg)
                .border_r_1()
                .border_color(border)
                .child(
                    div()
                        .id("pdf-toc-header")
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
                        .id("pdf-toc-scroll-wrap")
                        .debug_selector(|| "pdf-toc-scroll-wrap".into())
                        .absolute()
                        .top(px(46.))
                        .bottom(px(0.))
                        .left(px(0.))
                        .right(px(6.))
                        .child(
                            div()
                                .id("pdf-toc-list")
                                .debug_selector(|| "pdf-toc-list".into())
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(&self.toc_scroll)
                                .child(
                                    div()
                                        .id("pdf-toc-list-inner")
                                        .flex()
                                        .flex_col()
                                        .children(items),
                                ),
                        )
                        // 纵向滚动条：跟随 toc_scroll，滚动时显示（Scrolling 模式）
                        .child(
                            div()
                                .id("pdf-toc-vscroll")
                                .debug_selector(|| "pdf-toc-vscroll".into())
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

    /// 测试辅助：注入任意书签（默认先有 2 条），用于驱动 TOC 面板展开的布局回归测试。
    #[cfg(test)]
    pub(crate) fn seed_outlines_for_test(&mut self) {
        self.outlines = vec![
            PdfOutlineEntry {
                title: "测试章节".into(),
                level: 0,
                page_index: 0,
            },
            PdfOutlineEntry {
                title: "测试小节".into(),
                level: 1,
                page_index: 1,
            },
        ];
    }

    /// 测试辅助：直接置位目录展开（不依赖真实书签解析结果）。
    #[cfg(test)]
    pub(crate) fn force_toc_open_for_test(&mut self) {
        self.toc_open = true;
    }

    /// 测试辅助：读取折叠集合（校验折叠交互结果）。
    #[cfg(test)]
    pub(crate) fn toc_collapsed_for_test(&self) -> &HashSet<usize> {
        self.toc_collapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_visibility_all_visible_when_nothing_collapsed() {
        // 默认（未折叠）→ 全部可见
        let levels = vec![0, 1, 2, 1, 0, 1];
        let collapsed = HashSet::new();
        let v = toc_visibility(&levels, &collapsed);
        assert!(v.into_iter().all(|x| x), "未折叠时应全部可见");
    }

    #[test]
    fn toc_visibility_hides_descendants_of_collapsed_parent() {
        // 0: L0  ├1: L1  │└2: L2  ├3: L1  │└4: L2  └5: L1
        let levels = vec![0, 1, 2, 1, 2, 1];
        let mut collapsed = HashSet::new();
        // 折叠第 0 项（L0）→ 全部后代(1..=5)隐藏，仅第 0 项可见
        collapsed.insert(0);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, false, false, false, false, false],
            "折叠 L0 应隐藏其后全部后代"
        );

        // 折叠第 1 项（L1，父 0 未折叠）→ 只隐藏它的直系后代 2；3/4/5 不受影响
        let collapsed = HashSet::from([1usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, true, false, true, true, true],
            "折叠 L1(1) 只应隐藏其后代 2"
        );

        // 折叠第 3 项（L1 的第二个子树）→ 隐藏 4
        let collapsed = HashSet::from([3usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, true, true, true, false, true],
            "折叠 L1(3) 只应隐藏其后代 4"
        );

        // 同时折叠 0 与 3 → 全部后代都隐藏
        let collapsed = HashSet::from([0usize, 3usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, false, false, false, false, false],
            "折叠 L0 后 3 及其后代 4 均被 L0 遮蔽"
        );

        // 折叠一个无子项的条目（没有下级）→ 无效果
        let collapsed = HashSet::from([2usize]); // L2，下一项 L1 < L2 → 无子项
        let v = toc_visibility(&levels, &collapsed);
        assert!(
            v.into_iter().all(|x| x),
            "无子项的条目在折叠集合中也不应影响任何可见性"
        );
    }

    #[test]
    fn toc_visibility_nested_sibling_boundary() {
        // 兄弟边界：折叠第 1 项(L1) 后，第 3 项(L1，为 0 的第二子)应仍可见。
        let levels = vec![0, 1, 2, 1, 0, 1];
        let collapsed = HashSet::from([1usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, true, false, true, true, true],
            "折叠第一个 L1 不应影响第 0 的第二个孩子 L1(3)"
        );
    }

    #[test]
    fn toggle_collapse_only_affects_nodes_with_children() {
        // 构建 outlines，直接测真实判定 helper：有下级才可折叠
        let outlines = vec![
            PdfOutlineEntry { title: "A".into(), level: 0, page_index: 0 },
            PdfOutlineEntry { title: "A1".into(), level: 1, page_index: 1 },
            PdfOutlineEntry { title: "A2".into(), level: 1, page_index: 2 },
            PdfOutlineEntry { title: "B".into(), level: 0, page_index: 3 },
            PdfOutlineEntry { title: "B1".into(), level: 1, page_index: 4 },
        ];
        assert!(toc_node_has_child(&outlines, 0), "A(L0) 有子项 A1");
        assert!(!toc_node_has_child(&outlines, 1), "A1(L1) 下一项同级 → 无子项");
        assert!(!toc_node_has_child(&outlines, 2), "A2(L1) 下一项更浅 → 无子项");
        assert!(toc_node_has_child(&outlines, 3), "B(L0) 有子项 B1");
        assert!(!toc_node_has_child(&outlines, 4), "末项无子项");
    }
}
