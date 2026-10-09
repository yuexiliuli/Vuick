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

pub(crate) mod dimensions;
mod row;
pub(crate) mod style;
mod table;

/// 行内代码 pill 背景区间的统一扩展规则（预览与 WYSIWYG 共用单一行为源；
/// 定义在 `row`，作为 view 层 API 再导出给 `wysiwyg`）。
pub(crate) use row::expand_bg_over_whitespace;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use std::cell::Cell;

use std::cell::RefCell;

use gpui::{
    anchored, deferred, div, list, point, px, rgba, App, Bounds, ClipboardItem, Context, Div,
    FocusHandle, Hsla, InteractiveElement, IntoElement, KeyDownEvent, ListAlignment, ListOffset,
    ListState, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, prelude::FluentBuilder as _,
};
use ropey::Rope;
use crate::ui::scrollbar::{Scrollbar, ScrollbarHandle, ScrollbarMode};

use crate::config::limits::{
    CODE_HL_DEFER_BLOCKS, CODE_HL_DEFER_BYTES, CODE_HL_LRU_CAP, CODE_HL_PARSE_BLOCKS_PER_FRAME,
    CODE_HL_PARSE_BYTES_PER_FRAME, WINDOWED_MIN_BLOCKS, WINDOWED_OVERDRAW_BLOCKS,
    WINDOWED_OVERDRAW_FACTOR, WINDOWED_OVERDRAW_MAX,
};
use crate::config::markdown::{IMAGE_ROW_DEFAULT_H, ROW_OVERDRAW_PX, WIDTH_BUFFER_PX};
use crate::config::timing::HIGHLIGHT_MS;
use crate::markdown::parse::{
    citation_index_from_rows, parse_block_window, parse_block_window_rope, parse_to_rows,
    rows_memory_estimate, scan_blocks, scan_citations, BlockIndex, BlockMeta, CitationIndex, Row,
    RowKind, RowPayload, Toc,
};
use crate::syntax::{parse_code_lines, CodeLineRuns};

use dimensions::{est_row_width, est_row_width_meta, image_dimensions, image_dimensions_bytes};
use row::{render_row, RowRenderCtx};
use style::StyleParams;

/// EPUB 脚注悬浮命中：一行内某个 `[n]` 标记在**上一帧**的画面矩形 + 对应注释正文。
/// 由 [`row::NoteMarkers`] 在 paint 时写入共享表（窗口坐标）；鼠标移动时视图据此判 hover，
/// 点击时据此做「文内 ↔ 末尾定义」双向跳转。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NoteHit {
    /// 全局块下标（渲染行标识；同一行每次重绘覆盖旧条目）。
    pub(crate) row_ix: usize,
    /// 注记号。
    pub(crate) n: usize,
    /// 是否末尾定义标记（`[!n]`）；false = 文内引用 `[^n]`。
    pub(crate) is_def: bool,
    /// 标记矩形（窗口坐标，上一帧）。
    pub(crate) bounds: Bounds<Pixels>,
}

/// EPUB 注记渲染的共享上下文：注释文本表 + 上帧命中矩形表。
#[derive(Clone, Default)]
pub(crate) struct NoteCtx {
    /// 注记号 → 注释正文（`set_epub` 注入；空 = 非 EPUB）。
    pub(crate) notes: std::collections::BTreeMap<usize, SharedString>,
    /// 命中矩形共享表（row.rs 写、view 读）。
    pub(crate) hits: Rc<RefCell<Vec<NoteHit>>>,
}

/// 最近一帧某个可见文本行的字形命中数据：供「鼠标点选字符」反查。
#[derive(Clone)]
pub(crate) struct TextHit {
    /// 全局块下标。
    pub(crate) row_ix: usize,
    /// 显示文本（选中复制用）。
    pub(crate) text: gpui::SharedString,
    /// 文本布局（request_layout/prepaint 后已填；Rc 克隆共享，命中时可直接反查）。
    pub(crate) layout: gpui::TextLayout,
    /// 该行在窗口坐标中的起点。
    pub(crate) origin: gpui::Point<gpui::Pixels>,
}

/// 文本命中共享表（row.rs paint 写、view 鼠标处理器读）。
#[derive(Clone, Default)]
pub(crate) struct TextCtx {
    pub(crate) hits: Rc<RefCell<Vec<TextHit>>>,
}

/// 延后分片解析的一批代码块现场：`(源码代, 待解析队列)`，队列项 `(全局块下标, 语言, 代码快照)`。
type PendingCodeHl = (u64, std::collections::VecDeque<(usize, String, String)>);

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
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.map.entry(key) {
            // 已存在：更新内容并视为最近使用（移到队尾）
            e.insert(value);
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

/// 固定窗口虚拟化（预览窗口模式）：ListState 恒持这么多条目（O(1)，与块数无关）。
/// 要求 > 可见块数 + 过扫 + 重定位前移余量（2048 块 × 平均高远超任何单屏）。
/// 窗口内条目**不预铺统一高度**（全部 Unmeasured、overdraw=0）→ List 自身无可滚
/// 高度、滚轮不作动，纵向滚动完全由预览自持像素模型驱动（见 `scroll_to_px`）。
const PREVIEW_LIST_WINDOW: usize = 2048;
/// 窗口重定位时目标块之前保留的前置块数（滚动余量，减少频繁重定位/重解析窗口）。
const WINDOW_LEAD_BLOCKS: usize = 96;
/// 块高抽样目标（块）：`stride = max(1, 块数 / 目标)`。换宽/字号变化时重建
/// 抽样块高模型 O(N/stride)（4k 次 `index_block_height` 纯算术 < 1ms），
/// 替代原先全量 O(N) 的 20~40ms 平均行高估算。
const HEIGHT_SAMPLE_TARGET: usize = 4096;
/// 超过该块数才启用块高抽样（否则逐块精确：几十万块 ≈ 几 ms，仅换源/换宽发生）。
const HEIGHT_FULL_BLOCKS_MAX: usize = 262_144;

/// 预览的原始文本源。窗口级懒解析在滚动/重解析时要反复对**原文**切片，
/// 因此预览侧保存的源可以是：
/// - [`Self::Str`]：小文档/一次性喂入的 `SharedString`；
/// - [`Self::Rope`]：编辑器正文 rope 的 O(1) 克隆（打字同步的主路径）——
///   ropey 2.0 `Rope::clone` 共享同一棵根节点 Arc（引用计数 +1），不拷贝文本，
///   快照可安全交后台线程。
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
                use crate::editor::rope_ext::RopeExt;
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
    /// 目录：文档内标题行的 (行号, 级别, 文本)。分段持久化（[`Toc`]）与索引共享同一
    /// 份标题数据：源同步 O(1) 引用共享 + 后缀行号随编辑整体平移，侧边栏按需物化。
    toc: Toc,
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
    /// EPUB 阅读排版模式：标题居中、正文首行缩进 2 字符、注记悬浮卡片（由 [`Self::set_epub`] 开）。
    epub_mode: bool,
    /// EPUB 注释共享上下文（注记悬浮卡片用；非 EPUB 为空表）。
    note_ctx: Rc<NoteCtx>,
    /// 当前悬浮的引用/注记标记（`Some(hit)` 时渲染详情卡片）；鼠标离开标记矩形即清。
    /// 存整条 [`NoteHit`]（含 is_def / bounds）而非只有注记号：卡片既能定位到当前
    /// 悬浮的标记，又能在 EPUB 注记表缺失时按 `cit.details` 取引用详情正文。
    hover_hit: Option<NoteHit>,
    /// 引用「文内 ↔ 末尾定义」双向跳转索引（重解析时作废）。窗口模式在首次交互时
    /// **懒重扫**整篇源码（见 [`Self::ensure_citations`]），不打字同步热路径。
    cit: CitationIndex,
    /// 源码代（每次 [`Self::set_source_with_index`] 递增）；`cit` 落后于它即需重建。
    cit_gen: u64,
    /// `cit` 实际构建到的源码代。
    cit_built_gen: u64,
    /// 点击跳转后要加亮的 (渲染行, 注记号, 是否定义)；滚动定位后由 render 给该行的
    /// 对应 `[n]` 标记加亮，[`HIGHLIGHT_MS`] 后自动消失。
    marker_hl: Option<(usize, usize, bool)>,
    /// 加亮代序列号：每次发起/清除跳转加亮 +1，定时器只清除同代（防旧定时器误清新亮）。
    marker_hl_seq: u64,
    /// 文本命中表（最近一帧可见行）：鼠标点选字符用。
    text_ctx: Rc<TextCtx>,
    /// 键盘焦点句柄（鼠标点选 + Ctrl+C 复制需要预览获得焦点）。
    focus_handle: FocusHandle,
    /// 文本选择（对数序，`((起始行, 起始字节), (结束行, 结束字节))`）；None = 无选择。
    selection: Option<((usize, usize), (usize, usize))>,
    /// 拖拽起点（行, 字节）；拖拽中 = Some，落下后 = None（保留 selection）。
    select_anchor: Option<(usize, usize)>,
    /// 统一行高提示（px）：宽度感知的平均行高估算，用于 `ListState::with_uniform_item_height`，
    /// 让竖滚动条量程从头一开始就接近真实总高（不用一次性排版整篇）。
    est_hint: f32,
    /// 生成 `est_hint` 时的视口宽度（px）；<0 表示尚未按宽度估算（需要重算）。
    est_hint_width: f32,
    /// 抽样块高模型对应的**块数**：与 `item_count()`（当前块索引）不一致（换行/删改
    /// 改变块数）时在 render 重建模型；打字不改块数则模型跨键保留（省掉每键 O(N) 重建）。
    height_model_n: usize,
    /// 上一次 `update_height_hint` 收到的渲染宽度（px）：连续宽度变化（拖分屏）期间
    /// 做重挂去抖；宽度安定（<2px 抖动）后坍缩**立即**重挂自愈，不受 120ms 精确估算
    /// 节流影响（否则打开/换宽后的前 120ms 里滚动条量程坍缩，立刻拖动会拖不到底）。
    last_render_width: f32,
    /// 上次「精确估算全篇平均行高」的时刻（px）：换宽/拖分屏期间宽度逐帧变化，
    /// 精确估算对 16 万块是 O(n)（~30ms），不能每帧重算 → 用与重挂同频的冷却。
    last_hint_estimate: std::time::Instant,
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
    /// 固定窗口虚拟化（窗口模式）：ListState 窗口首行的**绝对块下标**。
    /// ListState 只持 [`PREVIEW_LIST_WINDOW`] 条，渲染闭包把相对 item_ix 抬成
    /// 绝对块 `= win_base + ix` 取行；目标滚出窗口即重定位本字段并 `reset`。
    win_base: usize,
    /// 用户纵向滚动目标（px，全篇绝对坐标），与纵向滚动条句柄共享
    /// （`Rc<Cell<f32>>`）。滚动条拖动只写这里，render 逐帧按它重定位窗口。
    desired_scroll: std::rc::Rc<Cell<f32>>,
    /// 全篇块高（px）抽样模型（窗口模式；按当前 宽/换行/字号 由 [`Self::rebuild_height_model`]
    /// 计算，见 [`HEIGHT_SAMPLE_TARGET`]）。`height_pref[i]` = 第 i 组**采样块**的起始 px，
    /// 第 i 组覆盖块 `[i*stride, min((i+1)*stride, n))`、每组近似等高于组首块
    /// `block_h[i]`。用于「像素↔块」确定性换算（窗口重定位 + 纵向量程 + 滚轮）。
    block_h: Vec<f32>,
    height_pref: Vec<f32>,
    height_stride: usize,
    /// Ctrl+滚轮缩放字号时的滚动锚点 `(块号, 块内比例 0..1)`：字号变化高度模型
    /// 重建后，据此把缩放前视口顶部的内容点放回原位（同 PDF 视口锚定）。
    /// `None` = 无待恢复的锚点；由字号变化捕获、render 重建模型后消费。
    zoom_anchor: Option<(usize, f32)>,
    /// 大跳转的**延后分片解析**现场：触发时源码代 + 待解析代码块队列
    /// `(全局块下标, 语言, 代码文本快照)`。建房时新进代码块文本累计超
    /// [`CODE_HL_DEFER_BYTES`] 则入队、本帧先纯文本/继承上色；后续帧由
    /// `dispatch_pending_code_hl` 按字节预算逐批解析并入 `code_hl`/LRU（见
    /// [`CODE_HL_PARSE_BYTES_PER_FRAME`]）。源码代不符（编辑/换源）即作废。
    pending_code_hl: Option<PendingCodeHl>,
    /// 上一帧**窗口顶部块**（窗口模式用；`None` = 尚未建房）。连续滚动（拖滚动条/
    /// 快速滚轮）期窗口顶部逐帧变化 → `prepare_render` 判定「扫动中」，跳过
    /// `dispatch_pending_code_hl`（零 syntect 帧、滑块/内容紧跟鼠标）；停稳
    /// （相邻帧窗口一致）再派发补齐着色——与编辑器 06f6013「滚动持续时延后上色、
    /// 停稳后补齐」同手法。不能用 `is_scrollbar_dragging()`：窗口模式 `apply_desired`
    /// 重建 ListState 会清掉冻结值，且窗口模式本不冻结。
    last_sweep_lo: Option<usize>,
}

