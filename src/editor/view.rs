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
    cell::Cell,
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    div, list, point, px, rgb, size, AnyElement, App, Bounds, ClipboardItem, Context, DispatchPhase,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, LayoutId, ListAlignment, ListOffset, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, StyledText,
    TextInputConfiguration, UTF16Selection, Window, prelude::FluentBuilder as _,
};
use gpui_component::input::Rope;
use gpui_component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use gpui_component::input::{
    Backspace, Copy, Cut, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, Enter,
    IndentInline, MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight,
    MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveUp, OutdentInline, Paste,
    Redo, SelectAll, SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart,
    SelectToStart, SelectToStartOfLine, Undo,
};

use crate::config::layout::{
    BRACKET_GUIDE_CLOSE_TAIL_LINES, BRACKET_GUIDE_OPEN_HEAD_LINES, EDITOR_RULER_COLUMNS,
};
use crate::config::limits::EDITOR_VSCROLL_BOTTOM_EXTRA_LINES;
use crate::config::limits::FOLD_CANDIDATE_FULL_MAX_LINES;
use crate::editor::fold::{scan_bracket_pairs, BracketPair, Fold, FoldMap};
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
    /// 本行是折叠首行时：折叠指示「…」的 x（相对文本起点，px）。`None` 非折叠首行。
    /// 独立元素渲染（不进 `StyledText`/shape_text），不污染行内测量。
    folded: Option<f32>,
}

