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

mod input;
mod render;

mod document;
mod layout;
mod rows;
mod interaction;
mod editing;
mod find_goto;

pub(crate) use find_goto::{FindBtn, FindTipState};

pub(crate) use render::merge_highlights;
// `char_align_highlights` 仅测试经此路径消费（`editor::tests`），非测试构建会触发
// unused 警告 → 按测试门控（render.rs 内在用法直接引用同模块函数，不受影响）。
#[cfg(test)]
pub(crate) use render::char_align_highlights;

use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    ops::Range,
    rc::Rc,
    time::{Instant},
};

use gpui::{
    Bounds,
    FocusHandle,
    HighlightStyle,
    Hsla,
    ListState,
    Pixels,
    Point,
    ScrollHandle,
    SharedString,
    Window,
};

use crate::editor::markdown::MarkdownDecor;
use crate::editor::fold::{
    BracketPair,
    Fold,
    FoldMap,
};
use crate::editor::highlight::Highlighter;
use crate::editor::undo::{
    UndoManager,
};
use crate::editor::{
    Buffer,
    Selection,
};

/// 代码行高亮回看行数（多行注释/字符串上下文不错位）。
const HL_LOOKBACK_LINES: usize = 16;
/// 语法高亮「分片补色」每帧行数预算（`build_rows` 延后帧：从可见区顶部起本帧只解析
/// 这么些行的真颜色，窗口其余行仍空样式）。
///
/// 基准实测（debug，本仓库大 .rs 样本 12 万行，见 `bench_first_frame_coloring` 校准）：
/// syntect 逐行 parse ≈ **1.3-1.7ms/行**。若把「每补色帧 ≤5ms」的目标硬换算成行数只有
/// ~3 行/帧——那种摊法首批颜色要跨几十帧、满窗总成本反而更高（每帧固定开销 + 帧调度
/// 损耗）。按实测取 **32 行/帧 ≈45-55ms**：首批颜色 <60ms（观感目标）、整窗 ~5 帧；
/// 滚动停住后由 `settle_pending` 一次有界重锚补齐整窗（含上方过扫）、清 pending，
/// 全窗总耗落回 ~200-300ms 级（不再 1s 级死白）。
const HL_FILL_BUDGET_ROWS: usize = 32;
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
/// 折叠指示箭头的**固定槽宽**（px）。槽恒存在（无箭头时为空但照占位）→ 行号右缘
/// 锚点不随有无箭头而位移，有箭头行不与无箭头行错位；槽内右对齐放「▸/▾」字形。
const FOLD_ARROW_SLOT_PX: f32 = 14.;

/// 编辑器对外事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    /// 文本被修改（撤销/重做/输入/删除均触发）。
    Edited,
    /// 光标位置 / 选区变化（不修改文本）：鼠标点击、方向键移动、程序化定位（搜索
    /// 跳转 / 侧边栏目录）都发此事件——宿主据此刷新状态栏 `Ln x, Col y` 等派生数据。
    SelectionChanged,
    Focus,
    Blur,
}

/// 行静态内容层（跨帧缓存，不可变）：buffer 文本 + 整窗批量解析的原始 runs + 无选区
/// 合并结果。滚动帧命中直接 `Rc` 浅复用，零分配零合并。
#[derive(Clone)]
struct RowContent {
    /// 渲染窗口内的文本（`buffer.line_text(row)` 一次性物化，跨帧共享）。
    text: SharedString,
    /// 行内原始样式段（syntect / md_decor 整窗批量解析的**选区合并前**快照）。
    /// 既是缓存有效性校验基准（与本帧新鲜解析逐位比对，见 [`RowCache::probe`]），
    /// 也是选区活动时动态重合并的输入（见 [`RowData::highlight_override`]）。
    hl_styles: Vec<(Range<usize>, HighlightStyle)>,
    /// 无选区合并结果（= `merge_highlights(hl_styles, None, _)`，按当前代码合并语义
    /// 定格）：选区坍缩时直接复用，跳过每行 O(段²) 的合并与分配。
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

impl RowContent {
    /// 物化一行内容：文本 + 样式定格。选区坍缩帧可直接复用 `highlights`。
    fn materialize(
        buffer: &Buffer,
        row: usize,
        hl_styles: &[(Range<usize>, HighlightStyle)],
    ) -> Self {
        let text = buffer.line_text(row);
        // 无选区合并不使用 sel_bg（见 merge_highlights：仅选区覆盖段才上底色）。
        let highlights = merge_highlights(hl_styles, None, Hsla::default());
        Self {
            text,
            hl_styles: hl_styles.to_vec(),
            highlights,
        }
    }

