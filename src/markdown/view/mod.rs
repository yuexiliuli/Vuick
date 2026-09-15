//! Markdown 渲染器的视图层：行级虚拟化 + gpui_component 滚动条（双向）。
//!
//! 核心思路（参考 vellum / zorite 的行级虚拟化）：
//! - 文档被拆成**一行一条**的渲染行（见 [`crate::markdown::parser`]）；
//! - 用 gpui 的 `List` 做虚拟化：只构建 / 绘制可见行，滚动时按需重建；
//! - **竖滚动条量程**：`measure_all()` 会首帧排版整篇（十万行一次 1GB+），所以改用
//!   `with_uniform_item_height` 以「宽度感知的平均行高」铺设量程（见 `update_height_hint`）
//!   ——行数 × 平均高 ≈ 真实总高，滚动条从一开始就能拖动到全文，内存只随可见窗口走；
//! - 横向溢出走**原生滚动容器**：`List` 的内容宽（估算的最宽行 `content_w`）由
//!   `w(content_w).min_w_full()` 在布局期一次算定（超宽行撑宽、普通文档撑满视口），
//!   外面套一层 `overflow_x_scroll` + `track_scroll(&h_handle)`；
//! - gpui_component 的滚动条都按官方叠层挂法（`absolute + inset_0` + 滚动条本体）
//!   挂在 `md-preview` 上：横向跟踪 `h_handle`、竖向跟踪 `ListState`，组件自绘
//!   （含拖拽、悬浮、自动隐藏），保证内容超宽时横条一定可见。
//!
//! 本目录按职责拆分（替代原先的单文件 `view.rs` 长文件）：
//! - `style`：排版参数快照（[`style::StyleParams`]）
//! - `dimensions`：行宽 / 表格列宽估算、图片头尺寸解析
//! - `row`：单行渲染（[`row::render_row`]）
//! - `table`：GFM 表格渲染（[`table::render_table`] / `render_table_row`）
//! - `tests`：视图层测试

mod dimensions;
mod row;
mod style;
mod table;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use gpui::{
    div, list, px, rgba, App, Context, Hsla, InteractiveElement, IntoElement, ListAlignment,
    ListState, ParentElement, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::input::Rope;
use gpui_component::scroll::{Scrollbar, ScrollbarMode, ScrollableElement as _};

use crate::config::limits::{
    CODE_HL_LRU_CAP, WINDOWED_MIN_BLOCKS, WINDOWED_OVERDRAW_BLOCKS, WINDOWED_OVERDRAW_FACTOR,
    WINDOWED_OVERDRAW_MAX,
};
use crate::config::markdown::{IMAGE_ROW_DEFAULT_H, ROW_OVERDRAW_PX, WIDTH_BUFFER_PX};
use crate::config::timing::HIGHLIGHT_MS;
use crate::markdown::parser::{
    parse_block_window, parse_block_window_rope, parse_to_rows, rows_memory_estimate, scan_blocks,
    BlockIndex, BlockMeta, Row, RowKind, RowPayload,
};
use crate::syntax::{parse_code_lines, CodeLineRuns};

use dimensions::{est_row_width, est_row_width_meta, image_dimensions, image_dimensions_bytes};
use row::render_row;
use style::StyleParams;

/// 窗口模式代码块语法高亮的小型 LRU：键 = **全局块下标** → `Rc<CodeLineRuns>`。
///
/// `ensure_window` 每次窗口移动都会对新窗口内的代码块重跑 syntect（整块解析是滚动
/// 手感的主成本，debug 下单块几十 µs~ms）。滚动时相邻窗口大半代码块重叠，加一个
/// 容量受限的本地缓存，重叠块直接复用 `Rc`（O(1)），只有新进入窗口的块才重解析。
/// 源一旦变化就 [`CodeHlCache::clear`]（缓存键依赖块号/内容，不支持跨源复用）。
struct CodeHlCache {
    map: std::collections::HashMap<usize, Rc<CodeLineRuns>>,
    /// 访问序：队头最旧、队尾最新；命中/插入时移到队尾（真 LRU 近似）。
    lru: std::collections::VecDeque<usize>,
    cap: usize,
}

impl CodeHlCache {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            lru: std::collections::VecDeque::new(),
            cap: CODE_HL_LRU_CAP,
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.lru.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }

    fn get(&mut self, key: usize) -> Option<Rc<CodeLineRuns>> {
        if let Some(v) = self.map.get(&key) {
            // 命中 → 移到队尾（真 LRU：最近用过的块最后被淘汰）
            if let Some(pos) = self.lru.iter().position(|&k| k == key) {
                self.lru.remove(pos);
                self.lru.push_back(key);
            }
            Some(v.clone())
        } else {
            None
        }
    }

    fn insert(&mut self, key: usize, value: Rc<CodeLineRuns>) {
        if self.map.contains_key(&key) {
            // 已存在：更新内容并视为最近使用（移到队尾）
            self.map.insert(key, value);
            if let Some(pos) = self.lru.iter().position(|&k| k == key) {
                self.lru.remove(pos);
                self.lru.push_back(key);
            }
            return;
        }
        if self.map.len() >= self.cap {
            // 容量满：逐出最久未用的块
            if let Some(old) = self.lru.pop_front() {
                self.map.remove(&old);
            }
        }
        self.map.insert(key, value);
        self.lru.push_back(key);
    }
}

/// 自适应过扫余量（块数）：随可见块数增长，让高视口/快速滚动少重建窗口。
fn adaptive_overdraw(visible_blocks: usize) -> usize {
    let adaptive = (visible_blocks as f32 * WINDOWED_OVERDRAW_FACTOR) as usize;
    adaptive.clamp(WINDOWED_OVERDRAW_BLOCKS, WINDOWED_OVERDRAW_MAX)
}

/// 预览的原始文本源。窗口级懒解析在滚动/重解析时要反复对**原文**切片，
/// 因此预览侧保存的源可以是：
/// - [`Self::Str`]：小文档/一次性喂入的 `SharedString`；
/// - [`Self::Rope`]：编辑器正文 rope 的 O(1) COW 克隆（打字同步的主路径）——
///   与编辑器共享底层块，不物化全文，快照可安全交后台线程。
#[derive(Clone)]
pub enum PreviewSrc {
    Str(SharedString),
    Rope(Rope),
}

impl PreviewSrc {
    /// 源码字节数（内存估算用）。
    pub fn len(&self) -> usize {
        match self {
            PreviewSrc::Str(s) => s.len(),
            PreviewSrc::Rope(r) => r.len(),
        }
    }

    /// 源码行数（诊断输出用；`Rope` 用与 `split('\n')` 一致的行数口径）。
    pub fn line_count(&self) -> usize {
        match self {
            PreviewSrc::Str(s) => s.lines().count(),
            PreviewSrc::Rope(r) => {
                use gpui_component::input::RopeExt;
                r.lines_len()
            }
        }
    }
}

