//! 渲染层：`Render`/`Focusable`/`EventEmitter` 实现 + 行/gutter/滚动条/输入包装
//! 元素等渲染助手（`render()` 从「行数据窗口 → UI 骨架」的全阶段与行级元素构造）。

// 阴影/描边绘制与排版类型 → 放行
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use std::cell::Cell;
use std::ops::Range;

use crate::actions::{
    Backspace, Copy, Cut, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, IndentInline,
    MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight, MoveToEnd,
    MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveUp, OutdentInline, Paste, Redo, SelectAll,
    SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart, SelectToStart,
    SelectToStartOfLine, Undo,
};
use crate::editor::fold::BracketChar;
use gpui::{
    AnimationExt, AnyElement, App, Bounds, Context, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, EventEmitter, FocusHandle, Focusable, GlobalElementId,
    HighlightStyle, Hsla, InteractiveElement, IntoElement, LayoutId, ListState, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, StyledText, Window, div, list, point,
    prelude::FluentBuilder as _, px, size,
};
use ui::scrollbar::{Scrollbar, ScrollbarHandle, ScrollbarMode};

use super::editing::bracket_guide_color;
use super::{
    CARET_WIDTH_PX, CodeEditor, EditorEvent, FOLD_ARROW_SLOT_PX, FindBtn, GUTTER_LEFT_PAD_PX,
    GUTTER_RIGHT_PAD_PX, MIN_VISIBLE_ROWS, RowData, RowView,
};
use config::layout::{
    BRACKET_GUIDE_ACTIVE_WIDTH, BRACKET_GUIDE_WIDTH, BRACKET_GUIDES_MODE, BracketGuidesMode,
    EDITOR_RULER_COLUMNS,
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

        // 拖选帧级合并（B2）：`on_mouse_move` 只记最新拖点，这里每帧开头统一落地
        // 一次选区/滚动（在行窗口构建**之前** → 当帧即渲染新选区）。详见
        // `flush_pending_drag` 注释。
        self.flush_pending_drag(window, cx);

        // 行级数据窗口（可见 + 过扫）——vis_top 用**绝对视觉行号**（窗口基址 + 相对
        // 滚动；折叠后视觉行 = 逻辑行 − 其前隐藏行数，行窗口以视觉行为上界）。
        let lh = self.line_height.as_f32().max(1.);
        let (vis_top, vis_off) = self.abs_scroll_top();
        let view_h = self.list_state.viewport_bounds().size.height.as_f32();
        let nrows = ((view_h / lh).ceil() as usize).max(MIN_VISIBLE_ROWS);
        let lo = vis_top.saturating_sub(nrows);
        let last_v = self.folds.visual_line_count().saturating_sub(1);
        let hi = vis_top.saturating_add(nrows * 2).min(last_v);
        let show_caret = self.caret.caret_shown_now();
        let rows = self.build_rows(window, lo, hi, vis_top, show_caret, self.caret.focused);

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
        let harea = self.text_area_layer(rv.clone(), content_w_px, self.list_state.clone());

        // 行号 gutter：固定编辑区左缘、不随内容横向滚动；同一帧反推绝对行号叠层
        // （不建独立 O(N) 树，见 `gutter_overlay`）。
        let gv = std::rc::Rc::new(GutterView {
            line_height: self.line_height,
            mono: self.mono.clone(),
            font_size: self.font_size,
            fg: self.gutter_fg,
            active_fg: self.fg,
        });
        let gutter = self.gutter_overlay(&rv, view_h, lh, bg, gv);

        // 垂直标尺（margin guide）：纯视觉覆盖层（见 `ruler_overlay`）。
        let ruler = self.ruler_overlay(avail_w);

        // 缩进引导线（indentation guides，§8 视觉主源）：逐可视行缩进级竖线叠层
        // （见 `indentation_guide_overlay`；默认开，VS Code 同款）。
        let guides = self.indentation_guide_overlay(
            vis_top,
            vis_off,
            lh,
            harea_w,
            view_h,
            window.scale_factor(),
        );

        // 括号竖线（bracket guides）：竖线层按模式挂载（Off = 不画括对彩线，只留钩线；
        // Active/All 见 config/layout.rs:94 `BRACKET_GUIDES_MODE`（默认 Off，翻转常量即
        // 还原））。钩线层独立恒挂（`bracket_guide_hooks_layer`，Off 模式也保留，
        // bracketPairsHorizontal='active' 口径）。
        let hooks = self.bracket_guide_hooks_layer(vis_top, vis_off, lh, window.scale_factor());
        let bracket_guides = (BRACKET_GUIDES_MODE != BracketGuidesMode::Off).then(|| {
            self.bracket_guide_overlay(
                vis_top,
                vis_off,
                lh,
                harea_w,
                view_h,
                window.scale_factor(),
                BRACKET_GUIDES_MODE,
            )
        });

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
            // 折叠/光标行整行底色叠层：标尺之上、引导竖线之下（拆出行元素，见
            // `row_bg_overlay`；竖线在折叠/光标行上清晰可见且保持原色）。
            .child(row_bg)
            // 缩进引导线叠层：盖在行底色叠层之上、文字/选区之下（不挡文本）。
            .child(guides)
            // 括号横钩线：恒挂（bracketPairsHorizontal='active'，仅严格含光标的 active
            // 括对；Off 模式也保留），盖在缩进线之上（同列/同行交接处清晰）。
            .child(hooks)
            // 括号竖线可选叠加：按模式挂载（Off = 不挂载）；开启时盖在缩进线之上（同列压过，
            // 历史规范见 docs/bracket-guide-redesign.md §8.4）。
            .when(bracket_guides.is_some(), |this| {
                this.child(bracket_guides.expect("已判 Some"))
            })
            .child(harea)
            // 行号列叠层（同一绝对滚动顶）：盖在横向滚动文本区左缘，不随内容横移，
            // 竖向滚动与文本逐行一致。
            .child(gutter)
            // 常显滚动条（Always）：编辑器需要可随时拖动的量程正确的滚动条。
            // 用显式 Scrollbar 叠层（同 md 预览的横向条），而不是
            // `ScrollableElement::vertical_scrollbar`（默认 Scrolling 模式，
            // 空闲 2s 自动隐藏，用户误以为滚动条“时有时无/不准确”）。
            .child(self.vscrollbar_overlay(list_state, content_w_px))
            .child(self.hscrollbar_overlay())
            // 编辑区内查找卡片（Ctrl+F 打开）：编辑区右上角内缘的紧凑悬浮卡片（输入框
            // + 计数 + 上一项/下一项/关闭按钮），盖在内容/滚动条之上；右缘避开滚动条命中
            // 区（元素 id find-* 命名；本卡片取代旧顶部通栏 `render_find_bar`）。
            .when(self.find.open, |this| {
                this.child(self.render_find_card(window, cx))
            })
            // 编辑区内跳转行卡片（Ctrl+G）：顶部居中的紧凑悬浮卡片（可复用输入框 +
            // 内联提示区），盖在内容/滚动条之上；与右上 find 卡可共存（id/命中原样）。
            .when(self.goto.open, |this| {
                this.child(self.render_goto_card(window, cx))
            });

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
        rv: &std::rc::Rc<RowView>,
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
        let gutter_items = self.gutter_items_for(rv, g_top_abs, gutter_rows, view_h, lh);
        let gutter_col = div()
            .id("ce-gutter-col")
            .debug_selector(|| "ce-gutter-col".into())
            .absolute()
            .left(px(0.))
            .top(px(-gutter_offset))
            .w_full()
            .children(gutter_items.into_iter().map(move |item| match item {
                Some((logical, arrow, bg, folded)) => {
                    number_element(logical, arrow, bg, folded, &gv).into_any_element()
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

    /// 括号标尺竖线段端点吸附到**渲染行网格**（纯函数）。根因：sf 为小数（如 1.75）时
    /// `lh·sf` 为小数，1px 竖线按 lh 累计会与真实渲染行距逐行累积偏差。真实行距经 gpui
    /// `round_half_toward_zero` 预吸附成**整数设备 px**（vendored util.rs:130，正值 =
    /// `ceil(lh·sf − 0.5)`，与 taffy 同式）。顶部就近吸附到设备像素（保持贴行）；底部
    /// = 行顶 + 整行数×吸附行距，恰贴「下一行顶」、**永不越过**渲染行界（只会「贴」或
    /// 「短」，不会「超过」）。返回 `(新 top, 新高)`。
    fn snap_guide_extent(y: f32, h: f32, lh: f32, scale_factor: f32) -> (f32, f32) {
        let sf = scale_factor.max(1e-6);
        // 每行真实渲染设备像素（rhtz 正值变体：ceil(lh·sf−0.5)，实证 = taffy 预吸附）。
        let pitch_dev = super::layout::pitch_for_line_height(lh * sf);
        // 顶：就近吸附到设备像素。
        let top_dev = (y * sf).round();
        // 段占整行数（head/tail=0 时 h=n·pitch_log 恰整行；就地近取整）。分母用吸附行距
        // pitch_log（= pitch_dev/sf），防深块 P/P′（h 略超整行）通胀多算 1 行、溢过行界。
        let rows = (h / (pitch_dev / sf)).round().max(0.0);
        let bottom_dev = top_dev + rows * pitch_dev;
        (top_dev / sf, (bottom_dev - top_dev).max(0.0) / sf)
    }

    /// 括号标尺（bracket guides，对齐 VSCode bracket guides）：每个跨行括号配对一条
    /// 竖线，从开括号列画到闭行。坐标在**文本区本地**（x 含横向滚动换算、y 含纵向滚动
    /// 换算），随内容滚动；与标尺一样是纯视觉覆盖层，放在文本之下（`harea` 之前）。
    /// **按括对 `depth % 3` 循环池上色**（`theme colors.bracket_guide_palette`）：active
    /// 段（= 活动括对 [`active_pair`] 的段，由光标行**块体模型**派生、随光标移动跟随）
    /// alpha 1.0、inactive 段 alpha 0.3（VSCode 同口径）；宽度两态一致（1px，VSCode
    /// 不加宽）。
    /// `mode` = [`BracketGuidesMode`]（Off 默认：本竖线层不挂载、只留钩线，见
    /// [`Self::bracket_guide_hooks_layer`]；Active = 仅活动括对、All = 全部产段）。
    /// `sf` = `window.scale_factor()`，用于把段端点吸附到物理像素网格（见 `snap_guide_extent`）。
    fn bracket_guide_overlay(
        &self,
        vis_top: usize,
        vis_off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
        sf: f32,
        mode: BracketGuidesMode,
    ) -> gpui::Stateful<gpui::Div> {
        let active_pair = self.active_pair();
        // Off → 不产竖线（只留钩线层；Active = 仅活动括对、All = 全部产段，见
        // `bracket_guide_segments_flagged`）。Off 由调用方守卫一般不到达，防御兜底。
        let guide_segments = if mode == BracketGuidesMode::Off {
            Vec::new()
        } else {
            self.bracket_guide_segments_flagged(
                vis_top,
                vis_off,
                lh,
                sf,
                harea_w,
                view_h,
                active_pair,
                mode,
            )
        };
        let palette = self.bracket_guide_palette;
        div()
            .id("ce-bracket-guides")
            .debug_selector(|| "ce-bracket-guides".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(self.gutter_width_px)
            .right(px(0.))
            .overflow_hidden()
            .children(guide_segments.into_iter().map(|(x, y, h, depth, active)| {
                // 端点吸附**渲染行网格**：底 = 行顶 + 整行数×吸附行距（rhtz，与 taffy
                // 同式）——恰贴「下一行顶」、绝不越过渲染行界（见 `snap_guide_extent`）。
                // x 不动（横向 1px 竖线，无需 snap）。
                let (gy, gh) = Self::snap_guide_extent(y, h, lh, sf);
                // 括对色取色单源：`bracket_guide_color(palette, depth, active)`（E3 封口，
                // palette[depth%3] + active → alpha 1.0 / 否则 0.3，见 editing.rs）。
                let col = bracket_guide_color(&palette, depth, active);
                div()
                    .absolute()
                    .debug_selector(|| "ce-bracket-guide".into())
                    .left(px(x))
                    .top(px(gy))
                    .w(px(BRACKET_GUIDE_WIDTH))
                    .h(px(gh))
                    .bg(col)
            }))
    }

    /// 括号**横向钩线**层（bracketPairsHorizontal='active'，仅 active 括对、须**严格含光标**，
    /// 2026-11-14 第 4 片）：开行/闭行端点连到引导列。端点 x = 真实宽度单位列 × cw − hscroll
    /// （与竖线段同坐标空间 = 文本区本地）；行边界 y 用吸附行距 pitch（横线落在渲染行网格的
    /// 行界上）。**独立恒挂**（与 `BRACKET_GUIDES_MODE` 无关：用户真机口径下 Off 模式也保留
    /// 钩线，见 `bracket_guide_overlay`）。三分支：
    /// - 开行：开列 > 引导列 → **底边**横段（y = 开行下沿），否则不画；
    /// - 闭行前有文本（`} else` / `x }`，`hasTextBeforeClosingBracket`）→ **底边**横段
    ///   + 竖线段已含闭行；
    /// - 闭行独占 `}` → **顶边**横段 + 竖线段不含闭行。
    ///
    /// 横段厚 1 物理 px（÷sf），色 = active 括对色（palette[depth%3] 100%）。
    fn bracket_guide_hooks_layer(
        &self,
        vis_top: usize,
        vis_off: f32,
        lh: f32,
        sf: f32,
    ) -> gpui::Stateful<gpui::Div> {
        let mut hooks: Vec<(f32, f32, f32, f32, &'static str, Hsla)> = Vec::new();
        if let Some(ap) = self.active_pair() {
            let pitch = super::layout::pitch_for_line_height(lh * sf) / sf;
            let snap = |v: f32| (v * sf).round() / sf;
            let row_edge = |r: usize, top: bool| {
                // 行 r 的上/下沿（编辑器本地 y，相对叠层顶）：上沿 = (r−top)·pitch − off，
                // 下沿 = 上沿 + pitch。
                let y = (r as f32 - vis_top as f32) * pitch - vis_off;
                if top {
                    y
                } else {
                    y + pitch
                }
            };
            // col→px 走 `col_to_px`（d2 单源；下方再叠设备像素 snap，保留现状口径）。
            let guide_x = self.col_to_px(self.guide_pair_column_lite(&ap));
            let open_x = self.col_to_px(self.guide_col_prefix_units(ap.open_line, ap.open_col));
            let close_x =
                self.col_to_px(self.guide_col_prefix_units(ap.close_line, ap.close_col));
            let vo = self.folds.visual_row(ap.open_line);
            let vc = self.folds.visual_row(ap.close_line);
            let hairline = 1.0 / sf;
            let active_col = bracket_guide_color(&self.bracket_guide_palette, ap.depth, true);
            // 开行：开列 > 引导列 → 底边横段（y = 开行下沿）。
            if open_x > guide_x {
                let y_top = row_edge(vo, false) - hairline;
                hooks.push((
                    snap(guide_x),
                    snap(y_top),
                    (snap(open_x) - snap(guide_x)).max(0.0),
                    hairline,
                    "ce-bracket-hook-open",
                    active_col,
                ));
            }
            // 闭行：闭列 > 引导列才画；前置文本 → 底边（含闭行）、独占 → 顶边。
            if close_x > guide_x {
                let top_edge = !self.has_text_before_close(ap.close_line, ap.close_col);
                let y_top = row_edge(vc, top_edge) - if top_edge { 0.0 } else { hairline };
                hooks.push((
                    snap(guide_x),
                    snap(y_top),
                    (snap(close_x) - snap(guide_x)).max(0.0),
                    hairline,
                    "ce-bracket-hook-close",
                    active_col,
                ));
            }
        }
        div()
            .id("ce-bracket-hooks")
            .debug_selector(|| "ce-bracket-hooks".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(self.gutter_width_px)
            .right(px(0.))
            .overflow_hidden()
            .children(hooks.into_iter().map(|(x, y, w, h, sel, col)| {
                div()
                    .absolute()
                    .debug_selector(move || sel.into())
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .h(px(h))
                    .bg(col)
            }))
    }

    /// 缩进引导线（indentation guides，§8 视觉主源）：逐可视行按缩进级画竖线（折叠感知：
    /// 折叠体内不产线、头行照常画）。普通引导线用 `guide_color`、active 高亮行用
    /// [`Self::active_guide_highlight_color`]（active 视觉**块体模型**：高亮
    /// [`CodeEditor::active_guide_group`] 返回列在**块体区间** `[体首, 体末+1)` 内的**全部
    /// 连续段**，整块体点亮；2026-11-14 用户实测唯一真值——hl 色随「关联括对」括对色，
    /// 无关联括对则 VSCode 灰）。
    /// 坐标在**文本区本地**，随内容滚动；放在行底色之上、文本之下。端点同样经
    /// `snap_guide_extent` 吸附渲染行网格。
    fn indentation_guide_overlay(
        &self,
        vis_top: usize,
        vis_off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
        sf: f32,
    ) -> gpui::Stateful<gpui::Div> {
        // active 高亮 = `active_guide_group()` 块体模型（2026-11-14 用户实测唯一真值，统一
        // headless getter 与渲染路径）：(active_col, 体首可视, 体末可视+1) → `active` =
        // (col, 体首, 体末+1, 光标可视行)，半开区间 `[体首, 体末+1)` 与
        // `indentation_guide_segments_flagged` 的段化规则对齐（块体内全部连续段点亮，不只
        // 光标行）；方向键/点击/折叠均跟随（终稿块体口径见 editing.rs
        // active_pair / active_guide_group doc，2026-11-14 用户实测）。
        let active = self
            .active_guide_group()
            .map(|(col, first_vis, last_vis_excl)| {
                let caret = self.selection.head().min(self.buffer.len());
                let logical = self.buffer.line_of_offset(caret);
                let vmax = self.folds.visual_line_count().saturating_sub(1);
                (
                    col,
                    first_vis,
                    last_vis_excl,
                    self.folds.visual_row(logical).min(vmax),
                )
            });
        // C3 让位（2026-11-14 第 4 片）：仅 **All** 模式（全部括对彩色竖线接管画面）传让位表
        // 压掉缩进灰线/active 高亮；缺省 Off 与 Active 用空表 → 灰缩进与 active 全量回显、
        // 不受任何括对抑制（用户真机口径：彩线默认关掉后，缩进引导线是唯一竖线主源，须完整）。
        // 表由 `bracket_guide_suppress` 生成、与缩进段化同源（空表 → 行为与旧实现逐位一致）。
        let suppress = if BRACKET_GUIDES_MODE == BracketGuidesMode::All {
            self.bracket_guide_suppress(vis_top, view_h, lh, BRACKET_GUIDES_MODE)
        } else {
            Vec::new()
        };
        let guide_segments = self.indentation_guide_segments_flagged(
            vis_top,
            vis_off,
            lh,
            sf,
            harea_w,
            view_h,
            active,
            &suppress,
        );
        let guide_color = self.guide_color;
        // active 高亮色：光标行关联括对（`active_pair`，块体模型）→ 该括对色
        // （`bracket_guide_palette[depth % 3]`）；否则 VSCode 默认灰 `active_guide_color`
        // （2026-11-14 用户实测口径「active 高亮随关联括对」）。
        let active_color = self.active_guide_highlight_color();
        div()
            .id("ce-indent-guides")
            .debug_selector(|| "ce-indent-guides".into())
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .left(self.gutter_width_px)
            .right(px(0.))
            .overflow_hidden()
            .children(guide_segments.into_iter().map(|(x, y, h, active)| {
                // 端点吸附**渲染行网格**（与 bracket 段同式 `snap_guide_extent`）——
                // 底 = 行顶 + 整行数×吸附行距，恰贴「下一行顶」、绝不越过渲染行界。
                let (gy, gh) = Self::snap_guide_extent(y, h, lh, sf);
                div()
                    .absolute()
                    .debug_selector(|| "ce-indent-guide".into())
                    .left(px(x))
                    .top(px(gy))
                    .w(px(if active {
                        BRACKET_GUIDE_ACTIVE_WIDTH
                    } else {
                        BRACKET_GUIDE_WIDTH
                    }))
                    .h(px(gh))
                    .bg(if active { active_color } else { guide_color })
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
        // 双向对称（镜像 wysiwyg/core.rs `sync_focus`）：聚焦→开始闪；失焦（如
        // find/goto 输入框持焦使编辑区不再是焦点叶子）→ `on_blur` 置 false 停闪。
        // 两分支各只在 true↔false 转变帧触发一次（此后不再满足条件），不逐帧 notify。
        if self.focus_handle.is_focused(window) && !self.caret.focused {
            self.caret.focused = true;
            self.caret.kick();
            if !self.caret.blink_running {
                self.start_blink(cx);
            }
            cx.notify();
        } else if !self.focus_handle.is_focused(window) && self.caret.focused {
            self.on_blur(window, cx);
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

    /// 可见行号叠层条目：`Some((逻辑行号, 箭头状态, 行号单元格背景色, 是否折叠行))` 或
    /// `None`（底部预留区/文档末尾）。第二项 = [`CodeEditor::fold_arrow_state`] 直接
    /// 透传：`Some(true)`=已折叠首行、`Some(false)`=仅候选起点、`None`=不画箭头（渲染
    /// 口径与测试同源，不在此重判）。第三项 = 光标行/折叠行整行高亮延伸到行号列时的
    /// 单元格底色（折叠优先、否则持焦光标行，互斥口径与 `row_bg_overlay` 一致），无
    /// 高亮为 `None`；第四项 = 该视觉行是否折叠首行（决定 selector，见 `number_element`）。
    fn gutter_items_for(
        &self,
        rv: &std::rc::Rc<RowView>,
        gutter_start: usize,
        gutter_rows: usize,
        view_h: f32,
        lh: f32,
    ) -> Vec<Option<(usize, Option<bool>, Option<Hsla>, bool)>> {
        let gutter_count = ((view_h / lh).ceil() as usize).clamp(1, 4096) + 1;
        let last_v = self.folds.visual_line_count().saturating_sub(1);
        let fold_bg = rv.fold_bg;
        let active_line_bg = rv.active_line_bg;
        (gutter_start..gutter_start + gutter_count)
            .map(|vix| {
                if vix < gutter_rows {
                    let l = self.folds.logical_row(vix);
                    let arrow = self.fold_arrow_state(l);
                    // 越窗/越过文档末尾（底部预留区）取空行兜底（未激活、无底色）：
                    // 折叠优先于光标行（同 `row_bg_overlay` 口径，两处叠层不打架）。
                    let rd = if vix <= last_v && vix >= rv.lo && vix - rv.lo < rv.rows.len() {
                        &rv.rows[vix - rv.lo]
                    } else {
                        &RowData::empty()
                    };
                    let folded = rd.folded.is_some();
                    let bg = if folded {
                        Some(fold_bg)
                    } else if rd.active {
                        Some(active_line_bg)
                    } else {
                        None
                    };
                    Some((l, arrow, bg, folded))
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

    /// 编辑区内查找卡片（Ctrl+F 打开）。取代旧顶部通栏 `render_find_bar`：一段紧凑
    /// 悬浮卡片锚在**编辑区右上角内缘**（顶部 ~8px、右缘让出滚动条 ~8px），含：
    /// ①输入框 ②index/count 计数（紧凑「i/N / 无匹配 / i/N+」）③上一项 ↑ ④下一项 ↓
    /// ⑤关闭 ×。圆角 + 边框 + 阴影，背景/边框/文字/按钮悬停色全走 `ui::theme` 语义色
    /// （亮暗双主题可读）。
    ///
    /// 键盘语义零改动：Enter/Shift+Enter/Escape 在 `on_key_down` 原样按键处理（用
    /// modifiers 区分 shift）——关键：此处**不挂 `on_action(&Enter)` 拦截**（gpui 键动作
    /// 先于 KeyDownEvent 派发，拦截会同时丢掉 Enter 导航与 Shift+Enter 的上一个，见旧
    /// render_find_bar 注释）。按钮只是附加鼠标通道，点击仍走同一套
    /// `find_step`/`close_find`（见 [`CodeEditor::find_btn_clicked`]），回焦输入框。
    ///
    /// 事件互斥（K15）：卡片自身 `on_mouse_down` stop——点卡片任意处（含按钮）不落
    /// 编辑区光标/不误触编辑；卡片**外**照常编辑文本。
    ///
    /// 元素链不变式（feature-delivery §6）：卡片为 `.absolute()` 叠层，链上不再新增
    /// `.relative()` 覆盖既有 `.absolute()`；既有 `find-bar`/`find-bar-input-wrap` id
    /// 保持稳定，新增 `find-count` / `find-prev-btn` / `find-next-btn` / `find-close-btn`
    /// 及无 id 的悬停提示小浮层（无 id → 无 hitbox → 不拦鼠标、不抢焦点）。
    fn render_find_card(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let status = self.find.status();
        let input = self.find_input.clone();
        let colors = ui::theme::Theme::global(cx).colors;
        // 悬浮卡片语义色：背景用「弹层卡」色（亮白/暗灰，与编辑区底区分）、边框/文字/
        // 悬停/禁用全走主题（亮暗双主题可读，禁止裸色）。
        let card_bg: Hsla = colors.popover_bg.into();
        let border: Hsla = colors.border.into();
        let fg: Hsla = colors.fg.into();
        let fg_muted: Hsla = colors.fg_muted.into();
        let fg_disabled: Hsla = colors.fg_disabled.into();
        let hover_fill: Hsla = colors.selected_bg.into();
        // 悬浮阴影底色 = 主题 card_shadow（默认不透明黑，与旧散值 hsla(0,0,0,α) 逐通道同值）。
        let shadow_base: Hsla = colors.card_shadow.into();

        // 卡片：右上角内缘，右缘让出滚动条命中区（沿用既有「right=scrollbar_width+边距」
        // 做法，顶边距 ~8px）。高度自适应内容；宽度紧凑自适（输入框 ≥160px）。
        let card_w = ui::scrollbar::scrollbar_width() + px(8.);
        let mut card = div()
            .id("find-bar")
            .debug_selector(|| "find-bar".into())
            .absolute()
            .top(px(8.))
            .right(card_w)
            .flex()
            .items_center()
            .gap(px(4.))
            .pl(px(8.))
            .pr(px(4.))
            .py(px(3.))
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .rounded_lg()
            // 悬浮阴影（弱单层，同 ui::tooltip 卡片）。
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(2.), shadow_base.opacity(0.18))
                    .blur_radius(px(8.)),
            ])
            // 点击卡片任意处不穿透到编辑区（移动光标）；Enter/Esc 已由输入框统一收敛
            // 为 Submit/Cancel 事件上抛（find_goto.rs 订阅处理），不再挂容器 on_key_down。
            .on_mouse_down(MouseButton::Left, cx.listener(Self::find_bar_mouse_down));

        // ①输入框（`ui::InputState`，IME/组合/UTF-16 依赖其既有实现，本任务不动）。
        card = card.child(
            div()
                .id("find-bar-input-wrap")
                .debug_selector(|| "find-bar-input-wrap".into())
                .w(px(180.))
                .h(px(24.))
                .child(
                    input
                        .map(|i| i.into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                ),
        );
        // ②index/count 计数（紧凑「i/N / 无匹配 / i/N+」）。
        card = card.child(
            div()
                .id("find-count")
                .debug_selector(|| "find-count".into())
                .flex()
                .flex_none()
                .items_center()
                .h(px(24.))
                .px(px(2.))
                .text_color(fg_muted)
                .text_size(px(12.))
                .child(status),
        );
        // ③④⑤ 上一项 / 下一项 / 关闭（图标钮，`FindBtn` 自管 glyph/id/提示文案）。
        for (btn, enabled) in [
            (FindBtn::Prev, self.find_btn_enabled(FindBtn::Prev)),
            (FindBtn::Next, self.find_btn_enabled(FindBtn::Next)),
            (FindBtn::Close, true),
        ] {
            card = card.child(self.find_icon_button(btn, enabled, fg, fg_disabled, hover_fill, cx));
        }
        // 悬停提示小浮层：按钮 hover 延迟后显示中文提示，锚在卡片下方右缘（按钮簇右侧
        // 下方），无 id → 无 hitbox → 不拦鼠标、不抢焦点。
        if let Some(btn) = self.find_tip.shown_btn() {
            card = card.child(
                div()
                    .debug_selector(|| "find-tip".into())
                    .absolute()
                    .top(px(32.))
                    .right(px(0.))
                    .bg(card_bg)
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .px_2()
                    .py_1()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .text_color(fg_muted)
                    .child(btn.tip_label().to_string()),
            );
        }
        card
    }

    /// find 卡片图标按钮：glyph + 悬停底色 + 悬停提示 + 点击导航。禁态（无匹配/无查询）
    /// 置灰文字、点击不动作（`find_btn_clicked` 内守卫）；关闭钮恒可用。点击导航后
    /// `find_btn_clicked` 回焦输入框，按钮自身不抢键盘焦点。
    fn find_icon_button(
        &self,
        btn: FindBtn,
        enabled: bool,
        fg: Hsla,
        fg_disabled: Hsla,
        hover_fill: Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 悬停态由 `self.find_tip.hover == Some(btn)` 派生（按钮内 hover 状态）。
        let hovered = self.find_tip.hover == Some(btn);
        let id = btn.id();
        // 事件（hover/click）先挂在本体元素上，`with_spring` 最后包装只做底色淡入
        // （对齐 ui::menu 同款顺序：事件监听留在内层元素，弹簧包装不改变命中/派发）。
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex()
            .flex_none()
            .size(px(22.))
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .cursor_pointer()
            .text_size(px(15.))
            .text_color(if enabled { fg } else { fg_disabled })
            .child(btn.glyph().to_string())
            .on_hover(cx.listener(move |this, hovered, _window, cx| {
                this.find_btn_hover(cx, btn, *hovered);
            }))
            .on_click(cx.listener(move |this, _e, window, cx| {
                this.find_btn_clicked(window, cx, btn);
            }))
            // 悬停底色淡入（对齐 ui::menu 同款 hover_spring），弹簧态按按钮 id 键控。
            .with_spring(
                format!("find-{id}-hover-bg"),
                ui::motion::hover_spring(if hovered { 1.0 } else { 0.0 }),
                move |d, v| d.bg(hover_fill.opacity(v)),
            )
    }

    /// find 条空白处点按：不穿透到编辑区（防止误把光标搬走）。
    fn find_bar_mouse_down(
        &mut self,
        _e: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
    }

    /// 编辑区内跳转行卡片（Ctrl+G）。取代旧 shell 命令面板 GoToLine：一段紧凑悬浮卡片
    /// **顶部居中**（水平居中于编辑区，非整行通栏；不遮右上角 find 卡），含：
    /// ①输入框 ②内联提示区（钳制 / 非法 / 空）。圆角 + 边框 + 阴影，背景/边框/文字
    /// 全走 `ui::theme` 语义色（亮暗双主题可读）。
    ///
    /// 键盘语义：Enter = 立即 flush（取消防抖、当场跳转，卡片**不关**、输入框保持焦点）；
    /// Esc = 关闭（焦点归还编辑器）。点击卡片内部不穿透编辑区（on_mouse_down stop）；
    /// **点击卡片外**（含编辑区文本 / 滚动条）→ `on_mouse_down_out`（Capture 阶段按
    /// 「鼠标不在本卡 hitbox 内」触发）关闭卡片并还焦点，落点点击语义由编辑区
    /// `on_mouse_down` 照常处理（不吞事件）。
    ///
    /// 元素 id：`goto-card` / `goto-card-input` / `goto-card-hint`（新，稳定可测）。
    /// 链不变式：absolute 链不新增 relative 覆盖既有 absolute；卡片可与 find 卡（右上）
    /// 共存（id/命中原样）。
    fn render_goto_card(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let input = self.goto_input.clone();
        let colors = ui::theme::Theme::global(cx).colors;
        // 悬浮卡片语义色：背景用「弹层卡」色（与编辑区底区分）、边框/文字/错误提示
        // 全走主题（亮暗双主题可读，禁止裸色）。
        let card_bg: Hsla = colors.popover_bg.into();
        let border: Hsla = colors.border.into();
        let fg_muted: Hsla = colors.fg_muted.into();
        let error: Hsla = colors.error.into();
        // 悬浮阴影底色 = 主题 card_shadow（默认不透明黑，与旧散值 hsla(0,0,0,α) 逐通道同值）。
        let shadow_base: Hsla = colors.card_shadow.into();
        // 内联提示文案（owned：`.when` 闭包内 child 需 'static）。
        let hint = self.goto.hint.clone();
        let is_err = self.goto.hint_error;

        // 卡片：顶部居中（外层通栏 absolute + flex 居中，无 id → 无 hitbox → 不拦鼠标、
        // 不抢焦点）；高度自适应内容；宽度紧凑自适。
        let mut card = div()
            .id("goto-card")
            .debug_selector(|| "goto-card".into())
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(8.))
            .pr(px(8.))
            .py(px(3.))
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .rounded_lg()
            // 悬浮阴影（弱单层，同 find 卡片）。
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(2.), shadow_base.opacity(0.18))
                    .blur_radius(px(8.)),
            ])
            // 点击卡片任意处不穿透到编辑区（不落光标）；点击卡片外（Capture 阶段按
            // hitbox 判定）关闭卡片；Enter/Esc 已由输入框统一收敛为 Submit/Cancel 事件
            // 上抛（find_goto.rs 订阅处理），不再挂容器 on_key_down。
            .on_mouse_down(MouseButton::Left, cx.listener(Self::goto_card_mouse_down))
            .on_mouse_down_out(cx.listener(Self::goto_card_mouse_down_out));

        // ①输入框（`ui::InputState`，IME/组合/UTF-16 依赖其既有实现，本任务不动）。
        card = card.child(
            div()
                .id("goto-card-input")
                .debug_selector(|| "goto-card-input".into())
                .w(px(120.))
                .h(px(24.))
                .child(
                    input
                        .map(|i| i.into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                ),
        );
        // ②内联提示区（钳制 / 非法 / 空）：非法用错误色，其余次要前景。
        card = card.child(
            div()
                .id("goto-card-hint")
                .debug_selector(|| "goto-card-hint".into())
                .flex()
                .flex_none()
                .items_center()
                .h(px(24.))
                .px(px(2.))
                .text_size(px(12.))
                .text_color(if is_err { error } else { fg_muted })
                .whitespace_nowrap()
                .when(!hint.is_empty(), |this| this.child(hint.clone())),
        );

        // 外层：顶部通栏 absolute 定位 + 水平居中（无 id → 无 hitbox → 不拦鼠标）。
        div()
            .absolute()
            .top(px(8.))
            .left(px(0.))
            .right(px(0.))
            .flex()
            .justify_center()
            .child(card)
    }

    /// goto 卡片内部点按：不穿透到编辑区（防止误把光标搬走）。
    fn goto_card_mouse_down(
        &mut self,
        _e: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
    }

    /// 点击卡片外（含编辑区文本 / 滚动条 / find 卡）：关闭卡片并还焦点编辑器；落点
    /// 点击语义由编辑区 `on_mouse_down` 照常处理（本钩子不 stop_propagation）。
    fn goto_card_mouse_down_out(
        &mut self,
        _e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_goto(window, cx);
    }
}

/// 把键盘/鼠标/输入 action 挂到核心元素上（keymap 由 `crate::actions::bind_input_keys`
/// 在 "Input" 上下文绑定，与菜单分发（dispatch_action Undo/Cut/…）共用同一套 handler）。
fn attach_input_actions(
    core: gpui::Stateful<gpui::Div>,
    entity: &Entity<CodeEditor>,
    window: &mut Window,
) -> gpui::Stateful<gpui::Div> {
    core.on_key_down(window.listener_for(entity, CodeEditor::on_key_down))
        .on_mouse_down(
            MouseButton::Left,
            window.listener_for(entity, CodeEditor::on_mouse_down),
        )
        .on_mouse_move(window.listener_for(entity, CodeEditor::on_mouse_move))
        .on_mouse_up(
            MouseButton::Left,
            window.listener_for(entity, CodeEditor::on_mouse_up),
        )
        // 拖出元素/窗口外松开（Capture 阶段、`!is_hovered`）→ 同样收口拖选态，防
        // `dragging` 残留（拖选生命周期终点之一，见 CodeEditor::cancel_mouse_drag）。
        .on_mouse_up_out(
            MouseButton::Left,
            window.listener_for(entity, CodeEditor::on_mouse_up_out),
        )
        // —— 输入 action（keymap 由 crate::actions::bind_input_keys 在 "Input" 上下文绑定，
        // 与菜单分发（dispatch_action Undo/Cut/…）共用同一套 handler ——
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveLeft, _w, cx| {
                ed.arrow(false, -1, 0, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveRight, _w, cx| {
                ed.arrow(false, 1, 0, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveUp, _w, cx| {
                ed.arrow(false, 0, -1, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveDown, _w, cx| {
                ed.arrow(false, 0, 1, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveHome, _w, cx| {
                ed.move_line_start(false, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveEnd, _w, cx| {
                ed.move_line_end(false, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &SelectToStartOfLine, _w, cx| {
                ed.move_line_start(true, cx);
                cx.stop_propagation()
            },
        ))
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &SelectToEndOfLine, _w, cx| {
                ed.move_line_end(true, cx);
                cx.stop_propagation()
            },
        ))
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveToStart, _w, cx| {
                ed.move_doc_start(false, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MoveToEnd, _w, cx| {
                ed.move_doc_end(false, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToStart, _w, cx| {
                ed.move_doc_start(true, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectToEnd, _w, cx| {
                ed.move_doc_end(true, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &MoveToPreviousWord, _w, cx| {
                ed.move_word_left(cx);
                cx.stop_propagation()
            },
        ))
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &MoveToNextWord, _w, cx| {
                ed.move_word_right(cx);
                cx.stop_propagation()
            },
        ))
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &SelectToPreviousWordStart, _w, cx| {
                ed.select_word_left(cx);
                cx.stop_propagation()
            },
        ))
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &SelectToNextWordEnd, _w, cx| {
                ed.select_word_right(cx);
                cx.stop_propagation()
            },
        ))
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MovePageUp, _w, cx| {
                ed.page(false, -1, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &MovePageDown, _w, cx| {
                ed.page(false, 1, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &Backspace, _w, cx| {
                ed.backspace(cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &Delete, _w, cx| {
                ed.delete_forward(cx);
                cx.stop_propagation()
            }),
        )
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &DeleteToPreviousWordStart, _w, cx| {
                ed.backspace_word(cx);
                cx.stop_propagation()
            },
        ))
        .on_action(window.listener_for(
            entity,
            |ed: &mut CodeEditor, _: &DeleteToNextWordEnd, _w, cx| {
                ed.delete_word_forward(cx);
                cx.stop_propagation()
            },
        ))
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &IndentInline, _w, cx| {
                ed.tab(false, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &OutdentInline, _w, cx| {
                ed.tab(true, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &SelectAll, _w, cx| {
                ed.select_all(cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &Undo, _w, cx| {
                ed.undo(cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &Redo, _w, cx| {
                ed.redo(cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &Copy, window, cx| {
                ed.copy(window, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &Cut, window, cx| {
                ed.cut(window, cx);
                cx.stop_propagation()
            }),
        )
        .on_action(
            window.listener_for(entity, |ed: &mut CodeEditor, _: &Paste, window, cx| {
                ed.paste(window, cx);
                cx.stop_propagation()
            }),
        )
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
/// pub(crate)：`rows::build_rows` 用它预筛「可能深横滚的超长行」才建宽度索引。
pub(crate) const LONG_LINE_HVIEW_MIN_CHARS: usize = 128;

/// 超长行「横向窗口化」的可视窗两侧**过扫余量**（宽度单位，× 格宽 = px）：
/// 裁剪左/右缘都取「可视窗 ± 余量」，再按**真实字形宽**（`layout_width`）落字符边界。
/// 作用：①横滚时新字符提前入窗被整形（不出现“滚到边缘才突然出现”）；②Tab/emoji
/// 等真实宽度大于单位估算的字符不会被边界截缺；③边界上的亚像素字形足够充裕。
pub(crate) const LONG_LINE_HVIEW_MARGIN_UNITS: usize = 32;

/// 超长行横滚窗口的截取结果（见 [`window_bytes_and_offset`]）。
#[derive(Clone, PartialEq, Debug)]
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
/// - **深横滚快道**：超长行且已横滚（`build_rows` 预建了每字符前缀宽索引
///   [`RowCumWidths`]，随 `rd.widths` 传入）时，走 `window_bytes_and_offset_with_widths`
///   的二分路径——起点/偏移/终点直接由索引定位，每帧不再重扫 O(hscroll) 前缀
///   （minified JS/JSON 等的每帧 O(前缀) 扫描，见 `window_bytes_and_offset_with_widths`）。
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
        rd.widths.as_deref(),
        window,
    ) {
        let LongLineWindow { bytes, offset_px } = win;
        let cropped = SharedString::from(&text[bytes.clone()]);
        let shifted = shift_highlights(&highlights, bytes.start, cropped.len());
        let aligned = char_align_highlights(cropped.as_ref(), &shifted);
        return (StyledText::new(cropped).with_highlights(aligned), offset_px);
    }
    (
        StyledText::new(rd.content.text.clone()).with_highlights(highlights),
        0.,
    )
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
    widths: Option<&RowCumWidths>,
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
    window_bytes_and_offset_with_widths(text, x_left, x_right, &mut char_w, widths)
}

/// 深横滚快道的「每字符前缀真实宽」扁平索引（由 [`build_row_cum_widths`] 一次物化，
/// 跨帧缓存于 `RowWidthCache`、随 `RowData.widths` 传入渲染）。
///
/// - `cum[i]` = 前 `i` 个字符的**真实渲染宽**和（`cum.len() == 字符数+1`、`cum[0]==0`）；
/// - `byte_offs[i]` = 第 `i` 个字符起点的**字节偏移**（与 `cum` 平行、同长，落到 UTF-8
///   字符边界；u32 存以省内存，行 > 4 GiB 时不建索引回落逐字扫描）；
/// - 零宽字符（组合符等 `w<=0`）不推进 `cum`——与逐字扫描的 `w <= 0.0 → continue`
///   逐位一致，故二分定位结果与扫描等价。
///
/// 宽度单调不减 → 起点/终点的「首个越过阈值」都可在 `cum` 上二分（O(log 字符数)），
/// 起点偏移直接读 `cum[s]`（= 前缀真实宽，与扫描累加同值），把超长行深横滚每帧的
/// 「0 → 右缘」O(前缀) 逐字布局查询降为 O(log + 可视窗) 量级（minified 行收益最大）。
pub(crate) struct RowCumWidths {
    pub(crate) cum: Vec<f32>,
    pub(crate) byte_offs: Vec<u32>,
}

/// 物化一行的前缀宽索引：按行文本逐字符用 `char_w`（生产 = TextSystem 真实字形宽，
/// 测试 = `char_width_units` 等宽度函数）累加一次，O(字符数)。偶发性的一次成本，
/// 之后深横滚窗口化每帧只 O(log + 视窗) 消费本索引。`None` = 行超 4GiB（u32 偏移
/// 溢出）或极短行无收益（外层另有长度门）。
pub(crate) fn build_row_cum_widths(
    text: &str,
    char_w: &mut impl FnMut(char) -> f32,
) -> Option<RowCumWidths> {
    if text.len() > u32::MAX as usize {
        return None; // u32 字节偏移溢出 → 回落逐字扫描
    }
    let mut cum = Vec::with_capacity(text.len().saturating_add(1));
    let mut byte_offs = Vec::with_capacity(cum.capacity());
    cum.push(0.0);
    byte_offs.push(0);
    let mut x = 0.0f32;
    let mut byte = 0usize;
    for ch in text.chars() {
        let w = char_w(ch);
        if w > 0.0 {
            x += w;
        }
        byte += ch.len_utf8();
        cum.push(x);
        byte_offs.push(byte as u32);
    }
    Some(RowCumWidths { cum, byte_offs })
}

/// 生产侧建索引（`build_rows` 用）：等宽字体族 + 字号解析真实字形宽后建
/// [`RowCumWidths`]。与 `long_line_window` 的 `char_w` 同一 `layout_width` 口径；
/// 同字符连续段用备忘短路，长同质行（minified "xxx…"）只查一次字体。
pub(crate) fn build_row_cum_widths_real(
    text: &str,
    mono: &SharedString,
    font_size: Pixels,
    window: &mut Window,
) -> Option<RowCumWidths> {
    let ts = window.text_system();
    let font_id = ts.resolve_font(&gpui::font(mono.clone()));
    let mut last: Option<(char, f32)> = None;
    build_row_cum_widths(text, &mut |ch| {
        if let Some((lc, lw)) = last
            && lc == ch
        {
            return lw;
        }
        let w = ts.layout_width(font_id, font_size, ch).as_f32();
        last = Some((ch, w));
        w
    })
}

/// 用 [`RowCumWidths`] 的二分路径定位横向窗口（结果与逐字扫描逐位等价，见
/// `window_bytes_and_offset_with_widths` 的推导）：
/// - 终点字符（排他）`e` = 首个 `cum[k] ≥ x_right` 的 `k`；若行尾恰切在右缘
///   （`e == n` 且 `cum[n] == x_right`，最后一个字符以右缘结束、其后整行都放得下）
///   → 回落 `None`（与扫描的「循环走完 → None」一致）；
/// - 起点字符 `s` = 首个 `cum[j] > x_left` 的 `j − 1`（该字符下沿越过 x_left，整字包含）；
/// - `offset_px = cum[s]`（起点前真实前缀宽）、字节区间 `byte_offs[s] .. byte_offs[e]`。
/// 未锁定左缘（整行窄于 x_left，`s` 不存在）→ 走扫描同款防御分支（`0 .. byte_offs[e]`）。
fn window_by_width_index(
    x_left: f32,
    x_right: f32,
    widths: &RowCumWidths,
) -> Option<LongLineWindow> {
    let cum = &widths.cum;
    let byte_offs = &widths.byte_offs;
    let n = cum.len().saturating_sub(1); // 字符数
    if n == 0 {
        return None;
    }
    // 终点（排他字符下标 e）与起点（字符下标 s），均在单调不减的 cum 上二分。
    let e = cum.partition_point(|&c| c < x_right); // 首个 cum ≥ x_right 的下标
    let e = (e <= n).then_some(e); // e==len → 整行 < x_right → 放得下 → None
    let s = {
        let j = cum.partition_point(|&c| c <= x_left); // 首个 cum > x_left 的下标
        (j > 0 && j <= n).then_some(j - 1) // 起点字符 = j−1；J==len → 未锁定左缘 → None
    };
    match (s, e) {
        (Some(s), Some(e)) if e > s && !(e == n && cum[n] == x_right) => Some(LongLineWindow {
            bytes: byte_offs[s] as usize..byte_offs[e] as usize,
            offset_px: cum[s],
        }),
        // 未锁定左缘（整行窄于 x_left）：终点命中处 = 前缀 + 偏移 0（扫描防御分支）。
        (None, Some(e)) if e > 0 => Some(LongLineWindow {
            bytes: 0..byte_offs[e] as usize,
            offset_px: 0.0,
        }),
        _ => None,
    }
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
    char_w: impl FnMut(char) -> f32,
) -> Option<LongLineWindow> {
    window_bytes_and_offset_with_widths(text, x_left, x_right, char_w, None)
}

/// [`window_bytes_and_offset`] 的**深横滚快道**：额外的 `widths` = 该行的前缀真实宽
/// 索引（[`RowCumWidths`]，由 `build_rows` 预建、随 `RowData.widths` 传入）。
///
/// - `widths = Some` 且 `x_left > 0`（已深横滚）→ 走 `window_by_width_index` 二分：
///   起点/终点/偏移直接由索引定位，**不再逐字重扫码前缀**——把每帧 O(hscroll) 的
///   `layout_width` 查询降为 O(log 字符数) 数组二分 + 视窗量级的 O(1) 读数。
/// - `widths = None` 或未横滚 → 回落下方**逐字扫描**（原实现，语义逐位不变）。
///
/// 两条路径结果**逐位等价**（宽度单调不减 → 累计宽在字符上二分成立）：起点 = 首个
/// 越过 x_left 右缘的整字、终点排他 = 首个起点 ≥ x_right 的字符、偏移 = 起点前真实
/// 前缀宽——CJK/emoji 多字节字符不被劈开（字节区间取自 `byte_offs[s..e]`，落字符
/// 边界）。
pub(crate) fn window_bytes_and_offset_with_widths(
    text: &str,
    x_left: f32,
    x_right: f32,
    mut char_w: impl FnMut(char) -> f32,
    widths: Option<&RowCumWidths>,
) -> Option<LongLineWindow> {
    if text.is_empty() || x_right <= x_left {
        return None;
    }
    // 深横滚快道：已具宽度索引 → 二分定位，不重扫前缀。
    if x_left > 0.0
        && let Some(w) = widths
    {
        return window_by_width_index(x_left, x_right, w);
    }
    // —— 逐字扫描（未深滚 / 无索引时语义不变）——
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
        && s.end > s.start
    {
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
            && r.start < b
        {
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

/// 通用版 `merge_highlights`：叠加**一组**（互不重叠、升序）overlay 段，每段覆盖
/// `background_color`（`merge_highlights` 只支持单个选区的特化；find 高亮层有多段、
/// 两种底色：当前命中/其余匹配）。
///
/// 输入：`runs`（非重叠、升序，含既有语法/选区合并结果）与 `overlays`（非重叠、升序）
/// 均按字节区间切单元，产出严格递增、不重叠、每段都在行内的合并结果。overlay 只改
/// 底色，其余样式保留 base run 的。
pub(crate) fn merge_multi_highlights(
    runs: &[(Range<usize>, HighlightStyle)],
    overlays: &[(Range<usize>, HighlightStyle)],
) -> Vec<(Range<usize>, HighlightStyle)> {
    if overlays.is_empty() {
        return runs.to_vec();
    }
    // 收集所有切分边界：样式段边界 ∪ overlay 边界。
    let mut bounds: Vec<usize> = Vec::with_capacity(runs.len() * 2 + overlays.len() * 2 + 2);
    for (r, _) in runs {
        let s = r.start.min(r.end);
        let e = r.end.max(r.start);
        if e <= s {
            continue;
        }
        bounds.push(s);
        bounds.push(e);
    }
    for (r, _) in overlays {
        let s = r.start.min(r.end);
        let e = r.end.max(r.start);
        if e <= s {
            continue;
        }
        bounds.push(s);
        bounds.push(e);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut ri = 0usize;
    let mut oi = 0usize;
    for (i, a) in bounds.iter().copied().enumerate() {
        let Some(&b) = bounds.get(i + 1) else { break };
        if b <= a {
            continue;
        }
        while ri < runs.len() && runs[ri].0.end <= a {
            ri += 1;
        }
        while oi < overlays.len() && overlays[oi].0.end <= a {
            oi += 1;
        }
        let mut style = HighlightStyle::default();
        if let Some((r, s)) = runs.get(ri)
            && r.start < b
        {
            style = *s;
        }
        // overlays 互不重叠：首个 `end > a` 者即唯一可能覆盖本单元的 overlay。
        if let Some((o, os)) = overlays.get(oi)
            && o.start < b
            && o.end > a
            && let Some(bg) = os.background_color
        {
            style.background_color = Some(bg);
        }
        if style != HighlightStyle::default() {
            out.push((a..b, style));
        }
    }
    out
}

/// 括号字符**前景色**叠层（括号文本着色，对齐 VS Code bracketPairColorization）：
/// 与 [`merge_multi_highlights`] 同构，但 overlay 是**单字节**字符 span（`col..col+1`）
/// 且只覆 `color`（`palette[depth % 3]`，与括号引导线同色池），bg/weight/italic/其余
/// span 保持 `base` 原样。输入 `base`（非重叠、升序，含语法/选区合并结果）与 `chars`
/// （按 line 内的 col 升序）均按字节区间切单元，产出严格递增、不重叠、每段都在行内。
///
/// find/选区叠层只改 bg → 不吞括号前景色（VSCode 口径：颜色与选中底色正交）。
pub(crate) fn merge_color_overrides(
    base: &[(Range<usize>, HighlightStyle)],
    chars: &[BracketChar],
    palette: &[Hsla; 3],
) -> Vec<(Range<usize>, HighlightStyle)> {
    if chars.is_empty() {
        return base.to_vec();
    }
    // 收集所有切分边界：样式段边界 ∪ 括号字符 span 边界。
    let mut bounds: Vec<usize> = Vec::with_capacity(base.len() * 2 + chars.len() * 2 + 2);
    for (r, _) in base {
        let s = r.start.min(r.end);
        let e = r.end.max(r.start);
        if e <= s {
            continue;
        }
        bounds.push(s);
        bounds.push(e);
    }
    for c in chars {
        bounds.push(c.col);
        bounds.push(c.col + 1);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut bi = 0usize;
    let mut ci = 0usize;
    for (i, a) in bounds.iter().copied().enumerate() {
        let Some(&b) = bounds.get(i + 1) else { break };
        if b <= a {
            continue;
        }
        while bi < base.len() && base[bi].0.end <= a {
            bi += 1;
        }
        while ci < chars.len() && chars[ci].col < a {
            ci += 1;
        }
        let mut style = HighlightStyle::default();
        if let Some((r, s)) = base.get(bi)
            && r.start < b
        {
            style = *s;
        }
        // chars 互不重叠（单字节升序）：首个 `col+1 > a` 者即唯一可能覆盖本单元的字符。
        if let Some(c) = chars.get(ci)
            && c.col < b
            && c.col + 1 > a
        {
            style.color = Some(palette[c.depth % 3]);
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
    /// 行号常规色（`gutter_fg`，折叠/普通行灰显用）。
    fg: Hsla,
    /// 活动行（持焦光标行）行号强调色（= 编辑器主文字色 `self.fg`，主题驱动、
    /// 比 `gutter_fg` 亮）。
    active_fg: Hsla,
}

/// 行号单元格：行高与文本行一致，由共享 ListState 的第二个 `list()` 驱动 → 行号与
/// 文本行对齐、随滚动同步移动且不闪烁。`arrow` = 折叠指示箭头状态
/// （[`CodeEditor::fold_arrow_state`] 透传）：`Some(true)`=已折叠首行 → 右向「▸」；
/// `Some(false)`=候选起点 → 下向「▾」；`None`=不画。
///
/// `bg` = 光标行/折叠行整行高亮延伸到行号列时的单元格底色（`Some` 时铺满整格、
/// 兼顾行号+箭头槽；`None` 无底色）；`folded` = 该行是否折叠首行，决定底色
/// selector（折叠行 `ce-gutter-fold-bg` / 光标行 `ce-gutter-active-bg`，镜像文本侧
/// `ce-fold-row-bg`/`ce-active-line-bg` 的分叉命名）。**行号数字本身**在活动行
/// （有底色且非折叠行）用 `gv.active_fg`（主文字色）强调并挂
/// `ce-gutter-number-active`；其余行保持 `gv.fg` 与 `ce-gutter-number`。布局：
/// flex-grow 的**数字格**（内部 `justify_end` 右对齐）+ 固定宽**箭头槽**（恒占位）→
/// 无论有无箭头，数字右缘都钉在「gutter 右缘 − 右内边距 − 槽宽」，有箭头行不把行号
/// 左推、与无箭头行数字列对齐（避免真机「行号被挤/错位」回归）。
fn number_element(
    ix: usize,
    arrow: Option<bool>,
    bg: Option<Hsla>,
    folded: bool,
    gv: &GutterView,
) -> gpui::Div {
    let number = SharedString::from((ix + 1).to_string());
    // 活动行 = 有底色且非折叠行（折叠行保持灰显在折叠底色上）。
    let active = bg.is_some() && !folded;
    let number_selector = if active {
        "ce-gutter-number-active"
    } else {
        "ce-gutter-number"
    };
    div()
        .h(gv.line_height)
        .flex()
        .items_center()
        .pl(px(GUTTER_LEFT_PAD_PX))
        .pr(px(GUTTER_RIGHT_PAD_PX))
        .text_color(gv.fg)
        .text_size(gv.font_size)
        .font_family(gv.mono.clone())
        // 光标行/折叠行高亮延伸到行号列：整格（含行号+箭头槽）铺同一底色；有底色时
        // 按行语义挂分叉 selector（折叠 `ce-gutter-fold-bg` / 光标 `ce-gutter-active-bg`）。
        .when_some(bg, |this, color| {
            let sel = if folded {
                "ce-gutter-fold-bg"
            } else {
                "ce-gutter-active-bg"
            };
            this.debug_selector(move || sel.into()).bg(color)
        })
        .child(
            div()
                .debug_selector(move || number_selector.into())
                .h_full()
                .flex()
                .flex_grow(1.0)
                .justify_end()
                .items_center()
                .text_color(if active { gv.active_fg } else { gv.fg })
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

    /// 行网格吸附（单元）：顶就近吸附设备像素格、底 = 行顶 + 整行数×渲染行距
    /// floor(lh·sf)；顶/底都落设备格，底不越过原底。
    #[test]
    fn snap_guide_extent_snaps_to_row_grid() {
        // sf=2.0、lh=10.0 → 渲染行距 floor(10*2)=20。6 行块 h=60：
        // 顶 round(10.2*2=20.4)=20 → 10.0（向下取整 → 底不越过）；底 20+6*20=140。
        let (top, h) = CodeEditor::snap_guide_extent(10.2, 60.0, 10.0, 2.0);
        assert_eq!((top, h), (10.0, 60.0));
        assert_eq!(top * 2.0, 20.0, "(a) 顶在设备格");
        assert_eq!((top + h) * 2.0, 140.0, "(a) 底在设备格");
        assert!(
            (top + h) * 2.0 <= (10.2 + 60.0) * 2.0,
            "(b) 底不越过原底 140.4"
        );

        // .5 临界：round 半离开 0 → 顶 10.75*2=21.5 → 22 → 11.0；底 22+3*20=82。
        let (top, h) = CodeEditor::snap_guide_extent(10.75, 30.0, 10.0, 2.0);
        assert_eq!(top, 11.0, "round(21.5)=22");
        assert_eq!(h, 30.0);
        assert_eq!((top + h) * 2.0, 82.0, "(a) 底在设备格");

        // sf=1.0：整数输入零变化（常规/测试环境不动画）。
        assert_eq!(
            CodeEditor::snap_guide_extent(10.0, 20.0, 10.0, 1.0),
            (10.0, 20.0)
        );
    }

    /// 复刻本机真实数据：lh=43.077、sf=1.75（lh·sf=75.385 为小数）→ 渲染行距
    /// floor=75。6 行块（y=43.08,h=258.46）：顶 round(75.39)=75、底 75+6*75=525；
    /// 3 行块（h=129.23）：底 75+3*75=300——底落在 75 的行网格上、不越过原底。
    #[test]
    fn snap_guide_extent_lands_on_row_grid() {
        let lh = 43.077_f32;
        let sf = 1.75_f32;
        // 6 行块：y*sf=75.39 → 顶 75；rows=round(258.46/43.077)=6 → 底 525。
        let (gy, gh) = CodeEditor::snap_guide_extent(43.08, 258.46, lh, sf);
        assert_eq!(gy * sf, 75.0, "(a) 顶就近在设备格");
        assert_eq!((gy + gh) * sf, 525.0, "(c) 底 = 75 + 6*75 行网格");
        assert!(((gy + gh) * sf).fract() == 0.0, "(a) 顶/底都在设备格");
        assert!(
            (gy + gh) * sf <= (43.08 + 258.46) * sf,
            "(b) 不越过原底 527.695"
        );

        // 3 行块：底 75 + 3*75 = 300。
        let (gy, gh) = CodeEditor::snap_guide_extent(43.08, 129.23, lh, sf);
        assert_eq!(gy * sf, 75.0);
        assert_eq!((gy + gh) * sf, 300.0, "(c) 3 行档 = 75+3*75");
        assert!(
            (gy + gh) * sf <= (43.08 + 129.23) * sf,
            "(b) 不越过原底 301.54"
        );
    }

    /// 退化输入（`h` 极小，仅纯函数边界用例，真实路径 `h ≥ lh` 恒满足）：段占整行数
    /// 就近取整到 0 → `bottom_dev == top_dev` → `gh` 被 `.max(0)` 夹 0，不 panic。
    #[test]
    fn snap_guide_extent_clamps_degenerate_negative_height() {
        // rows = round(0.2/10)=0 → 底 == 顶 → gh 夹 0（不画反向/负高竖线）。
        let (top, gh) = CodeEditor::snap_guide_extent(10.75, 0.2, 10.0, 2.0);
        assert_eq!(top, 11.0, "顶仍就近吸附 10.75*2=21.5 → round 22 → 11.0");
        assert_eq!(gh, 0.0, "(d) rows=0 → gh 夹 0");
        assert!(gh >= 0.0, "不产生负高");
    }

    /// lh·sf 小数部分 >0.5 时，旧 floor 行距每行短 1px、底端累计不贴行（旧代码会短
    /// rows×1px）；rhtz（ceil(lh·sf−0.5)）与 taffy 同式 → 底恰贴「下一行顶」。
    /// lh=43.2、sf=1.75：lh·sf=75.6 → 吸附行距 76 设备px（旧 floor=75，每行短 1px）。
    #[test]
    fn snap_guide_extent_frac_above_half_lands_on_real_row_top() {
        let lh = 43.2_f32;
        let sf = 1.75_f32;
        let pitch = 76.0_f32 / sf; // rhtz 吸附行距（设备 76px；旧 floor 会 75px）。
        // 浅块：y=0、6 行。底 = 0 + 6·76 = 456（旧 floor 会 450，短 6 = rows×1px）。
        let (top, h) = CodeEditor::snap_guide_extent(0.0, 6.0 * pitch, lh, sf);
        assert!(
            ((top + h) * sf - 456.0).abs() < 0.01,
            "6 行 → 底 456 == 行6顶精确（旧 floor=450 短 1px/行）"
        );
        // 深块+滚动：y=10·pitch−1.3（= ct−top_vis=10 行、off=1.3、head=0）、6 行。
        // top_dev=round(10·76 − 1.3·1.75)=round(757.725)=758；底=758+6·76=1214==行cb顶。
        let y = 10.0 * pitch - 1.3;
        let (top2, h2) = CodeEditor::snap_guide_extent(y, 6.0 * pitch, lh, sf);
        assert_eq!(top2 * sf, 758.0, "顶就近吸附 758 设备px");
        assert!(
            ((top2 + h2) * sf - 1214.0).abs() < 0.01,
            "底 1214 == 行cb顶精确（旧 floor 会短 6px）"
        );
        assert!((h2 * sf - 456.0).abs() < 0.01, "段高 6 行 = 456 设备px");
        // 20 行：底 = 20·76 = 1520（旧 floor=1500，短 20px）。
        let (top3, h3) = CodeEditor::snap_guide_extent(0.0, 20.0 * pitch, lh, sf);
        assert!(
            ((top3 + h3) * sf - 1520.0).abs() < 0.01,
            "20 行 → 底 1520（旧 floor=1500 短 20px）"
        );
    }

    /// head/tail 非零（HEAD=TAIL=0.25·lh）时，段 y/h 含端点半行、经吸附后底仍落
    /// 「≥ 行cb顶、< 行cb底」区间、不越过渲染行界。段由 `bracket_guide_segments_flagged`
    /// 发射（y=(ct−top_vis)·pitch−off+head、h=(cb−ct)·pitch−head+tail，off=top_vis=0），
    /// head/tail 相互抵消于高 → h = 整行数×pitch；底 = round(y·sf) + rows·pitch_dev。
    #[test]
    fn snap_guide_extent_head_tail_bottom_stays_within_close_row() {
        let lh = 43.2_f32;
        let sf = 1.75_f32;
        // BRACKET_GUIDE_OPEN_HEAD_LINES / BRACKET_GUIDE_CLOSE_TAIL_LINES 现为 0.0/0.0；
        // 本用例手写 0.25 常数验证「非零端点半行」语义（与 config 常量无关的纯几何用例）。
        let head = 0.25_f32 * lh;
        let tail = 0.25_f32 * lh;
        let pitch = 76.0_f32 / sf;
        let (ct, cb) = (2.0_f32, 5.0_f32); // 段覆盖视觉行 2..5（3 行），闭行 = 行5
        let y = ct * pitch + head; // = (ct−top_vis)·pitch − off + head
        let h = (cb - ct) * pitch - head + tail; // 3·pitch（head/tail 抵消）
        let (top, gh) = CodeEditor::snap_guide_extent(y, h, lh, sf);
        // 底（设备px）推导：round(y·sf) = round(2·76 + 0.25·lh·sf) = round(152+18.9) = 171；
        // rows = round(3·pitch/pitch) = 3；底 = 171 + 3·76 = 399。行 cb(=5) 顶 = 5·76 = 380、
        // 底 = 6·76 = 456 → 399 ∈ [380, 456) 且不越过行 cb 底（旧 floor 行距会 375/450）。
        let bottom = (top + gh) * sf;
        assert!(
            (bottom - 399.0).abs() < 0.01,
            "非零 head/tail 底仍精确贴行网格 399"
        );
        assert!(
            bottom >= 380.0 && bottom < 456.0,
            "底落在行 cb(=5) 区间 [行顶 380, 行底 456)、不越过渲染行界：{bottom}"
        );
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
        assert_eq!(
            w.bytes,
            15..48,
            "不劈开 CJK：起点落到包含 11 的首字（单位 10）"
        );
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
        assert_eq!(
            out,
            vec![(0..5, s), (3..10, s)],
            "首段收窄+平移、窗内段平移、窗外段丢弃"
        );
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

    /// 红测（性能结构）：深横滚既有宽度索引时，窗口化不得再逐字重扫前缀。
    /// 判别式：把 `window_bytes_and_offset_with_widths` 的 `Some(widths)` 分支退回
    /// 逐字扫描 → `char_w` 访问 O(100k)、本断言红；实现二分快道后 → 绿。
    #[test]
    fn window_deep_scroll_widths_index_not_prefix_scan() {
        let text = "x".repeat(200_000);
        // 建索引 = 一次性摊销价（每字符一次真实宽查询，属缓存构建，不计窗口化成本）。
        let widths = build_row_cum_widths(&text, &mut |_| 1.0).unwrap();
        // 已建索引的深横滚（x_left=100k）窗口化自身的字符访问：不得 O(100k)。
        let mut calls = 0usize;
        let w = window_bytes_and_offset_with_widths(
            &text,
            100_000.,
            100_080.,
            |ch| {
                calls += 1;
                units_w(ch)
            },
            Some(&widths),
        )
        .unwrap();
        assert!(
            calls <= 8,
            "深横滚窗口化应走索引/二分、不逐字查宽（calls={calls}）"
        );
        assert_eq!(w.bytes, 100_000..100_080);
        assert_eq!(w.offset_px, 100_000.);
    }

    /// 快道（宽度索引二分）与逐字扫描必须逐位等价——覆盖 CJK/emoji/混排/右缘恰切
    /// 行尾/零宽等既有边界：改快道不得悄悄改窗口语义。
    #[test]
    fn width_index_fast_path_matches_scan_equivalence() {
        let cases: Vec<(String, f32, f32)> = vec![
            ("中".repeat(30), 10., 30.),
            ("中".repeat(30), 11., 31.),
            ("中".repeat(30), 0., 29.),
            ("🍎".repeat(20), 9., 15.),
            ("a中b中c".to_string(), 2., 4.),
            ("x".repeat(200), 100., 140.),
            ("x".repeat(3), 0., 3.), // 行尾恰切在右缘：循环走完 → None
            ("x".repeat(200_000), 100_000., 100_080.),
        ];
        for (text, xl, xr) in cases {
            let widths = build_row_cum_widths(&text, &mut units_w).unwrap();
            let scan = window_bytes_and_offset(&text, xl, xr, units_w);
            let fast = window_bytes_and_offset_with_widths(&text, xl, xr, units_w, Some(&widths));
            assert_eq!(fast, scan, "快道与扫描对 {text:?}[{xl},{xr}) 应等价");
        }
    }

    /// 零宽字符（组合符）+ 深滚：快道不劈开、不把前缀组合字丢弃，且与扫描等价。
    #[test]
    fn width_index_fast_path_matches_scan_zero_width() {
        // "x…x" 前缀含组合重音（≈0 宽）：扫描与快道在 x_left 落在组合字位置时同解。
        let text = format!("x\u{301}{}", "y".repeat(1000));
        let widths = build_row_cum_widths(&text, &mut |ch| {
            if ch == '\u{301}' { 0.0 } else { 1.0 }
        })
        .unwrap();
        for (xl, xr) in [(0.5, 200.), (1.5, 210.), (500., 700.)] {
            let scan =
                window_bytes_and_offset(
                    &text,
                    xl,
                    xr,
                    |ch| {
                        if ch == '\u{301}' { 0.0 } else { 1.0 }
                    },
                );
            let fast = window_bytes_and_offset_with_widths(
                &text,
                xl,
                xr,
                |ch| {
                    if ch == '\u{301}' { 0.0 } else { 1.0 }
                },
                Some(&widths),
            );
            assert_eq!(fast, scan, "快道与扫描对 x_left={xl} 应等价");
        }
    }
}