    fn empty() -> Self {
        Self {
            text: SharedString::new_static(""),
            hl_styles: Vec::new(),
            highlights: Vec::new(),
        }
    }
}

/// 行渲染数据（`list` 闭包只读）。
#[derive(Clone)]
struct RowData {
    /// 静态内容层（`Rc` 浅复用：无编辑滚动帧直接引用缓存条目，不重新物化）。
    content: Rc<RowContent>,
    /// 选区活动帧的动态合并结果（= `merge_highlights(hl_styles, Some(sel), bg)`）。
    /// `None` = 选区坍缩：直接读 `content.highlights`（滚动帧零分配零合并）。
    highlight_override: Option<Vec<(Range<usize>, HighlightStyle)>>,
    /// 本行是否绘制光标 + 光标 x（相对文本起点，px）。按帧状态现算，不入缓存。
    caret: Option<f32>,
    /// 本行是折叠首行时折叠指示「…」的 x（相对文本起点，px）。折叠状态按帧现算，
    /// 不入缓存。`None` 非折叠首行。独立元素渲染（不进 `StyledText`/shape_text），
    /// 不污染行内测量。
    folded: Option<f32>,
    /// 本行是否为「光标所在行」（持焦且 selection head 落本行范围内）。按帧现算、
    /// 不入缓存，与 `caret`/`folded` 同级——但不受光标闪烁影响（head 位置稳定即
    /// 恒高亮）。折叠优先于本高亮（见 `row_element` 的互斥 `when`）。
    active: bool,
}

impl RowData {
    /// 渲染消费的样式段：选区坍缩用缓存的无选区合并结果；选区活动用逐帧动态合并。
    fn highlights(&self) -> &[(Range<usize>, HighlightStyle)] {
        match &self.highlight_override {
            Some(v) => v,
            None => &self.content.highlights,
        }
    }

    fn empty() -> Self {
        // 空行兜底：共享同一份零分配的内容层（thread_local 单例），避免窗口外每个
        // 空行都新建两个空 Vec。
        thread_local! {
            static EMPTY_CONTENT: Rc<RowContent> = Rc::new(RowContent::empty());
        }
        let content = EMPTY_CONTENT.with(|c| c.clone());
        Self {
            content,
            highlight_override: None,
            caret: None,
            folded: None,
            active: false,
        }
    }
}

/// 行级静态内容缓存：键 = `(buffer.version(), 逻辑行)`。
///
/// 目标：滚动帧把「整窗重物化（每行一次 String 分配 + 一次 O(段²) 样式合并）」降为
/// 「只物化新进窗口的行」——命中行 `Rc` 浅复用，零分配零合并。
///
/// 结构与选型理由：
/// - 键用**逻辑行**而非视觉行：文本与高亮本就按逻辑行解析；视觉行随折叠状态可变，
///   若按视觉行键会在折叠变化时让未改行整窗失效抖动。
/// - `HashMap` + 有界 FIFO 淘汰（`CAP` ≈ 十余个窗口高，与滚动访问近似单调匹配）：
///   滚动只换窗，增删查 O(1)；有界封顶防大文档一路滚动把缓存涨到 O(全文)。
/// - 有效性 = 键命中 **且** 缓存 raw runs 与本帧新鲜解析逐位相等（[`Self::probe`]）：
///   syntect/md 的重锚（滚动上移出缓存、大缺口下跳、回看上下文变化）会让同一
///   `(version, row)` 产出不同 runs——逐位比对把这些行自动打回重物化并覆写，
///   保证「任意编辑 + 滚动序列下渲染结果与每帧全建逐位一致」。
///
/// 失效语义：
/// - 编辑 / 撤销 / 重做：`buffer.version()` +1 → 键失配，旧条目不可达（有界淘汰回收）。
/// - 整篇替换（`reset_document`）：显式 `clear()`（`Buffer::new` 后 version 归零，
///   防「新旧文档同 version=0」命中上一份文档的行）。
/// - 折叠变化：不失效（内容与折叠无关；折叠指示是动态层，不在缓存里）。
/// - 字号 / 主题变化：不失效（内容 = 字节区间 + 样式，与字形/底色无关）。
#[derive(Default)]
struct RowCache {
    /// 键命中（probe 校验通过）→ 浅复用；无效/未命中 → 重物化并覆写。
    entries: HashMap<(u64, usize), Rc<RowContent>>,
    /// FIFO 淘汰序（仅最旧回收；滚动访问近似单调，无需 LRU 晋升）。
    order: VecDeque<(u64, usize)>,
    /// 命中计数（测试探针：滚动帧复用证明）。
    hits: usize,
}

impl RowCache {
    const CAP: usize = 1024;

