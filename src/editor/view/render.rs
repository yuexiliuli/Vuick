//! 渲染层：`Render`/`Focusable`/`EventEmitter` 实现 + 行/gutter/滚动条/输入包装
//! 元素等渲染助手（`render()` 从「行数据窗口 → UI 骨架」的全阶段与行级元素构造）。

use std::cell::Cell;
use std::ops::Range;

use gpui::{
    div, list, point, px, size, AnyElement, App, Bounds, Context, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, EventEmitter, Focusable, FocusHandle, GlobalElementId, HighlightStyle,
    Hsla, InteractiveElement, IntoElement, LayoutId, ListState, MouseButton, ParentElement, Pixels,
    Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled,
    StyledText, Window, prelude::FluentBuilder as _,
};
use gpui_component::input::{
    Backspace, Copy, Cut, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, Enter,
    IndentInline, MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight,
    MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveUp, OutdentInline, Paste,
    Redo, SelectAll, SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart,
    SelectToStart, SelectToStartOfLine, Undo,
};
use gpui_component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};

use crate::config::layout::{
    BRACKET_GUIDE_ACTIVE_WIDTH, BRACKET_GUIDE_WIDTH, EDITOR_RULER_COLUMNS,
};
use super::{
    CARET_WIDTH_PX, CodeEditor, EditorEvent, GUTTER_LEFT_PAD_PX, GUTTER_RIGHT_PAD_PX,
    MIN_VISIBLE_ROWS, RowData, RowView,
};

// -------------------------------------------------------------------------
// Render / Focusable / 事件
// -------------------------------------------------------------------------

impl Focusable for CodeEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<EditorEvent> for CodeEditor {}