impl RowData {
    fn empty() -> Self {
        Self {
            text: SharedString::new_static(""),
            highlights: Vec::new(),
            caret: None,
            folded: None,
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
    /// 折叠指示器/「…」等次要文字色。
    fold_color: Hsla,
}

/// 固定窗口虚拟化：ListState **不再持有全文 N 条目的 O(N) 树**（126MB / 3.2M 行
/// 白占 ~300MB）。改为 ListState 恒持 [`LIST_WINDOW_ROWS`] 条「窗口」，滚动位置由
/// [`CodeEditor::window_base`]（窗口首行绝对行号）+ ListState 相对滚动合成：
/// - 目标超出当前窗口即 `reset(WINDOW)` 重定位（O(窗口)，一刀切重定位、无需分帧）；
/// - 量程/clamp 全部按**全文** `scroll_item_count()×行高` 算（与窗口无关）；
/// - 打开大文件瞬时 O(1)：不再需要「两段式 + 分帧增长」机制。
/// 条目高度仍全部按需实测（不预铺 uniform），纵向量程由像素级确定性滚动条承担。

/// 固定窗口条目数：需 ≥ 最高分辨率的可见行 + 过扫余量（512 × 行高 ≈ 上万 px 视口）。
const LIST_WINDOW_ROWS: usize = crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS;

/// 自研代码编辑器（首版）。
pub struct CodeEditor {
    buffer: Buffer,
    selection: Selection,
    undo: UndoManager,
    focus_handle: FocusHandle,
    /// 文本区 ListState（竖向虚拟化 + 滚动 + 竖滚动条量程）。行号列**不是**独立的
    /// O(N) ListState（大文档会白存 3.2M 个空行条目 ≈ 300MB），而是同一帧用本列表
    /// 的 `logical_scroll_top` 反推出可见行号、以叠层绝对定位渲染（见 render gutter）——
    /// 与 List 行绘制共用同一滚动状态、同一帧，天然逐像素锁步。
    list_state: ListState,
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
    /// 上次 render 时列表元素的布局宽（px）：gpui 按「宽度变化」清空 size_hint。
    /// 用「宽度是否仍在变」替代纯 120ms 墙钟去抖——后者会在打开/换宽后的 120ms 内
    /// 挡住自愈重挂（此时量程坍缩，立刻拖滚动条会拖不到底）。
    last_list_width: f32,
    uniform_applied: Option<Pixels>,
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

    /// ListState 固定窗口的**首行绝对行号**：ListState 的 item_ix 是窗口内的相对下标，
    /// 绝对行 = `window_base + item_ix`（所有滚动/命中/行号/量程都以绝对行为准）。
    /// 目标滚出窗口即重定位本字段（`reset(LIST_WINDOW_ROWS)`，O(窗口)），不改文档。
    window_base: usize,
    /// 恒为 true 的「像素级确定性量程」标志（固定窗口虚拟化下条目不预铺 uniform，
    /// 纵向量程由滚动换算 + `ScrollbarHandle.content_height = 全文行数×行高` 承担，
    /// 不存在换宽/陈旧 size_hint 的量程坍缩）。保留为布尔以让旧分支/测试沿用。
    pixel_scroll: bool,
    /// 用户请求的纵向滚动位置（内容 px，全文绝对坐标），与纵向滚动条句柄共享
    /// （`Rc<Cell<f32>>`，见 [`VerticalScrollHandle`]）。滚动条拖动只写这里，render
    /// 每帧按它重定位窗口；键盘/滚轮/跳转经 [`Self::scroll_to_px`] 同步落窗口。
    desired_scroll: std::rc::Rc<Cell<f32>>,

    // —— 配置 ——
    tab_size: usize,
    /// 代码折叠（视图层投影：视觉行 ↔ 逻辑行映射；不改 Rope）。
    folds: FoldMap,
    /// 自动探测的「可折叠候选」（gutter 默认指示器 + 点击/快捷键折叠的区间来源）。
    /// 只对 ≤ [`FOLD_CANDIDATE_FULL_MAX_LINES`] 行的文档整篇维护（超大文档手动折叠）。
    /// 候选可与 `folds`（已激活折叠）共存/嵌套，仅作「这里能折」的提示，不参与视觉映射。
    fold_candidates: Vec<Fold>,
    /// 跨行括号配对（括号标尺竖线）：`{()}`/`[]`/`()` 每条跨行配对一根竖线，
    /// 从开括号列画到闭括号行。≤ 200k 行整篇扫描，随编辑/行数变化重扫。
    bracket_pairs: Vec<BracketPair>,
    /// 折叠候选 / 括号配对的**后台刷新**版本号：每次请求 +1；后台任务回来时若
    /// 版本已过期（期间又编辑/又请求）则丢弃旧结果、用最新快照重跑一轮。
    scan_epoch: u64,
    /// 是否已有一轮后台刷新在进行（单飞：防止连续输入堆积多个任务）。
    scan_inflight: bool,
    /// 编辑后待刷新：下一帧 render 挂起一次后台刷新（替代原同步整篇重扫）。
    scan_dirty_edit: bool,
    /// 垂直标尺线颜色（主题 `muted_foreground` 派生，refresh_metrics 设置；
    /// 已验证可见——行号列同款）。不依赖 `border`（可能为透明默认值）。
    ruler_color: Hsla,
    /// 括号标尺竖线颜色（更淡的次要前景）。
    guide_color: Hsla,
    /// 点击代码区域点亮的最内层括号块（`open_line`；`None` = 未点亮/已清除）。
    /// 编辑（`apply_edit_synced`）与整篇替换（`reset_document`）会复位为 `None`。
    active_guide_open: Option<usize>,
    /// 点亮竖线颜色：硬编码强调蓝 `rgb(0x0078d4).opacity(0.9)`（`refresh_metrics` 设置；
    /// **勿抽进 config**——用户已明确回滚过颜色抽取）。
    active_guide_color: Hsla,
}

impl CodeEditor {
    /// 创建编辑器（空文档）。
    pub fn new(cx: &mut App) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let buffer = Buffer::default();
        let row_count = buffer.line_count().max(1);
        let list_state = ListState::new(row_count, ListAlignment::Top, px(16.));
        Self {
            buffer,
            selection: Selection::collapsed(0),
            undo: UndoManager::new(),
            focus_handle,
            list_state,
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
            last_list_width: 0.,
            uniform_applied: None,
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
            // 固定窗口虚拟化：窗口首行绝对行号（初始 0）。
            window_base: 0,
            // 全文档统一像素级确定性量程（见 struct 的 pixel_scroll 注释）。
            pixel_scroll: true,
            desired_scroll: std::rc::Rc::new(Cell::new(0.)),
            tab_size: 4,
            folds: FoldMap::new(),
            fold_candidates: Vec::new(),
            bracket_pairs: Vec::new(),
            scan_epoch: 0,
            scan_inflight: false,
            scan_dirty_edit: false,
            ruler_color: rgb(0x000000).into(),
            guide_color: rgb(0x000000).into(),
            active_guide_open: None,
            active_guide_color: rgb(0x000000).into(),
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
        self.reset_document();
    }

    /// 与 [`Self::set_text`] 相同，但**连 Rope 与行宽统计都复用调用方（后台）算好的
    /// 结果**（O(1) 装配，不再全量重扫）。统计须与 `text` 严格对应（见
    /// `Buffer::from_parts`），否则横向滑动条量程/最长行列估算失真。打开大文档的主路径。
    pub fn set_rope_with_stats(
        &mut self,
        text: Rope,
        max_line_chars: usize,
        line_width_histogram: crate::editor::buffer::LineWidthHistogram,
    ) {
        self.buffer = Buffer::from_parts(text, max_line_chars, line_width_histogram);
        self.reset_document();
    }

    /// 大文档「先瞬时显示、后台补齐统计」用：全文已经由 [`Self::set_rope_with_stats`]
    /// 挂上（统计为占位），后台算好真统计后 O(1) 修正横向滑动条量程/最长行。
    /// **要求调用方保证统计与当前文本一致**（打开后未编辑）。
    pub fn apply_stats(
        &mut self,
        max_line_chars: usize,
        line_width_histogram: crate::editor::buffer::LineWidthHistogram,
    ) {
        self.buffer.apply_stats(max_line_chars, line_width_histogram);
    }

    /// 大文档「分块渐进装配」追加一块已归一化文本（整行语义）：只把该块追到 Buffer
    /// 末尾并更新行数，**不重建列表 / 不回滚光标 / 不产生编辑事件**——打开期间的
    /// 滚动位置与撤销栈保留。首块仍走 [`Self::set_rope_with_stats`]（建立文档）。
    ///
    /// **不在此做折叠候选/括号配对扫描**（旧实现每块整篇重扫 → 打开 160k 行文档
    /// UI 线程卡 ~2s）。打开收尾（`file_ops` 末块落定）由 [`Self::refresh_fold_brackets`]
    /// 在后台统一重扫一次。
    pub fn append_mounted_chunk(
        &mut self,
        chunk: &str,
        chunk_max_chars: usize,
        chunk_hist: crate::editor::buffer::LineWidthHistogram,
    ) {
        self.buffer.append_chunk(chunk, chunk_max_chars, chunk_hist);
        self.row_count = self.buffer.line_count().max(1);
        self.folds.set_total(self.row_count);
    }

    /// 整篇替换文档后的通用复位（撤销/选区/横向滚动/标记清空 + 列表行数同步）。
    fn reset_document(&mut self) {
        self.undo.clear();
        self.selection = Selection::collapsed(0);
        self.hscroll_cols = 0;
        self.marked_range = None;
        self.last_edit = None;
        // 整篇替换：点亮标尺清除（内容已变，旧“所属块”提示无意义）。
        self.active_guide_open = None;
        // 滚动目标回零（新文档从头开始；分帧增长中的深跳占位也随之复位）。
        self.desired_scroll.set(0.);
        // 整篇替换：折叠视图投影一并清空（文本结构已变）。
        self.folds.clear();
        self.resync_list(false);
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

    /// 竖向最大滚动量（px，测试断言用；像素路径含底部预留，滚到底时最后一行在
    /// 横向滚动条之上）。
    #[cfg(test)]
    pub(crate) fn list_max_offset(&self) -> f32 {
        if self.pixel_scroll {
            (self.scroll_item_count() as f32 * self.line_height.as_f32().max(1.)
                - self.list_state.viewport_bounds().size.height.as_f32())
                .max(0.)
        } else {
            self.list_state.max_offset_for_scrollbar().y.as_f32()
        }
    }

    /// 纵向量程总高（px）。像素路径 = 条目总数 × 行高（含底部预留空行；确定性），
    /// 诊断/测试用。
    pub(crate) fn vertical_total_px(&self) -> f32 {
        if self.pixel_scroll {
            self.scroll_item_count() as f32 * self.line_height.as_f32().max(1.)
        } else {
            self.list_state.max_offset_for_scrollbar().y.as_f32()
                + self.list_state.viewport_bounds().size.height.as_f32()
        }
    }

    /// 测试：暴露 ListState（共享 Rc）——测试拖拽路径 / 量程对账。
    #[cfg(test)]
    pub(crate) fn test_list_state(&self) -> ListState {
        self.list_state.clone()
    }

    /// 测试/诊断：最近一次 render 记录的列表布局宽（px）。
    #[cfg(test)]
    pub(crate) fn test_last_list_width(&self) -> f32 {
        self.last_list_width
    }

    /// 固定窗口虚拟化：列表始终是就绪的固定窗口（无两段式中间态），恒 false。
    #[cfg(test)]
    pub(crate) fn test_list_is_deferred(&self) -> bool {
        false
    }

    /// 测试：ListState 固定窗口条数（= LIST_WINDOW_ROWS，与文档行数无关）。
    #[cfg(test)]
    pub(crate) fn test_list_window_len(&self) -> usize {
        self.list_state.item_count()
    }

    /// 测试：当前固定窗口首行绝对行号。
    #[cfg(test)]
    pub(crate) fn test_window_base(&self) -> usize {
        self.window_base
    }

    /// 测试：文本区竖向滚动顶 `(绝对行号, offset_in_item)`。
    #[cfg(test)]
    pub(crate) fn test_text_scroll_top(&self) -> (usize, f32) {
        self.abs_scroll_top()
    }

    /// 测试：行号列竖向滚动顶——行号是文本列表的叠层（同一绝对滚动顶），恒等于
    /// [`Self::test_text_scroll_top`]。
    #[cfg(test)]
    pub(crate) fn test_gutter_scroll_top(&self) -> (usize, f32) {
        self.abs_scroll_top()
    }

    /// 测试:用户请求的竖向滚动目标（px，绝对坐标）。
    #[cfg(test)]
    pub(crate) fn test_desired_scroll_px(&self) -> f32 {
        self.desired_scroll.get()
    }

    /// 测试：纵向底部预留（px，已量化到行高整数倍）。
    #[cfg(test)]
    pub(crate) fn test_bottom_pad_px(&self) -> f32 {
        self.vscroll_bottom_pad_px()
    }

    /// 测试：列表条目总数（文档行数 + 底部预留空行）。增长完成后
    /// `test_list_state().item_count() == test_scroll_item_count()`。
    #[cfg(test)]
    pub(crate) fn test_scroll_item_count(&self) -> usize {
        self.scroll_item_count()
    }

    /// 测试：直接滚到「量程最大值（含底部预留空行）」——复现滚到底的真实渲染结果。
    #[cfg(test)]
    pub(crate) fn test_scroll_to_bottom(&mut self) {
        self.scroll_to_px(self.list_max_offset());
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
        // 垂直标尺线：次要前景（与行号同源、已验证明暗主题都可见；不用 `border`——
        // 主题默认可能为透明）。细微参考线不刺眼。
        self.ruler_color = colors.muted_foreground.opacity(0.55);
        // 括号标尺竖线：更淡，纯视觉参考不干扰阅读。
        self.guide_color = colors.muted_foreground.opacity(0.4);
        // 点亮竖线（guide click highlight）：硬编码强调蓝（用户口径，勿抽 config）。
        self.active_guide_color = rgb(0x0078d4).opacity(0.9).into();
        // 光标不能用 colors.accent：本应用 Light 主题的 accent=l≈0.961（#f5f5f5），
        // 白底上画白光标=看不见（用户反馈“点击无闪烁光标”就是这个）。用前景墨色，
        // 明/暗主题下都一定可见。
        self.caret_color = colors.foreground;
        // 选区底色同样避开浅灰 accent，用固定蓝（与标题/菜单高亮同源）。
        self.sel_bg = rgb(0x0078d4).opacity(if dark { 0.4 } else { 0.28 }).into();
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

    /// 重建 ListState 为固定窗口 `[0, LIST_WINDOW_ROWS)` 并回顶。首次 / 整篇替换 /
    /// 撤销重做都走这里：O(窗口) 常数，与文档规模无关——**不再是两段式 + 分帧增长**
    /// （改固定窗口虚拟化后打开大文件瞬时 O(1)，无需增长机制）。
    ///
    /// `defer_scans`：`true` = 撤销/重做等「文档行数已变、避免 UI 线程整篇重扫」——
    /// 折叠候选/括号配对改为**下一帧挂起后台刷新**（[`Self::scan_dirty_edit`]）；
    /// `false` = 整篇替换/打开（内容全新，同步重扫一次，首屏即就绪，测试依赖）。
    fn resync_list(&mut self, defer_scans: bool) {
        let new_n = self.buffer.line_count().max(1);
        self.row_count = new_n;
        let t = std::time::Instant::now();
        self.list_state.reset(LIST_WINDOW_ROWS);
        self.uniform_applied = None;
        self.pixel_scroll = true;
        self.window_base = 0;
        self.desired_scroll.set(0.);
        // 折叠投影总行数跟随文档行数（区间越界由 FoldMap 侧裁剪）。
        self.folds.set_total(new_n);
        if defer_scans {
            // 撤销/重做路径：不在这里整篇重扫（大文档 ~280ms UI 卡顿），交给后台刷新。
            self.scan_dirty_edit = true;
        } else {
            // 打开/整篇替换路径：同步重扫一次（内容全新，首屏即就绪）。
            self.recompute_fold_candidates();
            self.recompute_bracket_pairs();
        }
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] 编辑器 ListState 固定窗口 reset({LIST_WINDOW_ROWS})，{new_n} 行 + {} 预留空行: {} ms",
                self.pad_row_count(),
                t.elapsed().as_millis()
            );
        }
    }

    /// 就地编辑并同步行数（O(log n) 缓冲编辑）。固定窗口虚拟化下列表条目数与文档
    /// 无关，无需 splice——只更新 `row_count`；后续 `reveal_caret` 会按新光标行
    /// 重新定位窗口。
    fn apply_edit_synced(&mut self, range: Range<usize>, text: &str) -> crate::editor::buffer::Edit {
        // 编辑会改变块结构/跨度：清除熄灭点亮标尺（语义见 guide click highlight）。
        self.active_guide_open = None;
        // 先取被替换区间的**编辑前**行跨度（折叠区间同步需要，编辑后行号已变）。
        let prev_rows = self.row_count;
        let line_a = self.buffer.line_of_offset(range.start);
        let line_b = self.buffer.line_of_offset(range.end);
        let edit = self.buffer.apply_edit(range, text);
        self.row_count = self.buffer.line_count().max(1);
        // 折叠同步：编辑进折叠体 → 自动展开；折叠后整体平移；锚被删 → 丢弃。
        self.folds.sync_edit(line_a, line_b, text, self.row_count);
        // 折叠候选 / 括号配对：**不再同步整篇重扫**（大文档逐键卡顿）。行数变了或
        // 文本含括号字符 → 打标，由下一帧 render 挂起一轮后台刷新（单飞 + 版本号）。
        let has_bracket =
            text.bytes().any(|b| matches!(b, b'{' | b'}' | b'(' | b')' | b'[' | b']'))
                || edit
                    .old_text
                    .bytes()
                    .any(|b| matches!(b, b'{' | b'}' | b'(' | b')' | b'[' | b']'));
        self.scan_dirty_edit = self.scan_dirty_edit || self.row_count != prev_rows || has_bracket;
        edit
    }

    /// 统一高度的量程提示：仅在行高/条目结构变化后重挂（避免每帧 O(n)）。返回
    /// `true` 表示「本轮应用了统一高度提示」——调用方应 `cx.notify()` 请求多画一帧：
    /// gpui 在**本次布局**（prepaint 按 `last_layout_bounds` 宽度比对）会再次清掉刚填的
    /// size_hint，只有下一帧宽度对齐后才不会被清，提示才真正生效（与 Markdown 预览
    /// `update_height_hint` 的「换宽自愈」同一手法）。量程健康/冷却中返回 `false`。
    ///
    /// gpui 在「宽度变化」时会把 ListState 所有条目的 `size_hint` 清空（未测
    /// 条目高度按 0 计）→ `max_offset_for_scrollbar()` 坍缩，滚动条量程随之
    /// 失真（表现为“内容长但滚动条极短/拖动失效”）。这里在**每次渲染**用
    /// O(1) 读摘要判断量程是否坍缩，坍缩则重挂提示；健康时清掉重挂痕迹，
    /// 保证换宽后的首次坍缩立即重挂；连续坍缩风暴（拖分屏）120ms 至多重挂一次
    /// （`with_uniform_item_height` 是 O(n) SumTree 物化，不能每帧跑）。
    fn ensure_uniform_hint(&mut self) -> bool {
        if !self.metrics_ready {
            return false;
        }
        // 像素两段式（pixel_scroll）：条目不预铺 uniform，量程由像素滚动 + 确定性
        // ScrollbarHandle 承担，不存在 summary 坍缩问题——不要触发任何 O(N) 重铺。
        if self.pixel_scroll {
            return false;
        }
        let n = self.list_state.item_count();
        if n == 0 {
            return false;
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
                return false;
            }
            self.apply_uniform_hint();
            return true;
        }

        // 坍缩：宽度变化把 size_hint 清了 → 重挂整棵 SumTree（O(n)）。
        // 去抖只针对「宽度仍在连续变化」（拖分屏，刚重挂就被同帧 prepaint 清掉，
        // 每帧 O(n) 会卡）：用列表自身布局宽 `last_list_width`（即 gpui 判断是否清空的
        // 那个宽度）是否在变来判断。**宽度安定后坍缩立即重挂**——不再依赖 120ms 墙钟
        // 冷却，避免打开/换宽后的 ~120ms 内量程坍缩、立刻拖滚动条拖不到底。
        let list_w = self.list_state.viewport_bounds().size.width.as_f32();
        let width_active = (self.last_list_width - list_w).abs() >= 2.0;
        self.last_list_width = list_w;
        if self.hint_healthy
            && width_active
            && self
                .last_uniform_apply
                .is_some_and(|t| t.elapsed().as_millis() < 120)
        {
            return false;
        }

        self.apply_uniform_hint();
        true
    }