    /// 命中判定：键命中 **且** 缓存 raw runs 与本帧新鲜解析逐位相等才复用。
    fn probe(
        &mut self,
        ver: u64,
        row: usize,
        fresh: &[(Range<usize>, HighlightStyle)],
    ) -> Option<Rc<RowContent>> {
        match self.entries.get(&(ver, row)) {
            Some(c) if c.hl_styles == fresh => {
                self.hits += 1;
                Some(c.clone())
            }
            _ => None,
        }
    }

    fn put(&mut self, ver: u64, row: usize, content: Rc<RowContent>) {
        let key = (ver, row);
        if !self.entries.contains_key(&key) {
            self.order.push_back(key);
        }
        self.entries.insert(key, content);
        while self.entries.len() > Self::CAP
            && let Some(old) = self.order.pop_front()
        {
            self.entries.remove(&old);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.hits = 0;
    }

    #[cfg(test)]
    fn stats(&self) -> (usize, usize) {
        (self.hits, self.entries.len())
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
    /// 已折叠行整行底色（比选区淡，见 `refresh_metrics`）。
    fold_bg: Hsla,
    /// 光标所在行（持焦且 head 落行内）整行高亮底（=`colors.editor_active_line_bg`，
    /// 与折叠行同色，见 `refresh_metrics`）。
    active_line_bg: Hsla,
    /// 当前横向滚动像素（内容被推向左的量，见 [`CodeEditor::hscroll_px`]）——
    /// 超长行「横向窗口化」按它裁剪行文本（宽度限于可视窗 ± 余量，见
    /// `render::styled_row_text`）。渲染期快照，不入行缓存。
    hscroll_px: f32,
    /// 文本区可视宽（px，= `compute_content_width` 的 `harea_w`）——窗口化右缘依据。
    harea_w: f32,
    /// 等宽格宽（px）——窗口化余量 px 换算用（`char_width_units` 同一格宽口径）。
    char_width: f32,
}

/// 固定窗口虚拟化：ListState **不再持有全文 N 条目的 O(N) 树**（126MB / 3.2M 行/// 白占 ~300MB）。改为 ListState 恒持 [`LIST_WINDOW_ROWS`] 条「窗口」，滚动位置由
/// [`CodeEditor::window_base`]（窗口首行绝对行号）+ ListState 相对滚动合成：
/// - 目标超出当前窗口即 `reset(WINDOW)` 重定位（O(窗口)，一刀切重定位、无需分帧）；
/// - 量程/clamp 全部按**全文** `scroll_item_count()×行高` 算（与窗口无关）；
/// - 打开大文件瞬时 O(1)：不再需要「两段式 + 分帧增长」机制。
///
/// 条目高度仍全部按需实测（不预铺 uniform），纵向量程由像素级确定性滚动条承担。
///
/// 固定窗口条目数：需 ≥ 最高分辨率的可见行 + 过扫余量（512 × 行高 ≈ 上万 px 视口）。
const LIST_WINDOW_ROWS: usize = config::limits::EDITOR_LIST_PROVISIONAL_ROWS;

/// 测试探针（`cfg(test)`）：整篇 `set_text`/`set_rope_with_stats`（reset_document 级联）
/// 被调次数。app 层「WYSIWYG 写回走增量路径」回归断言：切回**编辑过的大文档**不得再
/// 走整篇 set_text（旧实现 debug ~11s；增量 `apply_replaces` 不计数）。
#[cfg(test)]
pub(crate) static CODE_EDITOR_FULL_RESET_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 语法高亮「分片补色」进度（`build_rows` 的延后帧用它逐帧从可见区顶部往下补色）。
///
/// 背景：大 `.rs` 拖滚动条/远跳时，`Highlighter::accept_deferred` 把窗口登记为
/// `pending`（本帧延后满窗空样式）。改进 = 延后帧不再全空：本帧只从**可见区顶部**
/// 逻辑行起解析 `HL_FILL_BUDGET_ROWS` 行真颜色（其余行空样式），相邻帧窗口一致 →
/// 游标继续下推，直到 `settle`（滚动停稳）一次有界重锚补齐整窗、清 pending。这样
/// 「滚动中可见区顶部一段始终有色、停稳后首批颜色几十 ms 出现」，告别全程死白。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HlFill {
    /// 本补色会话所属请求窗口 `(lo_l, hi_l)`（**逻辑行**）；与前帧窗口不一致（仍在
    /// 拖）/编辑器版本变了（编辑）→ 重锚回新窗口可见顶。
    window: (usize, usize),
    /// 会话建立时的 `buffer.version`；编辑后重锚从头补（下方缓存已随编辑失效）。
    version: u64,
    /// 会话可见区顶部逻辑行（= 补色起点，`vis_top` 非窗口 `lo`——窗口 `lo` 含上方过扫，
    /// 先给不可见行上色没用）。
    vis_top_l: usize,
    /// 下一个待补逻辑行（向下阶段：`vis_top_l → hi_l`）。`> hi_l` 表示可见区顶到窗口底
    /// 已补完，剩余「上方过扫 `[lo_l, vis_top_l)`」由 settle 一次补齐。
    cursor: usize,
    /// P6：本补色会话的「首帧零解析」是否已消费（打开首帧高亮缓存为空时，本帧
    /// **不解析任何 syntect**，直接渲染纯文本让「文件开头」即刻可见；第二帧起每帧
    /// 32 行补色）。窗口/版本变化重置会话时自动归 false。
    first_pass_done: bool,
}

impl HlFill {
    /// 分片补色会话是否仍在「该窗口 + 版本」的进行中（下一待补行 ≤ 窗口底）。
    /// `build_rows` 用它门控：补色期间不回落同步整窗解析、也不 `settle` 整窗重锚——
    /// 两者都会在补色早期一次性同步解析整窗（debug 数百 ms），把「打开大 .rs 首帧先出
    /// 纯文本」打回「一帧冻结空白」（P5 根因）。
    fn active(&self, version: u64, lo_l: usize, hi_l: usize) -> bool {
        self.version == version && self.window == (lo_l, hi_l) && self.cursor <= hi_l
    }
}

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
    /// 最近一次编辑对高亮的影响范围（`(min_row, tail_new, delta)`，见
    /// [`Highlighter::set_dirty`]）：由 `apply_edit_synced` 写、`build_rows` 取走喂给
    /// `Highlighter` 做「局部重解析 + 收敛早退」，避免逐键把整窗重跑 syntect。
    /// 整篇替换/打开/撤销重做置 `None`（走整段重解析）。
    hl_dirty: Option<(usize, usize, i64)>,
    /// 语法高亮「分片补色」进度（延后帧逐帧下刷，见 [`HlFill`]；滚动停稳 settle 后
    /// 复位为默认）。
    hl_fill: HlFill,
    /// M5-B2-3 行级缓存：`build_rows` 按 `(buffer.version(), 逻辑行)` 复用行静态内容
    /// （文本 + 合并后样式）。滚动帧只物化新进窗口的行；编辑/撤销/重做靠版本键失效，
    /// 整篇替换由 [`Self::reset_document`] 显式清空。见 [`RowCache`]。
    row_cache: RowCache,
    /// Markdown「所见即所得」滤镜：.md 时替代 syntect 行高亮，把记号画淡、正文
    /// 应用加粗/斜体/代码/链接/标题等富样式（只改样式，文本/光标/选区不变）。
    md_decor: MarkdownDecor,
    /// 是否为 Markdown 文档（true 时行渲染走滤镜扫描而非 syntect）。
    md_mode: bool,
    language: Option<String>,

