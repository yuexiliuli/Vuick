//! `CodeEditor` 视图：自研的文本/代码编辑组件（首版）。
//!
//! 渲染：复用 gpui 原生 `ListState`（统一行高、行级虚拟化、内置竖滚动条）+ 外圈
//! `overflow_x_scroll` 横向容器（`h_handle` + gpui-component 滚动条）——
//! 与 markdown 预览同一套成熟模式。行号 gutter 是编辑区左缘的固定叠层
//! （按 ListState 竖向滚动对齐，不随内容横向滚动），每行内容 = `StyledText`
//! （语法着色 + 选区底色）+ 光标竖条。
//!
//! 输入：
//! - 可打印字符/IME 经 [`EntityInputHandler`]（`EditorElement::paint` 里
//!   `window.handle_input`，与 gpui-component 同款挂钩）；
//! - 控制键（方向/Home/End/PageUp/Backspace/Delete/Enter/Tab）+ Ctrl 组合
//!   （C/X/V/A/Z/Y、词移动/删除）全部在 `on_key_down` 按 `Keystroke` 处理，
//!   不依赖 keymap/action 注册，天然拿到 `Modifiers`（Shift 扩展选区），
//!   也不与组件库按键映射打架；
//! - 鼠标：点击定位 / 拖动扩展 / Shift+点击扩展。
//!
//! 撤销：见 [`crate::editor::undo`]；高亮：见 [`crate::editor::highlight`]。
//!
//! 首版取舍：不放行内折行；光标列按等宽字符近似；无多光标；IME 组合各步为独立撤销单元。

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    div, list, point, px, rgb, size, AnyElement, App, Bounds, ClipboardItem, Context, DispatchPhase,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, LayoutId, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, StyledText, TextInputConfiguration,
    UTF16Selection, Window, prelude::FluentBuilder as _,
};
use gpui_component::input::Rope;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::input::{
    Backspace, Copy, Cut, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, Enter,
    IndentInline, MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight,
    MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveUp, OutdentInline, Paste,
    Redo, SelectAll, SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart,
    SelectToStart, SelectToStartOfLine, Undo,
};

use crate::editor::highlight::Highlighter;
use crate::editor::movement;
use crate::editor::undo::{EditIntent, UndoManager};
use crate::editor::{Buffer, Selection};

/// 代码行高亮回看行数（多行注释/字符串上下文不错位）。
const HL_LOOKBACK_LINES: usize = 16;
/// 光标闪烁半周期（ms）。
const BLINK_MS: u64 = 500;
/// 输入后光标暂停闪烁（实心显示，ms）。
const BLINK_PAUSE_MS: u64 = 300;
/// 光标条宽（px）。
const CARET_WIDTH_PX: f32 = 2.;
/// 可见行数下限（首帧视口未知时也至少预取这么多）。
const MIN_VISIBLE_ROWS: usize = 12;
/// 横向滚动右缘留白的列数缓冲。
const HSCROLL_MARGIN_COLS: usize = 4;
/// 行号与编辑区左缘（侧边栏侧）之间的留白（px）。
const GUTTER_LEFT_PAD_PX: f32 = 12.;
/// 行号与文本之间的留白（px）。
const GUTTER_RIGHT_PAD_PX: f32 = 6.;

/// 编辑器对外事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    /// 文本被修改（撤销/重做/输入/删除均触发）。
    Edited,
    Focus,
    Blur,
}

/// 行渲染数据（`list` 闭包只读）。
#[derive(Clone)]
struct RowData {
    /// 渲染窗口内的文本（已按横向滚动裁切）。
    text: SharedString,
    /// 样式段（相对 `text` 字节；语法色 + 选区底）。
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    /// 本行是否绘制光标 + 光标 x（相对文本起点，px）。
    caret: Option<f32>,
}

impl RowData {
    fn empty() -> Self {
        Self {
            text: SharedString::new_static(""),
            highlights: Vec::new(),
            caret: None,
        }
    }
}

/// `list` 闭包捕获的只读渲染上下文。
struct RowView {
    lo: usize,
    rows: Vec<RowData>,
    line_height: Pixels,
    mono: SharedString,
    font_size: Pixels,
    caret_color: Hsla,
}

/// 自研代码编辑器（首版）。
pub struct CodeEditor {
    buffer: Buffer,
    selection: Selection,
    undo: UndoManager,
    focus_handle: FocusHandle,
    /// 文本区 ListState（竖向虚拟化 + 滚动 + 竖滚动条量程）。行号列用独立的
    /// [`Self::gutter_list_state`]，两者**锁步**（见 `sync_gutter_scroll`），
    /// 不共用同一 StateInner——共用会让换宽/统一高度提示互相干扰（量程坍缩实测）。
    list_state: ListState,
    /// 行号列 ListState：与 `list_state` 同 item 数、同行高，滚动由
    /// `sync_gutter_scroll` 从文本区逐帧对齐。行号不再手工按 `logical_scroll_top`
    /// 反推 y（旧叠层每次 render 重建，滚轮滚动时与 List 行绘制不同步 → 序号
    /// 「翻动/闪烁/晃动」），而是由 List 自身虚拟化渲染。
    gutter_list_state: ListState,
    h_handle: ScrollHandle,
    highlighter: Highlighter,
    language: Option<String>,

    // —— 字体度量（render 时由主题解析）——
    mono: SharedString,
    font_size: Pixels,
    line_height: Pixels,
    char_width: Pixels,
    metrics_ready: bool,

    // —— 布局状态 ——
    row_count: usize,
    hscroll_cols: usize,
    visible_chars: usize,
    gutter_chars: usize,
    gutter_width_px: Pixels,
    content_w_px: f32,
    uniform_applied: Option<Pixels>,
    /// 行号列已应用统一高度提示的行高（行号行恒等于 line_height，避免首次整表
    /// 测量 O(n) 与量程失真；仅在行高/重置后重挂）。
    gutter_uniform_applied: Option<Pixels>,
    /// 统一高度提示最近一次重挂时间（节流：gpui 换宽会清空 size_hint，
    /// 导致量程坍缩，检测到坍缩后重挂，但同一时间窗口内至多重挂一次）。
    last_uniform_apply: Option<std::time::Instant>,
    /// 是否已观察过一次「量程健康」帧。健康后才启用 120ms 重挂冷却；
    /// 从未健康（首帧/新文档/紧跟其后的换宽坍缩）时不冷却，立即自愈。
    hint_healthy: bool,

    // —— 主题色快照 ——
    bg: Hsla,
    fg: Hsla,
    gutter_fg: Hsla,
    sel_bg: Hsla,
    caret_color: Hsla,

    // —— IME ——
    marked_range: Option<Range<usize>>,
    /// 最近一次编辑的字节区间 `(start, old_end, new_end)`（当前文本坐标），
    /// 由插入/撤销/重做写，供预览增量同步取走累积 dirty。
    last_edit: Option<(usize, usize, usize)>,
    /// 字号覆盖（px，`None` = 主题默认）。
    font_override: Option<f32>,

    // —— 交互状态 ——
    dragging: bool,
    focused: bool,
    caret_visible: bool,
    caret_pause_until: Option<Instant>,
    blink_epoch: u64,
    blink_running: bool,
    focus_registered: bool,
    element_bounds: Bounds<Pixels>,