impl Render for CodeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let avail_w = self.sync_render_state(window, cx);

        // 行级数据窗口（可见 + 过扫）——vis_top 用**绝对视觉行号**（窗口基址 + 相对
        // 滚动；折叠后视觉行 = 逻辑行 − 其前隐藏行数，行窗口以视觉行为上界）。
        let lh = self.line_height.as_f32().max(1.);
        let (vis_top, _vis_off) = self.abs_scroll_top();
        let view_h = self.list_state.viewport_bounds().size.height.as_f32();
        let nrows = ((view_h / lh).ceil() as usize).max(MIN_VISIBLE_ROWS);
        let lo = vis_top.saturating_sub(nrows);
        let last_v = self.folds.visual_line_count().saturating_sub(1);
        let hi = vis_top.saturating_add(nrows * 2).min(last_v);
        let show_caret = self.caret_shown_now();
        let rows = self.build_rows(window, lo, hi, show_caret);

        let (content_w_px, harea_w) = self.compute_content_width(avail_w);

        let rv = RowView {
            lo,
            rows,
            line_height: self.line_height,
            mono: self.mono.clone(),
            font_size: self.font_size,
            caret_color: self.caret_color,
            fold_color: self.gutter_fg,
        };
        let rv = std::rc::Rc::new(rv);
        let list_state = self.list_state.clone();
        let bg = self.bg;
        let fg = self.fg;

        // 列表 item_ix 是窗口内**相对**下标：绝对行 = `window_base + ix`。行数据窗口
        // (`rv.lo..rv.rows`) 用绝对行，超出窗口/文档末尾（含底部预留区）渲染空行。
        let window_base = self.window_base;
        let content = list(list_state.clone(), move |ix, _window, _cx| {
            let abs = window_base + ix;
            let rd = if abs >= rv.lo && abs - rv.lo < rv.rows.len() {
                &rv.rows[abs - rv.lo]
            } else {
                &RowData::empty()
            };
            row_element(rd, &rv).into_any_element()
        })
        // 关键：必须把 List 元素撑满容器（size_full → 视口=编辑器整块）。
        // 不加会走 List 的 Infer 测量——高度=内容高（一行 ~22px），视口≈0，
        // 列表永远不渲染任何行（md 预览正是这样挂的，见 markdown/view/mod.rs）。
        .size_full();

        let inner = div()
            .id("ce-list-wrap")
            .debug_selector(|| "ce-list-wrap".into())
            .w(px(content_w_px))
            .min_w_full()
            .h_full()
            .child(content);
        // 横向滚动容器只包住「文本区」：`absolute left=gutter_width` 使内容从行号
        // 列右缘开始滚动；行号本身在下方固定叠层（`gutter`）里，不随内容横移。
        let harea = div()
            .id("ce-harea")
            .debug_selector(|| "ce-harea".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(self.gutter_width_px)
            .right(px(0.))
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.h_handle)
            .child(inner);

        // 行号 gutter：固定编辑区左缘、不随内容横向滚动。**不是独立的 O(N) ListState**
        // ——126MB/3.2M 行时那棵「纯行号树」白占 ~300MB。改为同一帧用文本列表的
        // **绝对滚动顶**（`window_base + 相对 item_ix`）反推出可见行号、绝对定位叠层
        // 渲染：与 List 行绘制共用同一滚动状态、同一帧（无新旧半拍差异 → 序号不翻/
        // 不闪/不晃），末尾的底部预留区（ix ≥ row_count）自然不显示行号。视口外部分
        // 被 `overflow_hidden` 裁掉。
        let gv = std::rc::Rc::new(GutterView {
            line_height: self.line_height,
            mono: self.mono.clone(),
            font_size: self.font_size,
            fg: self.gutter_fg,
        });
        let (g_top_abs, gutter_offset) = self.abs_scroll_top();
        let gutter_rows = self.folds.visual_line_count().max(1);
        // 可见行号 = 绝对顶行起、覆盖一屏（+1 行抗舍入）。叠层列 top = −已滚进顶行的
        // offset：与 List 把首个可见 item 画在 `origin − offset_in_item` 完全一致。
        // 折叠后 gutter 按**视觉行**取逻辑行号（隐藏行不占视觉行 → 行号呈缺口，
        // 折叠首行渲染「▾」指示器）。先快照（closure 不捕获 self）。
        let gutter_items = self.gutter_items_for(g_top_abs, gutter_rows, view_h, lh);
        let gutter_col = div()
            .id("ce-gutter-col")
            .debug_selector(|| "ce-gutter-col".into())
            .absolute()
            .left(px(0.))
            .top(px(-gutter_offset))
            .w_full()
            .children(gutter_items.into_iter().map(move |item| match item {
                Some((logical, folded)) => {
                    number_element(logical, folded, &gv).into_any_element()
                }
                None => div().h(gv.line_height).into_any_element(),
            }));
        let gutter_wrap = div()
            .id("ce-gutter")
            .debug_selector(|| "ce-gutter".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(px(0.))
            .w(self.gutter_width_px)
            .bg(bg)
            .overflow_hidden()
            .child(gutter_col);

        // 垂直标尺（margin guide）：**纯视觉覆盖层**。坐标 = 行号列右缘 + 列×字宽
        // − 横向滚动像素（随内容横移、左移出文本区即被 overflow_hidden 裁掉，行为同
        // VS Code 方案 A）。不参与文本布局、不依赖 ListState——折叠/滚动/分帧都无关。
        // 放在 `core` 第一个子元素 → 画在编辑器背景之上、文本/选区/滚动条之下。
        let ruler_wrap = EDITOR_RULER_COLUMNS.iter().fold(
            div()
                .id("ce-rulers")
                .debug_selector(|| "ce-rulers".into())
                .absolute()
                .top(px(0.))
                .bottom(px(0.))
                .left(px(0.))
                .right(px(0.))
                .overflow_hidden(),
            |wrap, &col| match self.ruler_screen_x(col, avail_w) {
                Some(x) => wrap.child(
                    div()
                        .absolute()
                        .debug_selector(|| "ce-ruler-line".into())
                        .top(px(0.))
                        .bottom(px(0.))
                        .left(px(x))
                        .w(px(1.))
                        .bg(self.ruler_color),
                ),
                None => wrap,
            },
        );

        // 括号标尺（bracket guides）：每个跨行括号配对一条竖线，从开括号列画到闭行。
        // 坐标在**文本区本地**（x 含横向滚动换算、y 含纵向滚动换算），随内容滚动；
        // 与标尺一样是纯视觉覆盖层，放在文本之下（`harea` 之前）。点击代码区域点亮的
        // 最内层块竖线（`active_guide_open`）用强调蓝加宽（粗细见
        // [`crate::config::layout::BRACKET_GUIDE_ACTIVE_WIDTH`]）。
        let guide_segments = self.bracket_guide_segments_flagged(
            vis_top, _vis_off, lh, harea_w, view_h, self.active_guide_open,
        );
        let guide_color = self.guide_color;
        let active_guide_color = self.active_guide_color;
        let guide_layer = div()
            .id("ce-bracket-guides")
            .debug_selector(|| "ce-bracket-guides".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(self.gutter_width_px)
            .right(px(0.))
            .overflow_hidden()
            .children(guide_segments.into_iter().map(|(x, y, h, active)| {
                div()
                    .absolute()
                    .debug_selector(|| "ce-bracket-guide".into())
                    .left(px(x))
                    .top(px(y))
                    .w(px(if active {
                        BRACKET_GUIDE_ACTIVE_WIDTH
                    } else {
                        BRACKET_GUIDE_WIDTH
                    }))
                    .h(px(h.max(0.)))
                    .bg(if active { active_guide_color } else { guide_color })
            }));

        let core = div()
            .id("code-editor")
            .debug_selector(|| "code-editor".into())
            .size_full()
            .relative()
            .bg(bg)
            .text_color(fg)
            .key_context("Input")
            .track_focus(&self.focus_handle);
        let core = attach_input_actions(core, &entity, window);
        let core = core
            // 垂直标尺叠层：第一个子元素 → 位于文本/选区/滚动条之下（经典后台参考线）。
            .child(ruler_wrap)
            // 括号标尺竖线叠层：也在文本之下（与标尺同级序），盖在背景上、不挡文本。
            .child(guide_layer)
            .child(harea)
            // 行号列叠层（同一绝对滚动顶）：盖在横向滚动文本区左缘，不随内容横移，
            // 竖向滚动与文本逐行一致。
            .child(gutter_wrap)
            // 常显滚动条（Always）：编辑器需要可随时拖动的量程正确的滚动条。
            // 用显式 Scrollbar 叠层（同 md 预览的横向条），而不是
            // `ScrollableElement::vertical_scrollbar`（默认 Scrolling 模式，
            // 空闲 2s 自动隐藏，用户误以为滚动条“时有时无/不准确”）。
            .child(self.vscrollbar_overlay(list_state, content_w_px))
            .child(self.hscrollbar_overlay());

        EditorElement::new(entity, core.into_any_element())
    }
}