impl MarkdownPreview {
    pub fn new(cx: &mut App) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, px(ROW_OVERDRAW_PX)).measure_all();
        Self {
            rows: Rc::new(Vec::new()),
            list_state,
            bg: None,
            content_w: 0.0,
            h_handle: ScrollHandle::new(),
            wrap: false,
            font_override: None,
            toc: Toc::default(),
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
            epub_mode: false,
            note_ctx: Rc::new(NoteCtx::default()),
            hover_hit: None,
            cit: CitationIndex::default(),
            cit_gen: 0,
            cit_built_gen: 0,
            marker_hl: None,
            marker_hl_seq: 0,
            text_ctx: Rc::new(TextCtx::default()),
            focus_handle: cx.focus_handle(),
            selection: None,
            select_anchor: None,
            est_hint: 0.0,
            est_hint_width: -1.0,
            height_model_n: 0,
            last_render_width: -1.0,
            last_hint_estimate: std::time::Instant::now(),
            hint_applied: None,
            hint_recovered: false,
            index: None,
            src_buf: None,
            win_lo: 0,
            win_rows: Rc::new(Vec::new()),
            win_base: 0,
            desired_scroll: std::rc::Rc::new(Cell::new(0.)),
            block_h: Vec::new(),
            height_pref: Vec::new(),
            height_stride: 1,
            zoom_anchor: None,
            pending_code_hl: None,
            last_sweep_lo: None,
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

    /// 带视口锚定的**绝对**字号设置（宿主 `app` 全局字号推进的统一落点）。
    /// 改字号前先记录视口锚点，高度模型重建后视口顶部内容点不随之跳走——与缩放时的
    /// 相对增减共用同一套锚定机制，但这里直接设绝对值，使预览与编辑区 / WYSIWYG 共用
    /// 同一全局字号。`size` 应为已按 `FONT_MIN/FONT_MAX` 约束的全局字号。
    pub fn set_font_size_anchored(&mut self, size: f32, cx: &mut App) {
        if self.font_override.is_some_and(|cur| (cur - size).abs() < 0.001) {
            return;
        }
        // 缩放前先记录视口锚点：字号变化高度模型重建后，视口顶部内容点不随之跳走。
        self.capture_zoom_anchor();
        self.set_font_override(Some(size), cx);
    }

    /// 捕获当前视口顶部的内容点锚点 `(块号, 块内比例)`。比例 = 块内偏移 ÷ 块高，
    /// 与块高无关、跨字号稳定（缩放后按新块高乘回即可原比例放回顶部）。
    fn capture_zoom_anchor(&mut self) {
        let top_px = self.scroll_top_px();
        let (block, frac) = self.block_of_px(top_px);
        let h = self.block_height_at(block);
        let ratio = if h > 0.0 { (frac / h).clamp(0.0, 1.0) } else { 0.0 };
        self.zoom_anchor = Some((block, ratio));
    }

    /// 第 `block` 块的渲染高度（px；窗口模式 = 采样模型组高近似，小文档 = 平均高）。
    fn block_height_at(&self, block: usize) -> f32 {
        if self.height_pref.is_empty() {
            return self.est_hint.max(1.0);
        }
        let stride = self.height_stride.max(1);
        let g = (block / stride).min(self.block_h.len().saturating_sub(1));
        self.block_h.get(g).copied().unwrap_or(1.0).max(1.0)
    }

    /// 把缩放前的视口锚点恢复为新的滚动目标 px。必须在高度模型 / 平均行高已按
    /// 新字号重建后调用（窗口模式在 `rebuild_height_model` 后、小文档在
    /// `update_height_hint` 后），否则按旧模型换算仍会跳。恢复后清空锚点。
    fn restore_zoom_anchor(&mut self) {
        let Some((block, ratio)) = self.zoom_anchor.take() else {
            return;
        };
        let new_px = self.px_of_block(block) + ratio * self.block_height_at(block);
        if self.index.is_some() {
            self.desired_scroll.set(new_px);
        } else {
            self.scroll_to_px(new_px);
        }
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
                    .map(|b| est_row_width_meta(&b, &p))
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

    /// 开启/关闭 EPUB 阅读排版并注入脚注表（`load_epub` 调用）。
    /// 开启后：标题居中、正文首行缩进 2 字符、`[n]` 注记悬停显示卡片。
    pub fn set_epub(&mut self, on: bool, notes: std::collections::BTreeMap<usize, String>, cx: &mut App) {
        self.epub_mode = on;
        // 换新注记上下文（命中表也随之清空重来）。
        self.note_ctx = Rc::new(NoteCtx {
            notes: notes
                .into_iter()
                .map(|(k, v)| (k, SharedString::from(v)))
                .collect(),
            hits: Rc::new(RefCell::new(Vec::new())),
        });
        self.hover_hit = None;
        self.remeasure_dimensions(cx);
        self.list_state.remeasure();
        self.invalidate_height_hint();
    }

    /// EPUB 轻量视口分页读数：`(当前页, 总页数)`（1 基）——按「内容总高 / 视口高」
    /// 切页，当前页以视口**底部到达**的页计（clamp 到 `[1, 总页数]`）。口径与纵向
    /// 滚动条量程同源（窗口模式 = `desired_scroll`＋抽样块高模型总高；小文档 = 实测
    /// ListState px 滚动＋实测总高，见 `total_h`），故**拖到底必然「第 N/N 页」**、
    /// 字号调整/换宽后页码自动跟随。
    /// 非 EPUB / 无源返回 `None`（不显示页码）。纯展示读数，无翻页交互。
    pub(crate) fn viewport_page_bounds(&self) -> Option<(usize, usize)> {
        // 有源判定：小文档 `rows` 物化全量；窗口模式懒物化 `rows` 恒空，以 `index`
        // 为准（否则大 EPUB 会误判为无源而不显示页码）。
        if !self.epub_mode || (self.index.is_none() && self.rows.is_empty()) {
            return None;
        }
        let view_h = self
            .list_state
            .viewport_bounds()
            .size
            .height
            .as_f32()
            .max(1.);
        // **就绪门控**：视口尚未布局 / 小文档平均行高未估（render 节流后才估）/
        // 窗口高度模型未建（render 后才有）时返回 `None`——宁可不显示，也不让首帧
        // 乱值（视口 0 / est_hint 0 时页数会被算成假值）造成「1/45 跳 1/55」跳动。
        if view_h < 16.0 {
            return None;
        }
        if self.index.is_none() && self.est_hint <= 0.0 {
            return None;
        }
        if self.index.is_some() && self.height_pref.is_empty() {
            return None;
        }
        let content_h = self.total_h().max(view_h);
        let total = ((content_h / view_h).ceil()).max(1.) as usize;
        // 滚动 top（px，与量程同源）：
        // - 窗口模式：像素模型用户目标 `desired_scroll`；
        // - 小文档：绝对逻辑行 × 统一行高 + 行内偏移（`list_state.scroll_px_offset_*`
        //   在 summary 未就绪时常恒 0，不能用于页码）。
        let top_px = if self.index.is_some() {
            self.desired_scroll.get().max(0.)
        } else {
            // 小文档：与量程同源的**实测**滚动 px（`scroll_px_offset_for_scrollbar`，
            // 已由 `update_height_hint` 铺 uniform 高度 → summary 完备、取值为真实 px）。
            // 用估算（行×平均高）会停在估算末尾——真机实测>估算时拖到底页码永远
            // < 总页数；实测 px 保证「拖到底 = 第 N/N 页」。
            -self.list_state.scroll_px_offset_for_scrollbar().y.as_f32()
        };
        // 当前页 = 视口**底部到达**的页（底部到达页语义）：某页顶部滚进视口底边才算
        // 「翻到那页」。这与「拖到底 = 第 N/N 页」自洽——视口底 = 内容底时当前页必然
        // = 总页数；若用「顶部到达页」语义，最后不满一屏时顶部永远停在 `floor(C/v)`
        // 页、到不了总页数（真机实测「44/47」的来源之一），也无法滚过末尾补上那页。
        // `-0.5` 为亚像素护栏：恰好落在页边界时钳回前一页，不把末页顶进下一页。
        let bottom = top_px + view_h;
        let page = ((bottom - 0.5) / view_h).floor().max(0.) as usize;
        Some((page.min(total - 1) + 1, total))
    }

    /// 清空上帧注记/文本命中与悬浮状态（换源 / 换滚动位置 / 关 EPUB 排版时调用）。
    fn clear_note_state(&mut self) {
        self.note_ctx.hits.borrow_mut().clear();
        self.text_ctx.hits.borrow_mut().clear();
        self.hover_hit = None;
    }

    /// 鼠标按下：聚焦预览并开启文本点选（记录拖拽锚点 = 命中的 (行, 字符)）。
    /// 点空白处清除已有选择。**点击引用标记 `[n]` 优先**：做文内 ↔ 末尾定义双向跳转，
    /// 不进入文本点选（标记即链接）。
    fn on_preview_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        if event.button == MouseButton::Left
            && let Some(hit) = self.note_hit_at(event.position) {
                self.navigate_marker(hit, cx);
                return;
            }
        let pos = event.position;
        match self.hit_test(pos) {
            Some((row, byte)) => {
                self.select_anchor = Some((row, byte));
                self.selection = Some(((row, byte), (row, byte)));
            }
            None => {
                self.select_anchor = None;
                self.selection = None;
            }
        }
        cx.notify();
    }

    /// 命中引用标记（上一帧注记矩形共享表），返回该标记的 [`NoteHit`]。
    fn note_hit_at(&self, pos: Point<Pixels>) -> Option<NoteHit> {
        self.note_ctx
            .hits
            .borrow()
            .iter()
            .find(|h| h.bounds.contains(&pos))
            .copied()
    }

    /// 确保引用跳转索引就绪（懒建：只在源码代变化后重建；小文档走行集，大文档全源逐块扫描）。
    fn ensure_citations(&mut self) {
        if self.cit_built_gen == self.cit_gen {
            return;
        }
        self.cit = match &self.index {
            // 窗口模式：整篇源码逐块扫描（懒，仅交互到引用时；不占用打字同步热路径）。
            Some(_) => {
                let src = match &self.src_buf {
                    Some(PreviewSrc::Str(s)) => s.to_string(),
                    Some(PreviewSrc::Rope(r)) => r.to_string(),
                    None => String::new(),
                };
                scan_citations(&src)
            }
            // 小文档：从已解析行集直接取（几乎零成本）。
            None => citation_index_from_rows(&self.rows),
        };
        self.cit_built_gen = self.cit_gen;
    }

    /// 点击引用标记 `hit` 后跳转对端：
    /// - 文内引用 `[^n]` → 末尾定义 `[!n]`（引用详情）；
    /// - 末尾定义 `[!n]` → 文内引用 `[^n]`。
    ///
    /// 跳到对端所在渲染行并给该行的 `[n]` 标记加亮；无对端（孤立标记）仅清点选、不跳转。
    fn navigate_marker(&mut self, hit: NoteHit, cx: &mut Context<Self>) {
        self.ensure_citations();
        let target = if hit.is_def {
            self.cit.refs.get(&hit.n).cloned()
        } else {
            self.cit.defs.get(&hit.n).cloned()
        };
        let Some((row, _range)) = target else {
            self.select_anchor = None;
            self.selection = None;
            cx.notify();
            return;
        };
        // scroll_to_row 内部已清悬浮/命中（滚动会改变注记矩形）；随后加亮对端标记。
        self.scroll_to_row(row);
        self.set_marker_highlight(row, hit.n, !hit.is_def, cx);
        cx.notify();
    }

    /// 跳转后给目标行的 `[n]` 标记加亮（`is_def` 指明是定义还是文内引用），
    /// [`HIGHLIGHT_MS`] 后自动消失（代序号防旧定时器误清）。
    fn set_marker_highlight(&mut self, row: usize, n: usize, is_def: bool, cx: &mut Context<Self>) {
        self.marker_hl = Some((row, n, is_def));
        self.marker_hl_seq = self.marker_hl_seq.wrapping_add(1);
        let seq = self.marker_hl_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(HIGHLIGHT_MS))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.marker_hl_seq == seq {
                    view.marker_hl = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 鼠标移动：
    /// - 按住左键 → 拖拽选择（更新选择区间到当前命中的 (行, 字符)）；
    /// - 未按住 → 注记悬浮（命中上帧注记矩形 → 显示注释卡片）。
    fn on_preview_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pos = window.mouse_position();
        if event.pressed_button == Some(MouseButton::Left) {
            if let Some(anchor) = self.select_anchor
                && let Some(cur) = self.hit_test(pos) {
                    let (s, e) = if (anchor.0, anchor.1) <= (cur.0, cur.1) {
                        (anchor, cur)
                    } else {
                        (cur, anchor)
                    };
                    if self.selection != Some((s, e)) {
                        self.selection = Some((s, e));
                        cx.notify();
                    }
                }
            return;
        }
        let found = self.note_hit_at(pos);
        // 悬浮到标记：确保引用索引就绪（悬浮详情正文走 `cit.details`，非 EPUB 也有）。
        if found.is_some() {
            self.ensure_citations();
        }
        if found != self.hover_hit {
            self.hover_hit = found;
            cx.notify();
        }
    }

    /// 鼠标松开：结束拖拽（保留选择区间），不再记录锚点。
    fn on_preview_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.select_anchor.is_some() {
            self.select_anchor = None;
            cx.notify();
        }
    }

    /// Ctrl/Cmd+C：把当前选择复制到剪贴板。
    fn on_preview_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = event.keystroke.modifiers;
        if (m.control || m.platform) && !m.alt && !m.shift && event.keystroke.key == "c" {
            self.copy_selection(window, cx);
        }
        cx.notify();
    }

    /// 把当前文本选择复制到剪贴板（供 Ctrl/Cmd+C 与测试调用）。
    fn copy_selection(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// 鼠标点 → (渲染行, 字符字节偏移)：在最近一帧文本命中表里找包含该点的行，
    /// 再用该行文本布局反查字符偏移。
    fn hit_test(&self, pos: Point<Pixels>) -> Option<(usize, usize)> {
        let hits = self.text_ctx.hits.borrow();
        for hit in hits.iter() {
            let origin = hit.origin;
            let line_h = hit.layout.line_height();
            let lines = hit.layout.line_layouts();
            let Some(line) = lines.first() else {
                continue;
            };
            let h = line.size(line_h).height.as_f32();
            let w = line.width().as_f32();
            if pos.y.as_f32() >= origin.y.as_f32() - 1.0
                && pos.y.as_f32() <= origin.y.as_f32() + h + 1.0
                && pos.x.as_f32() >= origin.x.as_f32() - 1.0
                && pos.x.as_f32() <= origin.x.as_f32() + w + 1.0
            {
                let byte = hit
                    .layout
                    .index_for_position(pos)
                    .unwrap_or(0)
                    .min(hit.text.len());
                return Some((hit.row_ix, byte));
            }
        }
        None
    }

    /// 取第 `ix` 个渲染行的文本（窗口模式只能用已物化窗口行，尽力而为）。
    fn row_text_at(&self, ix: usize) -> Option<SharedString> {
        if self.index.is_some() {
            self.win_rows.get(ix - self.win_lo).map(|r| r.text.clone())
        } else {
            self.rows.get(ix).map(|r| r.text.clone())
        }
    }

    /// 当前选择的文本（跨行用「行1\n行2…」拼接）；无选择返回 None。
    fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection?;
        let mut out = String::new();
        for ix in s.0..=e.0 {
            let text = self.row_text_at(ix)?;
            let a = if ix == s.0 { s.1 } else { 0 };
            let b = if ix == e.0 { e.1 } else { text.len() };
            let a = a.min(text.len());
            let b = b.min(text.len());
            // 防御：复制路径也把字节边界吸收到字符边界（防止非对齐偏移切进 CJK 崩）。
            let r = crate::markdown::view::row::char_align_range(text.as_ref(), a..b);
            if r.end > r.start {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text[r]);
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// 悬浮卡片的显示位置（窗口坐标，位于注记矩形右下方，带边距）。
    fn note_card_point(&self, hit: &NoteHit) -> Point<Pixels> {
        point(
            px(hit.bounds.right().as_f32() + 8.0),
            px(hit.bounds.top().as_f32() + 4.0),
        )
    }

    /// 目录条目：(行号, 标题级别, 标题文本)。供侧边栏目录视图使用。
    /// 每调物化一份（O(目录数)）——仅侧边栏渲染等低频消费走它，不做每键路径。
    pub(crate) fn toc_entries(&self) -> Vec<(usize, u8, SharedString)> {
        self.toc.to_vec()
    }

    /// 滚动到指定行（目录跳转）。窗口模式走自持像素模型（瞬时重定位窗口），
    /// 小文档仍用 ListState 的 reveal。
    pub fn scroll_to_row(&mut self, ix: usize) {
        if ix >= self.item_count() {
            return;
        }
        // 滚动会改变注记矩形 → 上帧命中失效，先清悬浮。
        self.clear_note_state();
        if self.index.is_some() {
            self.scroll_to_px(self.px_of_block(ix));
        } else {
            self.list_state.scroll_to_reveal_item(ix);
        }
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
                Some(idx) => idx.blocks.at(mid).src_line as usize,
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
            self.scroll_to_row(ix);
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
        // WY_TRACE breakdown：预览数据同步段（编辑后增量装配：窗口模式懒解析 / 小文档
        // 全量重建 + 高度模型失效）。事件链里随编辑 op 结算进同一行 breakdown。
        let _t_src = crate::prof::begin();
        // 内容变了，旧的跳转高亮随之失效；代码块高亮缓存键依赖块号/内容，一并作废
        // （否则旧源的高亮会按新源的块号被误复用）。
        self.clear_highlight();
        self.marker_hl = None;
        self.marker_hl_seq = self.marker_hl_seq.wrapping_add(1);
        self.cit_gen = self.cit_gen.wrapping_add(1);
        self.code_hl_cache.clear();
        self.pending_code_hl = None;
        self.last_sweep_lo = None;
        self.clear_note_state();
        // 记录当前滚动位置：源更新后恢复到同一逻辑位置（否则大文档每次编辑同步
        // 都会重建 ListState 跳回开头）。
        let prev_scroll = self.list_state.logical_scroll_top();
        if index.blocks.len() > WINDOWED_MIN_BLOCKS {
            // ---- 大文档：窗口级懒解析 + 固定窗口虚拟化 ----
            // 整篇只保留轻量块索引（行号/类型/长度 ≈16B/行）+ 原始文本（rope COW
            // 共享、不物化全文），渲染按可见窗口惰性物化 `Row`，驻留不随文档增长。
            // ListState 恒 [`PREVIEW_LIST_WINDOW`] 条（O(1)，不再是 O(块数) 的大树）；
            // 滚动由自持像素模型（`rebuild_height_model` 抽样块高）驱动 + 窗口重定位。
            let prev_abs = self.abs_scroll_top();
            // WY_TRACE breakdown：源同步窗口分支内部拆账（toc 克隆 / 状态复位 /
            // 高度模型失效），定位 set_source 每键 ~2ms 的落点。
            let _t_toc = crate::prof::begin();
            self.toc = index.toc.clone();
            crate::prof::end("md.ss.toc", _t_toc);
            let _t_reset = crate::prof::begin();
            let _t_set = crate::prof::begin();
            self.index = Some(index.clone());
            // 直接移动（`src` 按值入参）：Rope 克隆在 ropey 2.0 是 O(节点树)（6.4MB
            // ≈0.9ms），不是 O(1) 引用共享——移动省掉每键一次整树克隆。
            self.src_buf = Some(src);
            crate::prof::end("md.ss.set", _t_set);
            let _t_drops = crate::prof::begin();
            self.win_lo = 0;
            self.win_rows = Rc::new(Vec::new());
            self.rows = Rc::new(Vec::new());
            self.code_scroll = Default::default();
            self.code_hl = Default::default();
            self.table_scroll = Default::default();
            self.list_state =
                ListState::new(PREVIEW_LIST_WINDOW, ListAlignment::Top, px(0.));
            self.win_base = 0;
            self.hint_applied = None;
            self.hint_recovered = false;
            crate::prof::end("md.ss.drops", _t_drops);
            crate::prof::end("md.ss.reset", _t_reset);
            // 高度/块宽模型：不在此失效（否则每键打字的下一帧都 O(N) 重建）。小编辑只改
            // 局部块高，模型近似仍可用；块数/宽度/字号变化由 render 里的 n/width 判据重建。
            if self.content_w <= 0.0 {
                self.remeasure_dimensions(cx);
            }
            // 恢复滚动：窗口重定位回旧绝对位置（px），render 的 apply_desired 落窗口。
            if prev_abs.0 > 0 || prev_abs.1 > 0.0 {
                self.desired_scroll.set(self.px_of_block(prev_abs.0) + prev_abs.1);
            }
            if std::env::var_os("MD_MEM_DEBUG").is_some() {
                let src_len = self.src_buf.as_ref().map_or(0, |s| s.len());
                let src_lines = self.src_buf.as_ref().map_or(0, |s| s.line_count());
                let n = index.blocks.len();
                let index_b = n * std::mem::size_of::<crate::markdown::parse::BlockMeta>();
                eprintln!(
                    "[MD_MEM_DEBUG] preview set_source(窗口模式): 源 {} B / {} 行 → {} 块, 索引≈{} KB, 固定窗口 {} 条",
                    src_len,
                    src_lines,
                    n,
                    index_b / 1024,
                    PREVIEW_LIST_WINDOW,
                );
            }
            crate::prof::end("md.set_source", _t_src);
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
        self.toc = Toc::from_vec(
            rows.iter()
                .enumerate()
                .filter_map(|(ix, r)| match r.kind {
                    RowKind::Heading(level) => Some((ix, level, r.text.clone())),
                    _ => None,
                })
                .collect(),
        );
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
        // 行高估算只在视口宽度变化时由 render 里的 `update_height_hint` 重算；这里
        // 不主动作废，避免每次同步都迫使 O(n) 重建统一高度 SumTree（大文档逐键卡顿源）。
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
        crate::prof::end("md.set_source", _t_src);
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
                && let Some((w, h)) = image_dimensions(std::path::Path::new(&resolved)) {
                    *img_w = Some(w);
                    *img_h = Some(h);
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

    /// 纵向滚动目标总量程（px，窗口模式 = 抽样块高模型总高；小文档 = **跟随
    /// ListState 实测** `max_offset + 视口`，与滚动条/拖拽/页码同源——真机实测可能
    /// 高于估算 `行数×平均高`，估算欠估会让滚轮/页码够不到真实底部）。布局未就绪
    /// （实测为 0）时回退估算，避免把 0 当总量程钳死滚动。
    fn total_h(&self) -> f32 {
        if self.index.is_some() {
            self.height_pref.last().copied().unwrap_or(0.0)
        } else {
            let measured = self.list_state.max_offset_for_scrollbar().y.as_f32()
                + self.list_state.viewport_bounds().size.height.as_f32();
            if measured >= 1.0 {
                measured
            } else {
                self.rows.len() as f32 * self.est_hint.max(1.0)
            }
        }
    }

    /// 重建全篇块高抽样模型（窗口模式专用）：O(N/stride) 纯算术，替代原先 O(N)
    /// 的 `estimated_avg_row_height`（20~40ms），换宽/换源成本降到 <1ms。
    fn rebuild_height_model(&mut self, p: &StyleParams, width: f32) {
        let _t_hm = crate::prof::begin();
        let Some(idx) = self.index.clone() else {
            return;
        };
        let n = idx.blocks.len();
        if n == 0 {
            self.block_h = Vec::new();
            self.height_pref = Vec::new();
            self.height_stride = 1;
            self.est_hint = 0.0;
            return;
        }
        let avail = (width - 24.0).max(40.0); // px_3 左右内缩，与旧估算一致
        // 抽样步长：中等文档（≤ 该阈值）**逐块精确**算高（几十万块 ≈ 几 ms，换源/换宽
        // 才发生）；超大文档才抽样（O(N/stride)，保住 <1ms）。确定性偏移抽样对「高度
        // 周期复用块」有偏差，故只在 N 大到精确成本不可接受时启用。
        let stride = if n > HEIGHT_FULL_BLOCKS_MAX {
            (n / HEIGHT_SAMPLE_TARGET).max(1)
        } else {
            1
        };
        let groups = n / stride + 1;
        let mut block_h: Vec<f32> = Vec::with_capacity(groups);
        let mut pref: Vec<f32> = Vec::with_capacity(groups + 1);
        pref.push(0.0);
        let mut si = 0usize;
        while si < n {
            let b = &idx.blocks.at(si.min(n - 1));
            let h = index_block_height(b, p, self.wrap, avail).max(1.0);
            block_h.push(h);
            let group_len = stride.min(n - si);
            pref.push(pref.last().copied().unwrap_or(0.0) + h * group_len as f32);
            si += stride;
        }
        self.block_h = block_h;
        self.height_pref = pref;
        self.height_stride = stride;
        self.est_hint = self.total_h() / n.max(1) as f32;
        self.height_model_n = n;
        self.est_hint_width = width;
        crate::prof::end("md.hm.rebuild", _t_hm);
    }

    /// 块 → 像素（抽样模型近似；未建模型退化为 平均高×块号）。
    fn px_of_block(&self, block: usize) -> f32 {
        if self.height_pref.is_empty() {
            return self.est_hint.max(1.0) * block as f32;
        }
        let stride = self.height_stride.max(1);
        let g = (block / stride).min(self.height_pref.len().saturating_sub(1));
        let base = self.height_pref[g];
        let gh = self.block_h.get(g).copied().unwrap_or(1.0);
        base + (block.saturating_sub(g * stride)) as f32 * gh
    }

    /// 像素 → (绝对块, 块内 px 偏移)（抽样模型二分；未建模型退化）。
    fn block_of_px(&self, px: f32) -> (usize, f32) {
        let n = self.item_count();
        if n == 0 {
            return (0, 0.0);
        }
        if self.height_pref.len() <= 1 {
            let hint = self.est_hint.max(1.0);
            let block = (px / hint).floor().max(0.) as usize;
            return (block.min(n - 1), (px - block as f32 * hint).max(0.));
        }
        let h = &self.height_pref;
        // 二分：最大的 i 使 h[i] <= px
        let mut lo = 0usize;
        let mut hi = h.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if h[mid] <= px {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let g = lo.saturating_sub(1).min(self.block_h.len().saturating_sub(1));
        let within = (px - self.height_pref[g]).max(0.0);
        let gh = self.block_h.get(g).copied().unwrap_or(1.0).max(1.0);
        let stride = self.height_stride.max(1);
        let k = (within / gh).floor() as usize;
        let block = (g * stride + k).min(n - 1);
        (block, within - k as f32 * gh)
    }

    /// 文本区 **绝对**滚动位置 `(绝对块, 块内 px)`（窗口模式 = base + 相对）。
    fn abs_scroll_top(&self) -> (usize, f32) {
        let s = self.list_state.logical_scroll_top();
        (self.win_base + s.item_ix, s.offset_in_item.as_f32())
    }

    /// 当前纵向滚动像素（全篇绝对坐标）。小文档按平均高近似（与纵向量程同口径）。
    fn scroll_top_px(&self) -> f32 {
        if self.index.is_some() {
            let (b, off) = self.abs_scroll_top();
            self.px_of_block(b) + off
        } else {
            let s = self.list_state.logical_scroll_top();
            self.est_hint.max(1.0) * s.item_ix as f32 + s.offset_in_item.as_f32()
        }
    }

    /// 滚到目标像素（夹到 [0, 总量−视口]）并写入共享 desired；窗口模式重定位窗口，
    /// 小文档直接交给 ListState（保持原行为）。
    fn scroll_to_px(&mut self, target_px: f32) {
        // 滚动会改变注记矩形 → 上帧命中缓存失效，先清悬浮。
        self.clear_note_state();
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        let max_px = (self.total_h() - view).max(0.0);
        let tp = target_px.clamp(0.0, max_px);
        self.desired_scroll.set(tp);
        if self.index.is_some() {
            self.apply_desired(tp);
        } else {
            let lh = self.est_hint.max(1.0);
            self.list_state.scroll_to(ListOffset {
                item_ix: (tp / lh).floor().max(0.) as usize,
                offset_in_item: px(tp - (tp / lh).floor() * lh),
            });
        }
    }

    /// 滚轮竖向滚动：dy = delta px（Lines 已乘平均高换算），负 = 向下。
    fn scroll_vertical(&mut self, dy: f32) {
        self.scroll_to_px(self.scroll_top_px() - dy);
    }

    /// 固定窗口重定位：目标块在窗口外 / 余量不足 → `reset(WINDOW)` + 移 `win_base`，
    /// 再 `scroll_to` 窗口内相对位置（O(窗口)，任意跳转瞬时可达）。
    fn apply_desired(&mut self, tp: f32) {
        if self.index.is_none() {
            return;
        }
        let (abs_block, frac) = self.block_of_px(tp);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        let vis = ((view / self.est_hint.max(1.0)).ceil() as usize).max(1);
        let rel = abs_block.saturating_sub(self.win_base);
        let in_window =
            abs_block >= self.win_base && rel + vis + WINDOW_LEAD_BLOCKS <= PREVIEW_LIST_WINDOW;
        if !in_window {
            self.win_base = abs_block.saturating_sub(WINDOW_LEAD_BLOCKS);
            self.list_state = ListState::new(PREVIEW_LIST_WINDOW, ListAlignment::Top, px(0.));
        }
        let rel = abs_block
            .saturating_sub(self.win_base)
            .min(PREVIEW_LIST_WINDOW.saturating_sub(1));
        self.list_state
            .scroll_to(ListOffset { item_ix: rel, offset_in_item: px(frac) });
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
        let cur_len = self.win_rows.len();
        let cur_hi = cur_lo.saturating_add(cur_len).saturating_sub(1);
        // 命中：当前窗口已覆盖所需区间（含上下过扫余量）→ 不重解析。
        // 注意不能要求 |want_lo-cur_lo| 很小：带余量重建后 cur_lo 本就比
        // 需求靠前 24 块，那样会每帧都判"没命中"而反复重解析。
        if !self.win_rows.is_empty() && cur_lo <= want_lo && cur_hi >= want_hi {
            return;
        }
        let _t = crate::prof::begin();
        // 首建判定：进入时窗口为空 = 打开/换源后的**第一次建房** → 同步全量解析
        // （打开即有颜色）；滚动触发的重建且新进代码块过多 → 由
        // `build_window_scroll_handles` 决定延后分片（廉价帧）。
        let first_build = self.win_rows.is_empty();
        let margin = self.overdraw_margin.min(n / 2);
        let w_lo = want_lo.saturating_sub(margin);
        let w_hi = (want_hi + margin).min(n - 1);
        let w_len = w_hi.saturating_sub(w_lo).saturating_add(1);

        // 增量扩展：拖滚动条/滚轮滚动时，新窗口基本与当前窗口重叠，只有一两段新带
        // 露出。**只解析新增带头/尾并拼接复用**，整窗重解析只在每次拖动帧都发生时
        // 逐帧重物化几百行（这是「预览拖滚动条明显滞后·需多拖几次」的主因）。
        // 条件：非首建、新旧窗口有真实重叠（同带滚动）、拼接后不超 ListState 窗口
        // 上限；不满足（首建 / 大跳转 / 重叠极小）回退整窗重解析（原路径）。
        let overlap_lo = cur_lo.max(w_lo);
        let overlap_hi = cur_hi.min(w_hi);
        let incremental = !self.win_rows.is_empty()
            && overlap_lo <= overlap_hi
            && w_len <= PREVIEW_LIST_WINDOW;
        if incremental {
            let head_band = (w_lo < overlap_lo).then(|| {
                let lo = w_lo;
                let hi = overlap_lo.saturating_sub(1);
                let mut r = match &src {
                    PreviewSrc::Str(s) => parse_block_window(s, &idx, lo, hi),
                    PreviewSrc::Rope(rp) => parse_block_window_rope(rp, &idx, lo, hi),
                };
                self.resolve_image_rows(&mut r);
                r
            });
            let tail_band = (overlap_hi < w_hi).then(|| {
                let lo = overlap_hi.saturating_add(1);
                let mut r = match &src {
                    PreviewSrc::Str(s) => parse_block_window(s, &idx, lo, w_hi),
                    PreviewSrc::Rope(rp) => parse_block_window_rope(rp, &idx, lo, w_hi),
                };
                self.resolve_image_rows(&mut r);
                r
            });
            let span_lo = if head_band.is_some() {
                w_lo
            } else {
                overlap_lo
            };
            let span_hi = if tail_band.is_some() {
                w_hi
            } else {
                overlap_hi
            };
            let span_len = span_hi.saturating_sub(span_lo).saturating_add(1);
            // 复用段（当前窗口与 [span_lo, span_hi] 的交集）用严格 slice 索引，避免
            // 越界：from/to 都按「当前窗口内相对下标」夹取。
            let cur_from = overlap_lo.saturating_sub(cur_lo).min(cur_len);
            let cur_to = overlap_hi.saturating_sub(cur_lo).min(cur_len).saturating_add(1);
            let mut rows = Vec::with_capacity(w_len);
            if let Some(r) = head_band {
                rows.extend_from_slice(&r);
            }
            rows.extend_from_slice(&self.win_rows[cur_from..cur_to.min(cur_len)]);
            if let Some(r) = tail_band {
                rows.extend_from_slice(&r);
            }
            debug_assert_eq!(rows.len(), span_len, "增量窗口拼接长度应等于跨度");
            let _ = span_len;
            self.win_lo = span_lo;
            self.win_rows = Rc::new(rows);
        } else {
            let mut rows = match &src {
                PreviewSrc::Str(s) => parse_block_window(s, &idx, w_lo, w_hi),
                PreviewSrc::Rope(r) => parse_block_window_rope(r, &idx, w_lo, w_hi),
            };
            self.resolve_image_rows(&mut rows);
            self.win_lo = w_lo;
            self.win_rows = Rc::new(rows);
        }
        self.build_window_scroll_handles(first_build);
        crate::prof::end("md_ensure_window", _t);
    }

    /// 为当前窗口建立代码/表格块的稳定横向滚动句柄与代码块语法高亮（键 = **全局块下标**，
    /// 与渲染按 `ix` 查表一致）。代码块高亮不再每次都整窗重跑 syntect：先查
    /// [`CodeHlCache`]（滚动时相邻窗口大半块重叠 → 直接复用 `Rc`，O(1)），仅新进入
    /// 窗口的块才解析并写入缓存。
    ///
    /// `first_build`：打开/换源后的首次建房 → 同步全量解析（打开即有颜色）；否则若
    /// 新进代码块文本累计超 [`CODE_HL_DEFER_BYTES`]（大跳转），延后到 `pending_code_hl`
    /// 队列由后续帧按字节预算分片补色——本帧这些块先纯文本/继承上色，帧不卡。
    fn build_window_scroll_handles(&mut self, first_build: bool) {
        let win_rows = self.win_rows.clone(); // 借 win_rows 的同时要 &mut self（缓存）
        let win_lo = self.win_lo;
        let win_hi = win_lo.saturating_add(win_rows.len());
        let mut cs_old = std::mem::take(&mut self.code_scroll);
        let mut ts_old = std::mem::take(&mut self.table_scroll);
        {
            let cs = Rc::make_mut(&mut cs_old);
            let ts = Rc::make_mut(&mut ts_old);
            // 窗口移动只给**新进窗口**的代码/表格块新建句柄，仍在窗口内的块复用旧句柄
            // —— 保留这些块的横向滚动位置（旧逻辑每次重建全部 new，纵向一滚宽代码块
            // 的横向位置即回零）。句柄键 = 全局块下标，跨窗口重叠稳定，语义不变。
            cs.retain(|ix, _| *ix >= win_lo && *ix < win_hi);
            ts.retain(|ix, _| *ix >= win_lo && *ix < win_hi);
            for (j, r) in win_rows.iter().enumerate() {
                let ix = win_lo + j;
                match r.kind {
                    RowKind::Code => {
                        cs.entry(ix).or_default();
                    }
                    RowKind::Table => {
                        ts.entry(ix).or_default();
                    }
                    _ => {}
                }
            }
        }
        // 窗口内代码行 → 高亮（先继承旧窗口中仍在窗口内的解析结果，其余按缓存优先
        // 补新进块——避免每次重建对重叠块重走一遍 LRU 查找与再插入）。
        let mut window_hl: std::collections::HashMap<usize, Rc<CodeLineRuns>> =
            std::collections::HashMap::new();
        let old_hl = std::mem::take(&mut self.code_hl);
        for (ix, runs) in old_hl.iter() {
            if *ix >= win_lo && *ix < win_hi {
                window_hl.insert(*ix, runs.clone());
            }
        }
        // 统一收口「新进代码块」：命中 LRU 直接并入；未命中记候选（语言+代码快照，
        // 供同步解析或延后队列两用，避免为计数多串一遍块文本）。
        let mut candid: Vec<(usize, String, String)> = Vec::new();
        let mut candid_bytes = 0usize;
        for (j, r) in win_rows.iter().enumerate() {
            let ix = win_lo + j;
            if window_hl.contains_key(&ix) || r.kind != RowKind::Code {
                continue;
            }
            if let Some(hit) = self.code_hl_cache.get(ix) {
                window_hl.insert(ix, hit);
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
            candid_bytes += code.len();
            candid.push((ix, lang.to_owned(), code));
        }
        if !first_build
            && !candid.is_empty()
            && (candid.len() > CODE_HL_DEFER_BLOCKS || candid_bytes > CODE_HL_DEFER_BYTES)
        {
            // 大跳转：新进代码块多/大 → 本帧先不解析（块暂纯文本/继承上色），
            // 交 `dispatch_pending_code_hl` 按字节×块数预算分片补色；源码代快照防串源。
            let src_gen = self.cit_gen;
            self.pending_code_hl = Some((src_gen, candid.into()));
        } else if !candid.is_empty() {
            // 首次建房 / 小步滚动：同步解析（现状；块级 LRU 已兜底重叠复用）。
            for (ix, lang, code) in candid {
                if let Some(runs) = parse_code_lines(&lang, &code) {
                    let runs = Rc::new(runs);
                    self.code_hl_cache.insert(ix, runs.clone());
                    window_hl.insert(ix, runs);
                }
            }
        }
        self.code_scroll = cs_old;
        self.table_scroll = ts_old;
        self.code_hl = Rc::new(window_hl);
    }

    /// 延后代码块高亮的**分片补色**：每帧从队列前部取出代码块，按字节预算
    /// （[`CODE_HL_PARSE_BYTES_PER_FRAME`]）同步解析并入 `code_hl`/LRU；预算用尽或队空
    /// 即止，未清空则 `notify()` 预约下一帧续。源码代不符（编辑/换源）整队列作废。
    fn dispatch_pending_code_hl(&mut self, cx: &mut Context<Self>) {
        let Some((src_gen, mut queue)) = self.pending_code_hl.take() else {
            return;
        };
        if src_gen != self.cit_gen {
            return; // 源码已变：结果作废；换源路径已清缓存，下一次建房重新解析
        }
        let mut budget = CODE_HL_PARSE_BYTES_PER_FRAME;
        let mut budget_blocks = CODE_HL_PARSE_BLOCKS_PER_FRAME;
        while budget > 0 && budget_blocks > 0 {
            let Some((ix, lang, code)) = queue.pop_front() else {
                break;
            };
            budget = budget.saturating_sub(code.len());
            budget_blocks -= 1;
            if let Some(runs) = parse_code_lines(&lang, &code) {
                let runs = Rc::new(runs);
                self.code_hl_cache.insert(ix, runs.clone());
                // 只并入仍在当前窗口的块（窗口可能又移走；移走的由队列/缓存兜底）。
                if ix >= self.win_lo && ix - self.win_lo < self.win_rows.len() {
                    Rc::make_mut(&mut self.code_hl).insert(ix, runs);
                }
            }
        }
        if !queue.is_empty() {
            self.pending_code_hl = Some((src_gen, queue));
        }
        cx.notify();
    }

    /// 估算当前预览解析结果的驻留内存（近似，见 [`rows_memory_estimate`]）。
    /// 供运行时诊断（`MD_MEM_DEBUG=1`）输出，判断解析放大了多少倍。
    pub fn memory_estimate(&self) -> (usize, usize) {
        match &self.index {
            Some(idx) => {
                let (_, win_b) = rows_memory_estimate(&self.win_rows);
                let n = idx.blocks.len();
                let index_b = n * std::mem::size_of::<crate::markdown::parse::BlockMeta>();
                // 源：`Str` 是预览自持副本（小文档 / EPUB 全量路径）如实计入；
                // `Rope` 与编辑器共享同一棵 Arc 树（O(1) 克隆、不物化），**不重复
                // 计入**预览驻留——大 md 的「源」不是第二份文本。
                let src_b = self.src_buf.as_ref().map(|s| match s {
                    PreviewSrc::Str(s) => s.len(),
                    PreviewSrc::Rope(_) => 0,
                }).unwrap_or(0);
                (n, index_b + src_b + win_b)
            }
            None => rows_memory_estimate(&self.rows),
        }
    }

    /// 测试探针：窗口模式下，内部持有的源码 rope 是否与 `rope` **共享同一棵 Arc 树**
    /// （ropey 2.0 `Rope` 是 Arc 树、`Clone` O(1)；算子来自 `extra::esoterica`，逐根
    /// `Arc::ptr_eq`）。成立 → 预览不另持一份源文本（大 md 的「+1× 源」实为共享句柄，
    /// 非副本）。仅测试用。
    #[cfg(test)]
    pub(crate) fn test_src_rope_shared_with(&self, rope: &Rope) -> bool {
        match &self.src_buf {
            Some(PreviewSrc::Rope(r)) => ropey::extra::esoterica::ropes_are_instances(r, rope),
            _ => false,
        }
    }

    /// 测试探针：取窗口模式内部持有的源码文本（O(n) 物化，仅测试 / COW 快照核验）。
    #[cfg(test)]
    pub(crate) fn test_src_text(&self) -> Option<String> {
        match &self.src_buf {
            Some(PreviewSrc::Rope(r)) => Some(r.to_string()),
            Some(PreviewSrc::Str(s)) => Some(s.to_string()),
            None => None,
        }
    }

    /// 纵向量程总高（px）= `max_offset_for_scrollbar().y + 视口高`。
    /// 统一高度提示（精确平均高）生效时 ≈ 全篇块高之和；抽样偏差/坍缩时失真。
    /// 诊断用：配合 `MD_OPEN_DEBUG` 校验“拖到底不到底”的量程是否精确。
    pub(crate) fn vertical_total_px(&self) -> f32 {
        if self.index.is_some() {
            // 窗口模式：量程 = 抽样块高模型总高（确定性，与固定窗口/未测量无关）。
            self.total_h()
        } else {
            self.list_state.max_offset_for_scrollbar().y.as_f32()
                + self.list_state.viewport_bounds().size.height.as_f32()
        }
    }

    /// 诊断：当前统一行高提示值（px）。应 ≈ 全篇块的精确平均行高（>0）。
    /// 测试：窗口模式绝对滚动顶 `(绝对块, 块内 px)`。
    #[cfg(test)]
    pub(crate) fn test_abs_scroll_top(&self) -> (usize, f32) {
        self.abs_scroll_top()
    }

    /// 诊断：当前统一行高提示值（px）。应 ≈ 全篇块的精确平均行高（>0）。
    #[cfg(test)]
    pub(crate) fn test_est_hint(&self) -> f32 {
        self.est_hint
    }

    /// 测试：暴露 ListState（共享 Rc）——测试拖拽路径（start_drag 重铺 + 冻结 + 拖底）。
    #[cfg(test)]
    pub(crate) fn test_list_state(&self) -> ListState {
        self.list_state.clone()
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
        self.toc = Toc::default();
        self.code_scroll = Default::default();
        self.code_hl = Default::default();
        self.code_hl_cache.clear();
        self.overdraw_margin = WINDOWED_OVERDRAW_BLOCKS;
        self.last_hint_apply = None;
        self.table_scroll = Default::default();
        self.highlight_search = None;
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        self.marker_hl = None;
        self.marker_hl_seq = self.marker_hl_seq.wrapping_add(1);
        self.cit = CitationIndex::default();
        // 代推进：后续 set_source 会再推进并自然失效；空视图置「已构建」防空扫。
        self.cit_gen = self.cit_gen.wrapping_add(1);
        self.cit_built_gen = self.cit_gen;
        self.est_hint = 0.0;
        self.est_hint_width = -1.0;
        self.hint_applied = None;
        self.hint_recovered = false;
        self.index = None;
        self.src_buf = None;
        self.win_lo = 0;
        self.win_rows = Rc::new(Vec::new());
        self.win_base = 0;
        self.desired_scroll = std::rc::Rc::new(Cell::new(0.));
        self.block_h = Vec::new();
        self.height_pref = Vec::new();
        self.height_stride = 1;
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
        // 小文档恒走 rows 逐行口径（行类型精确）；只有**大文档/窗口模式**（rows 懒物化
        // 为空）才走块索引口径——避免「首帧 index 在场、随后丢弃」导致平均行高在两种
        // 口径间跳变（EPUB 实测 77.6 ↔ 52，量程/总页数随之抖动）。
        let (n, total) = match (&self.index, self.rows.is_empty()) {
            (Some(idx), true) => {
                if idx.blocks.is_empty() {
                    return p.body_row_hint().as_f32();
                }
                // 大文档块索引十几万条：**精确求和全篇块高**（不抽样）。滚动条总长 =
                // 块数 × 平均高，只要平均高精确，总长就精确（与分布无关），拖到底必然
                // 到达真实末尾——抽样会把偏差写进量程，欠估 → 底部内容拖不到。
                // 成本 O(n) 纯算术（16 万块 ≈ 20~40ms），仅在换源/视口宽变化（120ms
                // 节流）时重算一次；`update_height_hint` 里已做节流。
                let n = idx.blocks.len();
                let total = idx
                    .blocks
                    .iter()
                    .map(|b| index_block_height(&b, p, wrap, avail))
                    .sum::<f32>();
                (n, total)
            }
            _ => {
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
        // gpui 的 `max_offset_for_scrollbar` = `max_scroll_offset()`，优先返回
        // **拖拽开始时冻结的** `scrollbar_drag_start_height`（见 list.rs）。若内置竖向
        // 滚动条在量程坍缩时收到过按下而没正常松手（或初始化路径留下的），冻结值会
        // 一直压在旧的小值上 → 量程/滚动永远恢复不了（“拖到底不到底”的强力元凶）。
        // 当前**没在拖拽**时清除陈旧冻结值，让量程回到真实 summary。真正拖拽中不碰。
        if !self.list_state.is_scrollbar_dragging() {
            self.list_state.scrollbar_drag_ended();
        }
        let n = self.item_count();
        if n == 0 {
            return false;
        }
        let width_changed = (self.est_hint_width - width).abs() >= 2.0;
        if width_changed {
            // 精确估算全篇平均行高是 O(n)（16 万块 ≈ 20~40ms）：换宽/拖分屏期间宽度
            // 逐帧变化，与重挂统一提示同频节流，避免每帧重算卡顿（量程短暂滞后、松手
            // 即收敛）。**首次建立**（est_hint_width<0，换源/初始化）不节流。
            let now = std::time::Instant::now();
            let interval = crate::config::timing::HINT_REAPPLY_INTERVAL_MS as u128;
            let allowed = self.est_hint_width < 0.0
                || now.duration_since(self.last_hint_estimate).as_millis() >= interval;
            if allowed {
                let p = StyleParams::from_theme_with_font(cx, self.font_override);
                self.est_hint = self.estimated_avg_row_height(&p, width);
                self.est_hint_width = width;
                self.last_hint_estimate = now;
            }
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
        if summary_h >= expected * 0.9 {
            // 量程健康：清除「重挂保护」痕迹——宽度安定后若提示再被清空，要立即可自愈，
            // 不能残留 `hint_recovered=true` 永远卡死不重挂（“拖到底不到底”的持久根因）。
            self.hint_recovered = false;
            if self.hint_applied == Some((n, hint)) {
                return false;
            }
            // 提示值变了但量程仍健康：直接更新记忆，不重建（省一次 O(n)）。
            self.hint_applied = Some((n, hint));
            self.last_hint_apply = Some(std::time::Instant::now());
            return false;
        }
        // 坍缩（size_hint 被 gpui 按换宽清空，或布局把窗口外条目戳上陈旧的小 size_hint）
        // → 重铺统一高度提示。**必须用 `reset_with_uniform_height` 强制重铺**：它先
        // `reset`（splice 成全新 Unmeasured{None}）再统一填高，覆盖任何陈旧戳值——
        // `with_uniform_item_height` 用 `unwrap_or` 会保留陈旧戳值（实测窗口外条目被
        // 戳成 height≈0，量程永远恢复不了）。滚动位置先保存再恢复（reset 会清零）。
        // 连续宽度变化（拖分屏，`last_render_width` 逐帧跳）中去抖：刚重挂过就被同帧
        // prepaint 清掉属于常态，每帧 O(n) 是卡顿源；宽度安定后坍缩立即重铺一次。
        let width_active = (self.last_render_width - width).abs() >= 2.0;
        self.last_render_width = width;
        // 宽度逐帧变化期间刚铺过又立刻被清 → 去抖那几帧（避免每帧 O(n)）。**仅在
        // 「宽仍在动」且「刚铺过」时跳过一次**；稳定宽度（打开/静止）下绝不因去抖拒铺
        // （否则打开 EPUB 量程保持坍缩、必须拖一次才自愈——“无法一次滚到底”根因）。
        if self.hint_recovered
            && width_active
            && self.last_hint_apply.is_some_and(|t| {
                (t.elapsed().as_millis() as u64) < crate::config::timing::HINT_REAPPLY_INTERVAL_MS
            })
        {
            return false;
        }
        let prev_scroll = self.list_state.logical_scroll_top();
        self.list_state.reset_with_uniform_height(n, px(hint));
        self.list_state.scroll_to(prev_scroll);
        self.last_hint_apply = Some(std::time::Instant::now());
        self.hint_applied = Some((n, hint));
        self.hint_recovered = true;
        true
    }

    /// 单调**上修** `content_w` 到能容纳当前窗口里最宽的行（只增不减）。
    /// 文本编辑时 `content_w` 不重算全局最大行（避免 List 宽度抖动 → gpui 清空全部
    /// size_hint → O(n) 重建统一高度提示，正是大文档逐键卡顿源）；这里按已物化窗口
    /// 的行宽上限补齐横向溢出即可，更宽的行滚入窗口时再上修。真正的回拨只发生在
    /// 换行 / 字号 / 换源重建（`remeasure_dimensions`）。
    fn bump_content_width_to_cover(&mut self, p: &StyleParams) {
        if self.wrap || self.win_rows.is_empty() {
            return;
        }
        let mut w = self.content_w.max(0.0);
        for r in self.win_rows.iter() {
            let rw = est_row_width(r, p) + WIDTH_BUFFER_PX;
            if rw > w {
                w = rw;
            }
        }
        self.content_w = w;
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

impl MarkdownPreview {
    /// 渲染期预铺：刷新统一行高/窗口模型量程，并预物化可见窗口（构造 list 闭包前；
    /// 闭包内只读窗口数据，避免在元素构造期改 `self`）。
    fn prepare_render(&mut self, p: &StyleParams, pane_w: f32, cx: &mut Context<Self>) {
        let _t_hint = crate::prof::begin();
        if self.index.is_some() {
            // 窗口模式：重建（如失效）抽样块高模型 + 把固定窗口对齐到用户目标
            // （滚动条拖动只写 desired；滚轮/跳转的 scroll_to_px 已同步，此处幂等兜底）。
            // 失效判据 = 未建/宽度变化/**块数变化**：打字（块数不变、宽度不变）跳过
            // 重建、跨键复用模型——否则每键 O(N) 全篇块高重算（6.4MB 实测 ~2.1ms）。
            if self.est_hint_width < 0.0
                || (self.est_hint_width - pane_w).abs() >= 2.0
                || self.item_count() != self.height_model_n
            {
                self.rebuild_height_model(p, pane_w);
            }
            // 字号缩放后：模型已按新字号重建，把缩放前的视口锚点换算出新的滚动目标，
            // 再落窗口（在 apply_desired 之前），保证 Ctrl+滚轮放大/缩小不带动页面滚动。
            self.restore_zoom_anchor();
            self.apply_desired(self.desired_scroll.get());
        } else {
            // 小文档：原来的统一高度提示量程路径不变。
            if self.update_height_hint(cx, pane_w) {
                cx.notify();
            }
            // 字号缩放后：平均行高已按新字号刷新 → 把视口锚点恢复为新的滚动位置。
            self.restore_zoom_anchor();
            // 滚动条 thumb 与内容统一经共享 `desired`（px）：
            // - **拖拽中**（`is_scrollbar_dragging` 冻结期）：set_offset 已同步写
            //   desired + 落到 ListState；这里兜底再落一次，保证同帧内 update_height_hint
            //   重铺/恢复滚动后两者仍一致。thumb 读 desired ⇒ 1:1 跟手，绝不因标签
            //   逐帧重测量而滑开。
            // - **非拖拽**（换轮/目录跳转/测量自愈）：从 ListState 实测 px 回填
            //   desired，让 thumb 与内容保持一致。
            if self.list_state.is_scrollbar_dragging() {
                self.list_state.set_offset_from_scrollbar(point(
                    px(0.),
                    px(-self.desired_scroll.get()),
                ));
            } else {
                let cur = -self.list_state.scroll_px_offset_for_scrollbar().y.as_f32();
                self.desired_scroll.set(cur.max(0.));
            }
        }
        crate::prof::end("md_hint", _t_hint);

        if self.index.is_some() {
            let (lo_vis, _off) = self.abs_scroll_top();
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
            // 连续滚动（拖滚动条/快速滚轮）期窗口顶部逐帧变化 → **扫动帧不派发代码块
            // 补色**（syntect 零消耗，帧廉价、滑块/内容紧跟鼠标）；停稳（相邻帧窗口
            // 顶部一致）再派发补齐着色。这比 `is_scrollbar_dragging` 可靠：窗口模式
            // `apply_desired` 重建 ListState 会清冻结值、且本不冻结，无法作拖拽信号。
            let sweeping = self.last_sweep_lo.is_some_and(|lo| lo != lo_vis);
            self.last_sweep_lo = Some(lo_vis);
            if !sweeping {
                self.dispatch_pending_code_hl(cx);
            }
            self.ensure_window(need_lo, need_hi);
            // 文本编辑期间 content_w 保持稳定（不做全局最大行重算，避免 List 宽度抖动
            // → gpui 清空 size_hint → O(n) 重建），只在窗口出现更宽行时单调上修。
            self.bump_content_width_to_cover(p);
            crate::prof::end("md_ensure_call", _t_ensure);
        }
    }

    /// 调试：把渲染期关键量追加到 `%TEMP%/kilo/mdhdebug.txt`（仅 debug_assertions）。
    fn debug_dump_render_state(&self) {
        if cfg!(debug_assertions) {
            let h = &self.h_handle;
            let file = std::env::temp_dir().join("kilo").join("mdhdebug.txt");
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "render: wrap={} content_w={:.0} max_off=({:.0},{:.0}) bounds={:?}",
                    self.wrap,
                    self.content_w,
                    h.max_offset().x.as_f32(),
                    h.max_offset().y.as_f32(),
                    h.bounds(),
                );
            }
        }
    }

    /// 搜索跳转高亮：只给命中**文本**着色（不吃整行）。把命中的源码行映射到渲染行，
    /// 在该行渲染文本里定位查询词的字节区间，连同主题感知背景色返回。
    fn compute_search_highlight(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(usize, std::ops::Range<usize>, Hsla)> {
        let mut hit: Option<(usize, std::ops::Range<usize>)> = None;
        if let Some((line, query)) = self
            .highlight_search
            .as_ref()
            .map(|(l, q)| (*l, q.clone()))
            && let Some(row_ix) = self.row_for_line(line)
        {
            let text = match &self.index {
                None => self.rows.get(row_ix).map(|r| r.text.clone()),
                Some(_) => {
                    // 窗口模式：先把命中块纳入窗口，再取文本
                    self.ensure_window(row_ix, row_ix);
                    if row_ix >= self.win_lo && row_ix - self.win_lo < self.win_rows.len() {
                        Some(self.win_rows[row_ix - self.win_lo].text.clone())
                    } else {
                        None
                    }
                }
            };
            if let Some(t) = text
                && let Some(range) = find_text_range(&t, &query)
            {
                hit = Some((row_ix, range));
            }
        }
        let color = if hit.is_some() {
            Some(if crate::ui::theme::Theme::global(cx).is_dark() {
                rgba(0xf59e0b47).into()
            } else {
                rgba(0xfef08a8c).into()
            })
        } else {
            None
        };
        hit.map(|(row_ix, range)| (row_ix, range, color.expect("命中则有背景色")))
    }

    /// 悬浮详情卡片：鼠标停在 `[n]` 标记（文内引用 / 末尾定义皆可）上时，浮出该引用
    /// 的详情正文。正文优先用 EPUB 注记表（`set_epub` 注入）；非 EPUB 的 markdown 用
    /// `cit.details`（定义 `[!n]` 行剥掉 `[n]` 标记后的文本）。
    fn with_hover_note_card(&self, core: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        let hit = self.hover_hit;
        match hit {
            Some(hit) => {
                let text = self
                    .note_ctx
                    .notes
                    .get(&hit.n)
                    .cloned()
                    .or_else(|| self.cit.details.get(&hit.n).cloned());
                match text {
                    Some(text) => {
                        let at = self.note_card_point(&hit);
                        let card = note_card(text, at, cx);
                        core.child(
                            deferred(
                                anchored()
                                    .snap_to_window_with_margin(px(8.))
                                    .position(at)
                                    .child(card),
                            )
                            .with_priority(1),
                        )
                    }
                    None => core,
                }
            }
            None => core,
        }
    }

    /// 纵向滚动条：确定性量程句柄 + 拖拽前重铺（拖到底必到全文末尾）。
    fn vertical_scrollbar(&self, list_state: &ListState) -> Stateful<Div> {
        let n = self.item_count();
        let eh = self.est_hint.max(0.0);
        let windowed = self.index.is_some();
        let v = PreviewVerticalScrollHandle {
            list: list_state.clone(),
            content_width: self.content_w,
            content_height: if windowed { self.total_h() } else { n as f32 * eh },
            est_hint: eh,
            n,
            windowed,
            desired: self.desired_scroll.clone(),
        };
        div()
            .id("md-vscrollbar-overlay")
            .debug_selector(|| "md-vscrollbar-overlay".into())
            .absolute()
            .inset_0()
            .child(
                Scrollbar::vertical(&v)
                    .mode(ScrollbarMode::Always)
                    .viewport_from_layout(),
            )
    }
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
        // 窗口模式：刷新统一行高/窗口模型量程 + 预物化可见窗口（构造 list 闭包前，
        // 闭包内只读窗口数据，避免在元素构造期改 `self`）。
        self.prepare_render(&p, pane_w, cx);
        let list_state = self.list_state.clone();
        let content_w = self.content_w;
        // 居中阅读栏（wrap 模式共用单一来源 `config::markdown::reading_col_px`）：
        // 列宽 = min(72ch, 视口宽)——窗口窄于 72ch 时退化为占满视口，不横向超界。
        // 正文按列宽折行；宽块（代码/表格）在列内各自横向滚动（行为保持）。
        let reading_col_w = if self.wrap && crate::config::markdown::READING_COL_ENABLED {
            Some(
                crate::config::markdown::reading_col_px(f32::from(p.para_size))
                    .min(pane_w.max(1.0)),
            )
        } else {
            None
        };

        self.debug_dump_render_state();

        // 行数据源与滚动/滚动条层（各自 &self 方法，详见 `build_list_content`/`render_core`）。
        let content = self.build_list_content(p.clone(), list_state.clone(), cx);

        // 预览核心：内容上下滚动容器 + 双滚动条 + 点选/悬浮监听（见 `render_core`）。
        let core = self.render_core(content, bg, list_state, content_w, reading_col_w, cx);
        let core = self.with_hover_note_card(core, cx).into_any_element();

        // 外层容器：相对定位承载目录浮层/面板开关；窗口模式普通滚轮走自持像素滚动。
        // **Ctrl+滚轮缩放不再由预览自持**——统一入口在宿主（app::panes 的 zoom_overlay
        // → change_font_size），预览字号由宿主渲染期 set_font_override 推进，避免两处
        // 各自调字号互相打架。`set_font_size_anchored` 即宿主推进全局字号时用的带锚定
        // 绝对落点（EPUB 等外部调用方直接缩放同样走它）。
        let root = div()
            .id("md-preview-root")
            .debug_selector(|| "md-preview-root".into())
            .size_full()
            .relative()
            .on_scroll_wheel(cx.listener(
                |this: &mut MarkdownPreview, event: &ScrollWheelEvent, _w, cx| {
                    // 窗口模式：List 固定窗口自身无可滚高度（overdraw=0、不预铺高度），
                    // 纵向滚动完全由像素模型驱动（覆盖任何 List 内部滚轮残留位移）。
                    if this.index.is_some() {
                        let dy = match event.delta {
                            ScrollDelta::Lines(lines) => lines.y * this.est_hint.max(4.0),
                            ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                        };
                        if dy != 0.0 {
                            this.scroll_vertical(dy);
                            cx.notify();
                        }
                    }
                },
            ))
            .child(core);

        let out = root;
        crate::prof::end("md_render", _t_render);
        out
    }
}

impl MarkdownPreview {
    /// 行数据源 List（窗口/全量两路）内容元素：按绝对块下标取行数据 → 搜索/选区/跳转
    /// 加亮定位 → 交 `render_row`。行源只读快照在构造闭包前取好（元素构造期不改 `self`）。
    fn build_list_content(
        &mut self,
        p: Rc<StyleParams>,
        list_state: ListState,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // 搜索跳转高亮：只给命中**文本**着色（不吃整行）。把命中的源码行映射到渲染行，
        // 在该行渲染文本里定位查询词的字节区间，连同主题感知背景色交给 render_row。
        let search_highlight = self.compute_search_highlight(cx);
        let sel_color = selection_color(cx);
        let wrap = self.wrap;
        let code_scroll = self.code_scroll.clone();
        let code_hl = self.code_hl.clone();
        let table_scroll = self.table_scroll.clone();
        // 行数据源：全量路径用整篇 `rows`；窗口路径用已物化的 `win_rows`（Rc，O(1) 克隆）。
        let rows = self.rows.clone();
        let win_rows = self.win_rows.clone();
        let win_lo = self.win_lo;
        let win_base = self.win_base;
        let windowed = self.index.is_some();
        let epub = self.epub_mode;
        let note_ctx = self.note_ctx.clone();
        let text_ctx = self.text_ctx.clone();
        let selection = self.selection;
        let marker_hl = self.marker_hl;
        list(list_state, move |ix, _window, cx| {
            // 列表 item_ix 是窗口内**相对**下标：绝对块 = `win_base + ix`（行数据窗口
            // `win_rows`/`rows` 用绝对块下标）。超出已物化窗口渲染空占位。
            let abs = if windowed { win_base + ix } else { ix };
            let ch = code_scroll.get(&abs).cloned();
            let chr = code_hl.get(&abs).cloned();
            let th = table_scroll.get(&abs).cloned();
            let search = search_highlight
                .as_ref()
                .filter(|(row_ix, _, _)| *row_ix == abs)
                .map(|(_, range, color)| (range.clone(), *color));
            let row = if !windowed {
                rows.get(abs)
            } else if abs >= win_lo && abs - win_lo < win_rows.len() {
                win_rows.get(abs - win_lo)
            } else {
                None
            };
            let Some(row) = row else {
                // 尚未物化（跳出了当前窗口的过扫范围）：占位空元素，
                // 滚动会触发下一帧 `ensure_window` 把窗口移过来。
                return div().size_full().into_any_element();
            };
            // 本行在选择区间内 → 截出 (区间, 背景色)。
            let sel = selection.and_then(|((sr, sb), (er, eb))| {
                if sr > abs || abs > er {
                    return None;
                }
                let rl = row.text.len();
                let a = if sr == abs { sb } else { 0 };
                let b = if er == abs { eb } else { rl };
                let a = a.min(rl);
                let b = b.min(rl);
                (b > a).then_some((a..b, sel_color))
            });
            // 跳转加亮：本行是被定位的目标行 → 把 (注记号, 是否定义) 交给 render_row，
            // 给该行对应的 `[n]` 标记加亮。
            let marker_hl = marker_hl
                .filter(|(row, _, _)| *row == abs)
                .map(|(_, n, is_def)| (n, is_def));
            render_row(
                row,
                &RowRenderCtx {
                    p: &p,
                    wrap,
                    code_handle: ch,
                    code_hl: chr,
                    table_handle: th,
                    search,
                    epub,
                    note_ctx: &note_ctx,
                    text_ctx: &text_ctx,
                    selection: sel,
                    marker_hl,
                    row_ix: abs,
                },
                cx,
            )
        })
        .size_full()
        .into_any_element()
    }

    /// 预览核心：换行（宽随视口 / 居中阅读栏）或非换行（横向滚动）共用外层横向滚动容器
    /// （换行无阅读栏时宽包装层保持视口宽、超宽代码带由列内各自横向滚动；非换行模式 =
    /// max(最宽内容,视口宽)）+ 常显横向/纵向量程滚动条 + 点选复制/悬浮脚注监听挂载。
    ///
    /// `reading_col_w = Some(w)` 表示启用居中阅读栏（wrap 模式）：内容列宽 = `min(72ch,
    /// 视口)` 并水平居中，正文按列宽折行；代码/表格等宽块仍在列内横向滚动。`None` 走
    /// 原语义（wrap = 撑满视口 / 非 wrap = 内容宽）。
    fn render_core(
        &self,
        content: gpui::AnyElement,
        bg: Hsla,
        list_state: ListState,
        content_w: f32,
        reading_col_w: Option<f32>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let inner = match reading_col_w {
            Some(col_w) => div()
                .id("md-list-wrap")
                .debug_selector(|| "md-list-wrap".into())
                .w(px(col_w))
                .h_full()
                .child(content),
            None => div()
                .id("md-list-wrap")
                .debug_selector(|| "md-list-wrap".into())
                .w(px(content_w))
                .min_w_full()
                .h_full()
                .child(content),
        };
        // 横向滚动容器：原生 `overflow_x_scroll`，由 `h_handle` 跟踪。
        let harea = div()
            .id("md-harea")
            .debug_selector(|| "md-harea".into())
            .size_full()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.h_handle)
            // 居中阅读栏：列窄于视口 → flex 行居中；列 = 视口（窗口窄于 72ch）时无空余，
            // 居中自然退化为占满，不引入横向超界。
            .when(reading_col_w.is_some(), |d| {
                d.flex().flex_row().justify_center()
            })
            .child(inner);
        div()
            .id("md-preview")
            .debug_selector(|| "md-preview".into())
            .size_full()
            .relative()
            .bg(bg)
            // 鼠标点选 + Ctrl/Cmd+C 复制：预览获得键盘焦点，拖拽选择文本。
            .track_focus(&self.focus_handle)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_preview_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_preview_mouse_up))
            .on_key_down(cx.listener(Self::on_preview_key_down))
            // 悬浮脚注命中：鼠标移动 → 判定「上帧注记矩形」→ 显示注释卡片。
            .on_mouse_move(cx.listener(Self::on_preview_mouse_move))
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
            // 纵向滚动条：确定性量程句柄 + 拖拽前重铺（拖到底必到全文末尾）。
            .child(self.vertical_scrollbar(&list_state))
    }
}

/// 文本选择的背景色（半透明蓝，浅/暗主题各一版；与搜索高亮同法，非主题强调色）。
/// WYSIWYG(`markdown/wysiwyg`) 也共用同一款，保证「选择区背景色」全局一致。
pub(crate) fn selection_color(cx: &App) -> Hsla {
    if crate::ui::theme::Theme::global(cx).is_dark() {
        gpui::rgba(0x4d6bfe61).into()
    } else {
        gpui::rgba(0x4d6bfe42).into()
    }
}

/// EPUB 脚注悬浮卡片：主题配色的 popover 样小卡片，展示一条注释正文。
fn note_card(text: SharedString, _at: Point<Pixels>, cx: &App) -> gpui::AnyElement {
    let theme = crate::ui::theme::Theme::global(cx);
    let colors = theme.colors;
    let card_bg: Hsla = colors.popover_bg.into();
    let border: Hsla = colors.border.into();
    let fg: Hsla = colors.fg.into();
    div()
        .debug_selector(|| "md-note-card".into())
        .bg(card_bg)
        .border_1()
        .border_color(border)
        .rounded_md()
        .p_3()
        .max_w(px(460.))
        .child(
            div()
                .whitespace_normal()
                .text_size(px(12.5))
                .text_color(fg)
                .child(text),
        )
        .into_any_element()
}

/// 在 `text` 内查找第一个忽略大小写命中小写 `needle` 的字节区间；未命中返回 `None`。
/// 用于搜索跳转为命中**文本**着色（非整行）。
/// `text` 内 `needle`（须已小写）**首次**出现的字节区间（逐字符取等长候选小写比较，
/// 正确处理 UTF-8 多字节）。WYSIWYG 搜索跳转高亮（`WyEditor`）与预览共用。
pub(crate) fn find_text_range(text: &str, needle: &str) -> Option<std::ops::Range<usize>> {
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

/// 预览纵向滚动条的「确定性量程」句柄（与编辑器 `VerticalScrollHandle` 同法）。
/// - **窗口模式**：量程 = 抽样块高模型总高（`content_height`，与窗口大小无关），
///   拖拽只写共享 `desired`（px，render 逐帧 `apply_desired` 落窗口重定位）——
///   确定性、无坍缩、拖到底必到全文末尾。
/// - **小文档**：量程跟随 `list` 自身（实时/冻结 summary + 视口），与 gpui 内置
///   ListState 滚动条同口径；拖拽前若检测到真坍缩，先 O(n) 统一重铺再冻结（原路径）。
///
/// 滚轮滚动由容器级处理器驱动（窗口模式），与本滚动条无关。
#[derive(Clone)]
struct PreviewVerticalScrollHandle {
    list: ListState,
    content_width: f32,
    content_height: f32,
    est_hint: f32,
    n: usize,
    windowed: bool,
    desired: std::rc::Rc<Cell<f32>>,
}

impl ScrollbarHandle for PreviewVerticalScrollHandle {
    fn viewport_bounds(&self) -> gpui::Bounds<gpui::Pixels> {
        self.list.viewport_bounds()
    }

    fn offset(&self) -> gpui::Point<gpui::Pixels> {
        if self.windowed {
            // 用「用户目标」而非固定窗口的实际位置：窗口只是渲染切片，量程是全文的。
            point(px(0.), px(-self.desired.get()))
        } else {
            // 小文档同样经共享 `desired`（px）表达 thumb —— **不读 ListState 实测树**：
            // 拖拽中标签会逐帧重测量（真实行高替换统一提示）→ 同一逻辑位置映射的 px
            // 逐帧漂移 → thumb 从抓点滑开、显得「不跟鼠标」。用 desired 作轴则 thumb
            // 恒 1:1，内容由 prepare 每帧按 desired 落位（见 prepare_render）。
            point(px(0.), px(-self.desired.get()))
        }
    }

    fn set_offset(&self, offset: gpui::Point<gpui::Pixels>) {
        let view = self.list.viewport_bounds().size.height.as_f32();
        if self.windowed {
            let off = (-offset.y.as_f32()).max(0.).min((self.content_height - view).max(0.));
            self.desired.set(off);
        } else {
            // 小文档：写共享 desired（thumb 轴），并立即落到 ListState（内容跟随）。
            // 上限 = 实测可滚上限（拖拽中被冻结 → 稳定且准确，拖到底必到真实末尾），
            // 不用 `content_height`（n×est_hint 估算，真机可能欠估 → 拖不到底）。
            let max_off = self.list.max_offset_for_scrollbar().y.as_f32();
            let off = (-offset.y.as_f32()).max(0.).min(max_off.max(0.));
            self.desired.set(off);
            self.list.set_offset_from_scrollbar(point(px(0.), px(-off)));
        }
    }

    fn content_size(&self) -> gpui::Size<gpui::Pixels> {
        if self.windowed {
            // 窗口模式：量程 = 抽样块高模型总高（确定性，与列表测量无关）。
            gpui::size(px(self.content_width), px(self.content_height))
        } else {
            // 小文档：量程**跟随 ListState 自身**（实时总高 / 拖拽时被冻结的
            // `scrollbar_drag_start_height` + 视口），与 gpui 内置
            // `ListState::ScrollbarHandle` 同口径。这样 thumb 的可走全程恰好等于
            // `set_offset_from_scrollbar` 映射所用的可滚域，拖到底必到真实末尾。
            // 不能用小文档的「估算」`n×est_hint`：真机布局（真实字体/DPI）若比估算高，
            // thumb 拖到底只到 `n×est_hint−视口`，离真正底部差一段——“无法一次拖到底”。
            let view = self.list.viewport_bounds();
            gpui::size(view.size.width, view.size.height + self.list.max_offset_for_scrollbar().y)
        }
    }

    fn start_drag(&self) {
        if self.windowed {
            // 窗口模式量程确定性，无坍缩，无需重铺/冻结。
            return;
        }
        // 小文档：拖拽开始前把量程铺到「确定性正确」再冻结（O(n) 仅在坍缩时一次）：
        // `items.summary().height` 会被冻结进 `scrollbar_drag_start_height`，
        // 若此时仍是坍缩值，整次拖拽都会被夹在旧量程 → 拖不到底、只能滚轮兜底。
        // 坍缩判定对照「期望全篇高」`n×est_hint`——只有**真坍缩**（summary 明显低于
        // 估算）才重铺；真实布局更高（估算偏小）时保持现值不重铺，content_size 随
        // ListState 走，拖到底仍是真实末尾。
        let total = self.list.max_offset_for_scrollbar().y.as_f32()
            + self.list.viewport_bounds().size.height.as_f32();
        if self.n > 0 && self.est_hint > 0.0 && total < self.content_height * 0.97 {
            let prev = self.list.logical_scroll_top();
            // 与 update_height_hint 的 apply 同法：reset 成全 None 再统一填高，
            // 覆盖陈旧 size_hint（with_uniform 的 unwrap_or 会保留陈旧值）。
            self.list.reset_with_uniform_height(self.n, px(self.est_hint));
            self.list.scroll_to(prev);
        }
        self.list.scrollbar_drag_started();
    }

    fn end_drag(&self) {
        if self.windowed {
            return;
        }
        self.list.scrollbar_drag_ended();
    }
}