    // —— 配置 ——
    tab_size: usize,
}

impl CodeEditor {
    /// 创建编辑器（空文档）。
    pub fn new(cx: &mut App) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let buffer = Buffer::default();
        let row_count = buffer.line_count().max(1);
        let list_state = ListState::new(row_count, ListAlignment::Top, px(16.));
        let gutter_list_state = ListState::new(row_count, ListAlignment::Top, px(16.));
        Self {
            buffer,
            selection: Selection::collapsed(0),
            undo: UndoManager::new(),
            focus_handle,
            list_state,
            gutter_list_state,
            h_handle: ScrollHandle::new(),
            highlighter: Highlighter::new(),
            language: None,
            mono: SharedString::new_static(""),
            font_size: px(0.),
            line_height: px(0.),
            char_width: px(0.),
            metrics_ready: false,
            row_count,
            hscroll_cols: 0,
            visible_chars: 80,
            gutter_chars: 3,
            gutter_width_px: px(0.),
            content_w_px: 0.,
            uniform_applied: None,
            gutter_uniform_applied: None,
            last_uniform_apply: None,
            hint_healthy: false,
            bg: rgb(0xffffff).into(),
            fg: rgb(0x000000).into(),
            gutter_fg: rgb(0x9aa0a6).into(),
            sel_bg: rgb(0xd0d7de).into(),
            caret_color: rgb(0x000000).into(),
            marked_range: None,
            last_edit: None,
            font_override: None,
            dragging: false,
            focused: false,
            caret_visible: false,
            caret_pause_until: None,
            blink_epoch: 0,
            blink_running: false,
            focus_registered: false,
            element_bounds: Bounds::default(),
            tab_size: 4,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 设置语法高亮语言（`Some("rust")` / `None` 关闭）。
    pub fn set_language(&mut self, language: Option<&str>) {
        self.language = language.map(|s| s.to_owned());
        self.highlighter.set_language(language);
    }

    /// 设置文档文本（清空撤销/重做与选区）。
    pub fn set_text(&mut self, text: &str) {
        self.buffer = Buffer::new(text);
        self.undo.clear();
        self.selection = Selection::collapsed(0);
        self.hscroll_cols = 0;
        self.marked_range = None;
        self.last_edit = None;
        self.resync_list();
    }

    /// 在选区处输入文本（等同连续打字：合并撤销、走同一编辑主链）。
    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.insert_edit(self.selection.range(), text, EditIntent::Typing, cx);
    }

    /// 底层 Rope（只读；O(1) COW 快照给后台线程）。
    pub fn text(&self) -> &Rope {
        self.buffer.text()
    }

    /// Rope 别称（语义化：给预览同步取快照）。
    pub fn rope(&self) -> &Rope {
        self.buffer.text()
    }

    /// 整篇文本（O(n) 物化；导出/保存用）。
    pub fn text_string(&self) -> String {
        self.buffer.to_string()
    }

    /// 兼容 `EditorState::value()`：返回全文 String（保存/测试用）。
    pub fn value(&self) -> String {
        self.buffer.to_string()
    }

    /// 编辑版本号：每次编辑 +1（预览同步判断“扫描期间是否有新编辑”）。
    pub fn version(&self) -> u64 {
        self.buffer.version()
    }

    /// 文本行数（LF 计行，尾换行含幻影空行）——编辑器行模型语义。
    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// 状态栏显示用行数（`str::lines()` 语义：`"a\nb\n"`→2，尾换行不追加一行）。
    pub fn display_line_count(&self) -> usize {
        self.buffer.display_line_count()
    }

    /// 列表视口高（px，测试断言用：List 必须撑满容器，否则可见行数为 0）。
    #[cfg(test)]
    pub(crate) fn list_viewport_height(&self) -> f32 {
        self.list_state.viewport_bounds().size.height.as_f32()
    }

    /// 竖向最大滚动量（px，测试断言用：换宽会清空 size_hint，量程不应坍缩）。
    #[cfg(test)]
    pub(crate) fn list_max_offset(&self) -> f32 {
        self.list_state.max_offset_for_scrollbar().y.as_f32()
    }

    /// 字符总数。
    pub fn char_count(&self) -> usize {
        self.buffer.char_count()
    }

    /// 行内字符列 → 字节偏移（越界夹到行尾；搜索跳转/程序化定位用）。
    pub fn offset_for_line_col(&self, line: usize, column_char: usize) -> usize {
        self.buffer.offset_for_char_column(line, column_char)
    }