impl CodeEditor {
    /// render 每帧的状态同步：焦点注册/对齐、度量刷新、后台折叠/括号扫描挂起、
    /// 窗口对齐用户滚动目标、量程自愈重挂、横向滚动位置回读、可见字符数与主题明暗。
    /// 返回 `avail_w`（编辑区可视宽：上一帧元素宽，首帧退窗口宽）。
    fn sync_render_state(&mut self, window: &mut Window, cx: &mut Context<Self>) -> f32 {
        // 首次渲染注册焦点事件
        if !self.focus_registered {
            let _ = cx.on_focus(&self.focus_handle, window, Self::on_focus);
            let _ = cx.on_blur(&self.focus_handle, window, Self::on_blur);
            self.focus_registered = true;
        }
        // 补同步：应用启动时 window.focus() 发生在 on_focus 注册之前 → 事件丢失，
        // `focused` 永远是 false → 光标不显示但也无法输入。这里按当前焦点态对齐。
        if self.focus_handle.is_focused(window) && !self.focused {
            self.focused = true;
            self.kick_caret();
            if !self.blink_running {
                self.start_blink(cx);
            }
            cx.notify();
        }

        self.refresh_metrics(window, cx);
        // 编辑后待刷新的折叠候选/括号配对：下一帧在这里挂起**后台**刷新
        // （替代原「输入含括号字符即同步整篇重扫」的卡顿路径；单飞 + 版本号取消）。
        if self.scan_dirty_edit {
            self.scan_dirty_edit = false;
            self.refresh_fold_brackets(cx);
        }
        // 每个 render 把固定窗口对齐到用户目标（desired_scroll）：滚动条拖动只写
        // desired、由这里落窗口；滚轮/键盘/跳转的 scroll_to_px 已同步，此处为幂等兜底。
        self.apply_desired_to_window();
        // 量程坍缩时重挂统一高度提示，并请求多画一帧让其真正生效（防「拖到底不到底」）。
        // （pixel_scroll 大文档路径由 ensure_uniform_hint 内部直接短路，见其注释。）
        if self.ensure_uniform_hint() {
            cx.notify();
        }
        // 横向滚动位置以容器（h_handle）为准（滚动条拖动直接改它）→ 回读对齐 hscroll_cols。
        if self.char_width.as_f32() > 0. {
            self.hscroll_cols = ((-self.h_handle.offset().x.as_f32())
                / self.char_width.as_f32())
                .round() as usize;
        }

        // 可见字符数（用上一帧元素宽，首帧退回窗口宽）
        let avail_w = if self.element_bounds.size.width > px(0.) {
            self.element_bounds.size.width.as_f32()
        } else {
            window.bounds().size.width.as_f32()
        };
        self.visible_chars = self.visible_chars_from_width(avail_w);

        // 高亮语言（主题明暗同步）
        let dark = gpui_component::Theme::global(cx).is_dark();
        self.highlighter.set_dark(dark);
        self.md_decor.set_dark(dark);

        avail_w
    }