    // —— 字体度量（render 时由主题解析）——
    mono: SharedString,
    font_size: Pixels,
    line_height: Pixels,
    char_width: Pixels,
    metrics_ready: bool,
    /// 上一次字体度量重算时的依赖指纹 `(等宽字体族, 字号)`：`resolve_font` +
    /// `ch_advance` 的唯一输入是二者，指纹未变即跳过昂贵重算。`None` = 尚未首算。
    metric_fp: Option<(SharedString, Pixels)>,
    /// 字体度量全量重算次数（含首帧）；测试断言「同主题连续帧不再重算」的可观察信号。
    metric_compute_count: usize,

    // —— 布局状态 ——
    row_count: usize,
    hscroll_cols: usize,
    visible_chars: usize,
    gutter_chars: usize,
    gutter_width_px: Pixels,
    /// 横向内容宽（px，Buffer O(1) 精确，不量化）：驱动 `harea` 容器/`h_handle`
    /// 布局量程的唯一真源。横向滚动条读取实测 `h_handle.max_offset().x`（= 本宽 −
    /// 文本区可视宽），二者同源且一致——不再有「估算 + 64px 量化」的第二份量程。
    content_w_px: f32,

    // —— 主题色快照 ——
    bg: Hsla,
    fg: Hsla,
    gutter_fg: Hsla,
    sel_bg: Hsla,
    /// 已折叠行的整行底色（`colors.editor_fold_bg`、比选区淡，见 `refresh_metrics`）。
    fold_bg: Hsla,
    /// 光标所在行（持焦且 selection head 落行内）的整行高亮底色
    /// （`colors.editor_active_line_bg`，与折叠行同色，见 `refresh_metrics`）。
    active_line_bg: Hsla,
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
    /// 拖选帧级合并（B2）：`on_mouse_move` 只记**最新拖点**（编辑区本地点），下一帧
    /// `render` 开头 `flush_pending_drag` 统一换算/落地选区——鼠标事件可比帧快得多，
    /// 不再逐事件重复 offset 换算 + set_head + emit + notify（对齐 scrollbar
    /// `pending_target`→prepaint 落地先例的视图层等价物）。
    pending_drag: Option<Point<Pixels>>,
    /// （仅测试）`flush_pending_drag` 落地次数（每帧至多一次）。
    #[cfg(test)]
    drag_flush_calls: Cell<usize>,
    /// 横向内容的**尾部留白**（px；默认 0）：仅当最宽行**溢出**可视区时才追加到
    /// `content_w_px` 之后——滚到底后最后一个字符与右裁切缘之间留出该空隙，不再贴缘
    /// 被裁/难以点中（数学编辑卡长公式专用；主编辑器保持 0 = 行为不变）。
    hscroll_tail_gap: f32,

