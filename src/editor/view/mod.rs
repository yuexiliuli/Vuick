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
    time::{Duration, Instant},
};

use gpui::{
    point, px, rgb, App, Bounds, ClipboardItem, Context, FocusHandle, HighlightStyle, Hsla,
    KeyDownEvent, ListAlignment, ListOffset, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollHandle, SharedString, Window,
};
use ropey::Rope;

use crate::config::layout::{BRACKET_GUIDE_CLOSE_TAIL_LINES, BRACKET_GUIDE_OPEN_HEAD_LINES};
use crate::editor::markdown::MarkdownDecor;
use crate::config::limits::EDITOR_VSCROLL_BOTTOM_EXTRA_LINES;
use crate::config::limits::FOLD_CANDIDATE_FULL_MAX_LINES;
use crate::editor::fold::{BracketPair, Fold, FoldMap};
use crate::editor::highlight::Highlighter;
use crate::editor::movement;
use crate::editor::rope_ext::RopeExt;
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
const LIST_WINDOW_ROWS: usize = crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS;

/// 测试探针（`cfg(test)`）：整篇 `set_text`/`set_rope_with_stats`（reset_document 级联）
/// 被调次数。app 层「WYSIWYG 写回走增量路径」回归断言：切回**编辑过的大文档**不得再
/// 走整篇 set_text（旧实现 debug ~11s；增量 `apply_replaces` 不计数）。
#[cfg(test)]
pub(crate) static CODE_EDITOR_FULL_RESET_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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
    /// 已折叠行的整行底色（`colors.accent` 派生、比选区淡，见 `refresh_metrics`）。
    fold_bg: Hsla,
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
    #[cfg(test)]
    live_replay_calls: Cell<usize>,
    /// （仅测试）`apply_replaces`（行级 diff 兜底）非空调用次数——同上，实例级确定性。
    #[cfg(test)]
    apply_replaces_calls: Cell<usize>,
    /// （仅测试）后台折叠候选/括号配对落地的**增量**次数（增量前提成立的刷新轮次）。
    #[cfg(test)]
    scan_inc_landings: Cell<usize>,
    /// （仅测试）后台折叠候选/括号配对落地的**整篇**次数（兜底/打开收尾轮次）。
    #[cfg(test)]
    scan_full_landings: Cell<usize>,
}