    /// 横向内容宽度（整篇最长行估算，按 64px 量化防量程坍缩）+ 内容变短时夹紧 `h_handle`。
    /// 返回 `(content_w_px, harea_w)`（`harea_w` = 文本区可视宽，标尺/括号标尺共用）。
    fn compute_content_width(&mut self, avail_w: f32) -> (f32, f32) {
        // 横向内容宽度 = 整篇最长行的字符数（Buffer O(1) 维护），按**文本区**计：
        // 文本区可视宽 = 编辑区宽 − 行号列宽（横向滚动容器 `harea` 从行号列右缘
        // 开始）。此前用整编辑区宽，会多出行号宽度的“幻影可滚范围”（量程/滚动到达
        // 边界提前），也让首帧 `reveal_caret` 误以为可以横向滚动。
        // 此前提过：用「可见窗口的行 max + 单调上修」会在拖垂直滚动条经过首次出现
        // 的更长行时内容宽突跳；用全局最长行永远稳定，只在编辑产生更长行/窗口宽度/
        // 字号变化时才变化。
        let harea_w = (avail_w - self.gutter_width_px.as_f32()).max(0.);
        let cw_max = self.char_width.as_f32().max(1.);
        // `max_line_units` 由 Buffer 行宽直方图（multiset）维护，编辑后**始终精确**
        // O(1) 读。内容宽按 64px 向上取整量化：gpui List 在「布局宽度变化」时会清空
        // 全部 size_hint → 纵向量程坍缩（vertical 滚动条突然变长），需 `ensure_uniform_hint`
        // 的 120ms 冷却后 O(n) 自愈。若内容宽逐键变化（在最宽行末尾打字），量程会一直
        // 坍缩（滚动条变长再变短）。量化后只在每 ~8 字符（64px）才变化一次，冷却期间
        // 即可自愈，纵向量程保持稳定。量化只加不减（= 上界），不影响不折行。
        const HSCROLL_QUANTUM_PX: f32 = 64.;
        let raw_max_w = self.buffer.max_line_units() as f32 * cw_max;
        let content_w_px = if raw_max_w <= harea_w {
            harea_w
        } else {
            (raw_max_w / HSCROLL_QUANTUM_PX).ceil() * HSCROLL_QUANTUM_PX
        };
        self.content_w_px = content_w_px;
        // 内容变短（量程缩小时）把 `h_handle` 夹回新量程（O(1)）——删除长内容后
        // 横向滚动位置不再停在旧更远处的空白处。ScrollableElement 布局也会夹，
        // 这里显式夹一遍即时生效。
        let cur_px = self.hscroll_px();
        let max_px = (content_w_px - harea_w).max(0.);
        if cur_px > max_px + 1.0 {
            let cols = (max_px / cw_max).round() as usize;
            self.set_hscroll(cols);
        }
        (content_w_px, harea_w)
    }

    /// 可见行号叠层条目：`Some((逻辑行号, 是否折叠指示器))` 或 `None`（底部预留区/文档末尾）。
    fn gutter_items_for(
        &self,
        gutter_start: usize,
        gutter_rows: usize,
        view_h: f32,
        lh: f32,
    ) -> Vec<Option<(usize, bool)>> {
        let gutter_count = ((view_h / lh).ceil() as usize).clamp(1, 4096) + 1;
        (gutter_start..gutter_start + gutter_count)
            .map(|vix| {
                if vix < gutter_rows {
                    let l = self.folds.logical_row(vix);
                    // 指示器：激活折叠首行，或自动探测的折叠候选起点（默认可见）。
                    let folded = self.has_fold_or_candidate(l);
                    Some((l, folded))
                } else {
                    None
                }
            })
            .collect()
    }