    /// 重挂统一行高提示。**必须用 `reset_with_uniform_height` 强制重铺**：它先
    /// `reset`（splice 成全 None）再统一填高，覆盖任何陈旧 size_hint——gpui 布局会把
    /// 窗口外条目戳上陈旧的小 `Some` 戳值，`with_uniform_item_height` 的
    /// `unwrap_or` 会保留它们 → 量程永远恢复不上（实测大文档摘要总高被卡死在 ~视口）。
    /// 滚动位置先保存再恢复（reset 会清零滚动顶）。
    fn apply_uniform_hint(&mut self) {
        let t = std::time::Instant::now();
        let prev = self.list_state.logical_scroll_top();
        let n = self.list_state.item_count();
        self.list_state
            .reset_with_uniform_height(n, self.line_height);
        self.list_state.scroll_to(prev);
        self.uniform_applied = Some(self.line_height);
        self.last_uniform_apply = Some(std::time::Instant::now());
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            let vb = self.list_state.viewport_bounds();
            eprintln!(
                "[MD_OPEN_DEBUG] 编辑器 reset_with_uniform_height({n} 行): {} ms 摘要总高={:.1} viewport_w={:.1} content_w={:.1}",
                t.elapsed().as_millis(),
                self.list_state.max_offset_for_scrollbar().y.as_f32()
                    + self.list_state.viewport_bounds().size.height.as_f32(),
                vb.size.width.as_f32(),
                self.content_w_px,
            );
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
        // 折叠首行：记「…」指示器 x（文本尾随位置，独立元素渲染，不进 StyledText）。
        let folded = if self.folds.fold_starting_at(row).is_some() {
            Some(
                self.row_layout(window, row)
                    .map(|layout| layout.width().as_f32())
                    .unwrap_or_else(|| {
                        text.chars().count() as f32 * self.char_width.as_f32().max(1.)
                    }),
            )
        } else {
            None
        };
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
                folded,
            };
        }

        // 语法着色：`hl_runs` 是已按整窗批量解析的窗口内本行范围，且是
        // **行内字节坐标**（syntect `parse_line` 相对本行文本），直接转样式段。
        // 注意：选区底色不能“附在语法段后面一并交出去”。gpui `StyledText`
        // 的 `compute_runs` 假定各 highlight **互不重叠**；选区必然横跨多个语法段，
        // 重叠会使 run 总量超出整行字节长（实测 42 > 18），`shape_text` 按行尾
        // 截断 → 尾部选区整段被丢（选中不出高亮）。这解释了同一份内容
        // `.txt/.md`（无语法段）可选、`.rs`（Rust 每行多段）“不可选择”。
        // 这里把语法色与选区色**合并**成非重叠的连续段：每个单元被语法覆盖就上
        // color、被选区覆盖就上 background_color。
        let sel_range = if !self.selection.is_collapsed() {
            let sel = self.selection.range();
            let lo = sel.start.max(line_start);
            let hi = sel.end.min(line_end);
            if hi > lo {
                Some((lo - line_start)..(hi - line_start))
            } else {
                None
            }
        } else {
            None
        };
        let highlights = merge_highlights(hl_runs, sel_range, self.sel_bg);

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
            folded,
        }
    }

    /// 行数据：渲染窗口内（`[lo, hi]`，**视觉行**）每行。
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
        // 窗口与滚动同在**视觉行**空间：折叠后行窗口以视觉行为界。
        let vtotal = self.folds.visual_line_count().max(1);
        let last_v = vtotal - 1;
        let lo = lo.min(last_v);
        let hi = hi.min(last_v);
        if hi < lo {
            return Vec::new();
        }
        // 把窗口两端映射成逻辑行，供高亮整窗批量解析与行文本提取。
        let lo_l = self.folds.logical_row(lo);
        let hi_l = self.folds.logical_row(hi);
        // 整窗一次批量解析，快照成 owned（`.to_vec()` 结束内部的 `&mut self`
        // 借用，否则后续 `self.row_data` 与 `highlight_window` 的借用冲突）。
        // 返回切片约定：第 0 项 = `lo_l` 行（`highlight_window` 已统一对齐到请求窗口）。
        let hl_lines = self
            .highlighter
            .highlight_window(&self.buffer, lo_l, hi_l, HL_LOOKBACK_LINES)
            .to_vec();
        // 输出按**视觉行**对齐（第 0 项 = 视觉 `lo` 行）：每个视觉行映射一个逻辑行。
        let mut out: Vec<RowData> = Vec::with_capacity(hi - lo + 1);
        for v in lo..=hi {
            let logical = self.folds.logical_row(v);
            let runs: &[(Range<usize>, Hsla)] = &hl_lines[logical - lo_l];
            out.push(self.row_data(window, logical, show_caret, runs));
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

    // -----------------------------------------------------------------
    // 纵向滚动：像素级确定性换算（大文档 pixel_scroll 路径）
    // -----------------------------------------------------------------
    // 像素模式下条目高度**不预铺 uniform**（None 提示按需实测），ListState 的
    // summary 高度全是 0，所以一切滚动都不能再走 summary 换算（scroll_by /
    // scroll_to_reveal_item / max_offset 相关），统一由这里以「行数×行高」的
    // 确定性像素模型换算成 ListOffset，再交给 `list_state.scroll_to`（只看 item_count，
    // 不看高度）。量程则全由 VerticalScrollHandle 的 content_height 承担。

    /// 纵向最大滚动量的**底部预留**（px）= 横向滚动条厚度 + 额外行数×行高，
    /// **向上量化到 line_height 的整数倍**：叠加在底部的横向滚动条厚度（16px）
    /// 通常不是行高的整数倍，若不量化，滚到底时视口停在「行间半格」上，最后一行
    /// 加载/点击命中/光标位置出现次像素误差。量化后一切纵向偏移都是整行像素，
    /// 宁可多留白半行也绝不出现 0.5px 的渲染模糊。
    fn vscroll_bottom_pad_px(&self) -> f32 {
        let lh = self.line_height.as_f32().max(1.);
        let raw = gpui_component::scroll::Scrollbar::width().as_f32()
            + EDITOR_VSCROLL_BOTTOM_EXTRA_LINES as f32 * lh;
        (raw / lh).ceil() * lh
    }

    /// 底部预留换算成的行数（`vscroll_bottom_pad_px` 已量化到行高整数倍，故整除）。
    ///
    /// 固定窗口虚拟化下预留区**不是**列表里的真实条目，而是「滚动上限包含它」：
    /// 滚到底时视口顶行 = `scroll_item_count − 可见行数`，该行落在文档正文内，窗口
    /// 从它往下渲染自然盖住预留区（超出 `row_count` 的窗口条目渲染为空行 = 预留空白，
    /// 见 render 的 RowView 映射）。gpui「自动填满视口」只会把窗口内的**空行**拉到
    /// 视口底，预留区仍是可见的空白。
    fn pad_row_count(&self) -> usize {
        let lh = self.line_height.as_f32().max(1.);
        (self.vscroll_bottom_pad_px() / lh).round() as usize
    }

    /// 滚动涉及的**全文条目数** = 视觉行数（逻辑行 − 折叠隐藏行）+ 底部预留空行数。
    /// 纵向滚动上限 / 量程 / clamp 全部以它为准（与固定窗口无关——窗口只是
    /// 「从哪里开始渲染」）。折叠会缩小它（与窗口大小无关，thumb 无坍缩）。
    fn scroll_item_count(&self) -> usize {
        self.folds
            .visual_line_count()
            .max(1)
            .saturating_add(self.pad_row_count())
    }

    /// 文本区 **绝对**滚动位置 `(绝对行号, 已滚进该行的 px)`。窗口内的相对
    /// `item_ix` 由 [`Self::window_base`] 抬成绝对行；行号/命中/渲染全文都以它为准。
    fn abs_scroll_top(&self) -> (usize, f32) {
        let s = self.list_state.logical_scroll_top();
        (self.window_base + s.item_ix, s.offset_in_item.as_f32())
    }

    /// ListState **实际**滚动位置（px，绝对坐标；读窗口合成，不读 `desired_scroll`）。
    /// 用于重建后把「用户目标」重新对齐 ListState（见 `resync_list`），以及常规读数。
    fn list_scroll_top_px(&self) -> f32 {
        let (abs_ix, off) = self.abs_scroll_top();
        let lh = self.line_height.as_f32().max(1.);
        abs_ix as f32 * lh + off
    }

    /// 当前竖向量程（px）= 全文条目数 × 行高 − 视口，夹 ≥0（绝对值，与窗口无关）。
    /// 滚动 clamp / 滚动条 thumb / 量程诊断共用这一份。
    fn reach_px(&self) -> f32 {
        let lh = self.line_height.as_f32().max(1.);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        (self.scroll_item_count() as f32 * lh - view).max(0.)
    }

    /// 当前竖向滚动量（px，绝对坐标；0 = 顶部）。
    fn scroll_top_px(&self) -> f32 {
        self.list_scroll_top_px()
    }

    /// 像素 → ListOffset（统一行高模型，窗口内相对坐标由调用方套 `window_base`）。
    fn px_to_offset(px: f32, lh: f32) -> ListOffset {
        let item_ix = (px / lh).floor().max(0.) as usize;
        let offset = px - item_ix as f32 * lh;
        ListOffset { item_ix, offset_in_item: gpui::px(offset.max(0.)) }
    }

    /// 把固定窗口对齐到用户目标（`desired_scroll`，绝对 px）：目标窗口覆盖不足一屏则
    /// 重定位 `window_base = abs_top` 并 `reset(LIST_WINDOW_ROWS)`（O(窗口)，任意跳转
    /// 瞬时可达，无分帧、无占位符）；窗口内则直接 `scroll_to` 相对位置。滚动条拖动只写
    /// `desired_scroll`，render 每帧调用本方法落窗口；滚轮/键盘/跳转也经它。
    fn apply_desired_to_window(&mut self) {
        let lh = self.line_height.as_f32().max(1.);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        let vis_rows = ((view / lh).ceil() as usize).max(1);
        let target = self.desired_scroll.get().clamp(0., self.reach_px());
        let abs_top = (target / lh).floor().max(0.) as usize;
        let frac = target - abs_top as f32 * lh;
        let rel = abs_top.saturating_sub(self.window_base);
        let covers = abs_top >= self.window_base && rel + vis_rows <= LIST_WINDOW_ROWS;
        if !covers {
            self.window_base = abs_top;
            self.list_state.reset(LIST_WINDOW_ROWS);
        }
        let rel = abs_top.saturating_sub(self.window_base);
        self.list_state.scroll_to(ListOffset { item_ix: rel, offset_in_item: px(frac) });
        self.desired_scroll.set(target);
    }

    /// 滚到目标像素（夹到 [0, 全文高−视口]），写入 `desired_scroll` 并落窗口。
    /// 底部预留让最后一行能滚到横向滚动条之上（见 `pad_row_count`）。
    fn scroll_to_px(&mut self, target_px: f32) {
        let px = target_px.clamp(0., self.reach_px());
        self.desired_scroll.set(px);
        self.apply_desired_to_window();
    }

    /// 滚轮竖向滚动（dy = 滚轮 delta，负=向下）。
    fn scroll_vertical(&mut self, dy: f32) {
        self.scroll_to_px(self.scroll_top_px() - dy);
    }

    /// 把目标**逻辑行**滚进可视区（统一行高模型；先经折叠映射成视觉行，再算像素：
    /// 折叠隐藏行的逻辑行会塌缩到折叠首行的视觉位置）。可视高度扣除底部预留，
    /// 保证被 reveal 的行不会贴到横向滚动条之下。
    fn reveal_row(&mut self, row: usize) {
        let lh = self.line_height.as_f32().max(1.);
        let vis = self.folds.visual_row(row);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        let rows_visible = ((view - self.vscroll_bottom_pad_px()) / lh).floor().max(1.) as usize;
        let cur_top = (self.scroll_top_px() / lh).floor() as usize;
        if vis < cur_top {
            self.scroll_to_px(vis as f32 * lh);
        } else if vis >= cur_top + rows_visible {
            let target = vis.saturating_add(1).saturating_sub(rows_visible);
            self.scroll_to_px(target as f32 * lh);
        }
    }

    fn reveal_caret(&mut self) {
        let head = self.selection.head();
        let row = self.buffer.line_of_offset(head);
        self.reveal_row(row);

        // 横向 reveal 用「宽度单位」列（CJK=2 格）与可视格数一致，否则中文长行
        // 少滚一半、光标仍跑出可视区（用户实测：内容超窗后新输入看不见）。
        // 注意 `visible_chars` 带有 +8 的估算余量（visible_chars_from_width），
        // 若直接当可视格数用，`right_edge` 会偏大 8 格 → 光标停在真实右缘之外 4 格
        // （用户实测：长行末尾输入后最新内容看不到）。先减掉余量得真实可视格。
        let col = self.buffer.unit_column(head);
        let margin = HSCROLL_MARGIN_COLS;
        let view_cells = self.visible_chars.saturating_sub(8).max(1);
        let right_edge = view_cells.saturating_sub(margin).max(1);
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
        self.reveal_row(row);
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

    /// 垂直标尺第 `col`（宽度单位列）列在**editor 本地 x**（px）。标尺随内容横向
    /// 滚动（方案 A）；移出文本区右缘或左滚出文本区（< 行号列右缘）→ `None`（被
    /// 裁掉/不可见）。`avail_w` = 编辑区可视宽（文本区右界）。
    fn ruler_screen_x(&self, col: usize, avail_w: f32) -> Option<f32> {
        if !self.metrics_ready || self.char_width.as_f32() <= 0. {
            return None;
        }
        let gutter = self.gutter_width_px.as_f32();
        let view_w = (avail_w - gutter).max(0.);
        if view_w <= 0. {
            return None;
        }
        let x = gutter + col as f32 * self.char_width.as_f32() - self.hscroll_px();
        if x >= gutter && x < gutter + view_w {
            Some(x.round().max(gutter))
        } else {
            None
        }
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
        // 用「有效滚动顶」：深跳占位期 = 用户目标（点击占位符命中其代表的目标行），
        // 常规期 = ListState 实际位置（一致）。
        let content_y = self.scroll_top_px() + position.y.as_f32();
        // 命中换算到**视觉行**，再经折叠映射成逻辑行（视觉行可被折叠压缩）。
        let vis = (content_y / lh).floor().max(0.) as usize;
        let vis = vis.min(self.folds.visual_line_count().saturating_sub(1));
        let row = self.folds.logical_row(vis);
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

    /// 测试用：折叠映射（只读）。
    pub(crate) fn test_folds(&self) -> &FoldMap {
        &self.folds
    }

    /// 测试用：程序化添加折叠 `[start, end)`（半开逻辑行）。
    pub(crate) fn test_set_fold(&mut self, start: usize, end: usize) {
        self.folds.toggle(start, end);
    }

    /// 测试用：自动探测的折叠候选（只读）。
    pub(crate) fn test_fold_candidates(&self) -> &[Fold] {
        &self.fold_candidates
    }

    /// 测试用：括号标尺配对（只读）。
    pub(crate) fn test_bracket_pairs(&self) -> &[BracketPair] {
        &self.bracket_pairs
    }

    /// 测试用：清空折叠候选与括号配对（验证后台刷新会重新落地）。
    pub(crate) fn test_clear_fold_brackets(&mut self) {
        self.fold_candidates.clear();
        self.bracket_pairs.clear();
    }

    /// 测试用：括号标尺在当前视口的竖线段 `(x, y_rel, h)`（文本区本地坐标）。
    pub(crate) fn test_bracket_guide_segments(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
    ) -> Vec<(f32, f32, f32)> {
        self.bracket_guide_segments(top_vis, off, lh, harea_w, view_h)
    }

    /// 测试用：当前点亮的标尺块（`open_line`，`None` = 未点亮）。
    pub(crate) fn test_active_guide(&self) -> Option<usize> {
        self.active_guide_open
    }

    /// 测试用：逻辑行 `logical` 应命中的最内层括号块（`open_line`）。
    pub(crate) fn test_guide_for_logical_row(&self, logical: usize) -> Option<usize> {
        self.guide_pair_for_logical_row(logical)
    }

    /// 测试用：`line` 是否起始一个可折叠候选/激活折叠（gutter 指示器口径）。
    pub(crate) fn test_has_fold_or_candidate(&self, line: usize) -> bool {
        self.has_fold_or_candidate(line)
    }

    /// 测试用：光标行折叠切换（gutter 点击同路径）。
    pub(crate) fn test_toggle_fold_at(&mut self, line: usize) {
        self.toggle_fold_at(line);
    }

    /// 测试用：缩进启发式块的结束行（开区间）。
    pub(crate) fn test_fold_block_end(&self, start: usize) -> Option<usize> {
        self.fold_block_end(start)
    }

    /// 测试用：垂直标尺第 `col` 列在当前横向滚动下的本地 x（px；不可见为 `None`）。
    pub(crate) fn test_ruler_x(&self, col: usize, avail_w: f32) -> Option<f32> {
        self.ruler_screen_x(col, avail_w)
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

    /// 测试用：光标所在宽度单位列（与横向 reveal 相同口径）。
    pub(crate) fn test_caret_units(&self) -> usize {
        self.buffer.unit_column(self.selection.head().min(self.buffer.len()))
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
        // gutter 内点击：命中**折叠首行或可折叠候选** → 切换折叠（不移动光标）；
        // 否则回落普通点击（x 在行号列 → 列 0，与既有行为一致）。
        if local.x >= px(0.) && local.x < self.gutter_width_px {
            let lh = self.line_height.as_f32().max(1.);
            let content_y = self.scroll_top_px() + local.y.as_f32();
            let vis = (content_y / lh).floor().max(0.) as usize;
            let vis = vis.min(self.folds.visual_line_count().saturating_sub(1));
            let logical = self.folds.logical_row(vis);
            if self.has_fold_or_candidate(logical) {
                self.toggle_fold_at(logical);
                self.dragging = false;
                return;
            }
        }
        let offset = self.offset_at_point(local, window);
        // 标尺点亮：代码区点击 → 该行所属最内层括号块的标尺竖线点亮；非块内行 → 清除。
        // （此处已越过 gutter 分支，标尺本身不可点选，只响应代码区。）
        let caret_row = self.buffer.line_of_offset(offset);
        self.active_guide_open = self.guide_pair_for_logical_row(caret_row);
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
        // 代码折叠：Ctrl+Shift+[ 折叠光标所在块；Ctrl+Shift+] 展开光标处折叠。
        if m.control && m.shift && !m.alt && !m.platform {
            match k.key.to_ascii_lowercase().as_str() {
                "[" => {
                    self.fold_under_caret(cx);
                    return;
                }
                "]" => {
                    self.unfold_under_caret(cx);
                    return;
                }
                _ => {}
            }
        }
    }

    /// 统一的移动+选区应用点：新光标若落进折叠隐藏体 → 跳到该折叠之后的首个可见行
    /// （VS Code 式「跳过折叠体」，不自动展开）。
    fn apply_move(&mut self, select: bool, new_head: usize, preferred: Option<u32>, cx: &mut Context<Self>) {
        let new_head = self.snap_out_of_hidden(new_head);
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

    /// 若 `off` 处于某折叠的隐藏体内部 → 返回该折叠**结束行**（首个可见行）的行首；
    /// 否则原样返回。
    fn snap_out_of_hidden(&self, off: usize) -> usize {
        let row = self.buffer.line_of_offset(off);
        match self.folds.hidden_containing(row) {
            Some(f) => self.buffer.line_start(f.end.min(self.row_count)),
            None => off,
        }
    }

    /// 折叠操作后保持「视口顶行逻辑行」稳定：把**编辑前**捕获的顶行逻辑行重新映射
    /// 到新视觉坐标并滚动到位——折叠/展开上方的折叠会改变视觉行数与像素位置，否则
    /// 视口会瞬间跳走。`top_logical` = 折叠前 `abs_scroll_top().0` 经映射得到的逻辑行。
    fn anchor_top_logical_after_fold(&mut self, top_logical: usize, off_px: f32) {
        let vis2 = self.folds.visual_row(top_logical);
        self.scroll_to_px(vis2 as f32 * self.line_height.as_f32().max(1.) + off_px);
    }

    /// 视口顶行的逻辑行（折叠操作前捕获用）。
    fn capture_top_logical(&self) -> (usize, f32) {
        let (top_vis, off) = self.abs_scroll_top();
        (self.folds.logical_row(top_vis), off)
    }

    /// gutter 点击切换：该行已有激活折叠 → 展开；否则有候选 → 按候选区间折叠；
    /// 否则尝试按缩进启发式创建。
    fn toggle_fold_at(&mut self, line: usize) {
        let line = line.min(self.row_count.saturating_sub(1));
        let top = self.capture_top_logical();
        let did = if let Some(f) = self.folds.fold_starting_at(line).copied() {
            self.folds.expand_at(f.start);
            true
        } else if let Some(c) = self.candidate_starting_at(line) {
            self.folds.toggle(c.start, c.end);
            true
        } else if let Some(end) = self.fold_block_end(line) {
            self.folds.toggle(line, end);
            true
        } else {
            false
        };
        if did {
            self.anchor_top_logical_after_fold(top.0, top.1);
        }
    }

    /// Ctrl+Shift+[：创建光标所在块（优先用折叠候选；否则缩进启发式）的折叠；
    /// 已折叠则保持。
    fn fold_under_caret(&mut self, cx: &mut Context<Self>) {
        let line = self.buffer.line_of_offset(self.selection.head());
        if self.folds.fold_starting_at(line).is_some() {
            return;
        }
        let fold_range = self
            .candidate_starting_at(line)
            .map(|c| (c.start, c.end))
            .or_else(|| self.fold_block_end(line).map(|end| (line, end)));
        if let Some((start, end)) = fold_range {
            let top = self.capture_top_logical();
            self.folds.toggle(start, end);
            self.anchor_top_logical_after_fold(top.0, top.1);
            cx.notify();
        }
    }

    /// Ctrl+Shift+]：展开光标所在/包含的折叠。
    fn unfold_under_caret(&mut self, cx: &mut Context<Self>) {
        let line = self.buffer.line_of_offset(self.selection.head());
        if self.folds.fold_at(line).is_some() {
            let top = self.capture_top_logical();
            self.folds.expand_at(line);
            self.anchor_top_logical_after_fold(top.0, top.1);
            cx.notify();
        }
    }

    /// 缩进启发式折叠块：从 `start` 行起向后找到块结束行（**开区间** end）。
    /// - 首个缩进 `< start 行缩进` 的行 → 块结束（dedent）；
    /// - 同缩进且以闭/尾记号（`}` `)` `]` `#` `` ` ``）开头 → 块结束；
    /// - 空行跳过；
    /// - 到文件尾 → 结束。
    /// 至少需隐藏 1 行（`end ≥ start+2`）才视为可折叠。
    fn fold_block_end(&self, start: usize) -> Option<usize> {
        if self.row_count == 0 || start >= self.row_count {
            return None;
        }
        let base_indent = leading_ws_len(self.buffer.line_text(start).as_ref());
        let mut end = None;
        for r in (start + 1)..self.row_count {
            let line = self.buffer.line_text(r);
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            let indent = leading_ws_len(line.as_ref());
            if indent < base_indent {
                end = Some(r);
                break;
            }
            if indent == base_indent
                && matches!(
                    trimmed.chars().next(),
                    Some('}') | Some(')') | Some(']') | Some('#') | Some('`')
                )
            {
                end = Some(r);
                break;
            }
        }
        let end = end.unwrap_or(self.row_count);
        (end >= start + 2).then_some(end)
    }

    /// 全篇扫描「可折叠候选」（不重叠可嵌套，O(行数) 单遍）：
    /// 行 `i` 是候选起点 ⟺ 其后的首个非空行缩进更深（该行将藏起一个块）；块结束 = 首个
    /// 缩进回退 `/` 同级闭记号（`}` 等）行。与 [`Self::fold_block_end`] 同口径，
    /// 供 gutter 默认指示器与点击/快捷键折叠使用。
    /// 委托给纯函数 [`crate::editor::fold::scan_fold_candidates`]（语义逐位一致）。
    fn scan_fold_candidates(&self) -> Vec<Fold> {
        crate::editor::fold::scan_fold_candidates(|r| self.buffer.line_text(r), self.row_count)
    }

    /// 重扫折叠候选：≤ 阈值的整篇扫，超大文档清空（改为按需折叠）。
    fn recompute_fold_candidates(&mut self) {
        if self.row_count > FOLD_CANDIDATE_FULL_MAX_LINES {
            self.fold_candidates.clear();
        } else {
            self.fold_candidates = self.scan_fold_candidates();
        }
    }

    /// 以 `line` 为起点的折叠候选（gutter 指示器/点击折叠的区间来源）。
    fn candidate_starting_at(&self, line: usize) -> Option<Fold> {
        self.fold_candidates
            .iter()
            .find(|f| f.start == line)
            .copied()
    }

    /// `line` 是否有「可折叠」标记：已激活折叠首行，或折叠候选起点。
    fn has_fold_or_candidate(&self, line: usize) -> bool {
        self.folds.fold_starting_at(line).is_some()
            || self.candidate_starting_at(line).is_some()
    }

    /// 重扫括号标尺配对：≤ 阈值整篇扫（跨字符串/注释词法），超大文档清空。
    fn recompute_bracket_pairs(&mut self) {
        if self.row_count > FOLD_CANDIDATE_FULL_MAX_LINES {
            self.bracket_pairs.clear();
        } else {
            let buffer = &self.buffer;
            self.bracket_pairs = scan_bracket_pairs(
                |r| buffer.line_text(r),
                self.row_count,
            );
        }
    }

    /// 在**后台线程**重算折叠候选 + 括号标尺配对并落地（单飞 + 版本号取消）。
    ///
    /// - 打开收尾（file_ops 末块落定）与编辑后的下一帧 render 都会调用；
    /// - 用 Rope O(1) COW 快照在 `background_executor` 上跑纯函数扫描（`fold.rs` /
    ///   [`scan_bracket_pairs`]），结果回来时若期间又有请求（`scan_epoch` 已变）则
    ///   丢弃旧结果、用最新快照重跑一轮；
    /// - 不阻塞 UI 线程：打开 160k 行文档的 ~280ms 全量扫描不再卡界面，输入含括号
    ///   字符也不再逐键整篇重扫。
    pub(crate) fn refresh_fold_brackets(&mut self, cx: &mut Context<Self>) {
        use crate::config::limits::FOLD_CANDIDATE_FULL_MAX_LINES as SCAN_LIMIT;
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
        if self.scan_inflight {
            return; // 已有任务在跑：其末尾会因 epoch 已变而用最新快照重跑一轮。
        }
        self.scan_inflight = true;
        cx.spawn(async move |this, cx| {
            loop {
                let snap = this.update(cx, |this, _| {
                    (this.scan_epoch, this.row_count, this.buffer.text().clone())
                });
                let Ok((epoch, row_count, rope)) = snap else { return };
                let (folds, brackets) = cx
                    .background_executor()
                    .spawn(async move {
                        if row_count > SCAN_LIMIT {
                            return (Vec::new(), Vec::new());
                        }
                        let line = |r: usize| crate::editor::buffer::line_shared(&rope, r);
                        // `line` 捕获 `&rope`（Copy 闭包）→ 按值传给两个纯函数扫描。
                        let folds = crate::editor::fold::scan_fold_candidates(line, row_count);
                        let brackets = scan_bracket_pairs(line, row_count);
                        (folds, brackets)
                    })
                    .await;
                let done = this
                    .update(cx, |this, cx| {
                        if epoch == this.scan_epoch {
                            this.fold_candidates = folds;
                            this.bracket_pairs = brackets;
                            this.scan_inflight = false;
                            cx.notify();
                            true
                        } else {
                            false // 期间又有编辑/请求 → 用最新快照重跑
                        }
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
            }
        })
        .detach();
    }

    /// 括号标尺在当前视口内的竖线段：`(x_px, y_rel, h_px)`（文本区本地坐标）。
    /// 每个跨行括号配对一根竖线，位于**块首行缩进列**（`guide_units` × 字宽，随横向
    /// 滚动），从开行**之后**画到闭行**内约 0.4 行深**（可配）——`}` 字形位于闭行
    /// 中部，竖线底端伸入闭行以贴近 `}` 字形顶端、与顶部距离一致（不画满整行，
    /// 避免穿过 `}`）。端点微调常量见 `crate::config::layout`。
    ///
    /// **同列区间合并**：同一列上重叠/相邻的竖线合并成一根连续线——否则两条 1px
    /// 线在同像素叠加会显得更暗/更粗（同列嵌套块会命中此情况）。
    /// `top_vis`/`off` = `abs_scroll_top()`；`lh` 行高；`harea_w` 文本区宽；`view_h` 视口高。
    /// 光标逻辑行 L 落在配对跨度 `(open_line, close_line]` 内即命中（点击点亮用）。
    /// 多对命中时取**最深**（`open_line` 最大的那个）——即光标所在的最内层块。
    fn guide_pair_for_logical_row(&self, logical: usize) -> Option<usize> {
        self.bracket_pairs
            .iter()
            .filter(|p| p.open_line < logical && logical <= p.close_line)
            .map(|p| p.open_line)
            .max()
    }

    /// 同 [`Self::bracket_guide_segments`]，但每个合并段额外带 `bool` = 该段是否命中
    /// `active`（点亮标尺的 `open_line`）。合并同列样本段时，任一样本命中则整段点亮。
    fn bracket_guide_segments_flagged(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
        active: Option<usize>,
    ) -> Vec<(f32, f32, f32, bool)> {
        if self.bracket_pairs.is_empty() {
            return Vec::new();
        }
        // 竖线端点微调（× 行高，见 config/layout.rs）：底部伸入闭行贴 `}` 字形，
        // 顶部从首覆盖行向下缩进控制留白。均为可配置行高系数。
        let head = BRACKET_GUIDE_OPEN_HEAD_LINES * lh;
        let tail = BRACKET_GUIDE_CLOSE_TAIL_LINES * lh;
        let cw = self.char_width.as_f32().max(1.);
        let hscroll = self.hscroll_px();
        let bottom_vis = top_vis.saturating_add(((view_h / lh).ceil() as usize) + 1);
        // (x, 视觉顶行, 视觉底行**开区间**, 是否点亮) —— 先整列收集，再合并同列重叠/相邻段。
        let mut intervals: Vec<(f32, usize, usize, bool)> = Vec::new();
        for p in &self.bracket_pairs {
            if p.open_line >= p.close_line {
                continue; // 过期（编辑后未及时重扫）的配对；重扫会清理。
            }
            let vs = self.folds.visual_row(p.open_line);
            let ve = self.folds.visual_row(p.close_line);
            // 区间 [open 行之后, close 行之前)。
            let seg_top = vs.saturating_add(1);
            let seg_bot = ve.saturating_sub(1);
            if seg_bot < seg_top {
                continue; // 相邻行块（无体）或折叠压缩 → 无竖线。
            }
            if seg_bot < top_vis || seg_top > bottom_vis {
                continue; // 与视口不相交。
            }
            let x = p.guide_units as f32 * cw - hscroll;
            if x < 0.0 || x > harea_w {
                continue; // 出文本区裁掉。
            }
            intervals.push((
                x.round().max(0.0),
                seg_top.max(top_vis),
                seg_bot.min(bottom_vis).saturating_add(1),
                active == Some(p.open_line),
            ));
        }
        // 按列排序，合并同列重叠/相邻区间。
        intervals.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        let mut out: Vec<(f32, f32, f32, bool)> = Vec::new();
        let mut cur: Option<(f32, usize, usize, bool)> = None;
        for (x, t, b, hl) in intervals {
            match &mut cur {
                Some((cx, ct, cb, ca)) if *cx == x && t <= *cb => {
                    *cb = (*cb).max(b);
                    *ca |= hl;
                }
                _ => {
                    if let Some((cx, ct, cb, ca)) = cur.take() {
                        out.push((
                            cx,
                            (ct as f32 - top_vis as f32) * lh - off + head,
                            (cb - ct) as f32 * lh - head + tail,
                            ca,
                        ));
                    }
                    cur = Some((x, t, b, hl));
                }
            }
        }
        if let Some((cx, ct, cb, ca)) = cur {
            out.push((
                cx,
                (ct as f32 - top_vis as f32) * lh - off + head,
                (cb - ct) as f32 * lh - head + tail,
                ca,
            ));
        }
        out
    }

    fn bracket_guide_segments(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
    ) -> Vec<(f32, f32, f32)> {
        // 未点亮（active=None）→ 与旧实现语义一致（3 元组）。
        self.bracket_guide_segments_flagged(top_vis, off, lh, harea_w, view_h, None)
            .into_iter()
            .map(|(x, y, h, _)| (x, y, h))
            .collect()
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
            // 撤销前该编辑处于「新文本已插入」状态：被替换区 = inserted_range。
            let la = self.buffer.line_of_offset(edit.start);
            let lb = self.buffer.line_of_offset(
                edit.inserted_range().end.min(self.buffer.len()),
            );
            self.buffer.undo_edit(edit);
            self.row_count = self.buffer.line_count().max(1);
            self.folds.sync_edit(la, lb, &edit.old_text, self.row_count);
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
        self.resync_list(true);
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
            // 重做前该编辑处于「旧文本被移除」状态：被替换区 = replaced_range。
            let la = self.buffer.line_of_offset(edit.start);
            let lb = self.buffer.line_of_offset(
                edit.replaced_range().end.min(self.buffer.len()),
            );
            self.buffer.redo_edit(edit);
            self.row_count = self.buffer.line_count().max(1);
            self.folds.sync_edit(la, lb, &edit.new_text, self.row_count);
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
        self.resync_list(true);
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
        // 顶部与行高都在视觉空间：逻辑行先映射成视觉行再算 y（折叠体塌缩到首行）。
        let (top_abs, off_px) = self.abs_scroll_top();
        let vis = self.folds.visual_row(row);
        let y = (vis as f32 - top_abs as f32) * lh - off_px;
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
        let gutter_start = g_top_abs;
        let gutter_count = (((view_h / lh) as f32).ceil() as usize).clamp(1, 4096) + 1;
        let gutter_items: Vec<Option<(usize, bool)>> = (gutter_start..gutter_start + gutter_count)
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
            .collect();
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
        // 最内层块竖线（`active_guide_open`）用强调蓝加粗 2px。
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
                    .w(px(if active { 2.0 } else { 1.0 }))
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
            .child(
                div()
                    .id("ce-vscrollbar-overlay")
                    .debug_selector(|| "ce-vscrollbar-overlay".into())
                    .absolute()
                    .inset_0()
                    .child({
                        let v = VerticalScrollHandle {
                            state: list_state.clone(),
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
                    }),
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

/// 把「语法着色段 +（可重叠的）选区底色」合并为**非重叠、总量不超订**的连续
/// 样式段，供 `StyledText` 使用。
///
/// 决不能把选区（通常横跨多个语法段）作为**重叠**的独立 highlight 附在语法段
/// 后面：gpui `StyledText::compute_runs` 假定 highlight 互不重叠（`text.rs` 里
/// 逐段按全长入 run，重叠会令 run 总长 > 行字节长），而 `shape_text` 消费到行尾
/// 即停 → 尾部的选区整段被丢弃（选中底色不显示，表现为“语法高亮文件无法鼠标
/// 选择”，同字节的 .txt/.md 因无语法段而正常）。
///
/// 这里把 0..行尾 按「语法段边界 ∪ 选区边界」切成连续单元，逐单元：语法覆盖
/// 则写 `color`，选区覆盖则写 `background_color`。产出严格递增、不重叠、每段
/// 都在行内，run 总量恰为各覆盖区间之和 ≤ 行字节长。
pub(crate) fn merge_highlights(
    runs: &[(Range<usize>, Hsla)],
    sel: Option<Range<usize>>,
    sel_bg: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    // 收集所有切分边界：语法段边界 ∪ 选区边界。
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
    if let Some(s) = &sel {
        if s.end > s.start {
            bounds.push(s.start);
            bounds.push(s.end);
        }
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
        // 语法色：runs 互不重叠且升序，取首个覆盖 [a, b) 的段。
        for (r, color) in runs {
            if r.end <= a {
                continue;
            }
            if r.start >= b {
                break;
            }
            style.color = Some(*color);
            break;
        }
        if sel.as_ref().is_some_and(|s| a >= s.start && b <= s.end) {
            style.background_color = Some(sel_bg);
        }
        if style.color.is_some() || style.background_color.is_some() {
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

/// 行首空白字节数（空格/Tab；缩进折叠启发式用）。
fn leading_ws_len(s: &str) -> usize {
    s.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}