    /// ListState 固定窗口的**首行绝对行号**：ListState 的 item_ix 是窗口内的相对下标，
    /// 绝对行 = `window_base + item_ix`（所有滚动/命中/行号/量程都以绝对行为准）。
    /// 目标滚出窗口即重定位本字段（`reset(LIST_WINDOW_ROWS)`，O(窗口)），不改文档。
    window_base: usize,
    /// 恒 true 的「像素级确定性量程」（固定窗口虚拟化，已去布尔）：条目不预铺
    /// uniform，纵向量程由滚动换算 + `ScrollbarHandle.content_height = 全文行数×
    /// 行高` 承担，不存在换宽/陈旧 size_hint 的量程坍缩（若未来恢复
    /// `pixel_scroll=false` 的小文档 List 委托，需重新加回该布尔与死分支）。
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
    /// 从开括号列画到闭括号行。≤ 200k 行整篇扫描，随编辑/行数变化增量维护。
    bracket_pairs: Vec<BracketPair>,
    /// （增量用）本次落地扫描时 fold 扫描器的「栈清空边界」行（升序）。从任一
    /// 边界以全新状态续扫与整篇扫描逐位一致，是增量重算的断点来源（见
    /// [`crate::editor::fold::scan_fold_segment`]）。
    fold_boundaries: Vec<usize>,
    /// （增量用）括号扫描器的「栈空 + 不在块注释内」边界行（升序），用途同上。
    bracket_boundaries: Vec<usize>,
    /// 折叠候选 / 括号配对的**后台刷新**版本号：每次请求 +1；后台任务回来时若
    /// 版本已过期（期间又编辑/又请求）则丢弃旧结果、用最新快照重跑一轮。
    scan_epoch: u64,
    /// 是否已有一轮后台刷新在进行（单飞：防止连续输入堆积多个任务）。
    scan_inflight: bool,
    /// 编辑后待刷新：下一帧 render 挂起一次后台刷新（替代原同步整篇重扫）。
    scan_dirty_edit: bool,
    /// （增量用）自上落地后累计的**单个**受影响行段
    /// `(line_a 编辑前行号, min_stop 新坐标首个未受影响行, delta 净行增量)`；
    /// 多个编辑段累积时置 [`Self::scan_multi_edit`]（退整篇重扫），落地后清空。
    scan_pending_edit: Option<(usize, usize, i64)>,
    /// （增量用）自上落地后累积了 **≥2** 个未落地编辑段 → 增量前提不成立，
    /// 下一轮后台刷新退化为整篇扫描（正确性兜底）；落地后复位。
    scan_multi_edit: bool,
    /// 垂直标尺线颜色（主题 `muted_foreground` 派生，refresh_metrics 设置；
    /// 已验证可见——行号列同款）。不依赖 `border`（可能为透明默认值）。
    ruler_color: Hsla,
    /// 括号标尺竖线颜色（更淡的次要前景）。
    guide_color: Hsla,
    /// 点击代码区域点亮的最内层括号块（`open_line`；`None` = 未点亮/已清除）。
    /// 编辑（`apply_edit_synced`）与整篇替换（`reset_document`）会复位为 `None`。
    active_guide_open: Option<usize>,
    /// 点亮竖线颜色：主题强调蓝 `colors.accent.opacity(0.9)`（`refresh_metrics` 设置；
    /// **勿抽进 config**——用户已明确回滚过颜色抽取）。
    active_guide_color: Hsla,
    /// （仅测试）`apply_live_replay`（方案2 op 重放主路径）成功调用次数——每个测试的
    /// `CodeEditor` 都是新实例、字段从 0 起，比进程级静态计数在串行/并行测试下都确定。
    live_replay_calls: Cell<usize>,
    /// （仅测试）`apply_replaces`（行级 diff 兜底）非空调用次数——同上，实例级确定性。
    apply_replaces_calls: Cell<usize>,
    /// （仅测试）`guide_pair_for_logical_row` 二分后回扫**实际检查**的配对步数（每次
    /// 调用前清零）——证明「点击块内不再整表线性扫」，典型块内命中应为 O(1) 步。
    #[cfg(test)]
    guide_scan_steps: Cell<usize>,
    /// （仅测试）后台折叠候选/括号配对落地的**增量**次数（增量前提成立的刷新轮次）。
    #[cfg(test)]
    scan_inc_landings: Cell<usize>,
    /// （仅测试）后台折叠候选/括号配对落地的**整篇**次数（兜底/打开收尾轮次）。
    #[cfg(test)]
    scan_full_landings: Cell<usize>,