    /// 竖直滚动条叠层（Always 常显）：量程 = 全文条目数 × 行高（含底部预留），
    /// 与编辑器 `scroll_to_px` 共享同一份「全文量程」。
    fn vscrollbar_overlay(
        &self,
        list_state: ListState,
        content_w_px: f32,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("ce-vscrollbar-overlay")
            .debug_selector(|| "ce-vscrollbar-overlay".into())
            .absolute()
            .inset_0()
            .child({
                let v = VerticalScrollHandle {
                    state: list_state,
                    content_width: content_w_px,
                    // 量程 = **全文**条目数 × 行高（含底部预留空行，见 pad_row_count）
                    // ——与窗口大小无关，thumb 始终反映全文可滚范围。
                    content_height: if self.pixel_scroll {
                        self.scroll_item_count() as f32
                            * self.line_height.as_f32().max(1.)
                    } else {
                        self.row_count as f32 * self.line_height.as_f32()
                    },
                    line_height: self.line_height,
                    pixel_scroll: self.pixel_scroll,
                    bottom_pad: self.vscroll_bottom_pad_px(),
                    desired_scroll: self.desired_scroll.clone(),
                    // 单一来源：与编辑器 scroll_to_px 同一份「全文量程」。
                    reach_px: self.reach_px(),
                };
                Scrollbar::vertical(&v)
                    .mode(ScrollbarMode::Always)
                    .viewport_from_layout()
            })
    }

    /// 横向滚动条叠层（Always 常显）。
    fn hscrollbar_overlay(&self) -> gpui::Stateful<gpui::Div> {
        div()
            .id("ce-hscrollbar-overlay")
            .debug_selector(|| "ce-hscrollbar-overlay".into())
            .absolute()
            .inset_0()
            .child(
                Scrollbar::horizontal(&self.h_handle.clone())
                    .mode(ScrollbarMode::Always)
                    .viewport_from_layout(),
            )
    }
}

