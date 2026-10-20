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
use crate::actions::{
    Backspace, Copy, Cut, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, Enter,
    IndentInline, MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight,
    MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveUp, OutdentInline, Paste,
    Redo, SelectAll, SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart,
    SelectToStart, SelectToStartOfLine, Undo,
};
use ui::scrollbar::{Scrollbar, ScrollbarHandle, ScrollbarMode};

use config::layout::{
    BRACKET_GUIDE_ACTIVE_WIDTH, BRACKET_GUIDE_WIDTH, EDITOR_RULER_COLUMNS,
};
use super::{
    CARET_WIDTH_PX, CodeEditor, EditorEvent, FOLD_ARROW_SLOT_PX, GUTTER_LEFT_PAD_PX,
    GUTTER_RIGHT_PAD_PX, MIN_VISIBLE_ROWS, RowData, RowView,
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
        let (vis_top, vis_off) = self.abs_scroll_top();
        let view_h = self.list_state.viewport_bounds().size.height.as_f32();
        let nrows = ((view_h / lh).ceil() as usize).max(MIN_VISIBLE_ROWS);
        let lo = vis_top.saturating_sub(nrows);
        let last_v = self.folds.visual_line_count().saturating_sub(1);
        let hi = vis_top.saturating_add(nrows * 2).min(last_v);
        let show_caret = self.caret_shown_now();
        let rows = self.build_rows(window, lo, hi, vis_top, show_caret, self.focused);

        let (content_w_px, harea_w) = self.compute_content_width(avail_w);

        let rv = RowView {
            lo,
            rows,
            line_height: self.line_height,
            mono: self.mono.clone(),
            font_size: self.font_size,
            caret_color: self.caret_color,
            fold_color: self.gutter_fg,
            fold_bg: self.fold_bg,
            active_line_bg: self.active_line_bg,
            hscroll_px: self.hscroll_px(),
            harea_w,
            char_width: self.char_width.as_f32().max(1.),
        };
        let rv = std::rc::Rc::new(rv);
        let list_state = self.list_state.clone();
        let bg = self.bg;
        let fg = self.fg;

        // 折叠/光标行整行底色叠层：标尺之上、括号竖线之下（层级见 `row_bg_overlay`）。
        let row_bg = self.row_bg_overlay(&rv, vis_top, vis_off, lh, view_h);

        // 文本区（List 文本 + 横向滚动容器）、行号列、标尺/括号竖线叠层 → 各自 &self 助手
        // （`vscrollbar_overlay` 之后仍需 `list_state`，故此处给 text_area_layer 传克隆）。
        let harea = self.text_area_layer(rv, content_w_px, self.list_state.clone());

        // 行号 gutter：固定编辑区左缘、不随内容横向滚动；同一帧反推绝对行号叠层
        // （不建独立 O(N) 树，见 `gutter_overlay`）。
        let gv = std::rc::Rc::new(GutterView {
            line_height: self.line_height,
            mono: self.mono.clone(),
            font_size: self.font_size,
            fg: self.gutter_fg,
        });
        let gutter = self.gutter_overlay(view_h, lh, bg, gv);

        // 垂直标尺（margin guide）：纯视觉覆盖层（见 `ruler_overlay`）。
        let ruler = self.ruler_overlay(avail_w);

        // 括号标尺（bracket guides）：跨行括号配对竖线叠层（见 `bracket_guide_overlay`）。
        let guides = self.bracket_guide_overlay(vis_top, vis_off, lh, harea_w, view_h);

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
            .child(ruler)
            // 折叠/光标行整行底色叠层：标尺之上、括号竖线之下（拆出行元素，见
            // `row_bg_overlay`；竖线在折叠/光标行上清晰可见且保持原色）。
            .child(row_bg)
            // 括号标尺竖线叠层：盖在行底色叠层之上、文字/选区之下（不挡文本）。
            .child(guides)
            .child(harea)
            // 行号列叠层（同一绝对滚动顶）：盖在横向滚动文本区左缘，不随内容横移，
            // 竖向滚动与文本逐行一致。
            .child(gutter)
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
    /// 文本区：List 文本列表（窗口内相对下标 → 绝对行取 `RowView.rows`，超窗空行）+
    /// 横向滚动容器（只包文本区，`left=gutter_width_px`；行号在下方固定叠层不横移）。
    fn text_area_layer(
        &self,
        rv: std::rc::Rc<RowView>,
        content_w_px: f32,
        list_state: ListState,
    ) -> gpui::Stateful<gpui::Div> {
        let window_base = self.window_base;
        // 列表 item_ix 是窗口内**相对**下标：绝对行 = `window_base + ix`。行数据窗口
        // (`rv.lo..rv.rows`) 用绝对行，超出窗口/文档末尾（含底部预留区）渲染空行。
        let content = list(list_state, move |ix, window, _cx| {
            let abs = window_base + ix;
            let rd = if abs >= rv.lo && abs - rv.lo < rv.rows.len() {
                &rv.rows[abs - rv.lo]
            } else {
                &RowData::empty()
            };
            row_element(rd, &rv, window).into_any_element()
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
        div()
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
            .child(inner)
    }

    /// 行号 gutter：固定左缘、不随内容横移。**不是独立的 O(N) ListState**——126MB/3.2M
    /// 行时那棵「纯行号树」白占 ~300MB。同一帧用文本列表的**绝对滚动顶**反推可见行号、
    /// 绝对定位叠层渲染：与 List 行绘制共用同一滚动状态/帧（序号不翻/不闪/不晃），
    /// 底部预留区自然不显示行号，视口外被 `overflow_hidden` 裁掉。
    fn gutter_overlay(
        &self,
        view_h: f32,
        lh: f32,
        bg: Hsla,
        gv: std::rc::Rc<GutterView>,
    ) -> gpui::Stateful<gpui::Div> {
        let (g_top_abs, gutter_offset) = self.abs_scroll_top();
        let gutter_rows = self.folds.visual_line_count().max(1);
        // 可见行号 = 绝对顶行起、覆盖一屏（+1 行抗舍入）。叠层列 top = −已滚进顶行的
        // offset：与 List 把首个可见 item 画在 `origin − offset_in_item` 完全一致。
        // 折叠后 gutter 按**视觉行**取逻辑行号（隐藏行不占视觉行 → 行号呈缺口，
        // 折叠首行按 `fold_arrow_state` 渲染方向箭头：已折叠「▸」/仅候选「▾」）。
        // 先快照（closure 不捕获 self）。
        let gutter_items = self.gutter_items_for(g_top_abs, gutter_rows, view_h, lh);
        let gutter_col = div()
            .id("ce-gutter-col")
            .debug_selector(|| "ce-gutter-col".into())
            .absolute()
            .left(px(0.))
            .top(px(-gutter_offset))
            .w_full()
            .children(gutter_items.into_iter().map(move |item| match item {
                Some((logical, arrow)) => {
                    number_element(logical, arrow, &gv).into_any_element()
                }
                None => div().h(gv.line_height).into_any_element(),
            }));
        div()
            .id("ce-gutter")
            .debug_selector(|| "ce-gutter".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(px(0.))
            .w(self.gutter_width_px)
            .bg(bg)
            .overflow_hidden()
            .child(gutter_col)
    }

    /// 垂直标尺（margin guide）：**纯视觉覆盖层**。坐标 = 行号列右缘 + 列×字宽 − 横向
    /// 滚动像素（随内容横移、左移出文本区即被 overflow_hidden 裁掉，行为同 VS Code
    /// 方案 A）。不参与文本布局、不依赖 ListState——折叠/滚动/分帧都无关。放在 `core`
    /// 第一个子元素 → 画在编辑器背景之上、文本/选区/滚动条之下。
    fn ruler_overlay(&self, avail_w: f32) -> gpui::Stateful<gpui::Div> {
        EDITOR_RULER_COLUMNS.iter().fold(
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
        )
    }

    /// 括号标尺（bracket guides）：每个跨行括号配对一条竖线，从开括号列画到闭行。
    /// 坐标在**文本区本地**（x 含横向滚动换算、y 含纵向滚动换算），随内容滚动；
    /// 与标尺一样是纯视觉覆盖层，放在文本之下（`harea` 之前）。点击代码区域点亮的
    /// 最内层块竖线（`active_guide_open`）用强调蓝加宽。
    fn bracket_guide_overlay(
        &self,
        vis_top: usize,
        vis_off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
    ) -> gpui::Stateful<gpui::Div> {
        let guide_segments = self.bracket_guide_segments_flagged(
            vis_top, vis_off, lh, harea_w, view_h, self.active_guide_open,
        );
        let guide_color = self.guide_color;
        let active_guide_color = self.active_guide_color;
        div()
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
            }))
    }

    /// render 每帧的状态同步：焦点注册/对齐、度量刷新、后台折叠/括号扫描挂起、
    /// 窗口对齐用户滚动目标、横向滚动位置回读、可见字符数与主题明暗。
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
        let dark = ui::theme::Theme::global(cx).is_dark();
        self.highlighter.set_dark(dark);
        self.md_decor.set_dark(dark);

        avail_w
    }

    /// 横向内容宽度（整篇最长行，Buffer O(1) 精确，不再量化）+ 内容变短时夹紧 `h_handle`。
    /// 返回 `(content_w_px, harea_w)`（`harea_w` = 文本区可视宽，标尺/括号标尺共用）。
    fn compute_content_width(&mut self, avail_w: f32) -> (f32, f32) {
        // 横向内容宽度 = 整篇最长行的宽度单位数（Buffer O(1) 精确维护的直方图）
        // × 格宽，按**文本区**计：文本区可视宽 = 编辑区宽 − 行号列宽（横向滚动容器
        // `harea` 从行号列右缘开始）。此前用整编辑区宽，会多出行号宽度的“幻影可滚
        // 范围”（量程/滚动到达边界提前），也让首帧 `reveal_caret` 误以为可以横向滚动。
        // 此前提过：用「可见窗口的行 max + 单调上修」会在拖垂直滚动条经过首次出现
        // 的更长行时内容宽突跳；用全局最长行永远稳定，只在编辑产生更长行/窗口宽度/
        // 字号变化时才变化。
        let harea_w = (avail_w - self.gutter_width_px.as_f32()).max(0.);
        let cw_max = self.char_width.as_f32().max(1.);
        // `max_line_units` 由 Buffer 行宽直方图（multiset）维护，编辑后**始终精确**
        // O(1) 读 → 内容宽就是最宽行的真实渲染宽（单调等宽：ASCII=1 格、全角=2 格、
        // Tab=4 格，与 `char_width` 同一口径），**不量化、不浮余**。
        //
        // 不再需要「64px 量化 + 120ms 冷却（ensure_uniform_hint）」历史补丁：
        // 纵向量程已由确定性像素模型承担（`VerticalScrollHandle.content_height =
        // 全文行数 × 行高`，与 `reach_px` 同源），gpui List 换宽清空 size_hint 不再
        // 导致量程坍缩；横向量程 = `h_handle.max_offset().x`——本容器按真实
        // `content_w_px` 布局后的**实测**值，滚动条/滚轮夹紧同读它，随内容实时精确。
        let raw_max_w = self.buffer.max_line_units() as f32 * cw_max;
        // 尾部留白（`hscroll_tail_gap`，默认 0 = 无）：仅当最宽行**溢出**可视区时追加
        // ——滚到底后末尾字符与右裁切缘之间留该空隙（数学编辑卡长公式用），内容变短
        // 不产生幻影量程（不溢出时 <0 语义不与 P7 的「无浮余」冲突：显式 if 而非
        // `.max()`，空档行不出现多余滚动条）。
        let content_w_px = if raw_max_w > harea_w {
            raw_max_w + self.hscroll_tail_gap
        } else {
            harea_w
        };
        self.content_w_px = content_w_px;
        // 内容变短（量程缩小时）把 `h_handle` 夹回新量程（O(1)）——删除长内容后
        // 横向滚动位置不再停在旧更远处的空白处。ScrollableElement 布局也会夹，
        // 这里显式夹一遍即时生效（本帧容器即将按新 `content_w_px` 布局，用当前值
        // 夹最及时；量程口径与实测 `h_handle.max_offset().x` 一致）。
        let cur_px = self.hscroll_px();
        let max_px = (content_w_px - harea_w).max(0.);
        if cur_px > max_px + 1.0 {
            let cols = (max_px / cw_max).round() as usize;
            self.set_hscroll(cols);
        }
        (content_w_px, harea_w)
    }

    /// 可见行号叠层条目：`Some((逻辑行号, 箭头状态))` 或 `None`（底部预留区/文档末尾）。
    /// 第二项 = [`CodeEditor::fold_arrow_state`] 直接透传：`Some(true)`=已折叠首行、
    /// `Some(false)`=仅候选起点、`None`=不画箭头（渲染口径与测试同源，不在此重判）。
    fn gutter_items_for(
        &self,
        gutter_start: usize,
        gutter_rows: usize,
        view_h: f32,
        lh: f32,
    ) -> Vec<Option<(usize, Option<bool>)>> {
        let gutter_count = ((view_h / lh).ceil() as usize).clamp(1, 4096) + 1;
        (gutter_start..gutter_start + gutter_count)
            .map(|vix| {
                if vix < gutter_rows {
                    let l = self.folds.logical_row(vix);
                    let arrow = self.fold_arrow_state(l);
                    Some((l, arrow))
                } else {
                    None
                }
            })
            .collect()
    }

    /// 折叠行/光标行整行底色**独立背景叠层**：从行元素拆出（`row_element` 不再上
    /// 不透明底色），按可见行绝对定位铺底色（复刻 `gutter_overlay`/`gutter_items_for`
    /// 的列式叠层，不建第二棵 ListState）。层级 = 标尺 < 本叠层 < 括号竖线 < 文字/
    /// 选区 < 行号 → 跨行括号竖线在任何折叠行/光标行上都清晰可见且保持原色，文字仍
    /// 画在竖线之上（不挡文本）。
    ///
    /// 行单位与 gutter 同口径（第 idx 个孩子高 = idx×lh、列顶 −vis_off、越窗/文档
    /// 末尾取透明占位），与 List 把首个可见 item 画在 `origin − offset_in_item` 完全
    /// 一致。底色随「可视带」锚定（`left=gutter_width`/`right=0`，横向滚动不移），
    /// 与括号竖线同口径——横向滚动时底色保持视口锚定，是刻意行为而非 bug。
    fn row_bg_overlay(
        &self,
        rv: &std::rc::Rc<RowView>,
        vis_top: usize,
        vis_off: f32,
        lh: f32,
        view_h: f32,
    ) -> gpui::Stateful<gpui::Div> {
        let last_v = self.folds.visual_line_count().saturating_sub(1);
        let count = ((view_h / lh).ceil() as usize).clamp(1, 4096) + 1;
        let fold_bg = rv.fold_bg;
        let active_line_bg = rv.active_line_bg;
        let rv = rv.clone();
        let row_bg_col = div()
            .absolute()
            .left(px(0.))
            .top(px(-vis_off))
            .w_full()
            .children((vis_top..vis_top + count).map(move |vix| {
                // 越窗/越过文档末尾（底部预留区）取空行兜底（透明占位，保列内行序对齐）。
                let rd = if vix <= last_v && vix >= rv.lo && vix - rv.lo < rv.rows.len() {
                    &rv.rows[vix - rv.lo]
                } else {
                    &RowData::empty()
                };
                if rd.folded.is_some() {
                    div()
                        .debug_selector(|| "ce-fold-row-bg".into())
                        .h(px(lh))
                        .w_full()
                        .bg(fold_bg)
                        .into_any_element()
                } else if rd.active {
                    div()
                        .debug_selector(|| "ce-active-line-bg".into())
                        .h(px(lh))
                        .w_full()
                        .bg(active_line_bg)
                        .into_any_element()
                } else {
                    div().h(px(lh)).into_any_element()
                }
            }));
        div()
            .id("ce-row-bg-layer")
            .debug_selector(|| "ce-row-bg-layer".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(self.gutter_width_px)
            .right(px(0.))
            .overflow_hidden()
            .child(row_bg_col)
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
                    content_height: self.scroll_item_count() as f32
                        * self.line_height.as_f32().max(1.),
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

/// 把键盘/鼠标/输入 action 挂到核心元素上（keymap 由 `crate::actions::bind_input_keys`
/// 在 "Input" 上下文绑定，与菜单分发（dispatch_action Undo/Cut/…）共用同一套 handler）。
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
        // 拖出元素/窗口外松开（Capture 阶段、`!is_hovered`）→ 同样收口拖选态，防
        // `dragging` 残留（拖选生命周期终点之一，见 CodeEditor::cancel_mouse_drag）。
        .on_mouse_up_out(MouseButton::Left, window.listener_for(entity, CodeEditor::on_mouse_up_out))
        // —— 输入 action（keymap 由 crate::actions::bind_input_keys 在 "Input" 上下文绑定，
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
/// 折叠行/光标行的整行底色**不在本行**——已拆到独立背景叠层
/// `CodeEditor::row_bg_overlay`（层级 = 标尺 < 行底色 < 括号竖线 < 文字/选区 < 行号，
/// 否则不透明整行底色会把跨行括号竖线盖住，见 `bracket_guides_stay_visible_...`）。
fn row_element(rd: &RowData, rv: &RowView, window: &mut Window) -> gpui::Div {
    // 超长行窗口化的文本起点：短行 = 0（整行从行左缘开始）；超长行 = 裁剪子串在
    // 行本地坐标的 x（真实前缀整形宽，见 `styled_row_text`）——横向滚动依赖该起点，
    // 否则左缘被裁的内容会错误地顶到行首。
    let (styled, text_x) = styled_row_text(rd, rv, window);
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
                .left(px(text_x))
                .h(rv.line_height)
                .font_family(rv.mono.clone())
                .text_size(rv.font_size)
                .child(styled),
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

/// 超长行「横向窗口化」的触发下限（字节数）：短于它的行一律整行直送 `StyledText`，
/// 不做窗口截取（避免为收益可忽略的行走「字符→宽度」换算与 `layout_width` 调用）。
/// 有效触发阈值 = 行宽 > `可视窗宽 + 2×[`LONG_LINE_HVIEW_MARGIN_UNITS`] 余量`
/// （见 [`styled_row_text`]：行在 `x_right` 内放得下即回落整行，None）。
const LONG_LINE_HVIEW_MIN_CHARS: usize = 128;

/// 超长行「横向窗口化」的可视窗两侧**过扫余量**（宽度单位，× 格宽 = px）：
/// 裁剪左/右缘都取「可视窗 ± 余量」，再按**真实字形宽**（`layout_width`）落字符边界。
/// 作用：①横滚时新字符提前入窗被整形（不出现“滚到边缘才突然出现”）；②Tab/emoji
/// 等真实宽度大于单位估算的字符不会被边界截缺；③边界上的亚像素字形足够充裕。
pub(crate) const LONG_LINE_HVIEW_MARGIN_UNITS: usize = 32;

/// 超长行横滚窗口的截取结果（见 [`window_bytes_and_offset`]）。
pub(crate) struct LongLineWindow {
    /// 截取字节区间（行内，两端均落在 UTF-8 字符边界——绝不劈开多字节字符/emoji）。
    pub(crate) bytes: Range<usize>,
    /// 截取子串左侧在**行本地坐标**的 x（px）= 前缀字符的真实整形宽（`layout_width`
    /// 逐字累加）——与 `row_layout`/光标 x 用的是同一真实字形宽口径：裁剪后文本与
    /// 光标/命中坐标严格对齐（CJK 全角按真实字形宽而非 2× 格宽近似，深滚无累积漂移）。
    pub(crate) offset_px: f32,
}

/// 超长行横向窗口化：行文本 + 合并后样式段 → 按「横向可视窗 ± 余量」截取子串进
/// `StyledText`（左/右被裁部分**不参与 shape**），样式段同步平移收窄（首尾 run 被
/// 截断则收窄范围、整体字节偏移减 `bytes.start`、裁剪边界外的 run 丢弃）。
///
/// - 仅对超过 [`LONG_LINE_HVIEW_MIN_CHARS`] **且**行宽超出可视窗（含余量）的行走
///   窗口化：短行/未超窗行走 `window_bytes_and_offset` 返回 `None` → 原样整行直送，
///   零额外开销（只多做一次长度判断与单位换算）。
/// - 窗口按当前 `hscroll`（渲染期快照于 `RowView`）现算，每次横滚都随重绘更新；
///   不给窗口/偏移加缓存层（任务约束），成本 = 一次「0 → 窗口右缘」的逐字宽累加
///   （≤ 可视窗 + 2×余量字符数，与行总长无关的算术，远低于整行 shape）。
/// - 返回 `(StyledText, 左偏移 px)`；`row_element` 用偏移定位子串，保持滚动几何。
fn styled_row_text(rd: &RowData, rv: &RowView, window: &mut Window) -> (StyledText, f32) {
    // 咽喉点兜底：`rd.highlights()` 是行内字节区间（md 滤镜 / syntect 缓存 / 选区合并
    // 后），编辑→重析后行文本变短或区间按字节切进 CJK/emoji 多字节字符内部时，gpui
    // `StyledText::with_highlights` 的 `is_char_boundary(run.end)` debug_assert 会崩
    // （`elements/text.rs`，dev 构建 panic）。这里把每个段吸收到行文本的字符边界、
    // 钳入行尾、空段丢弃、段间不重叠——`StyledText` 永不看到坏区间。
    let text = rd.content.text.as_ref();
    let highlights = char_align_highlights(text, rd.highlights());
    if let Some(win) = long_line_window(
        text,
        rv.hscroll_px,
        rv.harea_w,
        rv.char_width,
        &rv.mono,
        rv.font_size,
        window,
    ) {
        let LongLineWindow { bytes, offset_px } = win;
        let cropped = SharedString::from(&text[bytes.clone()]);
        let shifted = shift_highlights(&highlights, bytes.start, cropped.len());
        let aligned = char_align_highlights(cropped.as_ref(), &shifted);
        return (StyledText::new(cropped).with_highlights(aligned), offset_px);
    }
    (StyledText::new(rd.content.text.clone()).with_highlights(highlights), 0.)
}

/// 超长行横向窗口化的「计算侧」公共入口（`styled_row_text` 与测试探针共用，保证
/// 测试走的就是生产同一条路径）：行文本 + 当前横滚/可视宽/格宽 + 字体 → 窗口子串。
/// 阈值/余量/右缘判据集中于此（见 `window_bytes_and_offset`）。
pub(crate) fn long_line_window(
    text: &str,
    hscroll_px: f32,
    harea_w: f32,
    char_width: f32,
    mono: &SharedString,
    font_size: Pixels,
    window: &mut Window,
) -> Option<LongLineWindow> {
    if text.len() <= LONG_LINE_HVIEW_MIN_CHARS
        || harea_w <= 0.
        || char_width <= 0.
        || !char_width.is_finite()
    {
        return None;
    }
    let margin_px = LONG_LINE_HVIEW_MARGIN_UNITS as f32 * char_width;
    let x_left = (hscroll_px - margin_px).max(0.);
    let x_right = (hscroll_px + harea_w + margin_px).max(x_left + 1.);
    let ts = window.text_system();
    let font_id = ts.resolve_font(&gpui::font(mono.clone()));
    // 逐字真实宽（TextSystem 单字整形缓存命中即 O(1)）；同字符连续段用备忘短路，
    // 长同质行（minified "xxx…"）每帧只查一次字体。
    let mut char_w = {
        let mut last: Option<(char, f32)> = None;
        move |ch: char| -> f32 {
            if let Some((lc, lw)) = last
                && lc == ch
            {
                return lw;
            }
            let w = ts.layout_width(font_id, font_size, ch).as_f32();
            last = Some((ch, w));
            w
        }
    };
    window_bytes_and_offset(text, x_left, x_right, &mut char_w)
}

/// 纯函数（核心窗口算法）：给定「每字符渲染宽度」`char_w`，求横向窗口的字节区间与
/// 左偏移。`x_left`/`x_right` = 行本地坐标的可见窗（含余量）。行程进入 `x_right`
/// 内被完整放下 → 返回 `None`（整行直送）；否则返回覆盖 `[x_left, x_right)` 的
/// 子串区间——起点取**第一个越过 `x_left` 右缘的字符**（部分可见的左缘字符整字
/// 包含，不劈字），终点排他（**第一个起点 ≥ `x_right` 的字符**，其前的整字都含），
/// 两端均落在 UTF-8 字符边界；`offset_px` = 前缀（`0..bytes.start`）真实宽累加。
///
/// 生产用 `text_system.layout_width` 真实字形宽；测试可用 `char_width_units` 等
/// 宽度函数驱动，验证 CJK/emoji/短行/空行等边界（字符列↔字节换算沿用
/// [`crate::buffer::char_width_units`] 同一宽度口径）。
pub(crate) fn window_bytes_and_offset(
    text: &str,
    x_left: f32,
    x_right: f32,
    mut char_w: impl FnMut(char) -> f32,
) -> Option<LongLineWindow> {
    if text.is_empty() || x_right <= x_left {
        return None;
    }
    let mut x = 0.0f32;
    let mut byte = 0usize;
    let mut start_byte = 0usize;
    let mut start_x = 0.0f32;
    let mut started = x_left <= 0.0;
    for ch in text.chars() {
        let ch_byte = byte;
        byte += ch.len_utf8();
        // 本字符起点已在窗右之外 → 其前内容即窗口，本字符（及其后）不参与。
        if x >= x_right {
            return if started {
                Some(LongLineWindow {
                    bytes: start_byte..ch_byte,
                    offset_px: start_x,
                })
            } else {
                // 防御：尚未锁定左缘却已到右缘（异常宽度输入），回落整行前缀。
                Some(LongLineWindow {
                    bytes: 0..ch_byte,
                    offset_px: 0.0,
                })
            };
        }
        let w = char_w(ch);
        if w <= 0.0 {
            continue;
        }
        let x_after = x + w;
        if !started && x_after > x_left {
            start_byte = ch_byte;
            start_x = x;
            started = true;
        }
        if x_after > x_right {
            // 本字符起点在窗内、宽度越过右缘（部分可见）→ 整字包含后收尾。
            return if started {
                Some(LongLineWindow {
                    bytes: start_byte..byte,
                    offset_px: start_x,
                })
            } else {
                Some(LongLineWindow {
                    bytes: 0..byte,
                    offset_px: 0.0,
                })
            };
        }
        x = x_after;
    }
    None
}

/// 把整行（已对齐）样式段按窗口截取**平移收窄**：与 `[start, start+len)` 无关的段
/// 丢弃；首/尾被截断的段把越界部分裁掉；剩余段字节偏移整体减 `start`（落在窗口
/// 子串坐标系）。输出继续交给 [`char_align_highlights`] 做最终字符边界兜底。
fn shift_highlights(
    runs: &[(Range<usize>, HighlightStyle)],
    crop_start: usize,
    crop_len: usize,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let crop_end = crop_start + crop_len;
    let mut out = Vec::with_capacity(runs.len());
    for (r, style) in runs {
        if r.end <= crop_start || r.start >= crop_end {
            continue;
        }
        let rs = (r.start.max(crop_start) - crop_start).min(crop_len);
        let re = (r.end.min(crop_end) - crop_start).min(crop_len);
        if re > rs {
            out.push((rs..re, *style));
        }
    }
    out
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
    // 双指针归并：`bounds` 与 `runs` 均升序（runs 互不重叠且升序），`ri` 随 `a` 单调
    // 前进——每段只需跳到「首个 end > a」的 run（整段 O(n+m)），不再每切分单元从头部
    // 重扫（旧实现最坏 O(段²)，每个可见行每帧重复）。切分边界/合并规则保持不变。
    let mut ri = 0usize;
    for (i, a) in bounds.iter().copied().enumerate() {
        let Some(&b) = bounds.get(i + 1) else { break };
        if b <= a {
            continue;
        }
        while ri < runs.len() && runs[ri].0.end <= a {
            ri += 1;
        }
        let mut style = HighlightStyle::default();
        // 跳过 `end <= a` 后，`runs[ri]` 即首个可能覆盖 [a, b) 的段：`start < b` 则覆盖
        // （runs 非重叠 ⇒ 其后段起点更大，不可能更早覆盖本单元）。
        if let Some((r, s)) = runs.get(ri)
            && r.start < b {
                style = *s;
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

/// 把 `text` 上的一组样式段钳到 UTF-8 **字符边界**内（向前进位 + 钳入行尾，基元
/// [`crate::util::char_align_byte`]，与预览 `markdown::view::row::char_align_range`、
/// WYSIWYG `shape::runs_to_text_runs` 三处合一）、空段丢弃，并保持段间**互不
/// 重叠递增**（起点取 `max(对齐起点, 上一段终点)`、按 start 排序）。
///
/// 必要性：`StyledText::with_highlights` 在 gpui `elements/text.rs` 里有
/// `debug_assert!(text.is_char_boundary(run.start/end))`——编辑→重析后行文本变短、
/// 或上游（md 滤镜 / syntect 缓存 / 选区）按字节搬运的区间切进 CJK/emoji 多字节字符
/// 内部时，直接喂 `StyledText` 会在 dev 下 panic（用户实测「WYSIWYG 删除标题记号 →
/// 切换」即崩在 `run.end`）。本函数在 `styled_row_text` 的咽喉点兜底：返回的每个段
/// 都是合法边界内的不重叠段，越界 / 反向 / 中切字符的区间被吸收或丢弃。
pub(crate) fn char_align_highlights(
    text: &str,
    highlights: &[(Range<usize>, HighlightStyle)],
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut sorted: Vec<&(Range<usize>, HighlightStyle)> = highlights.iter().collect();
    sorted.sort_by_key(|(r, _)| (r.start, r.end));
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(sorted.len());
    let mut cursor = 0usize;
    for (r, style) in sorted {
        let s = crate::util::char_align_byte(text, r.start).max(cursor);
        let e = crate::util::char_align_byte(text, r.end).max(s);
        if e <= s {
            continue;
        }
        out.push((s..e, *style));
        cursor = e;
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

/// 行号单元格：行高与文本行一致，由共享 ListState 的第二个 `list()` 驱动 → 行号与
/// 文本行对齐、随滚动同步移动且不闪烁。`arrow` = 折叠指示箭头状态
/// （[`CodeEditor::fold_arrow_state`] 透传）：`Some(true)`=已折叠首行 → 右向「▸」；
/// `Some(false)`=候选起点 → 下向「▾」；`None`=不画。
///
/// 布局：flex-grow 的**数字格**（内部 `justify_end` 右对齐）+ 固定宽**箭头槽**（恒占位）
/// → 无论有无箭头，数字右缘都钉在「gutter 右缘 − 右内边距 − 槽宽」，有箭头行不把
/// 行号左推、与无箭头行数字列对齐（避免真机「行号被挤/错位」回归）。
fn number_element(ix: usize, arrow: Option<bool>, gv: &GutterView) -> gpui::Div {
    let number = SharedString::from((ix + 1).to_string());
    div()
        .h(gv.line_height)
        .flex()
        .items_center()
        .pl(px(GUTTER_LEFT_PAD_PX))
        .pr(px(GUTTER_RIGHT_PAD_PX))
        .text_color(gv.fg)
        .text_size(gv.font_size)
        .font_family(gv.mono.clone())
        .child(
            div()
                .debug_selector(|| "ce-gutter-number".into())
                .flex()
                .flex_grow(1.0)
                .justify_end()
                .items_center()
                .child(number),
        )
        .child(
            div()
                .debug_selector(|| "ce-gutter-arrow-slot".into())
                .w(px(FOLD_ARROW_SLOT_PX))
                .flex()
                .items_center()
                .justify_end()
                .when(arrow.is_some(), |this| {
                    let glyph = if arrow == Some(true) { "▸" } else { "▾" };
                    this.child(
                        div()
                            .debug_selector(|| "ce-fold-arrow".into())
                            .child(glyph),
                    )
                }),
        )
}

/// 纵向滚动条的「确定性量程」句柄：内容高 = 行数 × 行高（固定），而不用
/// `ListState::max_offset_for_scrollbar()`——gpui 在布局宽度变化（长行末尾输入使
/// 内容宽逐帧变化）时会清空全部 size_hint，使后者瞬时坍缩，滚动条 thumb 表现为
/// “突然变长又变短”。
///
/// 恒走像素模型（固定窗口虚拟化下条目不预铺高度、summary 恒为 0，**完全不能**走
/// `scroll_px_offset_for_scrollbar`/`set_offset_from_scrollbar`——它们按 summary
/// 换算）。偏移读取与设置全部用「行数×行高」的像素模型：读/写 `desired_scroll`。
/// 量程永远正确，天然没有坍缩问题（F2/F3 的根治）。若未来恢复
/// `pixel_scroll=false`（小文档委托 ListState 的拖拽冻结逻辑），需重加
/// `start_drag`/`end_drag` 的 `scrollbar_drag_started/ended` 与 offset 回退分支。
#[derive(Clone)]
struct VerticalScrollHandle {
    state: ListState,
    content_width: f32,
    content_height: f32,
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
        // 用「用户请求位置」而非 ListState 实际滚动顶：深跳占位期 thumb 指示
        // 目标（超量程时 Scrollbar 夹到量程尾端）；常规期二者一致。
        point(px(0.), px(-self.desired_scroll.get()))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let off = (-offset.y.as_f32()).max(0.).min(self.reach_px);
        // 拖动滚动条 = 写用户目标（desired_scroll，全文绝对坐标）。这里的 ListState
        // 是固定窗口内的相对坐标，不能直接 scroll_to 绝对 px——窗口重定位由 render
        // 每帧的 `apply_desired_to_window` 承担（O(窗口)，即时生效）。
        self.desired_scroll.set(off);
    }

    fn content_size(&self) -> gpui::Size<Pixels> {
        size(px(self.content_width), px(self.content_height))
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
        base::util::source_location()
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
        // WY_TRACE breakdown：编辑器 prepaint/布局段（元素 bounds 回写 + 内部树）。
        let _t_pp = crate::prof::begin();
        self.editor
            .update(cx, |editor, _| editor.element_bounds = bounds);
        self.inner.prepaint(window, cx);
        crate::prof::end("ed.prepaint", _t_pp);
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
        // WY_TRACE：编辑器 paint 段（帧内 editor 自身绘制）+ 操作级实时计时结算——
        // 与 WyEditorElement::paint 同构，编辑/滚动/撤销重做在此以「开始→绘制完成」
        // 墙钟结算一行 OP 记录。
        let _t_paint = crate::prof::begin();
        crate::prof::paint_mark();
        // 首帧光栅预热（一次/进程）：先打热 DWrite 彩字管线再画任何文字——冷进程首次
        // COLR 彩字光栅失败 → emoji 变空白/方框且被 sprite_atlas 钉住。
        // 源码编辑区若先于预览/WYSIWYG 画文本，也必须先预热
        // `base::warm::warm_emoji_rasterizer`）。warm 实现自 P1 下沉到 base，
        // editor 不再依赖 markdown（后者在 `wysiwyg/warm.rs` 保留薄转发）。
        if !base::warm::warm_emoji_is_done() {
            base::warm::warm_emoji_rasterizer(window, "editor");
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
        crate::prof::end("ed.paint", _t_paint);
        // 操作级实时计时结算：本帧若含未结算操作（ed_edit/ed_undo/ed_redo/ed_scroll…），
        // 在此以「开始→绘制完成」的墙钟 + 帧内阶段 breakdown 落地一行 OP 记录。
        crate::prof::op_finish_after_paint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::char_width_units;

    /// 把 `char_width_units`（空格=1/Tab=4/CJK=2）转成「每字符 px」驱动窗口算法——
    /// 与生产 `layout_width` 同一「字符→宽度」映射口径，纯算术、无需真实字体。
    fn units_w(ch: char) -> f32 {
        char_width_units(ch) as f32
    }

    #[test]
    fn window_ascii_slices_by_columns() {
        let text = "x".repeat(200);
        // 窗 [50, 150)：逐字宽 1，起点 = 越过 50 右缘的首字符（x=50），终点排他 = 起点 ≥ 150。
        let w = window_bytes_and_offset(&text, 50., 150., units_w).unwrap();
        assert_eq!(w.bytes, 50..150, "ASCII 字宽 1 → 字节=列");
        assert_eq!(w.offset_px, 50., "左偏移 = 前缀真实宽");
        // 起点落在字符右缘上（x_after == x_left → 该字符整字在窗内，下个字符起点 = x_left）。
        let w = window_bytes_and_offset(&text, 100., 140., units_w).unwrap();
        assert_eq!(w.bytes, 100..140);
        assert_eq!(w.offset_px, 100.);
    }

    #[test]
    fn window_falls_back_when_line_fits() {
        // 行短于/等于可视窗右缘 → None（整行直送，不窗口化）。
        assert!(window_bytes_and_offset("abc", 0., 10., units_w).is_none());
        assert!(window_bytes_and_offset("abc", 2., 5., units_w).is_none());
        // 空行 / 空窗 → None。
        assert!(window_bytes_and_offset("", 0., 10., units_w).is_none());
        assert!(window_bytes_and_offset("abc", 5., 2., units_w).is_none());
    }

    #[test]
    fn window_starts_at_line_head_for_left_edge() {
        // x_left ≤ 0（没往右滚 / 滚回行首）→ 从字节 0 起，不丢行首内容。
        let w = window_bytes_and_offset("abcdef", 0., 3., units_w).unwrap();
        assert_eq!(w.bytes, 0..3);
        assert_eq!(w.offset_px, 0.);
        let w = window_bytes_and_offset("abcdef", -10., 4., units_w).unwrap();
        assert_eq!(w.bytes, 0..4);
        assert_eq!(w.offset_px, 0.);
    }

    #[test]
    fn window_cjk_keeps_char_boundaries() {
        // 中文每字 2 格（3 字节）。窗 [10, 30)：第 5 字（单位起点 10，越过 10 右缘）起、
        // 下一字起点 ≥ 30（第 15 字）止 → 字节 5*3 .. 15*3，偏移 = 前缀真实宽 10。
        let text = "中".repeat(30);
        let w = window_bytes_and_offset(&text, 10., 30., units_w).unwrap();
        assert_eq!(w.bytes, 15..45, "CJK 窗起点/终点都落在 UTF-8 字符边界");
        assert_eq!(w.offset_px, 10.);
        // 起始位置切在某个「中」中间（单位 11）→ 整字包含、起点抬到该字开头。
        let w = window_bytes_and_offset(&text, 11., 31., units_w).unwrap();
        assert_eq!(w.bytes, 15..48, "不劈开 CJK：起点落到包含 11 的首字（单位 10）");
        // 结束位置落在字中间（29）→ 终点止于「起点 ≥ 29」的下一字（单位 30 起），
        // 之前整字（第 14 字，单位 [28,30) 部分可见）全含 → 字节 0 .. 15*3。
        let w = window_bytes_and_offset(&text, 0., 29., units_w).unwrap();
        assert_eq!(w.bytes, 0..45, "不劈开 CJK：终点止于起点 ≥ 29 的首字前");
    }

    #[test]
    fn window_emoji_survives_multibyte_utf8() {
        // 🍎 4 字节/字、宽 2 格。窗 [6, 14)：起点 = 首字「终点 > 6」= 第 3 字（单位 [6,8)），
        // 终点 = 首字「起点 ≥ 14」= 第 7 字（单位 [14,16)）→ 字节 3*4 .. 7*4。
        let text = "🍎".repeat(20);
        let w = window_bytes_and_offset(&text, 6., 14., units_w).unwrap();
        assert_eq!(w.bytes, 12..28, "emoji 4 字节不被劈开");
        assert_eq!(w.offset_px, 6.);
        // 窗切进 emoji 字节内部（如 9）→ 起点抬到整字（第 4 字单位 [8,10)），
        // 终点止于起点 ≥ 15 的首字（第 7 字单位 [14,16) 起 14<15，含；第 8 字不含）。
        let w = window_bytes_and_offset(&text, 9., 15., units_w).unwrap();
        assert_eq!(w.bytes, 16..32);
        assert_eq!(w.offset_px, 8.);
    }

    #[test]
    fn window_mixed_ascii_cjk_offset_is_prefix_width() {
        // "a中b中c"：宽 1/2/1/2/1。窗 [2, 4)：`中`(1..3) 越过 2 → 起于 '中'；
        // `中`(4..6) 起点 4 ≥ 4 → 止于其前 → 字节 1..5（"中b"），偏移 = 'a' 宽 1。
        let w = window_bytes_and_offset("a中b中c", 2., 4., units_w).unwrap();
        assert_eq!(w.bytes, 1..5);
        assert_eq!(w.offset_px, 1.);
    }

    #[test]
    fn window_zero_width_char_at_head_is_kept() {
        // 组合字符（生产 `layout_width` 真实整形宽 ≈ 0）在行首：x_left≤0 → 从 0 起，
        // 组合字符随首字一起入窗（不劈开序列、不把前缀组合字丢弃）。
        let text = "e\u{301}x"; // e(1B) + 组合重音(2B、≈0 宽) + x(1B) = 4 字节
        let w = window_bytes_and_offset(text, 0., 1.5, |ch| {
            if ch == '\u{301}' {
                0.0
            } else {
                1.0
            }
        })
        .unwrap();
        assert_eq!(w.bytes, 0..4, "行首组合字符随首字一起入窗");
        assert_eq!(w.offset_px, 0.);
    }

    #[test]
    fn shift_highlights_clips_and_translates_runs() {
        // 整行 runs：跨窗/窗内/窗外（右）。
        let s = HighlightStyle::default();
        let runs = vec![(0..10, s), (8..20, s), (100..110, s)];
        let out = shift_highlights(&runs, 5, 10); // 窗 [5, 15)
        assert_eq!(out, vec![(0..5, s), (3..10, s)], "首段收窄+平移、窗内段平移、窗外段丢弃");
        // 空窗不产出。
        assert!(shift_highlights(&runs, 20, 0).is_empty());
    }

    /// 效果证据（纯算法层，真实字体路径见 `tests/view.rs` 同名集成测试）：200k 单行
    /// 窗口化产出的文本长度 ≈ 可视窗口（远小于整行 200k）。
    #[test]
    fn window_200k_line_shapes_only_visible_span() {
        let text = "x".repeat(200_000);
        // 可视窗 ≈ 80 列（宽 80）+ 过扫余量：产物 ≈ 80 + 2×16 字 = 112 字量级。
        let w = window_bytes_and_offset(&text, 0., 80., units_w).unwrap();
        let len = w.bytes.end - w.bytes.start;
        assert!(
            len < 200 && len > 50,
            "200k 行走窗口化后产出的文本应 ≈ 可视窗（len={len}）"
        );
        // 深滚到 ~100k 列：窗口仍只覆盖可视段，左/右被裁部分不入窗（不做整行整形）。
        let w = window_bytes_and_offset(&text, 100_000., 100_080., units_w).unwrap();
        let len = w.bytes.end - w.bytes.start;
        assert!(
            len < 200 && len > 50,
            "深滚 100k 后窗口文本应仍 ≈ 可视窗（len={len}）"
        );
        assert_eq!(w.offset_px, 100_000., "深滚左偏移 = 前缀宽");
    }
}