    // —— 编辑区内查找（Ctrl+F / F3）——
    /// 查找状态机/匹配表/导航（纯逻辑，见 `crate::editor::find`）。
    find: crate::editor::find::FindOverlay,
    /// 查找输入框「文本变化 → 置脏 + 防抖」订阅句柄（首开时懒创建）。
    _find_sub: Option<gpui::Subscription>,
    /// 查找「非当前匹配」高亮底色（主题 `editor_find_match_bg`，见 `refresh_metrics`）。
    find_match_bg: Hsla,
    /// 查找「当前命中」高亮底色（主题 `editor_find_current_bg`，见 `refresh_metrics`）。
    find_current_bg: Hsla,
    /// 查找输入框实体（`ui::InputState`，IME/编辑/焦点自理；首次打开懒创建）。
    find_input: Option<gpui::Entity<ui::input::InputState>>,
    /// find 卡片图标按钮的悬停提示状态（按钮内 hover + 卡片内小浮层）。
    find_tip: FindTipState,
    // —— 编辑区内跳转行（Ctrl+G）——
    /// 跳转行状态机/输入缓存/防抖代号/内联提示（纯逻辑，见 `crate::editor::goto`）。
    goto: crate::editor::goto::GoToOverlay,
    /// 跳转行输入框「文本变化 → 置脏 + 防抖」订阅句柄（首开时懒创建）。
    _goto_sub: Option<gpui::Subscription>,
    /// 跳转行输入框实体（`ui::InputState`，IME/编辑/焦点自理；首次打开懒创建）。
    goto_input: Option<gpui::Entity<ui::input::InputState>>,
}
impl CodeEditor {
    /// 测试用度量：`(char_width, line_height, gutter_width)`（px 的 f32）。
    pub fn test_metrics(&self) -> (f32, f32, f32) {
        (
            self.char_width.as_f32(),
            self.line_height.as_f32(),
            self.gutter_width_px.as_f32(),
        )
    }