impl From<Rope> for PreviewSrc {
    fn from(r: Rope) -> Self {
        PreviewSrc::Rope(r)
    }
}

impl From<SharedString> for PreviewSrc {
    fn from(s: SharedString) -> Self {
        PreviewSrc::Str(s)
    }
}

/// 自研 Markdown 预览器（行级虚拟化 + 双向滚动条）。
pub struct MarkdownPreview {
    rows: Rc<Vec<Row>>,
    list_state: ListState,
    /// 背景色覆盖（由应用传入，与编辑区保持一致；None 用主题背景）。
    bg: Option<Hsla>,
    /// 估算的最宽内容（px，跨全部行），决定横向滚动范围。
    content_w: f32,
    /// 横向滚动句柄（跟踪外层横向滚动容器，供 `horizontal_scrollbar` 使用）。
    h_handle: ScrollHandle,
    /// 是否启用文本自动换行（true：段落/标题/引述/列表折行、无横向滚动；false：单行横向滚动）。
    wrap: bool,
    /// 正文字号覆盖（None = 用主题默认；EPUB 缩放字号时设置）。
    font_override: Option<f32>,
    /// 目录：文档内标题行的 (行号, 级别, 文本)。
    toc: Vec<(usize, u8, String)>,
    /// markdown 文件所在目录（用于解析相对路径图片；None = 仅绝对路径/URL 有效）。
    base_dir: Option<std::path::PathBuf>,
    /// 内存图片注册表：虚拟 src（`epubimg://...`）→ 图片字节（不落盘）。
    memory_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    /// 代码块的横向滚动句柄（行号 → 句柄）。句柄跨帧稳定，供滚动条拖动。
    /// 装箱（[`Rc`]）：渲染时 O(1) 共享，避免每帧整张克隆 HashMap。
    code_scroll: std::rc::Rc<std::collections::HashMap<usize, ScrollHandle>>,
    /// 代码块语法高亮缓存（**全局块下标** → 每行语义名 run；键与 `code_scroll` 一致）。
    /// 建在 set_source / 窗口重建时整块解析一次，渲染只做颜色解析——
    /// 结果与主题无关，换明/暗主题无需重算。装箱同上。
    code_hl: std::rc::Rc<std::collections::HashMap<usize, std::rc::Rc<CodeLineRuns>>>,
    /// 窗口模式代码块高亮的小型 LRU（跨窗口移动复用已解析块，见 [`CodeHlCache`]）。
    /// 换源 / 释放时清空（键依赖块号/内容，不可跨源复用）。
    code_hl_cache: CodeHlCache,
    /// 本次渲染算出的**自适应过扫余量**（块数）：render 依视口高度更新，
    /// `ensure_window` 用它替代固定 [`WINDOWED_OVERDRAW_BLOCKS`]。
    overdraw_margin: usize,
    /// 最近一次统一行高提示重建的时刻：换宽/拖分屏时按 [`crate::config::timing::HINT_REAPPLY_INTERVAL_MS`]
    /// 节流 O(块数) 的 SumTree 重建，避免宽度每帧变化时每帧都卡一次。
    last_hint_apply: Option<std::time::Instant>,
    /// 表格的横向滚动句柄（行号 → 句柄）。句柄跨帧稳定，供滚动条拖动。装箱同上。
    table_scroll: std::rc::Rc<std::collections::HashMap<usize, ScrollHandle>>,
    /// 搜索跳转高亮：命中的源码行（0 基）与小写查询词；None = 无高亮。
    highlight_search: Option<(usize, String)>,
    /// 高亮代序列号：每次发起/清除高亮 +1，定时器只清除同代高亮，避免旧定时器误清新高亮。
    highlight_seq: u64,
    /// 统一行高提示（px）：宽度感知的平均行高估算，用于 `ListState::with_uniform_item_height`，
    /// 让竖滚动条量程从头一开始就接近真实总高（不用一次性排版整篇）。
    est_hint: f32,
    /// 生成 `est_hint` 时的视口宽度（px）；<0 表示尚未按宽度估算（需要重算）。
    est_hint_width: f32,
    /// 「统一行高提示」上次应用到的 (行数, 提示值)。同一 (行数, 提示值) 只应用一次：
    /// `with_uniform_item_height` 会逐项物化整棵 SumTree（16 万块一次 30~90ms），
    /// 若每帧都补会直接卡死；记忆后只有块数变化/换宽/换源才重做。
    hint_applied: Option<(usize, f32)>,
    /// 提示被 gpui 首布局按宽度变化清掉后，本轮是否已补过一份（补后不再反复补，
    /// 防每帧 O(块数) 重建的无限循环）。
    hint_recovered: bool,
    /// 大文档「窗口级懒解析」的整篇轻量块索引；`None` = 小文档走全量 `rows`。
    index: Option<Rc<BlockIndex>>,
    /// 窗口模式下保留的原始 markdown（窗口重解析需要）。rope COW 共享、不物化全文。
    src_buf: Option<PreviewSrc>,
    /// 已物化窗口的全局块下标（`win_rows[0]` 对应第 `win_lo` 块）。
    win_lo: usize,
    /// 已物化的当前窗口行（全局块下标 `win_lo..win_lo+len`；窗口模式才有内容）。
    win_rows: Rc<Vec<Row>>,
}