    /// 选中字节区间（搜索跳转高亮/程序化选区）：滚动到可见并通知。
    pub fn set_selection_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let s = self.buffer.clip(range.start);
        let e = self.buffer.clip(range.end);
        self.selection = Selection::from_range(s, e);
        self.undo.set_cooperative(false);
        self.kick_caret();
        self.reveal_caret();
        cx.notify();
    }

    /// 光标位置 `(行 0 基, 字符列 0 基)`。
    pub fn cursor_position(&self) -> (usize, usize) {
        let h = self.selection.head();
        (self.buffer.line_of_offset(h), self.buffer.char_column(h))
    }

    /// 取走最近一次编辑的字节区间 `(start, old_end, new_end)`（当前文本坐标）。
    /// 供预览增量扫描累积 dirty；取走后清空。
    pub fn take_last_edit(&mut self) -> Option<(usize, usize, usize)> {
        self.last_edit.take()
    }

    /// 软换行开关（暂未实现：首版 display 行 ≡ buffer 行，占位保持 API 一致）。
    pub fn set_soft_wrap(&mut self, _enable: bool) {}

    /// 字号覆盖（px；`None` 走主题默认）。由应用 Ctrl+=/- 调节。
    pub fn set_font_override(&mut self, size_px: Option<f32>) {
        self.font_override = size_px;
        self.metrics_ready = false;
    }

    /// 当前生效字号（px）。
    pub fn font_size_px(&self) -> f32 {
        self.font_size.as_f32()
    }

    pub fn buffer_text(&self) -> &gpui_component::input::Rope {
        self.buffer.text()
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// 光标移动到字节偏移（折叠选区）。
    pub fn set_caret_to(&mut self, offset: usize) {
        self.selection.place(self.buffer.clip(offset));
        self.undo.set_cooperative(false);
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    // -----------------------------------------------------------------
    // 度量与布局
    // -----------------------------------------------------------------

    fn refresh_metrics(&mut self, window: &mut Window, cx: &mut App) {
        let theme = gpui_component::Theme::global(cx);
        let typo = theme.semantic_tokens().typography;
        let colors = theme.semantic_tokens().colors;
        let dark = theme.is_dark();

        let mono = typo.mono.clone();
        let mono_md = typo.mono_md.clone();
        // 字号覆盖：应用 Ctrl+=/- 调节（默认 None = 主题字号）
        let (font_size, line_height) = match self.font_override {
            Some(px_override) => {
                let ratio = if mono_md.size.as_f32() > 0. {
                    mono_md.line_height.as_f32() / mono_md.size.as_f32()
                } else {
                    1.3
                };
                (px(px_override), px(px_override * ratio))
            }
            None => (mono_md.size, mono_md.line_height),
        };

        let text_system = window.text_system();
        let font = gpui::font(mono.clone());
        let char_width = text_system
            .ch_advance(text_system.resolve_font(&font), font_size)
            .unwrap_or(px(font_size.as_f32() * 0.6));

        self.mono = mono;
        self.font_size = font_size;
        self.line_height = line_height;
        self.char_width = char_width;
        self.bg = colors.background;
        self.fg = colors.foreground;
        self.gutter_fg = colors.muted_foreground;
        // 光标不能用 colors.accent：本应用 Light 主题的 accent=l≈0.961（#f5f5f5），
        // 白底上画白光标=看不见（用户反馈“点击无闪烁光标”就是这个）。用前景墨色，
        // 明/暗主题下都一定可见。
        self.caret_color = colors.foreground;
        // 选区底色同样避开浅灰 accent，用固定蓝（与标题/菜单高亮同源）。
        self.sel_bg = rgb(0x0078d4).opacity(if dark { 0.4 } else { 0.28 }).into();
        // 行号列统一高度提示：行号行恒等于 line_height，预填提示可跳过首次整表
        // 测量（大文档 O(n)），滚动/可视渲染也稳定。
        if self.gutter_uniform_applied != Some(self.line_height) {
            self.gutter_list_state = self
                .gutter_list_state
                .clone()
                .with_uniform_item_height(self.line_height);
            self.gutter_uniform_applied = Some(self.line_height);
        }
        self.metrics_ready = true;

        let digits = self.buffer.line_count().max(1).to_string().len();
        self.gutter_chars = digits.max(3);
        self.gutter_width_px = px(
            self.gutter_chars as f32 * char_width.as_f32()
                + GUTTER_LEFT_PAD_PX
                + GUTTER_RIGHT_PAD_PX
                + 4.,
        );
    }

    /// 列表条目数与行数一致（reset 场景：首次/整篇替换/撤销重做）。
    fn resync_list(&mut self) {
        let new_n = self.buffer.line_count().max(1);
        if new_n != self.list_state.item_count() {
            self.list_state.reset(new_n);
            self.gutter_list_state.reset(new_n);
            self.uniform_applied = None;
            // reset 会清空统一高度提示 → 下帧 refresh_metrics 重新预填。
            self.gutter_uniform_applied = None;
        }
        self.row_count = new_n;
    }

    /// 就地编辑并用 `splice` 增量同步行数（O(log n)，避免大文档整树重建）。
    fn apply_edit_synced(&mut self, range: Range<usize>, text: &str) -> crate::editor::buffer::Edit {
        let old_a = self.buffer.line_of_offset(range.start);
        let old_end = self.buffer.clip(range.end.max(range.start));
        let old_b = self.buffer.line_of_offset(old_end);
        let old_span = old_b - old_a + 1;
        let edit = self.buffer.apply_edit(range, text);
        let new_end = self.buffer.clip(edit.inserted_range().end.max(edit.inserted_range().start));
        let new_b = self.buffer.line_of_offset(new_end);
        let new_span = new_b - old_a + 1;
        if new_span != old_span {
            self.list_state
                .splice(old_a..old_a + old_span, new_span);
            self.gutter_list_state
                .splice(old_a..old_a + old_span, new_span);
        }
        self.row_count = self.buffer.line_count().max(1);
        edit
    }

    /// 统一高度的量程提示：仅在行高/条目结构变化后重挂（避免每帧 O(n)）。
    ///
    /// gpui 在「宽度变化」时会把 ListState 所有条目的 `size_hint` 清空（未测
    /// 条目高度按 0 计）→ `max_offset_for_scrollbar()` 坍缩，滚动条量程随之
    /// 失真（表现为“内容长但滚动条极短/拖动失效”）。这里在**每次渲染**用
    /// O(1) 读摘要判断量程是否坍缩，坍缩则重挂提示；健康时清掉重挂痕迹，
    /// 保证换宽后的首次坍缩立即重挂；连续坍缩风暴（拖分屏）120ms 至多重挂一次
    /// （`with_uniform_item_height` 是 O(n) SumTree 物化，不能每帧跑）。
    fn ensure_uniform_hint(&mut self) {
        if !self.metrics_ready {
            return;
        }
        let n = self.list_state.item_count();
        if n == 0 {
            return;
        }
        let lh = self.line_height.as_f32();
        let expected = n as f32 * lh.max(1.);
        let content_h = self.list_state.max_offset_for_scrollbar().y.as_f32()
            + self.list_state.viewport_bounds().size.height.as_f32();
        // 量程坍缩：未测条目把总高压到远低于预期（<97%）。
        let undercount = expected > 0. && content_h < expected * 0.97;

        if !undercount {
            // 量程健康：记录「已健康」，抹掉重挂痕迹。
            self.hint_healthy = true;
            self.last_uniform_apply = None;
            if self.uniform_applied == Some(self.line_height) {
                return;
            }
            self.list_state = self
                .list_state
                .clone()
                .with_uniform_item_height(self.line_height);
            self.uniform_applied = Some(self.line_height);
            return;
        }

        // 坍缩：宽度变化把 size_hint 清了 → 重挂整棵 SumTree（O(n)）。
        // 冷却仅在「已观察过健康帧」之后生效（否则首帧/新文档紧跟的换宽坍缩会被
        // 冷却误伤，量程长时间不自愈）。连续坍缩风暴（拖分屏）120ms 至多重挂一次。
        if self.hint_healthy
            && self
                .last_uniform_apply
                .is_some_and(|t| t.elapsed().as_millis() < 120)
        {
            return;
        }

        self.list_state = self
            .list_state
            .clone()
            .with_uniform_item_height(self.line_height);
        self.uniform_applied = Some(self.line_height);
        self.last_uniform_apply = Some(std::time::Instant::now());
    }

    /// 把文本区 ListState 的竖向滚动位置对齐到行号列（两者锁步）。
    /// 每次渲染调用一次（O(1) 差量读，仅在两者不同时才写一次）。行号列不挂
    /// 滚动条、不做统一高度提示，只需反映与文本相同的可视行。
    fn sync_gutter_scroll(&mut self) {
        let text_top = self.list_state.logical_scroll_top();
        let gutter_top = self.gutter_list_state.logical_scroll_top();
        let drifted = gutter_top.item_ix != text_top.item_ix
            || (gutter_top.offset_in_item.as_f32() - text_top.offset_in_item.as_f32()).abs() > 0.5;
        if drifted {
            self.gutter_list_state.scroll_to(text_top);
        }
    }

    fn visible_chars_from_width(&self, width_px: f32) -> usize {
        let cw = self.char_width.as_f32().max(1.);
        (((width_px - self.gutter_width_px.as_f32()).max(0.) / cw).ceil() as usize)
            .saturating_add(8)
            .max(80)
    }

    // -----------------------------------------------------------------
    // 行数据
    // -----------------------------------------------------------------

    fn row_data(
        &self,
        window: &mut Window,
        row: usize,
        show_caret: bool,
        hl_runs: &[(Range<usize>, Hsla)],
    ) -> RowData {
        if self.row_count == 0 || self.buffer.line_count() == 0 || row >= self.row_count {
            return RowData::empty();
        }

        let line_start = self.buffer.line_start(row);
        let line_end = self.buffer.line_end(row);
        let text = self.buffer.line_text(row);
        if text.is_empty() {
            // 空行：仅在光标停在本行（head==line_start==line_end）时画在 x=0。
            return RowData {
                text,
                highlights: Vec::new(),
                caret: if show_caret
                    && self.selection.head() == line_start
                {
                    Some(0.)
                } else {
                    None
                },
            };
        }

        // 语法着色：`hl_runs` 是已按整窗批量解析的窗口内本行范围，且是
        // **行内字节坐标**（syntect `parse_line` 相对本行文本），直接转样式段。
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for (r, color) in hl_runs {
            let s = r.start.min(text.len());
            let e = r.end.min(text.len());
            if e > s {
                highlights.push((
                    s..e,
                    HighlightStyle {
                        color: Some(*color),
                        ..Default::default()
                    },
                ));
            }
        }

        // 选区底色（本行与选区交叠）
        if !self.selection.is_collapsed() {
            let sel = self.selection.range();
            let lo = sel.start.max(line_start);
            let hi = sel.end.min(line_end);
            if hi > lo {
                highlights.push((
                    (lo - line_start)..(hi - line_start),
                    HighlightStyle {
                        background_color: Some(self.sel_bg),
                        ..Default::default()
                    },
                ));
            }
        }

        // 光标：真实字形宽度（CJK 双宽不再按等宽列近似）→ 光标停在真实字符边界上
        // （横向滚动交给外层容器，行内文本从 x=0 全宽绘制，这里只算文本本地 x）。
        let caret = if show_caret
            && self.selection.head() >= line_start
            && self.selection.head() <= line_end
        {
            let byte_col = self.selection.head() - line_start;
            let x = self
                .row_layout(window, row)
                .map(|layout| layout.x_for_index(byte_col).as_f32())
                .unwrap_or_else(|| byte_col as f32 * self.char_width.as_f32().max(1.));
            Some(x)
        } else {
            None
        };

        RowData {
            text,
            highlights,
            caret,
        }
    }

    /// 行数据：渲染窗口内（`[lo, hi]`）每行。
    ///
    /// 高亮必须整窗一次批量解析再按行进 `runs`：逐行调用 `highlight_window(row,row)`
    /// 会让缓存每行都失效并各自重解析 16 行回看上下文 → 每帧 O(窗口×回看) 重复解析，
    /// 是滚轮滚动"卡顿/不顺滑"的主因之一（syntect 上下文解析不便宜）。
    fn build_rows(
        &mut self,
        window: &mut Window,
        lo: usize,
        hi: usize,
        show_caret: bool,
    ) -> Vec<RowData> {
        if self.row_count == 0 {
            return Vec::new();
        }
        let last = self.row_count.saturating_sub(1);
        let lo = lo.min(last);
        let hi = hi.min(last);
        if hi < lo {
            return Vec::new();
        }
        // 整窗一次批量解析，快照成 owned（`.to_vec()` 结束内部的 `&mut self`
        // 借用，否则后续 `self.row_data` 与 `highlight_window` 的借用冲突）。
        // 返回切片约定：第 0 项 = `lo` 行（`highlight_window` 已统一对齐到请求窗口）。
        let hl_lines = self
            .highlighter
            .highlight_window(&self.buffer, lo, hi, HL_LOOKBACK_LINES)
            .to_vec();
        let mut out: Vec<RowData> = Vec::with_capacity(hi - lo + 1);
        for r in lo..=hi {
            let runs: &[(Range<usize>, Hsla)] = &hl_lines[r - lo];
            out.push(self.row_data(window, r, show_caret, runs));
        }
        out
    }

    // -----------------------------------------------------------------
    // 光标可见性
    // -----------------------------------------------------------------

    fn caret_shown_now(&self) -> bool {
        if !self.focused {
            return false;
        }
        let paused = self
            .caret_pause_until
            .is_some_and(|p| p.elapsed() < Duration::from_millis(BLINK_PAUSE_MS));
        paused || self.caret_visible
    }

    fn reveal_caret(&mut self) {
        let head = self.selection.head();
        let row = self.buffer.line_of_offset(head);
        self.list_state.scroll_to_reveal_item(row);

        // 横向 reveal 用「宽度单位」列（CJK=2 格）与可视格数一致，否则中文长行
        // 少滚一半、光标仍跑出可视区（用户实测：内容超窗后新输入看不见）。
        let col = self.buffer.unit_column(head);
        let margin = HSCROLL_MARGIN_COLS;
        let right_edge = self.visible_chars.saturating_sub(margin).max(1);
        if col < self.hscroll_cols + margin
            || col >= self.hscroll_cols.saturating_add(right_edge)
        {
            let target = if col < margin {
                col
            } else {
                col.saturating_sub(right_edge)
            };
            self.set_hscroll(target);
        }
    }

    /// 仅竖向 reveal（鼠标点击/拖动用）。点击必落在可视区内，无需横向自动滚动；
    /// 若不区分，点行首/空白行会把内容强行拉去最左 →「内容左右晃动」。
    fn reveal_caret_vertical(&mut self) {
        let row = self.buffer.line_of_offset(self.selection.head());
        self.list_state.scroll_to_reveal_item(row);
    }

    fn set_hscroll(&mut self, cols: usize) {
        self.hscroll_cols = cols;
        let x = cols as f32 * self.char_width.as_f32();
        self.h_handle.set_offset(point(px(-x), px(0.)));
    }

    /// 按像素横向滚动（容器滚到底/顶自动夹紧），并同步 `hscroll_cols`。
    /// `view_w` = 文本区可视宽（编辑区宽 − 行号列宽），`content_w_px` 也按文本区计。
    /// 亚格增量（<半格）直接丢弃：部分鼠标滚轮带轻微水平分量/倾斜轮噪声，若每帧
    /// 量化成整格会令内容（行号右缘）左右跳一格、看起来像在“晃”。
    fn scroll_horizontal_px(&mut self, dx_px: f32) {
        let cw = self.char_width.as_f32().max(1.);
        if dx_px.abs() < cw * 0.5 {
            return;
        }
        let view_w = (self.element_bounds.size.width.as_f32() - self.gutter_width_px.as_f32())
            .max(0.);
        let max_px = (self.content_w_px - view_w).max(0.);
        let cur_px = (-self.h_handle.offset().x.as_f32()).clamp(0.0, max_px);
        let target_px = (cur_px + dx_px).clamp(0.0, max_px);
        let cols = (target_px / cw).round() as usize;
        self.set_hscroll(cols);
    }

    /// 滚轮步长换算：`(行高, 字符宽)`。
    fn scroll_factors(&self) -> (f32, f32) {
        (
            self.line_height.as_f32().max(1.),
            self.char_width.as_f32().max(1.),
        )
    }

    // -----------------------------------------------------------------
    // 坐标换算（元素本地坐标）
    // -----------------------------------------------------------------

    /// 当前横向滚动像素（内容被推向左的量；`h_handle` offset 为负）。
    fn hscroll_px(&self) -> f32 {
        (-self.h_handle.offset().x.as_f32()).max(0.)
    }

    /// 整行文本整形为布局（与 `StyledText` 渲染同一个 font/size）。
    ///
    /// 供命中测试 / 光标 x / IME 坐标使用：CJK 全角字按真实字形宽度定位，
    /// 不再用 `col × char_width` 等宽近似（否则中文行点击/光标整体后移）。
    /// `TextSystem` 内置行布局缓存，同一行同帧重复整形是缓存命中，开销≈0。
    fn row_layout(&self, window: &mut Window, row: usize) -> Option<gpui::ShapedLine> {
        if !self.metrics_ready || self.font_size.as_f32() <= 0. {
            return None;
        }
        let text = self.buffer.line_text(row);
        if text.is_empty() {
            return None;
        }
        let run = gpui::TextRun {
            len: text.len(),
            font: gpui::font(self.mono.clone()),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        Some(window.text_system().shape_line(text, self.font_size, &[run], None))
    }

    fn offset_at_point(&self, position: Point<Pixels>, window: &mut Window) -> usize {
        if self.row_count == 0 || self.line_height.as_f32() <= 0. {
            return 0;
        }
        let lh = self.line_height.as_f32();
        let scroll = self.list_state.logical_scroll_top();
        let content_y =
            scroll.item_ix as f32 * lh + scroll.offset_in_item.as_f32() + position.y.as_f32();
        let row = (content_y / lh).floor().max(0.) as usize;
        let row = row.min(self.row_count.saturating_sub(1));

        // 命中的文本本地 x（补偿横向滚动像素）：x_text = local.x − gutter + 已滚像素。
        let line_start = self.buffer.line_start(row);
        let x_text = position.x.as_f32() - self.gutter_width_px.as_f32() + self.hscroll_px();
        if x_text <= 0. {
            return line_start;
        }
        if let Some(layout) = self.row_layout(window, row) {
            // closest_index_for_x 返回**行内字节索引**，且总落在字符边界上
            // （绝不把光标拆进 CJK/emoji 中间）。
            return self.buffer.clip(line_start + layout.closest_index_for_x(px(x_text)));
        }
        // 兜底（度量未就绪等）：等宽近似。
        let col_rel = (x_text / self.char_width.as_f32().max(1.)).floor() as usize;
        let line_chars = self.buffer.line_text(row).chars().count();
        let col = col_rel.min(line_chars);
        self.buffer.offset_for_char_column(row, col)
    }
}

// -------------------------------------------------------------------------
// 交互：焦点 / 鼠标 / 键盘 / 编辑
// -------------------------------------------------------------------------

#[cfg(test)]
impl CodeEditor {
    /// 测试用度量：`(char_width, line_height, gutter_width)`（px 的 f32）。
    pub(crate) fn test_metrics(&self) -> (f32, f32, f32) {
        (
            self.char_width.as_f32(),
            self.line_height.as_f32(),
            self.gutter_width_px.as_f32(),
        )
    }

    /// 测试用：`row` 行内 `byte_col`（行内字节列）字符边界在文本本地的 x（px）。
    /// 与渲染/命中测试共用 `row_layout`，可用来构造"点到真实字符边界"的鼠标事件。
    pub(crate) fn test_char_boundary_x(
        &self,
        window: &mut Window,
        row: usize,
        byte_col: usize,
    ) -> f32 {
        let byte_col = byte_col.min(self.buffer.line_len(row));
        self.row_layout(window, row)
            .map(|layout| layout.x_for_index(byte_col).as_f32())
            .unwrap_or(0.)
    }

    /// 测试用：当前横向滚动像素（断言横向滚动确实把内容推走了）。
    pub(crate) fn test_hscroll_px(&self) -> f32 {
        self.hscroll_px()
    }

    /// 测试用：当前横向内容宽度（px）。断言行宽估算不低估真实字形宽 → 不折行。
    pub(crate) fn test_content_w_px(&self) -> f32 {
        self.content_w_px
    }

    /// 测试用：横向滚动容器最大可滚量（px）——底部横向滚动条量程的依据。
    pub(crate) fn test_hscroll_max_px(&self) -> f32 {
        self.h_handle.max_offset().x.as_f32()
    }

    /// 测试用：当前可视格（宽度单位）数。
    pub(crate) fn test_visible_chars(&self) -> usize {
        self.visible_chars
    }

    /// 测试用：文本区竖向滚动顶 `(item_ix, offset_in_item)`。
    pub(crate) fn test_text_scroll_top(&self) -> (usize, f32) {
        let s = self.list_state.logical_scroll_top();
        (s.item_ix, s.offset_in_item.as_f32())
    }

    /// 测试用：行号列竖向滚动顶（应与文本区锁步一致）。
    pub(crate) fn test_gutter_scroll_top(&self) -> (usize, f32) {
        let s = self.gutter_list_state.logical_scroll_top();
        (s.item_ix, s.offset_in_item.as_f32())
    }
}

impl CodeEditor {
    fn on_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = true;
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
        cx.emit(EditorEvent::Focus);
        if !self.blink_running {
            self.start_blink(cx);
        }
    }

    fn on_blur(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = false;
        self.undo.set_cooperative(false);
        self.marked_range = None;
        cx.emit(EditorEvent::Blur);
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_running {
            return;
        }
        self.blink_running = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| loop {
            let elapsed = this
                .update(cx, |this, _| {
                    this.caret_pause_until
                        .map_or(u128::MAX, |p| p.elapsed().as_millis())
                })
                .unwrap_or(u128::MAX);
            if elapsed >= BLINK_PAUSE_MS as u128 {
                let _ = this.update(cx, |this, cx| {
                    this.caret_visible = !this.caret_visible;
                    cx.notify();
                });
            }
            cx.background_executor()
                .timer(Duration::from_millis(BLINK_MS))
                .await;
            let keep = this
                .update(cx, |this, _| this.focused && epoch == this.blink_epoch)
                .unwrap_or(false);
            if !keep {
                this.update(cx, |this, _| this.blink_running = false).ok();
                break;
            }
        })
        .detach();
    }

    fn kick_caret(&mut self) {
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        self.focus_handle.focus(window, cx);
        self.undo.set_cooperative(false);
        let local = event.position - self.element_bounds.origin;
        let offset = self.offset_at_point(local, window);
        if event.modifiers.shift {
            self.selection.set_head(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        } else {
            self.selection.place(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        }
        self.dragging = true;
        self.kick_caret();
        self.reveal_caret_vertical();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let local = event.position - self.element_bounds.origin;
        let offset = self.offset_at_point(local, window);
        self.selection.set_head(offset);
        self.selection.remember_column(self.buffer.char_column(offset));
        self.kick_caret();
        self.reveal_caret_vertical();
        cx.notify();
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.dragging = false;
    }

    /// 统一按键：仅处理 keymap 未覆盖的 **Shift+方向**（gpui-component 的
    /// Select{Left,Right,Up,Down} 是组件内部 action、公共 keymap 用了它但我们无法
    /// 注册 handler，故在原样 key_down 层处理）。其余键全部走 `on_action`。
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let m = k.modifiers;
        // 无修饰可打印字符 → IME/InputHandler
        if !m.control && !m.alt && !m.platform && k.key_char.is_some() {
            return;
        }
        if m.shift && !m.control && !m.alt && !m.platform {
            match k.key.to_ascii_lowercase().as_str() {
                "left" => {
                    self.arrow(true, -1, 0, cx);
                    return;
                }
                "right" => {
                    self.arrow(true, 1, 0, cx);
                    return;
                }
                "up" => {
                    self.arrow(true, 0, -1, cx);
                    return;
                }
                "down" => {
                    self.arrow(true, 0, 1, cx);
                    return;
                }
                _ => {}
            }
        }
    }

    /// 统一的移动+选区应用点。
    fn apply_move(&mut self, select: bool, new_head: usize, preferred: Option<u32>, cx: &mut Context<Self>) {
        if select {
            let col = self.buffer.char_column(new_head);
            self.selection.set_head(new_head);
            if preferred.is_some() {
                self.selection.remember_column(col);
            }
        } else {
            self.selection.move_cursor(new_head, preferred);
        }
        self.kick_caret();
        self.reveal_caret();
        cx.notify();
    }

    fn arrow(&mut self, select: bool, dc: isize, dr: isize, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        if dr != 0 {
            let (off, col) = movement::move_vertical(
                &self.buffer,
                self.selection.head(),
                dr,
                self.selection.preferred_column,
            );
            self.apply_move(select, off, Some(col), cx);
        } else {
            // 水平：有选区且不按住 Shift → 先收敛到选区起点/终点
            let head = self.selection.head();
            let mut new_head = if dc < 0 {
                movement::char_left(self.buffer.text(), head)
            } else {
                movement::char_right(self.buffer.text(), head)
            };
            if !select && !self.selection.is_collapsed() {
                new_head = if dc < 0 {
                    self.selection.start()
                } else {
                    self.selection.end()
                };
            }
            self.apply_move(select, new_head, None, cx);
        }
    }

    fn move_word_left(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_left(self.buffer.text(), self.selection.head());
        self.apply_move(false, new, None, cx);
    }

    fn move_word_right(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_right(self.buffer.text(), self.selection.head());
        self.apply_move(false, new, None, cx);
    }

    fn select_word_left(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_left(self.buffer.text(), self.selection.head());
        self.apply_move(true, new, None, cx);
    }

    fn select_word_right(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_right(self.buffer.text(), self.selection.head());
        self.apply_move(true, new, None, cx);
    }

    fn move_line_start(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::to_line_start(&self.buffer, self.selection.head());
        self.apply_move(select, new, None, cx);
    }

    fn move_line_end(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::to_line_end(&self.buffer, self.selection.head());
        self.apply_move(select, new, None, cx);
    }

    fn move_doc_start(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        self.apply_move(select, 0, None, cx);
    }

    fn move_doc_end(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        self.apply_move(select, self.buffer.len(), None, cx);
    }

    fn page_row_count(&self) -> usize {
        let vh = self.list_state.viewport_bounds().size.height.as_f32();
        let lh = self.line_height.as_f32().max(1.);
        ((vh / lh).floor() as usize).saturating_sub(1).max(1)
    }

    fn page(&mut self, select: bool, dir: isize, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let rows = self.page_row_count() as isize;
        let (off, col) = movement::move_vertical(
            &self.buffer,
            self.selection.head(),
            dir * rows,
            self.selection.preferred_column,
        );
        self.apply_move(select, off, Some(col), cx);
    }

    // -----------------------------------------------------------------
    // 编辑主链
    // -----------------------------------------------------------------

    fn insert_edit(&mut self, range: Range<usize>, text: &str, intent: EditIntent, cx: &mut Context<Self>) {
        self.undo
            .start_tx(intent, (self.selection.anchor, self.selection.head));
        let edit = self.apply_edit_synced(range, text);
        let head = edit.inserted_range().end;
        let old_end = edit.replaced_range().end;
        self.last_edit = Some((edit.start, old_end, head));
        self.undo.push_edit(edit);
        self.selection.place(head);
        // 打字/退格/删除保留合作（连续合并）；换行/原子编辑断开
        self.undo.set_cooperative(match intent {
            EditIntent::Atomic => false,
            _ => !text.contains(['\n', '\r']),
        });
        self.kick_caret();
        self.reveal_caret();
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head == 0 {
            return;
        }
        let start = movement::char_left(self.buffer.text(), head);
        self.insert_edit(start..head, "", EditIntent::Backspace, cx);
    }

    fn backspace_word(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head == 0 {
            return;
        }
        let start = movement::word_left(self.buffer.text(), head);
        self.insert_edit(start..head, "", EditIntent::Backspace, cx);
    }

    fn delete_forward(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head >= self.buffer.len() {
            return;
        }
        let end = movement::char_right(self.buffer.text(), head);
        self.insert_edit(head..end, "", EditIntent::Delete, cx);
    }

    fn delete_word_forward(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head >= self.buffer.len() {
            return;
        }
        let end = movement::word_right(self.buffer.text(), head);
        self.insert_edit(head..end, "", EditIntent::Delete, cx);
    }

    fn delete_selection_atomic(&mut self, cx: &mut Context<Self>) {
        let range = self.selection.range();
        if range.is_empty() {
            return;
        }
        self.insert_edit(range, "", EditIntent::Atomic, cx);
    }

    fn insert_newline(&mut self, cx: &mut Context<Self>) {
        let at = self.selection.head();
        let row = self.buffer.line_of_offset(at);
        let indent: String = self
            .buffer
            .line_text(row)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let text = format!("\n{indent}");
        let range = if self.selection.is_collapsed() {
            at..at
        } else {
            self.selection.range()
        };
        self.insert_edit(range, &text, EditIntent::Atomic, cx);
    }

    fn tab(&mut self, outdent: bool, cx: &mut Context<Self>) {
        if outdent {
            return self.outdent(cx);
        }
        self.insert_edit(self.selection.range(), "\t", EditIntent::Atomic, cx);
    }

    fn outdent(&mut self, cx: &mut Context<Self>) {
        let at = self.selection.head();
        let row = self.buffer.line_of_offset(at);
        let ls = self.buffer.line_start(row);
        let line = self.buffer.line_text(row);
        let prefix: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if prefix.is_empty() {
            return;
        }
        let remove_bytes = if prefix.starts_with('\t') {
            '\t'.len_utf8()
        } else {
            let spaces = prefix
                .chars()
                .take(self.tab_size)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            spaces.min(ls + line.len() - ls).max(1)
        };
        let end = (ls + remove_bytes).min(self.buffer.line_end(row) - ls + ls);
        let range = ls..end.max(ls).min(self.buffer.line_end(row));
        if range.end <= range.start {
            return;
        }
        self.insert_edit(range, "", EditIntent::Atomic, cx);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        self.selection = Selection::from_range(0, self.buffer.len());
        self.kick_caret();
        cx.notify();
    }

    fn copy(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            let text = self.buffer.text_in_range(self.selection.range());
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    fn cut(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            let text = self.buffer.text_in_range(self.selection.range());
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            self.delete_selection_atomic(cx);
        }
    }

    fn paste(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            if let Some(text) = item.text() {
                self.paste_text(&text, cx);
            }
        }
    }

    /// 在选区处粘贴（归一化 CRLF→LF；程序化/应用侧调用）。
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_edit(self.selection.range(), &normalized, EditIntent::Atomic, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(tx) = self.undo.undo() else {
            return;
        };
        for edit in tx.edits.iter().rev() {
            self.buffer.undo_edit(edit);
        }
        // 撤销整组：dirty 用覆盖所有编辑的最小−最大区间（bounding box，保守）
        if let (Some(a), Some(b)) = (
            tx.edits.iter().map(|e| e.start).min(),
            tx.edits
                .iter()
                .map(|e| e.start + e.old_text.len())
                .max(),
        ) {
            self.last_edit = Some((a, b, a));
        }
        self.selection = Selection::from_range(tx.before.0, tx.before.1);
        self.resync_list();
        self.kick_caret();
        self.reveal_caret();
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let Some(tx) = self.undo.redo() else {
            return;
        };
        for edit in tx.edits.iter() {
            self.buffer.redo_edit(edit);
        }
        if let (Some(a), Some(b)) = (
            tx.edits.iter().map(|e| e.start).min(),
            tx.edits
                .iter()
                .map(|e| e.start + e.new_text.len())
                .max(),
        ) {
            self.last_edit = Some((a, a, b));
        }
        self.selection = Selection::from_range(tx.after.0, tx.after.1);
        self.resync_list();
        self.kick_caret();
        self.reveal_caret();
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }
}