impl CodeEditor {
    /// 创建编辑器（空文档）。
    pub fn new(cx: &mut App) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let buffer = Buffer::default();
        let row_count = buffer.line_count().max(1);
        // 固定窗口虚拟化：ListState **恒持 `LIST_WINDOW_ROWS` 项**（与 `resync_list`
        // 的 `reset(LIST_WINDOW_ROWS)` 同一不变量）。绝不能用 `row_count` 初始化——
        // 空文档 row_count=1 → 列表只有 1 项，且 `apply_desired_to_window` 的窗口覆盖
        // 判定不会补建窗口 → 新建空标签后粘贴多行会「只有第一行能渲染」（回归测试
        // `empty_new_tab_paste_not_collapsed_to_first_row`）。
        let list_state = ListState::new(LIST_WINDOW_ROWS, ListAlignment::Top, px(16.));
        Self {
            buffer,
            selection: Selection::collapsed(0),
            undo: UndoManager::new(),
            focus_handle,
            list_state,
            h_handle: ScrollHandle::new(),
            highlighter: Highlighter::new(),
            hl_dirty: None,
            row_cache: RowCache::default(),
            md_decor: MarkdownDecor::default(),
            md_mode: false,
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
            bg: rgb(0xffffff).into(),
            fg: rgb(0x000000).into(),
            gutter_fg: rgb(0x9aa0a6).into(),
            sel_bg: rgb(0xd0d7de).into(),
            fold_bg: rgb(0xd0d7de).into(),
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
            hscroll_tail_gap: 0.0,
            // 固定窗口虚拟化：窗口首行绝对行号（初始 0）。
            window_base: 0,
            desired_scroll: std::rc::Rc::new(Cell::new(0.)),
            tab_size: 4,
            folds: FoldMap::new(),
            fold_candidates: Vec::new(),
            bracket_pairs: Vec::new(),
            fold_boundaries: Vec::new(),
            bracket_boundaries: Vec::new(),
            scan_epoch: 0,
            scan_inflight: false,
            scan_dirty_edit: false,
            scan_pending_edit: None,
            scan_multi_edit: false,
            ruler_color: rgb(0x000000).into(),
            guide_color: rgb(0x000000).into(),
            active_guide_open: None,
            active_guide_color: rgb(0x000000).into(),
            #[cfg(test)]
            live_replay_calls: Cell::new(0),
            #[cfg(test)]
            apply_replaces_calls: Cell::new(0),
            #[cfg(test)]
            scan_inc_landings: Cell::new(0),
            #[cfg(test)]
            scan_full_landings: Cell::new(0),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 设置语法高亮语言（`Some("rust")` / `None` 关闭）。
    pub fn set_language(&mut self, language: Option<&str>) {
        self.language = language.map(|s| s.to_owned());
        // Markdown 文档行渲染切到「所见即所得」滤镜（syntect 的 md 高亮不再使用，
        // 但仍保留 syntect 用于其余语言）。`.md` 由 app 统一以 "markdown" 接入。
        self.md_mode = matches!(language, Some("markdown"));
        self.highlighter.set_language(language);
        // 语言切换：高亮缓存将整段重解析，滞留的增量登记一并清除（防跨语言误用）。
        self.hl_dirty = None;
    }

    /// 设置文档文本（清空撤销/重做与选区）。
    pub fn set_text(&mut self, text: &str) {
        #[cfg(test)]
        CODE_EDITOR_FULL_RESET_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        #[cfg(test)]
        CODE_EDITOR_FULL_RESET_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // 整篇替换 = 文档在光标下被换掉：任何进行中的鼠标拖选都失去意义，一并取消。
        // （否则 `set_text` 只收拢选区、却残留 `dragging=true`，宿主切回视图后仅移动
        // 鼠标就从 anchor=0 自动扩选——见 app::tests::*wysiwyg_toggle_off* 回归。
        // 走统一生命周期钩子，与 on_mouse_up/on_blur/on_mouse_up_out 同一复位语义。）
        self.cancel_mouse_drag();
        self.hscroll_cols = 0;
        self.marked_range = None;
        self.last_edit = None;
        // 整篇替换：高亮增量登记 + 高亮缓存一并作废（下一帧整段重解析；
        // 防「新旧文档同 version=0」命中上一份文档的颜色）。
        self.hl_dirty = None;
        self.highlighter.invalidate();
        // 整篇替换：行级缓存同样显式清空（`Buffer::new` 后 version 归零，同防
        // 「新旧文档同 version=0」命中上一份文档的行静态内容）。
        self.row_cache.clear();
        // Markdown「所见即所得」滤镜缓存无需作废：缓存键含文档身份指纹（+version），
        // `Buffer::new` 后 version 归零也不会命中上一份文档的样式区间，`scan_window`
        // 首扫即指纹 miss → 按新文档自愈（M0.3；勿再手加 `md_decor.clear()` 兜底）。
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

    /// 批量增量替换（切回「预览编辑测试」整篇写回用）：按**从后往前**应用一组互不
    /// 重叠、已整行对齐的 `(字节区间, 文本)`，把文档改写为目标内容。每处走
    /// [`Self::apply_edit_synced`] 的 O(log n) Rope 编辑 + 窗口化高亮增量登记，**不整篇
    /// 重建**（区别于 `set_text` 的 `reset_document`：保留滚动/折叠/行静态缓存，只在高亮
    /// 器按编辑登记/版本跳变局部重解析）——按 diff 改写大文档只在受影响区间重解析，
    /// 典型小改动落到数百 ms 内（旧整篇 `set_text` 级联重排 debug ~11s）。
    ///
    /// 语义对齐 `set_text`：清空撤销栈（写回 = 原子替换，undo/redo 不逐个回滚本次
    /// 写回事务）、光标收到行首、无编辑事件（宿主写回后自行同步行/字符计数与右栏）。
    /// 空列表 = no-op。
    pub fn apply_replaces(&mut self, replaces: &[(Range<usize>, String)]) {
        if replaces.is_empty() {
            return;
        }
        #[cfg(test)]
        self.apply_replaces_calls
            .set(self.apply_replaces_calls.get() + 1);
        // 与 `set_text` 同语义：整篇写回 = 原子替换，撤销栈清空（旧内容快照作废）。
        self.undo.clear();
        let mut min_line_a = usize::MAX;
        let mut total_rows_delta: i64 = 0;
        for (range, text) in replaces.iter().rev() {
            // 编辑前行跨度（后→前应用：较晚偏移 hunk 不位移较早偏移 hunk 的行号）。
            let line_a = self.buffer.line_of_offset(range.start);
            let line_b = self.buffer.line_of_offset(range.end);
            total_rows_delta += text.matches('\n').count() as i64 + 1;
            total_rows_delta -= line_b.saturating_sub(line_a) as i64 + 1;
            min_line_a = min_line_a.min(line_a);
            // 缓冲写入 + 折叠同步 + 高亮增量登记（单 hunk 场景即精确值）。
            self.apply_edit_synced(range.clone(), text);
        }
        // 多 hunk：`apply_edit_synced` 的单槽登记彼此覆盖 → 归并为粗略一份；高亮器按
        // 编辑版本跳变（>1）回落有界整段重解析（O(窗口)，正确且与旧实现代价相当）。
        if replaces.len() > 1 {
            let tail = min_line_a.saturating_add(total_rows_delta.max(0) as usize);
            self.hl_dirty = Some((min_line_a, tail, total_rows_delta));
        }
        // 与 `set_text` 一致：整篇写回后光标行首、无待重放的单笔编辑轨迹。
        self.selection = Selection::collapsed(0);
        self.last_edit = None;
        self.marked_range = None;
    }

    /// WYSIWYG 编辑日志**按序重放**（切回「预览编辑测试」有改动路径，方案2）：把 live2
    /// 缴存的 `(字节区间, 新文本)` op（含撤/重做的净逆操作）按发生顺序逐条应用到当前
    /// 缓冲区。两者从同一「toggle ON 的源码 COW Rope 快照」起步、live 会话期间本缓冲区
    /// 不被改写 → op 字节坐标恒等，按序重放即得最终态。语义对齐 [`Self::set_text`] /
    /// [`Self::apply_replaces`]：清空撤销栈（整篇写回 = 原子替换）、光标收行首、无编辑事件；
    /// 每处走 [`Self::apply_edit_synced`] 的 O(log n) Rope 编辑 + 窗口化高亮增量登记，
    /// **不整篇重建**（区别于 `set_text` 的 reset_document 级联）。
    ///
    /// **护栏**（护栏成本必须 O(编辑量)+O(1)，绝不做全篇 O(n) 比对——那会把 6.4MB 大文档
    /// 的「1 字符切回」顶到几十 ms）：先在临时 COW Rope 上按序干跑一次，逐 op 校验越界 /
    /// 切中多字节字符（这些会污染缓冲区，必须先拦下），最后做 **O(1) 总长度比对**——方案2
    /// 的现实漂移模式（CRLF / BOM / 漏 op）**全部改变总字节长**，长度不等即拦截返回
    /// `false`；同长异字在「同源 Rope + 确定性 op」下不可能发生（且由 app 层全文等价测试
    /// 兜底）。干跑 + 长度栏都不通过 → **不改动缓冲区**，由调用方回退既有行级 diff 管线
    /// （`app::diff`）。
    pub fn apply_live_replay(&mut self, ops: &[(Range<usize>, gpui::SharedString)], expected: &str) -> bool {
        if ops.is_empty() {
            return expected == self.text_string();
        }
        let exp_len = expected.len();
        // —— 干跑自检（护栏）：逐 op 边界/字符边界校验 + 终态 O(1) 长度栏 ——
        let mut probe = self.buffer_text().clone();
        for (r, t) in ops {
            if r.start > r.end || r.end > probe.len() {
                return false;
            }
            if !probe.is_char_boundary(r.start) || !probe.is_char_boundary(r.end) {
                return false;
            }
            probe.replace(r.clone(), t.as_ref());
        }
        if probe.len() != exp_len {
            return false;
        }
        // —— 自检通过：落盘（与 apply_replaces 同语义的原子整篇写回）——
        self.undo.clear();
        for (range, text) in ops {
            self.apply_edit_synced(range.clone(), text.as_ref());
        }
        self.selection = Selection::collapsed(0);
        self.last_edit = None;
        self.marked_range = None;
        #[cfg(test)]
        self.live_replay_calls
            .set(self.live_replay_calls.get() + 1);
        true
    }

    /// （仅测试）本实例成功走 `apply_live_replay`（方案2 op 重放）的次数。
    #[cfg(test)]
    pub(crate) fn live_replay_calls(&self) -> usize {
        self.live_replay_calls.get()
    }

    /// （仅测试）本实例成功走 `apply_replaces`（行级 diff 兜底）的次数（非空替换）。
    #[cfg(test)]
    pub(crate) fn apply_replaces_calls(&self) -> usize {
        self.apply_replaces_calls.get()
    }

    /// （仅测试）后台折叠候选/括号配对落地次数 `(增量, 整篇)`——断言结构编辑后
    /// 走增量路径而非 O(全文) 整篇重扫。
    #[cfg(test)]
    pub(crate) fn test_scan_landings(&self) -> (usize, usize) {
        (self.scan_inc_landings.get(), self.scan_full_landings.get())
    }

    /// （仅测试）**同步**执行一轮折叠候选/括号配对刷新并落地——跳过 gpui 后台
    /// executor 调度（测试执行器对第二个及以后的 render 内 `cx.spawn` 不再轮询，
    /// 属观测器材性，生产由真实事件循环驱动），与后台刷新共用同一计算入口
    /// （[`crate::editor::fold::compute_fold_brackets`]）。返回是否走了增量路径。
    #[cfg(test)]
    pub(crate) fn test_scan_sync_land(&mut self) -> bool {
        use crate::config::limits::FOLD_CANDIDATE_FULL_MAX_LINES as SCAN_LIMIT;
        if !self.scan_dirty_edit {
            return false; // 无待刷新：不落地、不计数
        }
        self.scan_dirty_edit = false;
        let rope = self.buffer.text();
        let (folds, brackets, nfb, npb, was_incremental) =
            crate::editor::fold::compute_fold_brackets(
                |r| crate::editor::buffer::line_shared(rope, r),
                self.row_count,
                SCAN_LIMIT,
                &self.fold_candidates,
                &self.bracket_pairs,
                &self.fold_boundaries,
                &self.bracket_boundaries,
                self.scan_pending_edit,
                self.scan_multi_edit,
            );
        self.fold_candidates = folds;
        self.fold_candidates.sort_by_key(|f| f.start);
        self.bracket_pairs = brackets;
        self.bracket_pairs.sort_by_key(|p| p.open_line);
        self.fold_boundaries = nfb;
        self.bracket_boundaries = npb;
        self.scan_pending_edit = None;
        self.scan_multi_edit = false;
        if was_incremental {
            self.scan_inc_landings
                .set(self.scan_inc_landings.get() + 1);
        } else {
            self.scan_full_landings
                .set(self.scan_full_landings.get() + 1);
        }
        was_incremental
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
        (self.scroll_item_count() as f32 * self.line_height.as_f32().max(1.)
            - self.list_state.viewport_bounds().size.height.as_f32())
            .max(0.)
    }

    /// 纵向量程总高（px）。像素路径 = 条目总数 × 行高（含底部预留空行；确定性），
    /// 诊断/测试用。
    pub(crate) fn vertical_total_px(&self) -> f32 {
        self.scroll_item_count() as f32 * self.line_height.as_f32().max(1.)
    }

    /// 测试：暴露 ListState（共享 Rc）——测试拖拽路径 / 量程对账。
    #[cfg(test)]
    pub(crate) fn test_list_state(&self) -> ListState {
        self.list_state.clone()
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

    /// 测试：对指定逻辑行走真实的 `build_rows` 取数路径，返回该行**最终样式段**
    /// （Markdown 滤镜或 syntect，选区合并前）。用于验证 Markdown WYSIWYG 接线
    /// （`md_mode → scan_window → build_rows → merge_highlights`）。
    #[cfg(test)]
    pub(crate) fn test_line_styles(
        &mut self,
        window: &mut Window,
        logical_row: usize,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if logical_row >= self.row_count || logical_row >= self.buffer.line_count() {
            return Vec::new();
        }
        self.test_warm_highlight(logical_row, logical_row);
        self.build_rows(window, logical_row, logical_row, false)
            .into_iter()
            .next()
            .map(|rd| rd.content.highlights.clone())
            .unwrap_or_default()
    }

    /// 测试：走真实 `build_rows` 路径拍一行窗口快照（文本 + 渲染消费的合并后样式
    /// 段），供「无编辑滚动多帧后渲染不变 / 编辑后缓存失效」回归断言（M5-B2-3）。
    #[cfg(test)]
    #[allow(clippy::type_complexity)] // 测试专用（文本 + 样式段）复合快照类型。
    pub(crate) fn test_build_rows_snapshot(
        &mut self,
        window: &mut Window,
        lo: usize,
        hi: usize,
    ) -> Vec<(String, Vec<(Range<usize>, HighlightStyle)>)> {
        self.test_warm_highlight(lo, hi);
        self.build_rows(window, lo, hi, false)
            .into_iter()
            .map(|rd| (rd.content.text.to_string(), rd.highlights().to_vec()))
            .collect()
    }

    /// 测试：**不预热**直接走 `build_rows` 的取数路径（每行最终样式段）。
    /// 方案 A 后首屏（空缓存）同步上色、不再延后 + `request_animation_frame`，故在
    /// 帧外直调 `build_rows` 安全——用「新开文档首屏」验证**首帧即有着色**（大 .rs
    /// 打开不再先纯文本、后补色）。仅适用于「新开文档首屏」窗口（小缺口），勿用于
    /// 滚动大缺口窗口（那帧仍会延后 + RAF，帧外直调会 debug_assert 炸）。
    #[cfg(test)]
    #[allow(clippy::type_complexity)] // 测试专用（文本 + 样式段）复合快照类型。
    pub(crate) fn test_build_rows_first_frame(
        &mut self,
        window: &mut Window,
        lo: usize,
        hi: usize,
    ) -> Vec<(String, Vec<(Range<usize>, HighlightStyle)>)> {
        self.build_rows(window, lo, hi, false)
            .into_iter()
            .map(|rd| (rd.content.text.to_string(), rd.highlights().to_vec()))
            .collect()
    }

    /// 测试：直接调 `build_rows` 前先预热高亮缓存（仅非 Markdown 的 syntect 路径需要）。
    /// 首屏（方案 A）虽已同步上色，但**滚动大缺口**帧仍走「延后上色」且在 `build_rows`
    /// 里 `request_animation_frame`（只能在真实帧内调用）——测试在帧外直调 build_rows 会
    /// debug_assert 炸。这里绕过高亮器把 pending/settle 契约**帧外安全**执行完（与
    /// `build_rows` 的快路径/延后路径同一组 API），之后 `build_rows` 自然命中缓存，不再
    /// 走延后分支。缓存已覆盖则幂等（命中/小扩展）。Markdown（`md_mode`）走
    /// `scan_window`，不经过该延后分支，无需预热。
    #[cfg(test)]
    fn test_warm_highlight(&mut self, lo: usize, hi: usize) {
        if self.md_mode {
            return;
        }
        self.highlighter.set_dirty(self.hl_dirty.take());
        let _ = self.highlighter.accept_deferred(lo, hi, HL_LOOKBACK_LINES);
        let _ = self.highlighter.settle_pending(&self.buffer);
        let _ = self
            .highlighter
            .highlight_window(&self.buffer, lo, hi, HL_LOOKBACK_LINES)
            .len();
    }

    /// 测试：行缓存命中计数 / 条目数（滚动窗口内断言缓存复用、编辑后失效）。
    #[cfg(test)]
    pub(crate) fn test_row_cache(&self) -> (usize, usize) {
        self.row_cache.stats()
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
        // 程序化定位（搜索跳转/侧边栏目录）也发光标变化事件，状态栏跟随。
        cx.emit(EditorEvent::SelectionChanged);
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

    pub fn buffer_text(&self) -> &ropey::Rope {
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

    /// 设置元素包围盒（备用宽度来源）。内嵌宿主（数学编辑卡等浮层）在**首次渲染前**
    /// 用它给编辑器一个确定的可视宽——`sync_render_state` 的 `avail_w` 在
    /// `element_bounds` 未就绪（元素尚未 prepaint）时回退窗口宽，浮层内嵌编辑器首帧
    /// 会按窗口宽算 `content_w_px`/`visible_chars`/横向量程（长内容尾部不可达）。
    /// prepaint 每帧会用真实布局包围盒覆写本字段，故该值只影响首帧、随后自然收敛。
    pub fn set_element_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.element_bounds = bounds;
    }

    /// 设置横向内容的**尾部留白**（px；`0.` 关闭 = 默认）。仅当最宽行溢出可视区时，
    /// `compute_content_width` 会在内容宽上追加该值——滚到底后末尾字符与右裁切缘之间
    /// 留出空隙（避免尾部字符贴缘被裁/难以点中）。数学编辑卡长公式用；主编辑器不动。
    pub fn set_hscroll_tail_gap(&mut self, px: f32) {
        self.hscroll_tail_gap = px.max(0.);
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
        // 第 3 波迁移：typography/语义色单一来自 `crate::ui::theme`（不再读
        // gpui-component 主题 token）。等宽字体族与行高比例、缺省正文字号都由
        // `Theme::typography` 收敛（统一缩放架构，见 docs/ui-migration.md）。
        let theme = crate::ui::theme::Theme::global(cx);
        let typo = &theme.typography;
        let colors = theme.colors;
        let dark = theme.is_dark();

        let mono = typo.mono.clone();
        // 字号覆盖：应用 Ctrl+=/- 调节（默认 None = 编辑器等宽缺省字号/行高，保留
        // gpui-component `mono_md` 的 13/20 既有默认；应用侧恒有 override=14）。
        let (font_size, line_height) = match self.font_override {
            Some(px_override) => (
                px(px_override),
                px(px_override * typo.editor_line_height_ratio),
            ),
            None => (
                px(typo.editor_default_size),
                px(typo.editor_default_size * typo.editor_line_height_ratio),
            ),
        };

        let text_system = window.text_system();
        let font = gpui::font(mono.clone());
        let char_width = text_system
            .ch_advance(text_system.resolve_font(&font), font_size)
            .unwrap_or(px(font_size.as_f32() * 0.6));

        // 字号缩放（Ctrl+滚轮 / Ctrl+=/-）会同比改变行高：把「用户滚动目标」按行高比
        // 换算，把视口顶部的内容点锚定住（同 PDF 的视口锚定）——否则放大/缩小的瞬间
        // 行高变了但滚动 px 没变，正文会“跳”（窗口也随之滚动）。首次布局（行高 0）
        // 与行高不变时不动作。横向以 h_handle 的像素偏移为准，天然保持像素锚点。
        let old_lh = self.line_height.as_f32();
        let new_lh = line_height.as_f32();
        if old_lh > 0. && new_lh > 0. && (new_lh - old_lh).abs() >= 0.01 {
            let d = self.desired_scroll.get();
            self.desired_scroll.set(d * (new_lh / old_lh));
        }

        self.mono = mono;
        self.font_size = font_size;
        self.line_height = line_height;
        self.char_width = char_width;
        // 编辑区画布底色：暗色取 config `EDITOR_BG_DARK`（与欢迎页/预览同一内容底色）。
        self.bg = crate::ui::shell::content_bg(dark, colors.bg.into());
        self.fg = colors.fg.into();
        self.gutter_fg = colors.fg_muted.into();
        // 垂直标尺线：次要前景（与行号同源、已验证明暗主题都可见；不用 `border`——
        // 主题默认可能为透明）。细微参考线不刺眼。
        self.ruler_color = colors.fg_muted.opacity(0.55).into();
        // 括号标尺竖线：更淡，纯视觉参考不干扰阅读。
        self.guide_color = colors.fg_muted.opacity(0.4).into();
        // 点亮竖线（guide click highlight）：主题强调蓝 `colors.accent`（用户口径，勿抽 config）。
        self.active_guide_color = colors.accent.opacity(0.9).into();
        // 光标不能用 colors.accent：Light 主题 accent = #005fb8 蓝，白底上对比足够，
        // 但沿用既有口径用前景墨色，明/暗主题下都一定可见。
        self.caret_color = colors.fg.into();
        // 选区底色用主题强调蓝（与括号点亮、标题/菜单高亮同源单点出口，改调色板只动 theme）。
        self.sel_bg = colors.accent.opacity(if dark { 0.4 } else { 0.28 }).into();
        // 已折叠行的整行底色：与选区同源 accent，但 alpha 明显更淡（dark≈0.16 /
        // light≈0.10，约为选区 0.4/0.28 的一半不到）——一眼认出折叠位置、又绝不
        // 喧宾夺主，明/暗两主题下都肉眼可辨（锁「比选区淡」的回归见 tests/view.rs）。
        self.fold_bg = colors.accent.opacity(if dark { 0.16 } else { 0.10 }).into();
        self.metrics_ready = true;

        let digits = self.buffer.line_count().max(1).to_string().len();
        self.gutter_chars = digits.max(3);
        // 总宽 = 数字格 + 左右内边距 + 固定箭头槽（槽替代旧 +4 缓冲：数字格因此
        // 恒 = gutter_chars×字宽，行号右缘锚点固定，不会被 overflow_hidden 裁槽）。
        self.gutter_width_px = px(
            self.gutter_chars as f32 * char_width.as_f32()
                + GUTTER_LEFT_PAD_PX
                + GUTTER_RIGHT_PAD_PX
                + FOLD_ARROW_SLOT_PX,
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
        self.window_base = 0;
        self.desired_scroll.set(0.);
        // 整段重挂：滞留的高亮增量登记作废（撤销/重做后走整段重解析）。
        self.hl_dirty = None;
        // 折叠投影总行数跟随文档行数（区间越界由 FoldMap 侧裁剪）。
        self.folds.set_total(new_n);
        if defer_scans {
            // 撤销/重做路径：增量描述由 undo/redo 先行登记（`scan_pending_edit` /
            // `scan_multi_edit`），不在这里整篇重扫（大文档 ~280ms UI 卡顿），
            // 交给下一帧后台刷新做增量/整篇重算。
            self.scan_dirty_edit = true;
        } else {
            // 打开/整篇替换路径：同步重扫一次（内容全新，首屏即就绪）。
            self.recompute_fold_brackets_full();
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
        // WY_TRACE breakdown：编辑主链「缓冲应用」段（O(log n) Rope 编辑 + 高亮增量登记）。
        let _t_apply = crate::prof::begin();
        // 编辑会改变块结构/跨度：清除熄灭点亮标尺（语义见 guide click highlight）。
        self.active_guide_open = None;
        // 先取被替换区间的**编辑前**行跨度（折叠区间同步需要，编辑后行号已变）。
        let prev_rows = self.row_count;
        let line_a = self.buffer.line_of_offset(range.start);
        let line_b = self.buffer.line_of_offset(range.end);
        let edit = self.buffer.apply_edit(range, text);
        self.row_count = self.buffer.line_count().max(1);
        // 高亮增量登记：编辑前最小受影响行 + 编辑后首个未变尾部行 + 净行增量。
        // （一次性消费；`Highlighter` 据此从 `line_a - 回看` 局部重解析并收敛早退。）
        let insert_rows = text.matches('\n').count() + 1;
        let removed_rows = line_b.saturating_sub(line_a) + 1;
        self.hl_dirty = Some((
            line_a,
            line_a.saturating_add(insert_rows),
            insert_rows as i64 - removed_rows as i64,
        ));
        // 折叠同步：编辑进折叠体 → 自动展开；折叠后整体平移；锚被删 → 丢弃。
        self.folds.sync_edit(line_a, line_b, text, self.row_count);
        // 折叠候选 / 括号配对：**不再同步整篇重扫**（大文档逐键卡顿）。行数变了或
        // 文本含括号字符 → 登记**单个**受影响行段描述，由下一帧 render 挂起一轮
        // 后台**增量**刷新（受影响包络局部重扫 + 尾部行号平移，见 `fold.rs`）。
        let has_bracket =
            text.bytes().any(|b| matches!(b, b'{' | b'}' | b'(' | b')' | b'[' | b']'))
                || edit
                    .old_text
                    .bytes()
                    .any(|b| matches!(b, b'{' | b'}' | b'(' | b')' | b'[' | b']'));
        if self.row_count != prev_rows || has_bracket {
            // min_stop = 新坐标下首个未受影响行（替换文本占 `insert_rows` 行）。
            self.mark_scan_dirty_incremental(
                line_a,
                line_a.saturating_add(insert_rows),
                insert_rows as i64 - removed_rows as i64,
            );
        }
        crate::prof::end("ed.apply_sync", _t_apply);
        edit
    }

    /// 登记一次「折叠候选 / 括号配对后台增量刷新」的受影响行段：
    /// - 自上落地后仍只有**一个**未落地编辑段 → 记入 [`Self::scan_pending_edit`]；
    /// - 已有未落地段（本次是第二个或更多）→ 置 [`Self::scan_multi_edit`]（增量
    ///   前提不成立，下一轮退整篇重扫兜底），并始终打 [`Self::scan_dirty_edit`]。
    ///
    /// `line_a` = 编辑首行（编辑前坐标），`min_stop` = 新坐标下首个未受影响行
    /// （受影响包络闭区间上界），`delta` = 净行增量（新行数 − 旧行数）。
    fn mark_scan_dirty_incremental(&mut self, line_a: usize, min_stop: usize, delta: i64) {
        self.scan_dirty_edit = true;
        if self.scan_multi_edit || self.scan_pending_edit.is_some() {
            self.scan_multi_edit = true;
        } else {
            self.scan_pending_edit = Some((line_a, min_stop, delta));
        }
    }

    /// 由可视宽推导「可见宽度单位格数」：真实可视格 + 8 的估算余量（多余量供折行/
    /// 绘制安全，`reveal_caret` 已先减掉再算右缘）。**不再设 80 格下限**——该下限是
    /// 「按窗口切片渲染文本」时代的遗留：窄视口（数学编辑卡 ~57 格）会被虚高到 80，
    /// `reveal_caret` 的右缘按 80 算 → 光标/新输入停在真实右缘之外、被 `overflow_hidden`
    /// 裁掉看不见。主编辑器视口 ≫80 格，该下限从不生效、行为不变。
    fn visible_chars_from_width(&self, width_px: f32) -> usize {
        let cw = self.char_width.as_f32().max(1.);
        (((width_px - self.gutter_width_px.as_f32()).max(0.) / cw).ceil() as usize)
            .saturating_add(8)
            .max(1)
    }

    // -----------------------------------------------------------------
    // 行数据
    // -----------------------------------------------------------------

    /// 光标 x：按帧状态现算（真实字形宽度，CJK 双宽不再按等宽列近似）→ 光标停在
    /// 真实字符边界上（横向滚动交给外层容器，行内文本从 x=0 全宽绘制，这里只算文本
    /// 本地 x）。不入缓存——光标位置每帧都在变，且空行分支与普通分支值一致
    /// （空行 head==line_start==line_end → byte_col=0 → x=0）。
    fn row_caret(&self, window: &mut Window, row: usize, show_caret: bool) -> Option<f32> {
        if !show_caret {
            return None;
        }
        let line_start = self.buffer.line_start(row);
        let line_end = self.buffer.line_end(row);
        let head = self.selection.head();
        if head < line_start || head > line_end {
            return None;
        }
        let byte_col = head - line_start;
        Some(
            self.row_layout(window, row)
                .map(|layout| layout.x_for_index(byte_col).as_f32())
                .unwrap_or_else(|| byte_col as f32 * self.char_width.as_f32().max(1.)),
        )
    }

    /// 折叠首行指示：按帧状态现叠（折叠动作不失效内容缓存）。`Some(x)` = 折叠首行的
    /// 「…」指示器 x（= 行文本整形宽；独立元素渲染，不进 StyledText，不污染行内测量）。
    fn row_folded(&self, window: &mut Window, row: usize) -> Option<f32> {
        if self.folds.fold_starting_at(row).is_some() {
            Some(
                self.row_layout(window, row)
                    .map(|layout| layout.width().as_f32())
                    .unwrap_or_else(|| {
                        self.buffer.line_text(row).chars().count() as f32
                            * self.char_width.as_f32().max(1.)
                    }),
            )
        } else {
            None
        }
    }

    /// 选区合并层（动态，不入缓存）：选区坍缩返回 `None`（直接用缓存的无选区合并
    /// 结果，滚动帧零分配零合并）；选区活动返回本帧动态合并——`merge_highlights`
    /// 输入用缓存的 raw runs，主题底色 `sel_bg` 用当前值。选区/IME 组合变化绝不被
    /// 旧缓存吞掉：选中底色永远按 **当前** 选区现算（见 `merge_highlights` 的非重叠
    /// 语义，选区与语法段被合并成连续段，run 总量不超订）。
    fn row_highlight_override(
        &self,
        row: usize,
        content: &RowContent,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        if self.selection.is_collapsed() {
            return None;
        }
        let line_start = self.buffer.line_start(row);
        let line_end = self.buffer.line_end(row);
        let sel = self.selection.range();
        let lo = sel.start.max(line_start);
        let hi = sel.end.min(line_end);
        let sel_range = if hi > lo {
            Some((lo - line_start)..(hi - line_start))
        } else {
            None
        };
        Some(merge_highlights(&content.hl_styles, sel_range, self.sel_bg))
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
        // WY_TRACE breakdown：整窗行数据构建段（含语法/滤镜高亮解析 + 行内样式合并）。
        let _t_br = crate::prof::begin();
        // 整窗一次批量解析，快照成 owned（`.to_vec()` 结束内部的 `&mut self`
        // 借用，否则后续行物化/缓存写与高亮窗口的借用冲突）。
        // 返回切片约定：第 0 项 = `lo_l` 行（两级缓存均统一对齐到请求窗口）。
        // Markdown 文档走「所见即所得」滤镜（记号淡化 + 正文富样式）；其余语言走
        // syntect 语法高亮（`(Range, Hsla)` → 仅色 `HighlightStyle`）。
        // `deferred`：延后着色帧（大缺口/上移出缓存，本帧空样式渲染、下一帧 settle
        // 补色）——此帧**跳过缓存读写**，避免把占位空色冻结进缓存（否则颜色永久缺失）。
        let mut deferred = false;
        let hl_lines: Vec<Vec<(Range<usize>, HighlightStyle)>> = if self.md_mode {
            self.md_decor
                .scan_window(&self.buffer, lo_l, hi_l, HL_LOOKBACK_LINES)
                .to_vec()
        } else {
            // 把最近编辑的增量登记喂给高亮器（局部重解析 + 收敛早退），只取走一次。
            self.highlighter.set_dirty(self.hl_dirty.take());
            // 大文档拖滚动条：窗口未覆盖且需有界重锚时，**本帧先用空样式渲染**
            // （syntect 解析延后：帧廉价 → 滑块/内容紧跟鼠标）；滚动停住（相邻帧
            // 窗口一致）由 `settle_pending` 补一次有界重锚，颜色随后落位。
            let prev_pending = self.highlighter.pending();
            let quick = self.highlighter.accept_deferred(lo_l, hi_l, HL_LOOKBACK_LINES);
            let settle = !quick
                && prev_pending == Some((lo_l, hi_l, HL_LOOKBACK_LINES));
            if quick || settle {
                if settle {
                    self.highlighter.settle_pending(&self.buffer);
                }
                self.highlighter
                    .highlight_window(&self.buffer, lo_l, hi_l, HL_LOOKBACK_LINES)
                    .to_vec()
                    .into_iter()
                    .map(|runs| {
                        runs.into_iter()
                            .map(|(r, c)| {
                                (
                                    r,
                                    HighlightStyle {
                                        color: Some(c),
                                        ..Default::default()
                                    },
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            } else {
                // 延后：本窗空样式（文本/光标/选区仍正确，仅颜色下一帧补齐）。
                // 预约下一帧：滚动停住（窗口不再变）让 `settle` 补一次有界重锚上色；
                // 覆盖后不再预约，自动静止（无空转帧）。
                deferred = true;
                window.request_animation_frame();
                std::iter::repeat_n(Vec::new(), hi_l - lo_l + 1).collect()
            }
        };
        // 输出按**视觉行**对齐（第 0 项 = 视觉 `lo` 行）：每个视觉行映射一个逻辑行。
        let version = self.buffer.version();
        let mut out: Vec<RowData> = Vec::with_capacity(hi - lo + 1);
        for v in lo..=hi {
            let logical = self.folds.logical_row(v);
            let styles: &[(Range<usize>, HighlightStyle)] = &hl_lines[logical - lo_l];
            // 静态内容层（M5-B2-3 行级缓存）：命中缓存（键 + 本帧新鲜 runs 逐位比对）
            // → `Rc` 浅复用；未命中 → 物化新进窗口的行并入缓存（`deferred` 帧除外：
            // 占位空色不许入缓存）。内容只取决于 `(buffer.version(), 逻辑行)`，折叠/
            // 选区/光标是下面动态层，不影响命中判定。
            let content = if deferred {
                Rc::new(RowContent::materialize(&self.buffer, logical, styles))
            } else {
                match self.row_cache.probe(version, logical, styles) {
                    Some(c) => c,
                    None => {
                        let c = Rc::new(RowContent::materialize(&self.buffer, logical, styles));
                        self.row_cache.put(version, logical, c.clone());
                        c
                    }
                }
            };
            out.push(RowData {
                highlight_override: self.row_highlight_override(logical, &content),
                caret: self.row_caret(window, logical, show_caret),
                folded: self.row_folded(window, logical),
                content,
            });
        }
        crate::prof::end("ed.build_rows", _t_br);
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
    // 纵向滚动：像素级确定性换算（固定窗口虚拟化的像素路径，恒启用）
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
        let raw = crate::ui::scrollbar::scrollbar_width().as_f32()
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

    /// 视口顶部的**源码行**（0 基；绝对视觉行经折叠映射回逻辑行）。编辑区⇄预览
    /// 滚动同步用——同步单位 = 源码行，两侧都以它对齐。
    pub(crate) fn viewport_top_line(&self) -> usize {
        let (vis, _) = self.abs_scroll_top();
        self.folds.logical_row(vis)
    }

    /// 滚动到目标**源码行**置于视口顶部（统一行高模型；逻辑行经折叠映射成视觉行，
    /// 再换算像素交给 `scroll_to_px`——夹到量程内，底部预留让末行停在横向条上方）。
    pub(crate) fn scroll_to_line(&mut self, line: usize) {
        let lh = self.line_height.as_f32().max(1.);
        let vis = self.folds.visual_row(line);
        self.scroll_to_px(vis as f32 * lh);
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
        // WY_TRACE：滚轮竖向滚动登记一次 scroll 操作；编辑已在 insert_edit 起会话，
        // 不重复起（避免滚轮同帧把编辑会话误标成 scroll）。
        if !crate::prof::op_pending() {
            crate::prof::op_start("ed_scroll");
        }
        self.scroll_to_px(self.scroll_top_px() - dy);
    }

    /// 把目标**逻辑行**滚进可视区（统一行高模型；先经折叠映射成视觉行，再算像素：
    /// 折叠隐藏行的逻辑行会塌缩到折叠首行的视觉位置）。可视高度只扣除底部**横向
    /// 滚动条**（真正的非文本区），保证被 reveal 的行不会贴到横向滚动条之下。
    ///
    /// ⚠️ 这里**不能**把整段底部预留（`vscroll_bottom_pad_px` = 滚动条 16px + 8 行
    /// 呼吸区）当「无需滚动区」从可视高里扣——那是**量程**语义（滚到底让末行停在
    /// 滚动条之上，由 `reach_px`/`pad_row_count` 承担）。矮视口（数学编辑卡内嵌
    /// 编辑器高 200px、预留 ≈180px，见 P10 实测）全量扣预留 → `rows_visible`=1 →
    /// 点击任意非首行都触发 `scroll_to_px` 把该行顶到顶部（用户实测「点击某行直接
    /// 跳成第一行、内容滚下去」）。按真实可见行数算，点击任何可视行都不滚动。
    fn reveal_row(&mut self, row: usize) {
        let lh = self.line_height.as_f32().max(1.);
        let vis = self.folds.visual_row(row);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        let scrollbar = crate::ui::scrollbar::scrollbar_width().as_f32();
        let rows_visible = ((view - scrollbar) / lh).floor().max(1.) as usize;
        let cur_top = (self.scroll_top_px() / lh).floor() as usize;
        if vis < cur_top {
            self.scroll_to_px(vis as f32 * lh);
        } else if vis >= cur_top + rows_visible {
            let target = vis.saturating_add(1).saturating_sub(rows_visible);
            self.scroll_to_px(target as f32 * lh);
        }
    }

    fn reveal_caret(&mut self) {
        // WY_TRACE breakdown：编辑/移动后的「把光标滚进可视区」段（纵向 + 横向）。
        let _t_reveal = crate::prof::begin();
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
            // 左缘触发（col 在视口左缘 margin 内，含已滚到最左、col≥margin 的情形）：
            // 目标 = col - margin，让光标保留 margin 格左边上下文；col<margin 时为
            // 0，不产生无意义的向右滚动——否则空行/行首打字会把内容逐格左移、到
            // margin 边界又走右缘分支 snap 回 0，形成「界面往右弹又弹回」。
            // 右缘触发（col ≥ hscroll+right_edge）：目标 = col - right_edge。
            let target = if col < self.hscroll_cols + margin {
                col.saturating_sub(margin)
            } else {
                col.saturating_sub(right_edge)
            };
            self.set_hscroll(target);
        }
        crate::prof::end("ed.reveal", _t_reveal);
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
    /// 量程 = 容器**实测** `h_handle.max_offset().x`（= `content_w_px` − 文本区可视宽，
    /// 与横向滚动条读取同一真源；上一帧布局值，下一帧 read-back 对齐，同竖向量程
    /// `reach_px` 的确定性口径）。亚格增量（<半格）直接丢弃：部分鼠标滚轮带轻微
    /// 水平分量/倾斜轮噪声，若每帧量化成整格会令内容（行号右缘）左右跳一格、
    /// 看起来像在“晃”。
    fn scroll_horizontal_px(&mut self, dx_px: f32) {
        let cw = self.char_width.as_f32().max(1.);
        if dx_px.abs() < cw * 0.5 {
            return;
        }
        // WY_TRACE：横向滚动同竖向；有在途会话（编辑/竖向滚轮同帧）不重复起。
        if !crate::prof::op_pending() {
            crate::prof::op_start("ed_scroll");
        }
        let max_px = self.h_handle.max_offset().x.as_f32().max(0.);
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

    /// 测试用：`line` 行的折叠箭头状态（与渲染同口径的单一权威，见 [`Self::fold_arrow_state`]）。
    pub(crate) fn test_fold_arrow_state(&self, line: usize) -> Option<bool> {
        self.fold_arrow_state(line)
    }

    /// 测试用：选区底色与折叠行底色快照 `(sel_bg, fold_bg)`（`refresh_metrics` 里
    /// 同一 `colors.accent` 导出的两档 alpha，锁「折叠比选区淡」的回归）。
    pub(crate) fn test_sel_and_fold_bg(&self) -> (Hsla, Hsla) {
        (self.sel_bg, self.fold_bg)
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

    /// 测试用：元素布局包围盒（`EditorElement::prepaint` 回写；未就绪 = 默认 0 大小）。
    pub(crate) fn test_element_bounds(&self) -> Bounds<Pixels> {
        self.element_bounds
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
        // 失焦 = 编辑器离开活动态：进行中的鼠标拖选失去意义，随生命周期一并复位。
        // 拖选只依赖「按下 → 移动」；松开/失焦/切走任一中断拖拽闭环都不能留
        // `dragging` 残留（收敛到统一生命周期钩子，见 [`Self::cancel_mouse_drag`]）。
        self.cancel_mouse_drag();
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
                self.cancel_mouse_drag();
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
        // 鼠标点击移动光标（宿主刷新状态栏光标行列等派生数据）。
        cx.emit(EditorEvent::SelectionChanged);
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
        // 拖拽扩选过程也实时刷新（状态栏选区头跟随鼠标）。
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 松开点落在元素 hitbox 外 / 顶层叠层 / 窗口外：元素级 `on_mouse_up`（受
    /// `hitbox.is_hovered` 门控）不会派发，本钩子在 Capture 阶段无条件收口拖选态
    /// （对齐本仓 scrollbar / video / image / wysiwyg 的 `on_mouse_up_out` 拖拽清态惯例）。
    fn on_mouse_up_out(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 生命周期钩子：编辑器即将「失焦 / 隐藏 / 宿主切换」时统一复位鼠标拖选态
    /// （仅清 `dragging`，不动光标 / 选区，不改变交互模型、不改键位）。
    ///
    /// 拖选态生命周期：`on_mouse_down` 置位 → `on_mouse_up` / `on_mouse_up_out` /
    /// `on_blur` / 整篇写回（`reset_document`）/ 宿主切离 任一终点复位，散落清位全部
    /// 收敛到本钩子（宿主在编辑区被切离 / 隐藏的正确时机触发），取代历史 P4 的
    /// 「调用点散落清位」补丁。
    ///
    /// 背景（P4）：点菜单项时 GPUI 把鼠标按下按 `hitbox.is_hovered` 漏进编辑区（不按
    /// z-order 裁掉被菜单遮挡的区间）置 `dragging=true`；对应的 MouseUp 又被菜单项
    /// `activate` 的 `stop_propagation` 截断派发、到不了 `on_mouse_up` → `dragging`
    /// 残留，切回后仅移动鼠标就从 anchor 自动扩选。
    pub(crate) fn cancel_mouse_drag(&mut self) {
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
                }
                "]" => {
                    self.unfold_under_caret(cx);
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
        // 方向键/Home/End/翻页等移动光标（宿主刷新状态栏光标行列等派生数据）。
        cx.emit(EditorEvent::SelectionChanged);
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
///
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

    /// 以 `line` 为起点的折叠候选（gutter 指示器/点击折叠的区间来源）。
    ///
    /// `fold_candidates` 已按 `start` 升序（见 `recompute_fold_brackets_full` / 后台
    /// `refresh_fold_brackets` 落地处）→ 二分 O(log n)，避免 gutter 每帧对每行做
    /// O(候选数) 线性扫描（大文档数列 ~1k+ 候选 × 每帧 40 行 = 每键几 ms 白烧）。
    fn candidate_starting_at(&self, line: usize) -> Option<Fold> {
        let i = self.fold_candidates.partition_point(|f| f.start < line);
        self.fold_candidates.get(i).filter(|f| f.start == line).copied()
    }

    /// `line` 行的折叠指示箭头状态（渲染与测试共用的**单一权威**）：
    /// - `None` = 不画箭头；
    /// - `Some(true)` = 已激活折叠首行（隐藏体收起）→ 右向「▸」；
    /// - `Some(false)` = 仅折叠候选起点 → 下向「▾」。
    fn fold_arrow_state(&self, line: usize) -> Option<bool> {
        if self.folds.fold_starting_at(line).is_some() {
            Some(true)
        } else if self.candidate_starting_at(line).is_some() {
            Some(false)
        } else {
            None
        }
    }

    /// `line` 是否有「可折叠」标记：已激活折叠首行，或折叠候选起点。
    fn has_fold_or_candidate(&self, line: usize) -> bool {
        self.fold_arrow_state(line).is_some()
    }

    /// 打开/整篇替换路径的**同步**整篇重扫（内容全新，首屏即就绪，测试依赖）：
    /// 折叠候选 + 括号标尺配对一次算齐，并落地各自的「栈清空边界」
    /// （`fold_boundaries` / `bracket_boundaries`），供后续编辑的后台**增量**重算复用。
    /// ≤ 阈值的整篇扫，超大文档清空（改为按需折叠）。
    ///
    /// 落地后按 `start` / `open_line` 升序排序——gutter/标尺查询走二分
    /// （`candidate_starting_at` / `bracket_guide_segments_flagged`），
    /// 原始扫描是 LIFO 收栈序非单调，排序只改内部存储次序、集合与语义不变。
    fn recompute_fold_brackets_full(&mut self) {
        if self.row_count > FOLD_CANDIDATE_FULL_MAX_LINES {
            self.fold_candidates.clear();
            self.bracket_pairs.clear();
            self.fold_boundaries.clear();
            self.bracket_boundaries.clear();
        } else {
            let rope = self.buffer.text();
            let (folds, pairs, fb, pb) = crate::editor::fold::full_fold_brackets(
                |r| crate::editor::buffer::line_shared(rope, r),
                self.row_count,
            );
            self.fold_candidates = folds;
            self.fold_candidates.sort_by_key(|f| f.start);
            self.bracket_pairs = pairs;
            self.bracket_pairs.sort_by_key(|p| p.open_line);
            self.fold_boundaries = fb;
            self.bracket_boundaries = pb;
        }
        self.scan_pending_edit = None;
        self.scan_multi_edit = false;
    }

    /// 在**后台线程**重算折叠候选 + 括号标尺配对并落地（单飞 + 版本号取消）。
    ///
    /// - 打开收尾（file_ops 末块落定）与编辑后的下一帧 render 都会调用；
    /// - 编辑走**增量**：在受影响行段包络 `[B, stop)` 上局部重扫 + 未变段尾部行号
    ///   平移（见 [`crate::editor::fold::incremental_fold_brackets`]），
    ///   只读 Rope O(1) COW 快照在 `background_executor` 上跑纯函数；多编辑段累积
    ///   （`scan_multi_edit`）或增量前提缺失时退化为整篇扫描兜底；
    /// - 结果回来时若期间又有请求（`scan_epoch` 已变）则丢弃旧结果、用最新快照
    ///   重跑一轮；
    /// - 不阻塞 UI 线程：打开 160k 行文档的 ~280ms 全量扫描不再卡界面，输入含括号
    ///   字符也不再逐键整篇重扫（结构编辑事件的 O(全文) 双扫改 O(受影响段)）。
    pub(crate) fn refresh_fold_brackets(&mut self, cx: &mut Context<Self>) {
        use crate::config::limits::FOLD_CANDIDATE_FULL_MAX_LINES as SCAN_LIMIT;
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
        if self.scan_inflight {
            return; // 已有任务在跑：其末尾会因 epoch 已变而用最新快照重跑一轮。
        }
        self.scan_inflight = true;
        cx.spawn(async move |this, cx| {            loop {
                // 快照：最新文档 Rope（O(1) COW）+ 上次落地结果（增量对拍基）+ 增量描述。
                let snap = this.update(cx, |this, _| {
                    (
                        this.scan_epoch,
                        this.row_count,
                        this.buffer.text().clone(),
                        this.fold_candidates.clone(),
                        this.bracket_pairs.clone(),
                        this.fold_boundaries.clone(),
                        this.bracket_boundaries.clone(),
                        this.scan_pending_edit,
                        this.scan_multi_edit,
                    )
                });
                let Ok((epoch, row_count, rope, folds, pairs, fb, pb, pending, multi)) = snap
                else {
                    return;
                };
                let (folds, brackets, nfb, npb, was_incremental) = cx
                    .background_executor()
                    .spawn(async move {
                        let line = |r: usize| crate::editor::buffer::line_shared(&rope, r);
                        // 增量：受影响包络局部重扫 + 尾部平移；前提缺失 → 整篇重扫兜底
                        // （`was_incremental` 同时是测试探针，见 `compute_fold_brackets`）。
                        crate::editor::fold::compute_fold_brackets(
                            line,
                            row_count,
                            SCAN_LIMIT,
                            &folds,
                            &pairs,
                            &fb,
                            &pb,
                            pending,
                            multi,
                        )
                    })
                    .await;
                // 非测试构建：`was_incremental` 仅供测试探针统计落地路径，落为 `_`。
                #[cfg(not(test))]
                let _ = was_incremental;
                let done = this
                    .update(cx, |this, cx| {
                        if epoch == this.scan_epoch {
                            #[cfg(test)]
                            {
                                if was_incremental {
                                    this.scan_inc_landings.set(this.scan_inc_landings.get() + 1);
                                } else {
                                    this.scan_full_landings
                                        .set(this.scan_full_landings.get() + 1);
                                }
                            }
                            // 落地即排序：折叠候选按 `start`、括号配对按 `open_line`
                            // 升序（gutter/标尺查询走二分）。仅内部次序，语义不变。
                            this.fold_candidates = folds;
                            this.fold_candidates.sort_by_key(|f| f.start);
                            this.bracket_pairs = brackets;
                            this.bracket_pairs.sort_by_key(|p| p.open_line);
                            // 增量边界更新为本次全景（供下一次增量续用），并消费
                            // 增量描述（若期间又有编辑，epoch 已变、不会走到这里）。
                            this.fold_boundaries = nfb;
                            this.bracket_boundaries = npb;
                            this.scan_pending_edit = None;
                            this.scan_multi_edit = false;
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
        // 窗口化：配对已按 `open_line` 升序（两个落地处排序，见 `recompute_fold_brackets_full` /
        // `refresh_fold_brackets`）。竖线只可能来自 `open_line ∈ [top_vis − 回看, bottom_vis]`
        // 的配对——开括号在视口之下（`> bottom_vis`）的配对必不相交；开在「回看」行以上的
        // 大跨度配对（竖线远超一屏）视为无意义跳过。原实现对**全部**配对逐条算 `visual_row`
        // /相交判定（大文档 1k+ 对 × 每帧 = 每键几 ms 白烧），实际视口内只有个位数可见段。
        // 回看再放宽也不影响正确性（仍逐条跑原过滤），只决定「多长跨度的竖线还画」。
        let lb = self
            .bracket_pairs
            .partition_point(|p| p.open_line < top_vis.saturating_sub(
                crate::config::limits::BRACKET_GUIDE_WINDOW_LOOKBACK_LINES,
            ));
        let ub = self.bracket_pairs.partition_point(|p| p.open_line <= bottom_vis);
        // (x, 视觉顶行, 视觉底行**开区间**, 是否点亮) —— 先整列收集，再合并同列重叠/相邻段。
        let mut intervals: Vec<(f32, usize, usize, bool)> = Vec::new();
        for p in &self.bracket_pairs[lb..ub] {
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
        // 操作级实时追踪（WY_TRACE）：一切编辑主链的单一漏斗——打字/IME/退格/删除/
        // 换行/Tab/粘贴/剪切/程序化 insert_text 都经此，在此登记 per-key 事件链起点，
        // 到 EditorElement::paint 结算「按键→绘制完成」的真实感知延迟与阶段 breakdown。
        if crate::prof::trace_active() {
            crate::prof::op_start_detail("ed_edit", &format!("intent={intent:?}"));
        }
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
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text() {
                self.paste_text(&text, cx);
            }
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(tx) = self.undo.undo() else {
            return;
        };
        crate::prof::op_start("ed_undo");
        // 整组撤销的受替换区**bounding box**（撤销前坐标，见下方 last_edit 注）：
        // 编辑前先取首行/末行，供折叠候选 / 括号配对后台**增量**刷新登记行段。
        let a_off = tx.edits.iter().map(|e| e.start).min().unwrap_or(0);
        let oe_off = tx
            .edits
            .iter()
            .map(|e| e.start + e.new_text.len())
            .max()
            .unwrap_or(0)
            .min(self.buffer.len());
        let pre_rows = self.row_count;
        let line_a = self.buffer.line_of_offset(a_off);
        let line_b = self.buffer.line_of_offset(oe_off);
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
        // 撤销整组：dirty 用覆盖所有编辑的最小−最大区间（bounding box，保守）。
        // 撤销把「已插入的新文本」替换回「旧文本」：被替换区（撤销前坐标）
        // = min start .. max(start + new_text.len)，恢复文本末 = start + 恢复字节和
        // （旧实现误用 old_text.len 当被替换区长 → 纯插入的撤销得到零宽 (a,a,a)，
        // App 增量桥/预览同步在撤销时收不到正确区间）。
        if let (Some(a), Some(oe)) = (
            tx.edits.iter().map(|e| e.start).min(),
            tx.edits.iter().map(|e| e.start + e.new_text.len()).max(),
        ) {
            let restored = tx.edits.iter().map(|e| e.old_text.len()).sum::<usize>();
            self.last_edit = Some((a, oe, a + restored));
        }
        // 折叠候选 / 括号配对增量：行段 = [line_a, min_stop)（min_stop 取新坐标
        // 首个未受影响行的**上界**，conservative 偏大 → 增量包络只会更大、仍正确）。
        let delta = self.row_count as i64 - pre_rows as i64;
        let min_stop = line_b
            .saturating_add(delta.max(0) as usize)
            .saturating_add(1);
        self.mark_scan_dirty_incremental(line_a, min_stop, delta);
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
        crate::prof::op_start("ed_redo");
        // 整组重做的受替换区 bounding box（重做前坐标，见下方 last_edit 注）：先取
        // 首行/末行供折叠候选 / 括号配对后台**增量**刷新登记行段。
        let a_off = tx.edits.iter().map(|e| e.start).min().unwrap_or(0);
        let oe_off = tx
            .edits
            .iter()
            .map(|e| e.start + e.old_text.len())
            .max()
            .unwrap_or(0)
            .min(self.buffer.len());
        let pre_rows = self.row_count;
        let line_a = self.buffer.line_of_offset(a_off);
        let line_b = self.buffer.line_of_offset(oe_off);
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
        // 重做整组：把「旧文本」重新替换回「新文本」。被替换区（重做前坐标）
        // = min start .. max(start + old_text.len)，新文本末 = start + 新文本字节和。
        // （与撤销同款修正：旧实现误用 new_text.len 当被替换区，交叉删除/粘贴的重做
        // 同样给不出正确区间。）
        if let (Some(a), Some(oe)) = (
            tx.edits.iter().map(|e| e.start).min(),
            tx.edits
                .iter()
                .map(|e| e.start + e.old_text.len())
                .max(),
        ) {
            let inserted = tx.edits.iter().map(|e| e.new_text.len()).sum::<usize>();
            self.last_edit = Some((a, oe, a + inserted));
        }
        // 折叠候选 / 括号配对增量：行段 = [line_a, min_stop)（同撤销，conservative）。
        let delta = self.row_count as i64 - pre_rows as i64;
        let min_stop = line_b
            .saturating_add(delta.max(0) as usize)
            .saturating_add(1);
        self.mark_scan_dirty_incremental(line_a, min_stop, delta);
        self.selection = Selection::from_range(tx.after.0, tx.after.1);
        self.resync_list(true);
        self.kick_caret();
        self.reveal_caret();
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }
}



/// 行首空白字节数（空格/Tab；缩进折叠启发式用）。
fn leading_ws_len(s: &str) -> usize {
    s.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}