impl MarkdownPreview {
    pub fn new(_cx: &mut App) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, px(ROW_OVERDRAW_PX)).measure_all();
        Self {
            rows: Rc::new(Vec::new()),
            list_state,
            bg: None,
            content_w: 0.0,
            h_handle: ScrollHandle::new(),
            wrap: false,
            font_override: None,
            toc: Vec::new(),
            base_dir: None,
            memory_images: Default::default(),
            code_scroll: Default::default(),
            code_hl: Default::default(),
            code_hl_cache: CodeHlCache::new(),
            overdraw_margin: WINDOWED_OVERDRAW_BLOCKS,
            last_hint_apply: None,
            table_scroll: Default::default(),
            highlight_search: None,
            highlight_seq: 0,
            est_hint: 0.0,
            est_hint_width: -1.0,
            hint_applied: None,
            hint_recovered: false,
            index: None,
            src_buf: None,
            win_lo: 0,
            win_rows: Rc::new(Vec::new()),
        }
    }

    /// 搜索跳转后高亮「源码第 `line` 行」（0 基）内**第一个命中 `query` 的文本**，
    /// 只给搜索词本身着色（不吃整行），[`HIGHLIGHT_MS`] 后自动消失。
    pub fn highlight_search(&mut self, line: usize, query: &str, cx: &mut Context<Self>) {
        self.highlight_search = Some((line, query.to_lowercase()));
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        let seq = self.highlight_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(HIGHLIGHT_MS))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.highlight_seq == seq {
                    view.highlight_search = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 清除搜索跳转高亮。
    pub fn clear_highlight(&mut self) {
        self.highlight_search = None;
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
    }

    /// 当前搜索跳转高亮映射到的 **(渲染行, 行内字节区间)**；无高亮或命中文本不在
    /// 渲染文本中时返回 `None`。测试用，确认高亮只覆盖搜索词而非整行。
    #[cfg(test)]
    pub fn search_highlight_range(&self) -> Option<(usize, std::ops::Range<usize>)> {
        let (line, query) = self.highlight_search.as_ref()?;
        let row_ix = self.row_for_line(*line)?;
        let text = self.rows.get(row_ix)?.text.as_ref();
        let range = find_text_range(text, query)?;
        Some((row_ix, range))
    }

    /// 注册内存图片表（虚拟 src → 图片字节，不落盘），EPUB 用 `epubimg://...`。
    /// 需在 set_source 之前调用；后续每次 set_source 都会按虚拟 src 重新挂载。
    pub fn set_memory_images(
        &mut self,
        images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    ) {
        self.memory_images = images;
    }

    /// 设置 markdown 文件目录（相对路径图片解析依据）。
    pub fn set_base_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.base_dir = dir;
    }

    /// 解析图片源：URL / Windows 绝对路径 / 已绝对路径原样返回，其余基于 base 目录拼成绝对路径。
    fn resolve_image_src(base: &std::path::Path, src: &str) -> String {
        let lower = src.to_lowercase();
        if lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("file://")
        {
            return src.to_string();
        }
        if src.len() >= 2 && src.as_bytes()[1] == b':' {
            return src.to_string();
        }
        if std::path::Path::new(src).is_absolute() {
            return src.to_string();
        }
        base.join(src).to_string_lossy().to_string()
    }

    /// 设置预览背景色（打开/同步时由应用传入，使预览与编辑区同底色）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 设置正文字号覆盖（px）。None 使用主题默认；用于 EPUB 等阅读场景的缩放。
    pub fn set_font_override(&mut self, size: Option<f32>, cx: &mut App) {
        if self.font_override == size {
            return;
        }
        self.font_override = size;
        self.remeasure_dimensions(cx);
        self.list_state.remeasure();
        self.invalidate_height_hint();
    }

    /// 在当前字号基础上增减（EPUB 滚动缩放）；返回新的字号覆盖。
    pub fn bump_font_size(&mut self, delta: f32, cx: &mut App, current_default: f32) {
        let base = self.font_override.unwrap_or(current_default);
        self.set_font_override(Some((base + delta).clamp(9.0, 32.0)), cx);
    }

    /// 重建尺寸估算（换行/字号/内容变化后调用）。
    fn remeasure_dimensions(&mut self, cx: &mut App) {
        let p = StyleParams::from_theme_with_font(cx, self.font_override);
        self.content_w = if self.wrap {
            0.0
        } else {
            let max_w = match &self.index {
                Some(idx) => idx
                    .blocks
                    .iter()
                    .map(|b| est_row_width_meta(b, &p))
                    .fold(0.0, f32::max),
                None => self
                    .rows
                    .iter()
                    .map(|r| est_row_width(r, &p))
                    .fold(0.0, f32::max),
            };
            max_w + WIDTH_BUFFER_PX
        };
    }

    /// 设置是否文本自动换行（EPUB 阅读等场景）。切换后无需重解析，直接触发重渲染。
    pub fn set_wrap(&mut self, wrap: bool, cx: &mut App) {
        if self.wrap == wrap {
            return;
        }
        self.wrap = wrap;
        self.remeasure_dimensions(cx);
        self.list_state.remeasure();
        self.invalidate_height_hint();
    }

    /// 当前是否文本自动换行。
    pub fn is_wrap(&self) -> bool {
        self.wrap
    }

    /// 目录条目：(行号, 标题级别, 标题文本)。供侧边栏目录视图使用。
    pub(crate) fn toc_entries(&self) -> &[(usize, u8, String)] {
        &self.toc
    }

    /// 滚动到指定行（目录跳转）。
    pub fn scroll_to_row(&mut self, ix: usize) {
        if ix >= self.item_count() {
            return;
        }
        self.list_state.scroll_to_reveal_item(ix);
    }

    /// 返回覆盖**源码第 `line` 行**（0 基）的渲染行下标；空/无行返回 `None`。
    pub fn row_for_line(&self, line: usize) -> Option<usize> {
        let n = self.item_count();
        if n == 0 {
            return None;
        }
        // 最后一个 src_line <= line 的渲染行即覆盖该源码行的行（二分）
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let src_line = match &self.index {
                Some(idx) => idx.blocks[mid].src_line as usize,
                None => self.rows[mid].src_line,
            };
            if src_line <= line {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Some(lo.saturating_sub(1))
    }

    /// 滚动到包含**源码第 `line` 行**（0 基）的渲染行：搜索跳转（EPUB 只读预览）用。
    pub fn scroll_to_line(&mut self, line: usize) {
        if let Some(ix) = self.row_for_line(line) {
            self.list_state.scroll_to_reveal_item(ix);
        }
    }

    /// 设置预览源码（仅影响预览，原文不变）。块索引在调用内现算。
    pub fn set_source(&mut self, src: &str, cx: &mut App) {
        self.set_source_with_index(PreviewSrc::Str(SharedString::from(src)), Rc::new(scan_blocks(src)), cx);
    }

    /// 设置预览源码，但**复用外部预建的块索引**（异步打开时由后台线程扫好，
    /// 省掉 UI 线程上为了打开大文档做 `scan_blocks` 的几百毫秒）。
    ///
    /// `src`/`index` 均为成本 O(1) 的句柄：内部直接共享同一份（`Rope` COW 克隆 /
    /// `SharedString` 的 `Arc<str>`、`Rc<BlockIndex>` 共享读取）——**不把全文二次
    /// 物化、也不整份拷贝索引**。打字同步主路径传 [`PreviewSrc::Rope`]（编辑器正文
    /// 快照，O(1)），窗口模式按可见窗口从 rope 切片，驻留不随文档增长。
    pub fn set_source_with_index(&mut self, src: PreviewSrc, index: Rc<BlockIndex>, cx: &mut App) {
        // 内容变了，旧的跳转高亮随之失效；代码块高亮缓存键依赖块号/内容，一并作废
        // （否则旧源的高亮会按新源的块号被误复用）。
        self.clear_highlight();
        self.code_hl_cache.clear();
        // 记录当前滚动位置：源更新后恢复到同一逻辑位置（否则大文档每次编辑同步
        // 都会重建 ListState 跳回开头）。
        let prev_scroll = self.list_state.logical_scroll_top();
        if index.blocks.len() > WINDOWED_MIN_BLOCKS {
            // ---- 大文档：窗口级懒解析 ----
            // 整篇只保留轻量块索引（行号/类型/长度 ≈16B/行）+ 原始文本（rope
            // COW 共享、不物化全文），渲染按可见窗口惰性物化 `Row`，驻留不随文档增长。
            self.index = Some(index.clone());
            self.src_buf = Some(src.clone());
            self.win_lo = 0;
            self.win_rows = Rc::new(Vec::new());
            self.rows = Rc::new(Vec::new());
            self.toc = index.toc.clone();
            self.code_scroll = Default::default();
            self.code_hl = Default::default();
            self.table_scroll = Default::default();
            let new_n = index.blocks.len();
            if new_n == self.list_state.item_count() {
                // 块数未变（最常见的小编辑）：复用列表状态——保留 `last_layout_bounds`
                // 与统一高度提示，避免 gpui 每帧按宽度变化 O(块数) 清空/重建。
                // 内容变了只**重测可见窗口内**已物化的行高（O(窗口)，而非全量 remeasure
                // 的 O(块数) SumTree 重建）；窗口外的行本来就是未测量占位，滚动过去时
                // ensure_window 会用新源重建，无需此处铺满。
                let r_lo = self.win_lo;
                let r_hi = (r_lo + self.win_rows.len()).min(new_n);
                if r_lo < r_hi {
                    self.list_state.remeasure_items(r_lo..r_hi);
                }
            } else {
                // 块数变化：重建列表 + 恢复到更新前的滚动位置（块号越界自动夹紧到文末），
                // 并把「统一高度提示」标记为待应用（一次性 O(n)，不再每帧重复）。
                self.list_state =
                    ListState::new(new_n, ListAlignment::Top, px(ROW_OVERDRAW_PX));
                self.list_state.scroll_to(prev_scroll);
                self.hint_applied = None;
                self.hint_recovered = false;
            }
            if std::env::var_os("MD_MEM_DEBUG").is_some() {
                let n = index.blocks.len();
                let index_b = n * std::mem::size_of::<crate::markdown::parser::BlockMeta>();
                eprintln!(
                    "[MD_MEM_DEBUG] preview set_source(窗口模式): 源 {} B / {} 行 → {} 块, 索引≈{} KB, 行未物化",
                    src.len(),
                    src.line_count(),
                    n,
                    index_b / 1024,
                );
            }
            self.remeasure_dimensions(cx);
            self.invalidate_height_hint();
            return;
        }

        // ---- 小文档：全量路径（原有行为不变；rope 源在此物化，但仅小文档、低频） ----
        let mut parsed = match &src {
            PreviewSrc::Str(s) => parse_to_rows(s.as_ref()),
            PreviewSrc::Rope(r) => parse_to_rows(&r.to_string()),
        };
        self.resolve_image_rows(&mut parsed);
        let rows = parsed;
        // 提取目录：标题行（行号, 级别, 清理后的文本）
        self.toc = rows
            .iter()
            .enumerate()
            .filter_map(|(ix, r)| match r.kind {
                RowKind::Heading(level) => Some((ix, level, r.text.to_string())),
                _ => None,
            })
            .collect();
        // 代码行 → 稳定的横向滚动句柄（供块内滚动条拖动）
        self.code_scroll = Rc::new(
            rows.iter()
                .enumerate()
                .filter(|(_, r)| r.kind == RowKind::Code)
                .map(|(ix, _)| (ix, ScrollHandle::new()))
                .collect(),
        );
        // 表格行 → 稳定的横向滚动句柄（供块内滚动条拖动）
        self.table_scroll = Rc::new(
            rows.iter()
                .enumerate()
                .filter(|(_, r)| r.kind == RowKind::Table)
                .map(|(ix, _)| (ix, ScrollHandle::new()))
                .collect(),
        );
        self.rows = Rc::new(rows);
        // 代码块语法高亮：按围栏语言整块解析一次缓存（渲染只做颜色解析）。
        self.code_hl = Rc::new(build_code_highlights_for(&self.rows));
        if std::env::var_os("MD_MEM_DEBUG").is_some() {
            let (n, bytes) = rows_memory_estimate(&self.rows);
            eprintln!(
                "[MD_MEM_DEBUG] preview set_source: 源 {} B / {} 行 → {} 渲染行, 估算驻留 {} KB",
                src.len(),
                src.line_count(),
                n,
                bytes / 1024,
            );
        }
        self.remeasure_dimensions(cx);
        // 行高估算依赖视口宽度，换源后强制按新宽度重算（render 里执行）。
        self.invalidate_height_hint();
        if self.rows.len() != self.list_state.item_count() {
            // 行数变化 → 重建 ListState。注意**不能**用 `measure_all()`：它会首帧
            // 就把整篇所有行全部渲染并排版（十万行文档一次吃进 1GB+ 的字体测量缓存），
            // 而且每次开关预览重解析都会再生成一份、内存只涨不降。
            // 这里**故意不设**静态高度提示：若先设一个占位提示，首帧 summary 会被它
            // 垫到「看似有效」而跳过统一高度提示的重填，随后列表又在首次布局时把
            // size_hint 清成 None（见 `update_height_hint`），滚动条量程就再没人管了。
            // 直接全部留空，交给 `update_height_hint` 在首次渲染按视口宽度填平均高：
            // 滚动条量程从一开始就接近真实总高，但只对可见行测量/排版（行级虚拟化）。
            self.list_state =
                ListState::new(self.rows.len(), ListAlignment::Top, px(ROW_OVERDRAW_PX));
            // 行数变化只会重建列表：把滚动恢复到更新前的逻辑位置，避免跳回开头
            self.list_state.scroll_to(prev_scroll);
            self.hint_applied = None;
            self.hint_recovered = false;
        } else {
            // 行数不变但内容变了 → 重新测量高度（保留滚动位置；未测行继续用统一提示）
            self.list_state.remeasure();
        }
    }

    /// 图片行后处理：解析内存图/本地图尺寸（全量路径与窗口路径共用）。
    fn resolve_image_rows(&self, parsed: &mut [Row]) {
        let base = self.base_dir.clone();
        for r in parsed.iter_mut() {
            if r.kind != RowKind::Image {
                continue;
            }
            let Some(p) = r.payload.as_mut() else {
                continue;
            };
            let RowPayload::Image {
                src,
                img_w,
                img_h,
                img_data,
            } = p.as_mut()
            else {
                continue;
            };
            let s = src.clone();
            // 内存图片（EPUB/PDF 虚拟 src）：直接查注册表挂载字节（不落盘、不读文件）
            if let Some(img) = self.memory_images.get(s.as_ref()) {
                if let Some((w, h)) = image_dimensions_bytes(&img.bytes) {
                    *img_w = Some(w);
                    *img_h = Some(h);
                }
                *img_data = Some(img.clone());
                *src = s;
                continue;
            }
            let resolved = if let Some(base) = &base {
                Self::resolve_image_src(base, &s)
            } else {
                s.to_string()
            };
            // 本地文件 → 读固有尺寸（保持比例显示；URL/不可读时保持 None）
            if !resolved.starts_with("http://")
                && !resolved.starts_with("https://")
                && !resolved.starts_with("file://")
            {
                if let Some((w, h)) = image_dimensions(std::path::Path::new(&resolved)) {
                    *img_w = Some(w);
                    *img_h = Some(h);
                }
            }
            *src = resolved.into();
        }
    }

    /// 渲染条目总数：全量 = 渲染行数；窗口模式 = 块数。
    fn item_count(&self) -> usize {
        match &self.index {
            Some(idx) => idx.blocks.len(),
            None => self.rows.len(),
        }
    }

    /// 窗口模式：确保 `win_rows` 覆盖块区间 `[lo, hi]`（含上下过扫余量）。
    /// 命中缓存的窗口（或仅 ±1 内）不重解析，避免轻微滚动反复重建。
    fn ensure_window(&mut self, lo: usize, hi: usize) {
        let Some(idx) = self.index.clone() else {
            return;
        };
        let Some(src) = self.src_buf.clone() else {
            return;
        };
        let n = idx.blocks.len();
        if n == 0 {
            return;
        }
        let want_lo = lo.min(n - 1);
        let want_hi = hi.clamp(want_lo, n - 1);
        let cur_lo = self.win_lo;
        let cur_hi = cur_lo.saturating_add(self.win_rows.len()).saturating_sub(1);
        // 命中：当前窗口已覆盖所需区间（含上下过扫余量）→ 不重解析。
        // 注意不能要求 |want_lo-cur_lo| 很小：带余量重建后 cur_lo 本就比
        // 需求靠前 24 块，那样会每帧都判"没命中"而反复重解析。
        if !self.win_rows.is_empty() && cur_lo <= want_lo && cur_hi >= want_hi {
            return;
        }
        let _t = crate::prof::begin();
        let margin = self.overdraw_margin.min(n / 2);
        let w_lo = want_lo.saturating_sub(margin);
        let w_hi = (want_hi + margin).min(n - 1);
        let mut rows = match &src {
            PreviewSrc::Str(s) => parse_block_window(s, &idx, w_lo, w_hi),
            PreviewSrc::Rope(r) => parse_block_window_rope(r, &idx, w_lo, w_hi),
        };
        self.resolve_image_rows(&mut rows);
        self.win_lo = w_lo;
        self.win_rows = Rc::new(rows);
        self.build_window_scroll_handles();
        crate::prof::end("md_ensure_window", _t);
    }

    /// 为当前窗口建立代码/表格块的稳定横向滚动句柄与代码块语法高亮（键 = **全局块下标**，
    /// 与渲染按 `ix` 查表一致）。代码块高亮不再每次都整窗重跑 syntect：先查
    /// [`CodeHlCache`]（滚动时相邻窗口大半块重叠 → 直接复用 `Rc`，O(1)），仅新进入
    /// 窗口的块才解析并写入缓存。
    fn build_window_scroll_handles(&mut self) {
        let win_rows = self.win_rows.clone(); // 借 win_rows 的同时要 &mut self（缓存）
        let win_lo = self.win_lo;
        let mut cs_old = std::mem::take(&mut self.code_scroll);
        let mut ts_old = std::mem::take(&mut self.table_scroll);
        {
            let cs = Rc::make_mut(&mut cs_old);
            let ts = Rc::make_mut(&mut ts_old);
            cs.clear();
            ts.clear();
            for (j, _r) in win_rows.iter().enumerate() {
                let ix = win_lo + j;
                match _r.kind {
                    RowKind::Code => {
                        cs.insert(ix, ScrollHandle::new());
                    }
                    RowKind::Table => {
                        ts.insert(ix, ScrollHandle::new());
                    }
                    _ => {}
                }
            }
        }
        // 窗口内代码行 → 高亮（缓存优先）
        let mut window_hl: std::collections::HashMap<usize, Rc<CodeLineRuns>> =
            std::collections::HashMap::new();
        for (j, r) in win_rows.iter().enumerate() {
            let ix = win_lo + j;
            if let Some(runs) = self.code_hl_for_row(ix, r) {
                window_hl.insert(ix, runs);
            }
        }
        self.code_scroll = cs_old;
        self.table_scroll = ts_old;
        self.code_hl = Rc::new(window_hl);
    }

    /// 取第 `ix` 个块的代码高亮：命中 LRU 直接复用；未命中则按语言整块解析并写入缓存。
    fn code_hl_for_row(&mut self, ix: usize, r: &Row) -> Option<Rc<CodeLineRuns>> {
        if r.kind != RowKind::Code {
            return None;
        }
        if let Some(hit) = self.code_hl_cache.get(ix) {
            return Some(hit);
        }
        let lang = r.code_language()?;
        if lang.is_empty() {
            return None;
        }
        let mut code = String::new();
        for (i, l) in r.code_lines().iter().enumerate() {
            if i > 0 {
                code.push('\n');
            }
            code.push_str(l.as_ref());
        }
        let runs = Rc::new(parse_code_lines(lang, &code)?);
        self.code_hl_cache.insert(ix, runs.clone());
        Some(runs)
    }

    /// 估算当前预览解析结果的驻留内存（近似，见 [`rows_memory_estimate`]）。
    /// 供运行时诊断（`MD_MEM_DEBUG=1`）输出，判断解析放大了多少倍。
    pub fn memory_estimate(&self) -> (usize, usize) {
        match &self.index {
            Some(idx) => {
                let (_, win_b) = rows_memory_estimate(&self.win_rows);
                let n = idx.blocks.len();
                let index_b = n * std::mem::size_of::<crate::markdown::parser::BlockMeta>();
                let src_b = self.src_buf.as_ref().map(|s| s.len()).unwrap_or(0);
                (n, index_b + src_b + win_b)
            }
            None => rows_memory_estimate(&self.rows),
        }
    }

    /// 拼出已物化预览行的纯文本（测试/诊断用：验证预览内容是否同步到最新）。
    /// 窗口模式下仅返回当前窗口已物化的行（小文档全量路径返回整篇）。
    #[cfg(test)]
    pub(crate) fn rendered_text(&self) -> String {
        let rows: Vec<&str> = match &self.index {
            Some(_) => self.win_rows.iter().map(|r| r.text.as_ref()).collect(),
            None => self.rows.iter().map(|r| r.text.as_ref()).collect(),
        };
        rows.join("\n")
    }

    /// 释放预览重资源（解析出的行数据 + 列表测量缓存），关闭预览时调用。
    /// 否则隐藏的预览仍占着整篇文档的解析结果；反复开关预览会积累内存。
    /// 下次开启预览时 [`Self::set_source`] 会重新解析，行为不变。
    pub fn release(&mut self, cx: &mut App) {
        self.rows = Rc::new(Vec::new());
        self.list_state = ListState::new(0, ListAlignment::Top, px(ROW_OVERDRAW_PX));
        self.content_w = 0.0;
        self.toc.clear();
        self.code_scroll = Default::default();
        self.code_hl = Default::default();
        self.code_hl_cache.clear();
        self.overdraw_margin = WINDOWED_OVERDRAW_BLOCKS;
        self.last_hint_apply = None;
        self.table_scroll = Default::default();
        self.highlight_search = None;
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        self.est_hint = 0.0;
        self.est_hint_width = -1.0;
        self.hint_applied = None;
        self.hint_recovered = false;
        self.index = None;
        self.src_buf = None;
        self.win_lo = 0;
        self.win_rows = Rc::new(Vec::new());
        self.remeasure_dimensions(cx);
    }

    /// 标记「平均行高的宽度感知估算」已失效：换源 / 换行 / 字号变化后调用，
    /// 下一次 render（视口宽度已知）会重算并刷新统一高度提示。
    fn invalidate_height_hint(&mut self) {
        self.est_hint_width = -1.0;
    }

    /// 按当前视口宽度估算「平均渲染行高」作为列表的统一高度提示。
    /// 这样竖滚动条量程 = 行数 × 平均行高 ≈ 真实总高，从头一开始就可用滚动条
    /// 跳到任意位置（含文末），而无需像 `measure_all()` 那样一次性排版整篇。
    /// 估算规则尽量贴合 `render_row` 的几何：非文本行按公式精确，文本行按字符宽
    /// 折算行数（CJK ≈ 字号，其余 ≈ 0.55 × 字号），微差只会让滚动条映射略微
    /// 非线性，随行真正渲染后逐步收敛。
    fn estimated_avg_row_height(&self, p: &StyleParams, width: f32) -> f32 {
        let avail = (width - 24.0).max(40.0); // px_3 左右内缩
        let wrap = self.wrap;
        let (n, total) = match &self.index {
            Some(idx) => {
                if idx.blocks.is_empty() {
                    return p.body_row_hint().as_f32();
                }
                // 大文档的块索引有十几万条：平均行高按**等距采样**估算（预算 ~256 块），
                // 避免每次换源/字号变化在 UI 线程全量遍历——采样误差只让滚动条映射
                // 轻微非线性，随行真正渲染后由 `update_height_hint` 收敛，无需精确。
                let n = idx.blocks.len();
                let budget = 256usize;
                let total = if n <= budget {
                    idx.blocks
                        .iter()
                        .map(|b| index_block_height(b, p, wrap, avail))
                        .sum::<f32>()
                } else {
                    let step = n as f64 / budget as f64;
                    let mut sum = 0.0f32;
                    for k in 0..budget {
                        let i = (k as f64 * step).floor() as usize;
                        sum += index_block_height(&idx.blocks[i], p, wrap, avail);
                    }
                    sum / budget as f32 * n as f32
                };
                (n, total)
            }
            None => {
                if self.rows.is_empty() {
                    return p.body_row_hint().as_f32();
                }
                let mut total = 0.0f32;
                for r in self.rows.iter() {
                    let h = match r.kind {
                        RowKind::Blank => p.blank_h.as_f32(),
                        RowKind::Rule => p.rule_h.as_f32(),
                        RowKind::Code => {
                            (r.code_lines().len().max(1) as f32 + 2.0) * p.code_line.as_f32()
                        }
                        RowKind::Table => {
                            let row_h = p.para_size.as_f32() * 1.45 + 12.0;
                            (1.0 + r.table().map_or(0, |t| t.rows.len()) as f32) * row_h
                                + 2.0
                                + 8.0
                        }
                        RowKind::Image => IMAGE_ROW_DEFAULT_H,
                        RowKind::Heading(n) => {
                            let (fs, lh) = p.heading_metrics(n);
                            if wrap {
                                est_wrap_lines(&r.text, avail, fs.as_f32()).max(1) as f32
                                    * lh.as_f32()
                                    + 8.0
                            } else {
                                lh.as_f32()
                            }
                        }
                        RowKind::Paragraph | RowKind::Quote | RowKind::Bullet => {
                            if wrap {
                                let lines = est_wrap_lines(
                                    &r.text,
                                    avail,
                                    p.para_size.as_f32(),
                                )
                                .max(1) as f32;
                                (lines * p.para_line.as_f32()).max(p.para_line.as_f32())
                                    + p.para_gap.as_f32()
                                    + 8.0
                            } else {
                                p.para_line.as_f32() + p.para_gap.as_f32()
                            }
                        }
                    };
                    total += h;
                }
                (self.rows.len(), total)
            }
        };
        if n == 0 {
            return p.body_row_hint().as_f32();
        }
        total / n as f32
    }

    /// 视口宽度已知（render 里）后刷新统一高度提示。
    ///
    /// 背景：gpui 的 `list` 元素在**首次布局/视口宽度变化**时的 prepaint 里会把所有
    /// 未测量项的 size_hint 清为 `None`（`list.rs`，比较 `last_layout_bounds` 宽度），
    /// 之后滚动条总高就只算可见行、无法一开始拖到全文。这里每次渲染用 O(1) 的
    /// summary 总高对账：发现总高远小于「行数×平均高」（提示已被清掉）就重新套用。
    ///
    /// 关键约束：`with_uniform_item_height` 会**逐项物化整棵 SumTree**（16 万块一次
    /// 30~90ms），绝对不能在每一帧或每次重同步都调用。这里用 `hint_applied` 记忆
    /// 「(行数, 提示值) 已应用」：只有块数/宽度变化或换源才重做一次；若提示被 gpui
    /// 首布局按宽度变化清掉，则**每轮至多补一次**（`hint_recovered`），不再无限循环。
    fn update_height_hint(&mut self, cx: &mut App, width: f32) -> bool {
        let n = self.item_count();
        if n == 0 {
            return false;
        }
        let width_changed = (self.est_hint_width - width).abs() >= 2.0;
        if width_changed {
            let p = StyleParams::from_theme_with_font(cx, self.font_override);
            self.est_hint = self.estimated_avg_row_height(&p, width);
            self.est_hint_width = width;
        }
        let hint = self.est_hint;
        if hint <= 0.0 {
            return false;
        }
        if self.list_state.item_count() != n {
            return false;
        }
        // 列表当前总高 = 可滚动上限 + 视口高（尚未布局时二者皆 0）
        let summary_h = self.list_state.max_offset_for_scrollbar().y.as_f32()
            + self.list_state.viewport_bounds().size.height.as_f32();
        let expected = n as f32 * hint;
        if self.hint_applied == Some((n, hint)) {
            // 已应用；只有当提示被清掉（gpui 换宽清空）才补一次，且每轮至多补一次
            if summary_h >= expected * 0.5 || self.hint_recovered {
                return false;
            }
            self.list_state = self.list_state.clone().with_uniform_item_height(px(hint));
            self.hint_recovered = true;
            return true;
        }
        // 首次 / 块数或宽度变化 / 换源：应用一次 O(n)，之后由 `hint_applied` 记忆。
        // 换宽/拖分屏时宽度每帧变化 → 每帧都会走到这里；按最小间隔节流，避免 160k 块
        // 的 SumTree 重建在拖动期间每帧卡一次（量程短暂滞后、松手即收敛）。首次不节流。
        if width_changed && self.hint_applied.is_some() {
            let now = std::time::Instant::now();
            let interval = crate::config::timing::HINT_REAPPLY_INTERVAL_MS;
            if let Some(last) = self.last_hint_apply {
                if (now.duration_since(last).as_millis() as u64) < interval {
                    return false;
                }
            }
        }
        self.list_state = self.list_state.clone().with_uniform_item_height(px(hint));
        self.last_hint_apply = Some(std::time::Instant::now());
        self.hint_applied = Some((n, hint));
        self.hint_recovered = false;
        true
    }
}