// -------------------------------------------------------------------------
// IME / 文本输入
// -------------------------------------------------------------------------

impl EntityInputHandler for CodeEditor {
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.buffer.len_utf16())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = self.selection;
        Some(UTF16Selection {
            range: self.buffer.byte_to_utf16(sel.start())..self.buffer.byte_to_utf16(sel.end()),
            reversed: sel.reversed(),
        })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| {
            self.buffer.byte_to_utf16(r.start).min(self.buffer.len_utf16())
                ..self.buffer.byte_to_utf16(r.end).min(self.buffer.len_utf16())
        })
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let s = self.buffer.utf16_to_byte(range_utf16.start);
        let e = self.buffer.utf16_to_byte(range_utf16.end);
        Some(self.buffer.text_in_range(s..e).to_string())
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let byte_range = match range {
            Some(r) => self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16())),
            None => self
                .marked_range
                .clone()
                .unwrap_or_else(|| self.selection.range()),
        };
        self.insert_edit(byte_range, text, EditIntent::Typing, cx);
        // 提交/直输会结束一次 IME 组合：Windows 提交候选时只发
        // `replace_text_in_range(None, …)`（不调 unmark_text），不清掉
        // marked_range 的话，下一次组合会把已提交文本当成旧 marked 区域再次替换。
        self.marked_range = None;
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `None` 区间 = 替换当前 marked（正在组合的）文本；只有没有任何 marked 时的
        // 首个字符才落在普通 selection 上。以前用 selection 会让每次组合更新都
        // 「追加」到旧组合后面 → 垃圾残留。
        let byte_range = match range_utf16 {
            Some(r) => self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16())),
            None => self
                .marked_range
                .clone()
                .unwrap_or_else(|| self.selection.range()),
        };
        self.insert_edit(byte_range.clone(), new_text, EditIntent::Atomic, cx);
        if new_text.is_empty() {
            // 组合被取消：清掉 marked，光标停在原地（等价于删掉刚标记的占位）。
            self.marked_range = None;
            self.selection = Selection::collapsed(byte_range.start);
        } else {
            self.marked_range = Some(byte_range.start..byte_range.start + new_text.len());
            // `new_selected_range` 是**相对被标记文本（new_text）起点**的 UTF-16
            // 偏移（AppKit setMarkedText 的 selectedRange 语义），不是文档绝对偏移。
            // 以前当绝对偏移用 → 光标跳到文档开头附近（大文档任意位置输入中文时
            // 若光标落点恰在多字节字符中间，后续 insert 直接撞 `byte index is not
            // on a char boundary` 崩溃）。
            let (s, e) = match new_selected_range {
                Some(sel) => (
                    byte_range.start + ime_utf16_offset_to_byte(new_text, sel.start),
                    byte_range.start + ime_utf16_offset_to_byte(new_text, sel.end),
                ),
                None => (
                    byte_range.start + new_text.len(),
                    byte_range.start + new_text.len(),
                ),
            };
            self.selection = Selection::from_range(s, e);
            self.kick_caret();
        }
        cx.notify();
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let off = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let row = self.buffer.line_of_offset(off);
        let byte_col = off - self.buffer.line_start(row);
        let lh = self.line_height.as_f32().max(1.);
        let scroll = self.list_state.logical_scroll_top();
        let y = (row as f32 - scroll.item_ix as f32) * lh - scroll.offset_in_item.as_f32();
        // 字形真实宽度（IME 组合窗口/预选高亮定位不能再用等宽列近似，否则中文错位）。
        let x_text = self
            .row_layout(window, row)
            .map(|layout| layout.x_for_index(byte_col).as_f32())
            .unwrap_or_else(|| byte_col as f32 * self.char_width.as_f32().max(1.));
        let x = self.gutter_width_px.as_f32() + x_text - self.hscroll_px();
        Some(Bounds::new(
            point(
                element_bounds.origin.x + px(x),
                element_bounds.origin.y + px(y),
            ),
            size(px(CARET_WIDTH_PX), px(lh)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = point - self.element_bounds.origin;
        Some(self.buffer.byte_to_utf16(self.offset_at_point(local, window)))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let s = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let e = self.buffer.utf16_to_byte(range_utf16.end.min(self.buffer.len_utf16()));
        self.selection = Selection::from_range(s, e);
        self.kick_caret();
    }

    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.insert_edit(self.selection.range(), &normalized, EditIntent::Atomic, cx);
        }
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> TextInputConfiguration {
        TextInputConfiguration::default()
    }
}

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
        self.ensure_uniform_hint();
        // 行号列与文本区竖向锁步（覆盖滚轮/拖动滚动条/reveal 等一切滚动来源）。
        self.sync_gutter_scroll();
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

        // 行级数据窗口（可见 + 过扫）
        let lh = self.line_height.as_f32().max(1.);
        let vis_top = self.list_state.logical_scroll_top().item_ix;
        let view_h = self.list_state.viewport_bounds().size.height.as_f32();
        let nrows = ((view_h / lh).ceil() as usize).max(MIN_VISIBLE_ROWS);
        let lo = vis_top.saturating_sub(nrows);
        let last = self.row_count.saturating_sub(1);
        let hi = vis_top.saturating_add(nrows * 2).min(last);
        let show_caret = self.caret_shown_now();
        let rows = self.build_rows(window, lo, hi, show_caret);

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
        // O(1) 读——删除长内容后横向滚动条量程立即收敛，无需重扫。
        let content_w_px = (self.buffer.max_line_units() as f32 * cw_max)
            .max(harea_w)
            .max(1.);
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

        let rv = RowView {
            lo,
            rows,
            line_height: self.line_height,
            mono: self.mono.clone(),
            font_size: self.font_size,
            caret_color: self.caret_color,
        };
        let rv = std::rc::Rc::new(rv);
        let list_state = self.list_state.clone();
        let bg = self.bg;
        let fg = self.fg;

        let content = list(list_state.clone(), move |ix, _window, _cx| {
            let rd = if ix >= rv.lo && ix - rv.lo < rv.rows.len() {
                &rv.rows[ix - rv.lo]
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

        // 行号 gutter：固定编辑区左缘、不随内容横向滚动。**与文本共用同一个
        // ListState 的第二个 `list()`** —— 竖向滚动完全由 List 自身虚拟化+绘制，
        // 不再手工按 `logical_scroll_top` 反推每个行号 y（旧叠层每次 render 重建，
        // 滚轮滚动时与 List 行绘制差半拍 → 序号「翻动/闪烁/晃动」）。两列行高、
        // 滚动位置、可见范围来自同一状态，天然逐像素一致。
        let gv = std::rc::Rc::new(GutterView {
            line_height: self.line_height,
            mono: self.mono.clone(),
            font_size: self.font_size,
            fg: self.gutter_fg,
        });
        // 行号列用独立的 gutter_list_state（与文本锁步），不复用文本的
        // list_state —— 共用同一 StateInner 会让换宽/统一高度互相干扰（量程坍缩）。
        let gutter_content = list(self.gutter_list_state.clone(), move |ix, _window, _cx| {
            number_element(ix, &gv).into_any_element()
        })
        .size_full();
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
            .child(gutter_content);

        let core = div()
            .id("code-editor")
            .debug_selector(|| "code-editor".into())
            .size_full()
            .relative()
            .bg(bg)
            .text_color(fg)
            .key_context("Input")
            .track_focus(&self.focus_handle)
            .on_key_down(window.listener_for(&entity, CodeEditor::on_key_down))
            .on_mouse_down(MouseButton::Left, window.listener_for(&entity, CodeEditor::on_mouse_down))
            .on_mouse_move(window.listener_for(&entity, CodeEditor::on_mouse_move))
            .on_mouse_up(MouseButton::Left, window.listener_for(&entity, CodeEditor::on_mouse_up))
            // —— 输入 action（keymap 由 gpui_component::init 在 "Input" 上下文绑定，
            // 与菜单分发（dispatch_action Undo/Cut/…）共用同一套 handler ——
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveLeft, _w, cx| {
                ed.arrow(false, -1, 0, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveRight, _w, cx| {
                ed.arrow(false, 1, 0, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveUp, _w, cx| {
                ed.arrow(false, 0, -1, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveDown, _w, cx| {
                ed.arrow(false, 0, 1, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveHome, _w, cx| {
                ed.move_line_start(false, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveEnd, _w, cx| {
                ed.move_line_end(false, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &SelectToStartOfLine, _w, cx| {
                ed.move_line_start(true, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &SelectToEndOfLine, _w, cx| {
                ed.move_line_end(true, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveToStart, _w, cx| {
                ed.move_doc_start(false, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveToEnd, _w, cx| {
                ed.move_doc_end(false, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &SelectToStart, _w, cx| {
                ed.move_doc_start(true, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &SelectToEnd, _w, cx| {
                ed.move_doc_end(true, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveToPreviousWord, _w, cx| {
                ed.move_word_left(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MoveToNextWord, _w, cx| {
                ed.move_word_right(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &SelectToPreviousWordStart, _w, cx| {
                ed.select_word_left(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &SelectToNextWordEnd, _w, cx| {
                ed.select_word_right(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MovePageUp, _w, cx| {
                ed.page(false, -1, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &MovePageDown, _w, cx| {
                ed.page(false, 1, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Backspace, _w, cx| {
                ed.backspace(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Delete, _w, cx| {
                ed.delete_forward(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &DeleteToPreviousWordStart, _w, cx| {
                ed.backspace_word(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &DeleteToNextWordEnd, _w, cx| {
                ed.delete_word_forward(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Enter, _w, cx| {
                ed.insert_newline(cx);
                ed.undo.set_cooperative(false);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &IndentInline, _w, cx| {
                ed.tab(false, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &OutdentInline, _w, cx| {
                ed.tab(true, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &SelectAll, _w, cx| {
                ed.select_all(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Undo, _w, cx| {
                ed.undo(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Redo, _w, cx| {
                ed.redo(cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Copy, window, cx| {
                ed.copy(window, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Cut, window, cx| {
                ed.cut(window, cx);
                cx.stop_propagation()
            }))
            .on_action(window.listener_for(&entity, |ed: &mut CodeEditor, _: &Paste, window, cx| {
                ed.paste(window, cx);
                cx.stop_propagation()
            }))
            .child(harea)
            // 行号列（第二个 List，与文本共用 ListState）：盖在横向滚动文本区左缘，
            // 不随内容横移，竖向滚动与文本逐行一致。
            .child(gutter_wrap)
            // 常显滚动条（Always）：编辑器需要可随时拖动的量程正确的滚动条。
            // 用显式 Scrollbar 叠层（同 md 预览的横向条），而不是
            // `ScrollableElement::vertical_scrollbar`（默认 Scrolling 模式，
            // 空闲 2s 自动隐藏，用户误以为滚动条“时有时无/不准确”）。
            .child(
                div()
                    .id("ce-vscrollbar-overlay")
                    .debug_selector(|| "ce-vscrollbar-overlay".into())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::vertical(&list_state)
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
            )
            .child(
                div()
                    .id("ce-hscrollbar-overlay")
                    .debug_selector(|| "ce-hscrollbar-overlay".into())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::horizontal(&self.h_handle.clone())
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
            );

        EditorElement::new(entity, core.into_any_element())
    }
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
}

fn styled_row_text(rd: &RowData, _rv: &RowView) -> StyledText {
    StyledText::new(rd.text.clone()).with_highlights(rd.highlights.clone())
}

/// 行号列渲染上下文（供 gutter 列表闭包只读捕获）。
struct GutterView {
    line_height: Pixels,
    mono: SharedString,
    font_size: Pixels,
    fg: Hsla,
}

/// 行号单元格：右对齐、行高与文本行一致。由共享 ListState 的第二个 `list()`
/// 驱动 → 行号与文本行对齐、随滚动同步移动且不闪烁。
fn number_element(ix: usize, gv: &GutterView) -> gpui::Div {
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
        .child(number)
}

/// IME `new_selected_range` 辅助：把「相对被标记文本（`text`）起点」的 UTF-16
/// 偏移换算成 `text` 内的字节偏移。偏到两半的代理对按字符边界取整（停在该字符
/// 起点），越界夹到文本末尾。
fn ime_utf16_offset_to_byte(text: &str, offset_utf16: usize) -> usize {
    let mut bytes = 0usize;
    let mut units = 0usize;
    for ch in text.chars() {
        if units >= offset_utf16 {
            break;
        }
        let cu = ch.len_utf16();
        if units + cu > offset_utf16 {
            break;
        }
        units += cu;
        bytes += ch.len_utf8();
    }
    bytes
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
                    // gpui 滚轮约定：向下 = delta.y<0，而 ListState::scroll_by 直接
                    // 加到偏移上（正值向下）。取反一下才和滚轮方向一致。
                    ed.list_state.scroll_by(px(-dy));
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