/// 把键盘/鼠标/输入 action 挂到核心元素上（keymap 由 gpui_component::init 在
/// "Input" 上下文绑定，与菜单分发（dispatch_action Undo/Cut/…）共用同一套 handler）。
fn attach_input_actions(
    core: gpui::Stateful<gpui::Div>,
    entity: &Entity<CodeEditor>,
    window: &mut Window,
) -> gpui::Stateful<gpui::Div> {
    core
        .on_key_down(window.listener_for(entity, CodeEditor::on_key_down))
        .on_mouse_down(MouseButton::Left, window.listener_for(entity, CodeEditor::on_mouse_down))
        .on_mouse_move(window.listener_for(entity, CodeEditor::on_mouse_move))
        .on_mouse_up(MouseButton::Left, window.listener_for(entity, CodeEditor::on_mouse_up))
        // —— 输入 action（keymap 由 gpui_component::init 在 "Input" 上下文绑定，
        // 与菜单分发（dispatch_action Undo/Cut/…）共用同一套 handler ——
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveLeft, _w, cx| {
            ed.arrow(false, -1, 0, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveRight, _w, cx| {
            ed.arrow(false, 1, 0, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveUp, _w, cx| {
            ed.arrow(false, 0, -1, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveDown, _w, cx| {
            ed.arrow(false, 0, 1, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveHome, _w, cx| {
            ed.move_line_start(false, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveEnd, _w, cx| {
            ed.move_line_end(false, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToStartOfLine, _w, cx| {
            ed.move_line_start(true, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToEndOfLine, _w, cx| {
            ed.move_line_end(true, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveToStart, _w, cx| {
            ed.move_doc_start(false, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveToEnd, _w, cx| {
            ed.move_doc_end(false, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToStart, _w, cx| {
            ed.move_doc_start(true, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToEnd, _w, cx| {
            ed.move_doc_end(true, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveToPreviousWord, _w, cx| {
            ed.move_word_left(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveToNextWord, _w, cx| {
            ed.move_word_right(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToPreviousWordStart, _w, cx| {
            ed.select_word_left(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToNextWordEnd, _w, cx| {
            ed.select_word_right(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MovePageUp, _w, cx| {
            ed.page(false, -1, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &MovePageDown, _w, cx| {
            ed.page(false, 1, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Backspace, _w, cx| {
            ed.backspace(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Delete, _w, cx| {
            ed.delete_forward(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &DeleteToPreviousWordStart, _w, cx| {
            ed.backspace_word(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &DeleteToNextWordEnd, _w, cx| {
            ed.delete_word_forward(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Enter, _w, cx| {
            ed.insert_newline(cx);
            ed.undo.set_cooperative(false);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &IndentInline, _w, cx| {
            ed.tab(false, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &OutdentInline, _w, cx| {
            ed.tab(true, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectAll, _w, cx| {
            ed.select_all(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Undo, _w, cx| {
            ed.undo(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Redo, _w, cx| {
            ed.redo(cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Copy, window, cx| {
            ed.copy(window, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Cut, window, cx| {
            ed.cut(window, cx);
            cx.stop_propagation()
        }))
        .on_action(window.listener_for(entity, |ed: &mut CodeEditor, _: &Paste, window, cx| {
            ed.paste(window, cx);
            cx.stop_propagation()
        }))
}

/// 行元素：文本 + 光标。行号 gutter 已是编辑区左侧固定叠层（见 render `gutter`），
/// 不放进本行，否则会跟随横向滚动容器一起被推走；文本从行左缘（叠层右缘）开始。
fn row_element(rd: &RowData, rv: &RowView) -> gpui::Div {
    div()
        .h(rv.line_height)
        .relative()
        .w_full()
        // 兜底：行高固定 = 一屏行高。即使某行字形宽一度超出内容宽（宽度单位估算
        // 永远只增不减、正常情况下不会），也绝不把溢出文本画到下一行去（否则行号
        // 错位、点击/光标重叠）。
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .h(rv.line_height)
                .font_family(rv.mono.clone())
                .text_size(rv.font_size)
                .child(styled_row_text(rd, rv)),
        )
        .when(rd.caret.is_some(), |this| {
            this.child(
                div()
                    .absolute()
                    .debug_selector(|| "ce-caret".into())
                    .top(px(0.))
                    .left(px(rd.caret.unwrap_or(0.)))
                    .w(px(CARET_WIDTH_PX))
                    .h(rv.line_height)
                    .bg(rv.caret_color),
            )
        })
        // 折叠首行的「…」指示器：独立绝对定位元素（不进 StyledText，不污染行内
        // 测量），尾随在行文本之后，颜色用次要前景。
        .when(rd.folded.is_some(), |this| {
            let x = rd.folded.unwrap_or(0.);
            this.child(
                div()
                    .absolute()
                    .debug_selector(|| "ce-fold-marker".into())
                    .top(px(0.))
                    .left(px(x))
                    .h(rv.line_height)
                    .font_family(rv.mono.clone())
                    .text_size(rv.font_size)
                    .text_color(rv.fold_color)
                    .child("…"),
            )
        })
}

fn styled_row_text(rd: &RowData, _rv: &RowView) -> StyledText {
    StyledText::new(rd.text.clone()).with_highlights(rd.highlights.clone())
}

/// 把「着色/滤镜段 +（可重叠的）选区底色」合并为**非重叠、总量不超订**的连续
/// 样式段，供 `StyledText` 使用。输入已是最终 `HighlightStyle`（`build_rows` 里
/// syntect 的 `(Range, Hsla)` 已转成仅 `color` 的样式；Markdown 滤镜直接产出完整
/// 富样式：加粗 / 斜体 / 删除线 / 淡化 / 底色）。
///
/// 决不能把选区（通常横跨多个语法段）作为**重叠**的独立 highlight 附在语法段
/// 后面：gpui `StyledText::compute_runs` 假定 highlight 互不重叠（`text.rs` 里
/// 逐段按全长入 run，重叠会令 run 总长 > 行字节长），而 `shape_text` 消费到行尾
/// 即停 → 尾部的选区整段被丢弃（选中底色不显示，表现为“语法高亮文件无法鼠标
/// 选择”，同字节的 .txt/.md 因无语法段而正常）。
///
/// 这里把 0..行尾 按「样式段边界 ∪ 选区边界」切成连续单元，逐单元：取覆盖该单元
/// 的首个样式段的**完整样式**（color/weight/italic/strike/fade/背景），选区覆盖则
/// 覆盖 `background_color`。产出严格递增、不重叠、每段都在行内，run 总量恰为各
/// 覆盖区间之和 ≤ 行字节长。
pub(crate) fn merge_highlights(
    runs: &[(Range<usize>, HighlightStyle)],
    sel: Option<Range<usize>>,
    sel_bg: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    // 收集所有切分边界：样式段边界 ∪ 选区边界。
    let mut bounds: Vec<usize> = Vec::with_capacity(runs.len() * 2 + 3);
    for (r, _) in runs {
        let s = r.start.min(r.end);
        let e = r.end.max(r.start);
        if e <= s {
            continue;
        }
        bounds.push(s);
        bounds.push(e);
    }
    if let Some(s) = &sel
        && s.end > s.start {
            bounds.push(s.start);
            bounds.push(s.end);
        }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for (i, a) in bounds.iter().copied().enumerate() {
        let Some(&b) = bounds.get(i + 1) else { break };
        if b <= a {
            continue;
        }
        let mut style = HighlightStyle::default();
        // 样式段：runs 互不重叠且升序，取首个覆盖 [a, b) 的段（整段复制）。
        for (r, s) in runs {
            if r.end <= a {
                continue;
            }
            if r.start >= b {
                break;
            }
            style = *s;
            break;
        }
        if sel.as_ref().is_some_and(|s| a >= s.start && b <= s.end) {
            style.background_color = Some(sel_bg);
        }
        if style != HighlightStyle::default() {
            out.push((a..b, style));
        }
    }
    out
}

/// 行号列渲染上下文（供 gutter 列表闭包只读捕获）。
struct GutterView {
    line_height: Pixels,
    mono: SharedString,
    font_size: Pixels,
    fg: Hsla,
}

/// 行号单元格：右对齐、行高与文本行一致。由共享 ListState 的第二个 `list()`
/// 驱动 → 行号与文本行对齐、随滚动同步移动且不闪烁。`folded=true` 时在行号左侧
/// 渲染「▾」（指示该行为折叠首行、有隐藏内容，点击可展开——见 `on_mouse_down`）。
fn number_element(ix: usize, folded: bool, gv: &GutterView) -> gpui::Div {
    let number = SharedString::from((ix + 1).to_string());
    div()
        .h(gv.line_height)
        .flex()
        .items_center()
        .justify_end()
        .pl(px(GUTTER_LEFT_PAD_PX))
        .pr(px(GUTTER_RIGHT_PAD_PX))
        .text_color(gv.fg)
        .text_size(gv.font_size)
        .font_family(gv.mono.clone())
        .when(folded, |this| {
            this.child(
                div()
                    .debug_selector(|| "ce-fold-arrow".into())
                    .pr(px(2.))
                    .child("▾"),
            )
        })
        .child(number)
}

/// 纵向滚动条的「确定性量程」句柄：内容高 = 行数 × 行高（固定），而不用
/// `ListState::max_offset_for_scrollbar()`——gpui 在布局宽度变化（长行末尾输入使
/// 内容宽逐帧变化）时会清空全部 size_hint，使后者瞬时坍缩，滚动条 thumb 表现为
/// “突然变长又变短”。
///
/// 两条路径：
/// - `pixel_scroll=true`（大文档）：条目不预铺高度、summary 恒为 0，**完全不能**走
///   `scroll_px_offset_for_scrollbar`/`set_offset_from_scrollbar`（它们按 summary 换算）。
///   偏移读取与设置全部用「行数×行高」的像素模型：读 `logical_scroll_top`，写
///   `scroll_to(self::px_to_offset)`。量程永远正确，天然没有坍缩问题（F2/F3 的根治）。
/// - `pixel_scroll=false`（小文档）：继续委托 ListState（保留既有拖拽冻结逻辑）。
#[derive(Clone)]
struct VerticalScrollHandle {
    state: ListState,
    content_width: f32,
    content_height: f32,
    line_height: Pixels,
    pixel_scroll: bool,
    /// 底部预留（px，覆盖层横向滚动条厚 + 额外行数）：量程/滚到底都包含它，保证
    /// 最后一行能滚到横向滚动条之上，且 thumb 范围与「真实可滚距离」一致。
    bottom_pad: f32,
    /// 用户请求的竖向滚动（px，内容坐标）——与编辑器共享（`Rc<Cell<f32>>`）。
    /// 深跳占位期 thumb 用它显示目标（可能超出 ListState 实际可滚范围，Scrollbar
    /// 会把它夹到量程尾 = 「已到当前可滚底部」）；常规期恒等于 ListState 位置。
    /// 拖动滚动条也写它（新目标可落 ListState 的钳回由 `scroll_to` 承担）。
    desired_scroll: std::rc::Rc<Cell<f32>>,
    /// 拖动回钳上限 = 「已增长可滚范围」（px，与编辑器 [`CodeEditor::reach_px`]
    /// 同一来源：render 构造时快照）。量程增长中 thumb 拖动也到不了未增长的部分。
    reach_px: f32,
}

impl ScrollbarHandle for VerticalScrollHandle {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.state.viewport_bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        if self.pixel_scroll {
            // 用「用户请求位置」而非 ListState 实际滚动顶：深跳占位期 thumb 指示
            // 目标（超量程时 Scrollbar 夹到量程尾端）；常规期二者一致。
            point(px(0.), px(-self.desired_scroll.get()))
        } else {
            self.state.scroll_px_offset_for_scrollbar()
        }
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        if self.pixel_scroll {
            let off = (-offset.y.as_f32()).max(0.).min(self.reach_px);
            // 拖动滚动条 = 写用户目标（desired_scroll，全文绝对坐标）。这里的 ListState
            // 是固定窗口内的相对坐标，不能直接 scroll_to 绝对 px——窗口重定位由 render
            // 每帧的 `apply_desired_to_window` 承担（O(窗口)，即时生效）。
            self.desired_scroll.set(off);
        } else {
            self.state.set_offset_from_scrollbar(offset);
        }
    }

    fn content_size(&self) -> gpui::Size<Pixels> {
        size(px(self.content_width), px(self.content_height))
    }

    fn start_drag(&self) {
        if self.pixel_scroll {
            // 量程确定性，无坍缩，无需重铺/冻结。
            return;
        }
        // gpui 在布局宽度变化后清空全部 size_hint → `items.summary().height` 瞬时坍缩；
        // 若此刻开始拖拽，`scrollbar_drag_start_height` 会把坍缩值**冻结一整次拖拽**
        // （gpui list.rs，拖到底也到不了真正的底部）。拖拽开始前若检测到量程坍缩，用
        // *确定性行高强制重铺*（`reset_with_uniform_height` 覆盖陈旧戳值；O(n) 一次），
        // 保证冻结的量程是正确的；健康量程则直接冻结（省掉不必要的 O(n)）。
        // （clone/self.state 与编辑器共享同一 `Rc<RefCell<Inner>>`，换掉 items 即全局生效。）
        let total = self.state.max_offset_for_scrollbar().y.as_f32()
            + self.state.viewport_bounds().size.height.as_f32();
        if total < self.content_height * 0.97 {
            let prev = self.state.logical_scroll_top();
            self.state
                .reset_with_uniform_height(self.state.item_count(), self.line_height);
            self.state.scroll_to(prev);
        }
        self.state.scrollbar_drag_started();
    }

    fn end_drag(&self) {
        self.state.scrollbar_drag_ended();
    }
}


/// 包装元素：把内部 UI 委托给 gpui 布局/绘制，同时在 paint 阶段挂上
/// `hand_input`（IME/文本输入），并回写元素边界供坐标换算。
struct EditorElement {
    editor: Entity<CodeEditor>,
    inner: AnyElement,
}

impl EditorElement {
    fn new(editor: Entity<CodeEditor>, inner: AnyElement) -> Self {
        Self { editor, inner }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.inner.request_layout(window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.editor
            .update(cx, |editor, _| editor.element_bounds = bounds);
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 首帧光栅预热（一次/进程）：先打热 DWrite 彩字管线再画任何文字——冷进程首次
        // COLR 彩字光栅失败 → emoji 变空白/方框且被 sprite_atlas 钉住。
        // 源码编辑区若先于预览/WYSIWYG 画文本，也必须先预热
        // `crate::markdown::wysiwyg::warm_emoji_rasterizer`）。
        if !crate::markdown::wysiwyg::warm_emoji_is_done() {
            crate::markdown::wysiwyg::warm_emoji_rasterizer(window, "editor");
        }
        self.inner.paint(window, cx);
        let focus = self.editor.read(cx).focus_handle();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        // 滚轮：在列表原生滚动之前拦截，把 Line 增量按「行高 × 3 行/格」折算成
        // 像素再滚动（原生 List 用 20px/行 → 一格只见 ~1 行，太慢）。touchpad 的
        // Pixels 增量原样透传。注册在 `inner.paint` 之后 → bubble 先于列表触发，
        // `stop_propagation` 使列表原生 handler 不会二次滚动。
        let editor = self.editor.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            if !editor.read(cx).element_bounds.contains(&event.position) {
                return;
            }
            let (line_h, char_w) = editor.read(cx).scroll_factors();
            let (dx, dy) = match event.delta {
                ScrollDelta::Pixels(p) => (p.x.as_f32(), p.y.as_f32()),
                ScrollDelta::Lines(l) => {
                    // Windows 一格已送 Lines(y=±3)：行高×3 ≈ 3 行/格（标准手感）。
                    // 别再乘回 3，否则一格滚 ~9 行。
                    const LINES_PER_NOTCH: f32 = 1.0;
                    (
                        l.x * char_w * LINES_PER_NOTCH,
                        l.y * line_h * LINES_PER_NOTCH,
                    )
                }
            };
            editor.update(cx, |ed, cx| {
                if dy != 0. {
                    // 像素两段式：取反（正 dy=内容上移）→ scroll_to_px；小文档走 ListState。
                    ed.scroll_vertical(dy);
                }
                if dx != 0. {
                    ed.scroll_horizontal_px(dx);
                }
                cx.notify();
            });
            cx.stop_propagation();
        });
    }
}

