//! 通用文档目录（TOC）：与文档类型无关的树折叠/可见性纯函数 + 单条目录行的渲染。
//!
//! 目录统一由**侧边栏**提供（见 [`crate::app::sidebar`]），Markdown 标题与 PDF 书签都走
//! 这套公共实现；未来任何阅读器（EPUB、PDF…）只需喂入「层级序列」即可复用：
//!
//! - [`toc_node_has_child`] / [`toc_visibility`]：pre-order 树折叠可见性（无副作用、可单测）
//! - [`toc_item_frame`]：单条目录行可视件（hover 左指示条 + 折叠箭头 + 圆点 + 标题 + 页码），
//!   选择/折叠行为由调用方注入回调

use std::collections::HashSet;

use gpui::{
    div, px, App, AnimationExt, ClickEvent, CursorStyle, Hsla, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window,
};
use crate::ui::icon::{Icon, IconName};
use crate::ui::motion::hover_spring;

/// 断言 pre-order 条目 `idx` 是否有下级（可折叠）：紧邻下一项层级更深即有子项。
pub(crate) fn toc_node_has_child(levels: &[usize], idx: usize) -> bool {
    levels
        .get(idx + 1)
        .is_some_and(|&next| next > levels[idx])
}

/// 目录折叠可见性：按 pre-order 条目判断每个下标是否应显示。
///
/// 语义：
/// - 条目本身可见，除非它处于某个**已折叠祖先**之下（祖先 index ∈ `collapsed`）。
///   `collapsed` 只含「有下级的条目」。折叠即隐藏全部后代。
/// - 祖先链判定：pre-order 下，条目 i 的祖先 = 在其之前、层级严格递增且未被其后更浅
///   条目截断的那些条目；采用「层级递减栈」维护。任一祖先折叠 → 本条隐藏。
///
/// 纯函数、无副作用，便于单测。
pub(crate) fn toc_visibility(levels: &[usize], collapsed: &HashSet<usize>) -> Vec<bool> {
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

/// 单条目录行可视件：组 hover 左侧指示条 + 折叠箭头列 + 圆点 + 标题 + 右侧页码；
/// **整行可点跳转**（`on_select`），折叠箭头只折叠/展开（`on_toggle_collapse`，仅
/// `has_child` 时生效）。`top_accent`：顶级条目用强调色圆点；`page_label`：右侧页码
/// 文本（PDF 用页+1，Markdown 无页传 `None`）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn toc_item_frame(
    row_id: String,
    caret_id: String,
    level: usize,
    title: String,
    has_child: bool,
    is_collapsed: bool,
    top_accent: bool,
    page_label: Option<String>,
    fg: Hsla,
    muted: Hsla,
    hover_bg: Hsla,
    accent: Hsla,
    window: &mut Window,
    cx: &mut App,
    on_select: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_toggle_collapse: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let id2 = row_id.clone();
    let row_key = row_id.clone();
    // P2-10：hover 状态跨帧记录 → 行背景与左侧指示条淡入。
    let hover = window.use_keyed_state(format!("toc-{row_key}"), cx, |_, _| false);
    let hovered = *hover.read(cx);
    let view_id = window.current_view();
    let mut row = div()
        .id(SharedString::from(row_id))
        .debug_selector(move || id2.clone())
        .pl(px(6. + level as f32 * 14.))
        .pr(px(8.))
        .py(px(5.))
        .w_full()
        .relative()
        .flex()
        .items_center()
        .gap(px(6.))
        .cursor(CursorStyle::PointingHand)
        .group("toc-item")
        .rounded(px(4.))
        // hover 时的文字同色常驻（原 hover 把文字提到 fg，此处恒等消除闪变）
        .text_color(fg)
        .on_hover({
            let st = hover.clone();
            move |h, _w, cx| {
                st.update(cx, |v, _| *v = *h);
                cx.notify(view_id);
            }
        })
        .active(|style| style.bg(hover_bg).opacity(0.85));

    // 左侧当前项指示条（hover 时淡入，统一强调蓝）
    let indicator = div()
        .absolute()
        .top(px(5.))
        .bottom(px(5.))
        .left(px(2.))
        .w(px(2.))
        .rounded_full()
        .bg(accent)
        .with_spring(
            format!("toc-ind-{row_key}"),
            hover_spring(if hovered { 1.0 } else { 0.0 }),
            |this, v| this.opacity(v),
        );
    row = row.child(indicator);

    // 折叠箭头（有下级时）：点击只折叠/展开，不跳转
    if has_child {
        let chevron = if is_collapsed {
            Icon::new(IconName::ChevronRight)
        } else {
            Icon::new(IconName::ChevronDown)
        }
        .size(px(12.))
        .text_color(muted);
        let caret_id2 = caret_id.clone();
        let caret = div()
            .id(SharedString::from(caret_id))
            .debug_selector(move || caret_id2.clone())
            .w(px(16.))
            .h(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .text_color(muted)
            .group_hover("toc-item", |style| style.text_color(fg))
            .hover(|style| style.bg(hover_bg))
            .on_click(on_toggle_collapse)
            .child(chevron);
        row = row.child(caret);
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
                .bg(if top_accent {
                    accent
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
                // 收窄目录超出条目宽度时，以 … 提示后面还有内容（配合 overflow_hidden）
                .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                .text_color(fg)
                .group_hover("toc-item", |style| style.text_color(fg))
                .child(title),
        );

    // 右侧页码（PDF 书签）：灰色小号页码
    if let Some(page) = page_label {
        row = row.child(
            div()
                .flex_shrink_0()
                .pl_2()
                .text_size(px(10.5))
                .text_color(muted)
                .child(page),
        );
    }

    row.on_click(on_select)
        // P2-10：hover 背景淡入。
        .with_spring(
            format!("toc-bg-{row_key}"),
            hover_spring(if hovered { 1.0 } else { 0.0 }),
            move |this, v| this.bg(hover_bg.opacity(v)),
        )
        .into_any_element()
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
    fn toc_node_has_child_only_for_nodes_with_deeper_next() {
        // 直接测真实判定 helper：有下级（下一项更深）才可折叠
        let levels = vec![0usize, 1, 1, 0, 1];
        assert!(toc_node_has_child(&levels, 0), "L0(0) 下一项 L1 更深");
        assert!(!toc_node_has_child(&levels, 1), "L1(1) 下一项同级 → 无子项");
        assert!(!toc_node_has_child(&levels, 2), "L1(2) 下一项更浅 → 无子项");
        assert!(toc_node_has_child(&levels, 3), "L0(3) 下一项 L1 更深");
        assert!(!toc_node_has_child(&levels, 4), "末项无子项");
    }
}