/// 为给定行集重建代码块语法高亮缓存：按围栏语言**整块**解析一次，返回
/// 「行号 → 每行语义名 run」的映射（键与 `code_scroll` 一致；无语言/未知语法
/// 的代码行跳过）。结果与主题无关（明/暗色在渲染时解析），换主题无需重算。
fn build_code_highlights_for(rows: &[Row]) -> std::collections::HashMap<usize, Rc<CodeLineRuns>> {
    let mut hl = std::collections::HashMap::new();
    for (ix, r) in rows.iter().enumerate() {
        if r.kind != RowKind::Code {
            continue;
        }
        let Some(lang) = r.code_language() else {
            continue;
        };
        if lang.is_empty() {
            continue;
        }
        let mut code = String::new();
        for (i, l) in r.code_lines().iter().enumerate() {
            if i > 0 {
                code.push('\n');
            }
            code.push_str(l.as_ref());
        }
        if let Some(runs) = parse_code_lines(lang, &code) {
            hl.insert(ix, Rc::new(runs));
        }
    }
    hl
}

/// 单一「块元数据」的平均高度估算（窗口模式：只靠元数据，不物化 Row）。
/// 与 [`MarkdownPreview::estimated_avg_row_height`] 里 rows 全量路径的公式一一对应。
fn index_block_height(b: &BlockMeta, p: &StyleParams, wrap: bool, avail: f32) -> f32 {
    match b.kind {
        RowKind::Blank => p.blank_h.as_f32(),
        RowKind::Rule => p.rule_h.as_f32(),
        RowKind::Code => (b.aux.max(1) as f32 + 2.0) * p.code_line.as_f32(),
        RowKind::Table => {
            let row_h = p.para_size.as_f32() * 1.45 + 12.0;
            (1.0 + b.aux as f32) * row_h + 2.0 + 8.0
        }
        RowKind::Image => IMAGE_ROW_DEFAULT_H,
        RowKind::Heading(hh) => {
            let (fs, lh) = p.heading_metrics(hh);
            if wrap {
                est_wrap_len(b.raw_len as usize, avail, fs.as_f32()).max(1) as f32 * lh.as_f32() + 8.0
            } else {
                lh.as_f32()
            }
        }
        RowKind::Paragraph | RowKind::Quote | RowKind::Bullet => {
            if wrap {
                let lines =
                    est_wrap_len(b.raw_len as usize, avail, p.para_size.as_f32()).max(1) as f32;
                (lines * p.para_line.as_f32()).max(p.para_line.as_f32())
                    + p.para_gap.as_f32()
                    + 8.0
            } else {
                p.para_line.as_f32() + p.para_gap.as_f32()
            }
        }
    }
}