    /// 测试用：字体度量全量重算次数（含首帧）。指纹未变即跳过，计数应保持不变。
    pub fn test_metric_compute_count(&self) -> usize {
        self.metric_compute_count
    }

    /// 测试用：折叠映射（只读）。
    pub fn test_folds(&self) -> &FoldMap {
        &self.folds
    }

    /// 测试用：程序化添加折叠 `[start, end)`（半开逻辑行）。
    pub fn test_set_fold(&mut self, start: usize, end: usize) {
        self.folds.toggle(start, end);
    }

    /// 测试用：自动探测的折叠候选（只读）。
    pub fn test_fold_candidates(&self) -> &[Fold] {
        &self.fold_candidates
    }

    /// 测试用：括号标尺配对（只读）。
    pub fn test_bracket_pairs(&self) -> &[BracketPair] {
        &self.bracket_pairs
    }

    /// 测试用：清空折叠候选与括号配对（验证后台刷新会重新落地）。
    pub fn test_clear_fold_brackets(&mut self) {
        self.fold_candidates.clear();
        self.bracket_pairs.clear();
    }

    /// 测试用：括号标尺在当前视口的竖线段 `(x, y_rel, h)`（文本区本地坐标）。
    pub fn test_bracket_guide_segments(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        sf: f32,
        harea_w: f32,
        view_h: f32,
    ) -> Vec<(f32, f32, f32)> {
        self.bracket_guide_segments(top_vis, off, lh, sf, harea_w, view_h)
    }

    /// 测试用：当前点亮的标尺块（`open_line`，`None` = 未点亮）。
    pub fn test_active_guide(&self) -> Option<usize> {
        self.active_guide_open
    }

    /// 测试用：逻辑行 `logical` 应命中的最内层括号块（`open_line`）。
    pub fn test_guide_for_logical_row(&self, logical: usize) -> Option<usize> {
        self.guide_pair_for_logical_row(logical)
    }

    /// 测试用：最近一次 `guide_pair_for_logical_row` 二分后回扫**实际检查**的配对步数。
    #[cfg(test)]
    pub fn test_guide_scan_steps(&self) -> usize {
        self.guide_scan_steps.get()
    }

    /// 测试用：`line` 是否起始一个可折叠候选/激活折叠（gutter 指示器口径）。
    pub fn test_has_fold_or_candidate(&self, line: usize) -> bool {
        self.has_fold_or_candidate(line)
    }

    /// 测试用：`line` 行的折叠箭头状态（与渲染同口径的单一权威，见 [`Self::fold_arrow_state`]）。
    pub fn test_fold_arrow_state(&self, line: usize) -> Option<bool> {
        self.fold_arrow_state(line)
    }

    /// 测试用：编辑器行底色快照 `(sel_bg, fold_bg, active_line_bg)`（`refresh_metrics`
    /// 里直读的三个 `colors.editor_*` token，锁「折叠/光标行比选区更贴背景」的回归）。
    pub fn test_sel_and_fold_bg(&self) -> (Hsla, Hsla, Hsla) {
        (self.sel_bg, self.fold_bg, self.active_line_bg)
    }

    /// 测试用：光标行折叠切换（gutter 点击同路径）。
    pub fn test_toggle_fold_at(&mut self, line: usize) {
        self.toggle_fold_at(line);
    }

    /// 测试用：缩进启发式块的结束行（开区间）。
    pub fn test_fold_block_end(&self, start: usize) -> Option<usize> {
        self.fold_block_end(start)
    }

    /// 测试用：垂直标尺第 `col` 列在当前横向滚动下的本地 x（px；不可见为 `None`）。
    pub fn test_ruler_x(&self, col: usize, avail_w: f32) -> Option<f32> {
        self.ruler_screen_x(col, avail_w)
    }

    /// 测试用：`row` 行内 `byte_col`（行内字节列）字符边界在文本本地的 x（px）。
    /// 与渲染/命中测试共用 `row_layout`，可用来构造"点到真实字符边界"的鼠标事件。
    pub fn test_char_boundary_x(
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
    pub fn test_hscroll_px(&self) -> f32 {
        self.hscroll_px()
    }

    /// 测试用：当前横向内容宽度（px）。断言行宽估算不低估真实字形宽 → 不折行。
    pub fn test_content_w_px(&self) -> f32 {
        self.content_w_px
    }

    /// 测试用：元素布局包围盒（`EditorElement::prepaint` 回写；未就绪 = 默认 0 大小）。
    pub fn test_element_bounds(&self) -> Bounds<Pixels> {
        self.element_bounds
    }

    /// 测试用：横向滚动容器最大可滚量（px）——底部横向滚动条量程的依据。
    pub fn test_hscroll_max_px(&self) -> f32 {
        self.h_handle.max_offset().x.as_f32()
    }

    /// 测试用：当前可视格（宽度单位）数。
    pub fn test_visible_chars(&self) -> usize {
        self.visible_chars
    }

    /// 测试用：光标所在宽度单位列（与横向 reveal 相同口径）。
    pub fn test_caret_units(&self) -> usize {
        self.buffer.unit_column(self.selection.head().min(self.buffer.len()))
    }

    /// 测试用：逻辑行 `row` 在当前横滚/可视宽下走**生产同一条**超长行窗口化路径
    /// （`render::long_line_window`，真实字体 `layout_width`）的截取结果
    /// `(行内字节区间, 左偏移 px)`；短行/未超窗/空行 → `None`（整行直送）。
    /// 供「超长行产出文本 ≈ 可视窗口（远小于整行）」的效果断言与横滚更新断言。
    #[cfg(test)]
    pub(crate) fn test_row_window_slice(
        &self,
        window: &mut Window,
        row: usize,
    ) -> Option<(Range<usize>, f32)> {
        if row >= self.buffer.line_count() {
            return None;
        }
        let text = self.buffer.line_text(row);
        let harea_w = (self.element_bounds.size.width.as_f32()
            - self.gutter_width_px.as_f32())
        .max(0.);
        let char_width = self.char_width.as_f32().max(1.);
        render::long_line_window(
            text.as_ref(),
            self.hscroll_px(),
            harea_w,
            char_width,
            &self.mono,
            self.font_size,
            window,
        )
        .map(|w| (w.bytes, w.offset_px))
    }
}
/// 行首空白字节数（空格/Tab；缩进折叠启发式用）。
fn leading_ws_len(s: &str) -> usize {
    s.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}