/// 估算法：按字符宽（CJK/全角 ≈ font_size，其余 ≈ 0.55 × font_size）累计文本宽度，
/// 折出在可用宽度 `avail` 下的行数。用于「平均行高」估算，非精确排版。
fn est_wrap_lines(text: &str, avail: f32, font_size: f32) -> usize {
    if text.is_empty() {
        return 1;
    }
    let mut w = 0.0f32;
    for ch in text.chars() {
        let c = ch as u32;
        let cw = if (0x2E80..=0x9FFF).contains(&c) || (0xFF00..=0xFFEF).contains(&c) {
            font_size
        } else {
            font_size * 0.55
        };
        w += cw;
    }
    (w / avail).ceil() as usize
}

/// 按「字节长度」估算折行数（窗口模式用：块索引不含清理后文本）。
/// 以 0.55em/字节 的保守平均宽折算（CJK 每字节的实际宽/3 更小），避免严重低估。
fn est_wrap_len(bytes: usize, avail: f32, font_size: f32) -> usize {
    if bytes == 0 {
        return 1;
    }
    let w = bytes as f32 * font_size * 0.55;
    ((w / avail).ceil() as usize).max(1)
}

impl Render for MarkdownPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _t_render = crate::prof::begin();
        let _t_style = crate::prof::begin();
        let p = Rc::new(StyleParams::from_theme_with_font(cx, self.font_override));
        crate::prof::end("md_style", _t_style);
        let bg = self.bg.unwrap_or(p.bg);
        // 视口宽度已知 → 刷新统一行高提示，让竖滚动条量程从一开始就接近真实总高。
        // （首次布局/宽度变化后 gpui 会清空 size_hint，这里检测到就重建重填，并
        // 请求多画一帧让量程真正生效——下一帧量程即满、不会再通知，不会无限重画。）
        let pane_w = {
            let hw = self.h_handle.bounds().size.width.as_f32();
            if hw > 0.0 {
                hw
            } else {
                window.bounds().size.width.as_f32() * 0.5
            }
        };
        let _t_hint = crate::prof::begin();
        if self.update_height_hint(cx, pane_w) {
            cx.notify();
        }
        crate::prof::end("md_hint", _t_hint);
        // 窗口模式：按当前可见范围预物化行的窗口。必须在构造 list 闭包前完成，
        // 闭包内只读窗口数据（Rc），以避免在元素构造期改 `self`。
        if self.index.is_some() {
            let lo_vis = self.list_state.logical_scroll_top().item_ix;
            let view_h = self.list_state.viewport_bounds().size.height.as_f32();
            let hint = self.est_hint.max(20.0);
            let vis_base = ((view_h / hint).ceil() as usize).max(1);
            // 自适应过扫：余量随可见块数增长（高视口/快速滚动早日命中「窗口已覆盖」）。
            self.overdraw_margin = adaptive_overdraw(vis_base);
            let vis_n = vis_base.saturating_add(self.overdraw_margin);
            let last = self.item_count().saturating_sub(1);
            let need_lo = lo_vis.saturating_sub(self.overdraw_margin);
            let need_hi = lo_vis.saturating_add(vis_n).min(last);
            let _t_ensure = crate::prof::begin();
            self.ensure_window(need_lo, need_hi);
            crate::prof::end("md_ensure_call", _t_ensure);
        }
        let list_state = self.list_state.clone();
        let content_w = self.content_w;
        let wrap = self.wrap;
        let code_scroll = self.code_scroll.clone();
        let code_hl = self.code_hl.clone();
        let table_scroll = self.table_scroll.clone();

        if cfg!(debug_assertions) {
            let h = &self.h_handle;
            let file = std::env::temp_dir()
                .join("kilo")
                .join("mdhdebug.txt");
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file)
            {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "render: wrap={wrap} content_w={content_w:.0} max_off=({:.0},{:.0}) bounds={:?}",
                    h.max_offset().x.as_f32(),
                    h.max_offset().y.as_f32(),
                    h.bounds(),
                );
            }
        }

        // 搜索跳转高亮：只给命中**文本**着色（不吃整行）。把命中的源码行映射到渲染行，
        // 在该行渲染文本里定位查询词的字节区间，连同主题感知背景色交给 render_row。
        let search_highlight = {
            let mut hit: Option<(usize, std::ops::Range<usize>)> = None;
            if let Some((line, query)) = self
                .highlight_search
                .as_ref()
                .map(|(l, q)| (*l, q.clone()))
            {
                if let Some(row_ix) = self.row_for_line(line) {
                    let text = match &self.index {
                        None => self.rows.get(row_ix).map(|r| r.text.clone()),
                        Some(_) => {
                            // 窗口模式：先把命中块纳入窗口，再取文本
                            self.ensure_window(row_ix, row_ix);
                            if row_ix >= self.win_lo
                                && row_ix - self.win_lo < self.win_rows.len()
                            {
                                Some(self.win_rows[row_ix - self.win_lo].text.clone())
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(t) = text {
                        if let Some(range) = find_text_range(&t, &query) {
                            hit = Some((row_ix, range));
                        }
                    }
                }
            }
            let color = if hit.is_some() {
                Some(if gpui_component::Theme::global(cx).is_dark() {
                    rgba(0xf59e0b47).into()
                } else {
                    rgba(0xfef08a8c).into()
                })
            } else {
                None
            };
            hit.map(|(row_ix, range)| (row_ix, range, color.expect("命中则有背景色")))
        };

        // 行数据源：全量路径用整篇 `rows`；窗口路径用已物化的 `win_rows`（Rc，O(1) 克隆）。
        let rows = self.rows.clone();
        let win_rows = self.win_rows.clone();
        let win_lo = self.win_lo;
        let windowed = self.index.is_some();
        let content = {
            let p = p.clone();
            let code_scroll = code_scroll;
            let code_hl = code_hl;
            let table_scroll = table_scroll;
            let search_highlight = search_highlight;
            list(list_state.clone(), move |ix, _window, cx| {
                let ch = code_scroll.get(&ix).cloned();
                let chr = code_hl.get(&ix).cloned();
                let th = table_scroll.get(&ix).cloned();
                let search = search_highlight
                    .as_ref()
                    .filter(|(row_ix, _, _)| *row_ix == ix)
                    .map(|(_, range, color)| (range.clone(), *color));
                if !windowed {
                    render_row(&rows[ix], &p, wrap, ch, chr, th, search, cx)
                } else if ix >= win_lo && ix - win_lo < win_rows.len() {
                    render_row(&win_rows[ix - win_lo], &p, wrap, ch, chr, th, search, cx)
                } else {
                    // 尚未物化（跳出了当前窗口的过扫范围）：占位空元素，
                    // 滚动会触发下一帧 `ensure_window` 把窗口移过来。
                    div().size_full().into_any_element()
                }
            })
        }
        .size_full();

        // 预览核心：换行（宽随视口）或非换行（横向滚动）共用外层横向滚动容器。
        // 换行模式：宽包装层保持视口宽（正文按视口折行），超宽的代码带溢出交由 harea 统一横向滚动；
        // 非换行模式：宽包装层 = max(估算最宽内容, 视口宽)，超宽产生横向溢出。
        let inner = div()
            .id("md-list-wrap")
            .debug_selector(|| "md-list-wrap".into())
            .w(px(content_w))
            .min_w_full()
            .h_full()
            .child(content);
        // 横向滚动容器：原生 `overflow_x_scroll`，由 `h_handle` 跟踪。
        let harea = div()
            .id("md-harea")
            .debug_selector(|| "md-harea".into())
            .size_full()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.h_handle)
            .child(inner);
        let core: gpui::AnyElement = div()
            .id("md-preview")
            .debug_selector(|| "md-preview".into())
            .size_full()
            .relative()
            .bg(bg)
            .child(harea)
            .child(
                div()
                    .id("md-hscrollbar-overlay")
                    .debug_selector(|| "md-hscrollbar-overlay".into())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::horizontal(&self.h_handle)
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
            )
            .vertical_scrollbar(&list_state)
            .into_any_element();

        // 外层容器：Ctrl+滚轮缩放字号（EPUB 阅读）；相对定位承载目录浮层/面板开关。
        let root = div()
            .id("md-preview-root")
            .debug_selector(|| "md-preview-root".into())
            .size_full()
            .relative()
            .on_scroll_wheel(cx.listener(
                |this: &mut MarkdownPreview, event: &ScrollWheelEvent, _w, cx| {
                    if !event.modifiers.control {
                        return;
                    }
                    let delta_y = match event.delta {
                        ScrollDelta::Lines(lines) => lines.y,
                        ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                    };
                    let step = delta_y.clamp(-1.0, 1.0);
                    if step != 0.0 {
                        let default =
                            StyleParams::from_theme_with_font(cx, None).para_size.as_f32();
                        this.bump_font_size(step * 1.5, cx, default);
                        cx.notify();
                    }
                },
            ));

        let out = root.child(core);
        crate::prof::end("md_render", _t_render);
        out
    }
}

/// 在 `text` 内查找第一个忽略大小写命中小写 `needle` 的字节区间；未命中返回 `None`。
/// 用于搜索跳转为命中**文本**着色（非整行）。
fn find_text_range(text: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    let needle_chars = needle.chars().count();
    if needle_chars == 0 {
        return None;
    }
    let mut i = 0usize;
    while i < text.len() {
        let candidate: String = text[i..].chars().take(needle_chars).collect();
        if !candidate.is_empty() && candidate.to_lowercase() == needle {
            return Some(i..(i + candidate.len()));
        }
        let ch = text[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}
