//! 本项目管线实现的 WYSIWYG 编辑器——`markdown::wysiwyg`。
//!
//! 以本项目自研管线（`parser`/`cells`/`ir`/`doc`）实现所见即所得编辑，
//! 已取代 zorite 移植的 `markdown::live`（该模块已移除；其共享的
//! `recognition` / `inline_style` 识别层上移到 `markdown/` 顶层继续共用）。
//!
//! 本文件是第一步「初步实现」：
//! - **文档模型完全来自本项目** [`super::doc::Doc`]：IR 语义 + `BlockIndex` + 源；
//!   行物化 `rows_in_block_window` + 编辑映射 [`DocRow::byte_map`]（与预览逐字节
//!   一致，`parity` 钉死）。
//! - 编辑器 = auto-grow 自绘 [`element::WyEditorElement`]（宿主滚动，与 `live` 同宿主
//!   范式）；行内样式来自 `DocRow.runs`（`parse::RunKind`）。
//! - 非文本块已按类型渲染：代码围栏（等宽 + 语法高亮 + 圆角面板，**内容行可编辑**）、
//!   GFM 表格（网格，列宽自适应）、引述（灰底 + 左侧指示条）、独立图片行
//!   （`AnyImageCache` 异步加载，占位/失败回退 alt）；样式统一抄自预览
//!   [`super::view::style::StyleParams`]。
//! - 输入：普通文本/IME 组合命中（`marked` 区间追踪，修复重复串）、点击定位、
//!   鼠标拖选 + Ctrl+C/X/V/A（选区替换/删除）。
//! - 现状声明：代码块内容行、表格**单元格**、图片**题注（alt）**已可编辑（代码走块级
//!   `byte_map` + `multi_offsets`；表格走逐行逐格的 `DocRow.table_rows` → `table_edit`
//!   单元格排版，含对齐位移、渲染列↔源字节映射；图片行 = 图片盒 + 居中可编辑 alt
//!   题注行，题注走与普通文本行同构的 `byte_map`/`wrapped`/`x_off`；导航跳过只读
//!   围栏/分隔行，退格/删除有代码围栏、单元格、alt 起点/尾边界守卫，表格与图片
//!   alt 内回车 no-op）。选区面板高亮已覆盖：普通文本行、代码内容行、表格单元格
//!   （逐可见行 × 逐格与选区求交后按字形几何绘制）、图片题注行。仅作「预览编辑
//!   测试」入口，验证「本项目流程能编辑 + 正确映射回源」。
//!
//! **#4 统一文档层（与预览合并）**：预览与 WYSIWYG 将合为一个组件（`editable` 门控
//! 只读/可编辑，同一 `Doc` + 自绘管线）。跨管线基准（`bench_preview_vs_wy_readonly`）
//! 已验证自绘滚动带 3.67ms/步 vs 预览元素树 6.71ms/步（release，真实 6.4MB 文档）——
//! 合并方向滚动不劣反优。预览独有能力正逐个搬入：已搬**搜索跳转高亮**
//! （[`Self::highlight_search`]，paint 复用选区几何画命中色；app 侧边栏搜索在统一
//! 组件在场时同步高亮）。待搬：EPUB 阅读布局（标题居中/首行缩进/注记悬浮卡）。

mod element;
mod input;
pub(crate) mod frame;
pub(crate) mod math;
pub(crate) mod mermaid;
mod math_typeset;
pub(crate) mod shape;

// 子模块扁平重导出：`element::*` 把布局类型 + geom/math/shape 的 pub(crate) 项一并带进
// mod.rs 命名空间，frame/shape/math 以 `use super::*` 共享（对应 `element::*` 路径原样保留）。
// frame 不整块重导出（其内部 `warm_emoji_rasterizer` 与根级同名函数冲突），由 element.rs
// 经 `pub(crate) use super::frame::{…}` 精确暴露。
pub(crate) use element::*;

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext, Context, FocusHandle, Focusable, Font, Hsla, Pixels, RenderImage, SharedString,
    Window, font, point, px,
};

use super::doc::Doc;
use crate::markdown::parse::recognition::{self, LinkHit};
use ropey::Rope;

/// 块布局的可变引用（`ensure_block_layout` 返回 / 缓存槽共用）。
pub(crate) type BlockLayoutRef = Rc<std::cell::RefCell<element::BlockLayout>>;
use crate::markdown::parse::inline_style;
use crate::markdown::view::style::StyleParams;

/// 编辑器变更事件（宿主订阅，内容镜像回源码编辑区/保存流程）。
#[derive(Clone, PartialEq, Debug)]
pub enum WyEvent {
    Changed,
    /// 点击内部链接（wiki/tag/block-ref，`[[…]]`/`#tag`/`((id))`）→ 宿主打开对应页面。
    OpenPage(String),
}

/// A4：标题目录条目 `(源码行, 标题级别, 标题文本)` 容器（WyEditor 目录 / 侧边栏共用）。
pub(crate) type TocEntries = Vec<(usize, u8, SharedString)>;
/// A4：目录缓存条目（`content_gen` + 条目；侧边栏目录每帧渲染消费用）。
pub(crate) type TocCacheEntry = (u64, TocEntries);

/// 行内样式调色（创建时注入；后续随主题切换重建）。
///
/// **关键**：整份主题由预览的 [`StyleParams`] 一次性派生（`WyTheme::from_style`），
/// 颜色与排版数值**逐项抄自同一来源**——预览与 WYSIWYG 改样式只动 `StyleParams`，
/// 这里不再有任何缩放系数/字号的二次定义。`font_size`/`font` 在创建时由
/// [`WyEditor::new`] 从 `window.text_style()` **一次性**确定并存进主题——之后
/// `build_frame` 一律用主题值，绝不再次读 `window.text_style()`。因为 measure
/// （栈空=默认 16）与 prepaint（栈内含 `live-body` 的 `.text_size`）两阶段的
/// `text_style()` **不同**，若每帧现取会在两个字号间交替 = 滚动时「字体一会儿大
/// 一会儿小」。
#[derive(Clone)]
pub struct WyTheme {
    pub text: Hsla,
    /// 引述/次要文字色（预览 `muted_fg`）。
    pub muted: Hsla,
    /// 等宽字体（预览主题 `mono`）。
    pub mono: Font,
    /// 正文字体（应用默认字体）。
    pub font: Font,
    /// 正文字号（px）——由应用全局字号驱动（主题内正文基准）。
    pub font_size: Pixels,
    pub accent: Hsla,
    /// 链接色（预览 `StyleParams.link`，明 #0969da / 暗 #79c0ff 单源）。
    pub link: Hsla,
    /// 代码块背景（明/暗主题各自取值，与预览一致）。
    pub code_bg: Hsla,
    /// 行内代码（反引号）文字色（预览 `code_inline_fg`）。
    pub code_inline_fg: Hsla,
    /// 行内代码圆角 pill 底色（预览 `code_inline_bg`）。
    pub code_inline_bg: Hsla,
    /// 分隔线/表格边框色（预览 `divider`）。
    pub divider: Hsla,
    /// 表头底色（预览 `table_head_bg`）。
    pub table_head_bg: Hsla,
    /// 引述块背景（预览 `quote_bg`，浅灰卡片感）。
    pub quote_bg: Hsla,
    pub is_dark: bool,
    /// 正文行高 / 代码字号 / 代码行高（抄自 [`StyleParams`]，随字号覆盖变化）。
    pub(crate) para_line: Pixels,
    pub(crate) code_size: Pixels,
    pub(crate) code_line: Pixels,
    /// 各级标题字号/行高（抄自 [`StyleParams::heading_metrics`]，索引 1..=6，0 位不用）。
    heading_fs: [Pixels; 7],
    heading_lh: [Pixels; 7],
}

impl WyTheme {
    /// 从预览单一排版源 [`StyleParams`] 派生 WYSIWYG 主题（字号覆盖用 `font_size`）。
    pub(crate) fn from_style(p: &StyleParams, body_font: Font, font_size: Pixels) -> Self {
        let mut heading_fs = [px(0.); 7];
        let mut heading_lh = [px(0.); 7];
        for lv in 1..=6u8 {
            let (fs, lh) = p.heading_metrics(lv);
            heading_fs[lv as usize] = fs;
            heading_lh[lv as usize] = lh;
        }
        WyTheme {
            text: p.fg,
            muted: p.muted_fg,
            mono: font(p.mono.clone()),
            font: body_font,
            font_size,
            accent: p.accent,
            link: p.link,
            code_bg: p.code_bg,
            code_inline_fg: p.code_inline_fg,
            code_inline_bg: p.code_inline_bg,
            divider: p.divider,
            table_head_bg: p.table_head_bg,
            quote_bg: p.quote_bg,
            is_dark: p.is_dark,
            para_line: p.para_line,
            code_size: p.code_size,
            code_line: p.code_line,
            heading_fs,
            heading_lh,
        }
    }

    /// 由正文字号覆盖更新尺寸相关字段（缩放系数同样只来自
    /// [`crate::markdown::view::style`] 的 `typography_for_size` / `heading_metrics_for_size`，
    /// 不在此重复定义）。标题字号/行高也随正文字号等比缩放——预览/WYSIWYG 的 Ctrl+滚轮、
    /// 菜单调字号时 #/##/### 一起变大变小。
    pub(crate) fn set_font_size(&mut self, size: Pixels) {
        self.font_size = size;
        let body = f32::from(size).max(9.);
        let (pl, cs, cl) = crate::markdown::view::style::typography_for_size(body);
        self.para_line = pl;
        self.code_size = cs;
        self.code_line = cl;
        for lv in 1..=6u8 {
            let (fs, lh) = crate::markdown::view::style::heading_metrics_for_size(body, lv);
            self.heading_fs[lv as usize] = fs;
            self.heading_lh[lv as usize] = lh;
        }
    }

    /// 正文行高（预览 `para_line`）。
    pub fn para_line(&self) -> Pixels {
        self.para_line
    }
    /// 代码字号（预览 `code_size`）。
    pub fn code_size(&self) -> Pixels {
        self.code_size
    }
    /// 代码行高（预览 `code_line`）。
    pub fn code_line(&self) -> Pixels {
        self.code_line
    }
    /// 第 n 级标题字号（预览 `heading_metrics`：h1=24 h2=20 h3=18 h4=16 h5/6=15）。
    pub fn heading_fs(&self, level: u8) -> Pixels {
        self.heading_fs[(level as usize).clamp(1, 6)]
    }
    /// 第 n 级标题行高。
    pub fn heading_line(&self, level: u8) -> Pixels {
        self.heading_lh[(level as usize).clamp(1, 6)]
    }
}

/// 每帧光标几何（供 IME/`EntityInputHandler::bounds_for_range` 读取）。
#[derive(Clone, Copy, Default)]
pub(crate) struct CaretVis {
    pub x: Pixels,
    pub top: Pixels,
    pub line_height: Pixels,
}

thread_local! {
    static CARET_VIS: Cell<CaretVis> = const { Cell::new(CaretVis { x: px(0.), top: px(0.), line_height: px(0.) }) };
}

pub(crate) fn set_caret_vis(v: CaretVis) {
    CARET_VIS.with(|c| c.set(v));
}
fn caret_vis() -> CaretVis {
    CARET_VIS.with(|c| c.get())
}

/// 撤销/重做单元：**精确逆操作**（整组净一次编辑）——撤销/重做走 `replace_range` 同款
/// 增量管道（`Doc::edit` 增量块扫描 + Pass1 三段式 + derived 小修补 + 单块整形作废），
/// 不再像旧快照那样整体重建模型（`Doc::open_rope` 全量重索引 + 全毁派生/布局 → 实测
/// 6.4MB 撤销 ~467ms/次：恢复 355ms + build_frame pass1 全量重估 88ms）。
/// `insert` 当前占据 `range`；`delete` 是撤销时要换回的原文（对应单元前状态）。
/// B2 事务合并：连续同意图且邻接的编辑并入同一单元（`insert`/`delete` 累积成净一次替换）。
#[derive(Clone)]
struct UndoUnit {
    /// 当前文档中 `insert` 所占区间（撤销 = 把它替换回 `delete`）。
    range: Range<usize>,
    /// 该单元净插入（当前在 `range` 处）。
    insert: String,
    /// 该单元前该区间原本的文本（撤销后回到它）。
    delete: String,
    /// 单元起始光标（撤销恢复位）。
    caret_before: usize,
    /// 前向恢复光标（**按下撤销瞬间**的光标；`undo` 写入、`redo` 读取，同行 `editor/undo.rs`）。
    forward_caret: usize,
}

/// 项目管线的所见即所得编辑器：`Doc`（内部 rope）为唯一数据源，`caret` 是**源字节偏移**。
pub struct WyEditor {
    doc: Doc,
    focus: FocusHandle,
    caret: usize,
    /// 是否已装入过完整源（`set_source_text`/`install_open` 置真）。App 大文档 load 走
    /// 后台解析时，`new()` 产生**空**编辑器——在 `install_open` 完成前该编辑器没有内容，
    /// 宿主读取 `text()` 写回必须回退到源码编辑区（否则加载窗口内保存/退出会丢内容）。
    loaded: bool,
    /// 是否可编辑。默认 true（编辑核心用途）；`set_editable(false)` 切**只读预览**门控：
    /// 输入/IME/删除/撤销/粘贴短路（`replace_range` 硬门控）、不画光标；保留光标移动、
    /// 点选/复制、链接点击、折叠、搜索高亮、表格/代码纵向滚动条拖动。
    editable: bool,
    theme: WyTheme,
    /// 本帧行网格（measure→prepaint→paint 复用；**窗口化**：仅带内块被真正 shape，
    /// 带外块用 `BlockMeta` 估算高度占位，避免大文档整篇每帧 shape 卡死）。
    frame: RefCell<Option<std::rc::Rc<element::Frame>>>,
    /// 本帧元素在窗口中的布局 bounds（paint 写入；鼠标命中时全局坐标 → 元素局部）。
    last_bounds: Cell<Option<gpui::Bounds<Pixels>>>,
    /// 字号变化的**缩放锚点** `(块下标, 块内比例)`：`set_font_size` 重排前用
    /// [`Self::viewport_top_px`]（宿主推入的视口顶部）捕捉，帧按新字号重建后由
    /// `advance_zoom_restore`（element prepaint 自驱动）换算出新的滚动目标并写回
    /// `ScrollHandle`——Ctrl+滚轮缩放不带动滚动位置（对齐 `markdown/view`、`editor`
    /// 的视口锚定）。
    zoom_anchor: RefCell<Option<(usize, f32)>>,
    /// 宿主每帧推入的「当前视口顶部 px」（元素局部 y，来自 `#live-scroll` 的
    /// `ScrollHandle::offset` + #live-body 顶部内衬换算）。
    viewport_top_px: Cell<f32>,
    /// 编辑区⇄预览滚动同步的目标块**源码行**（宿主渲染期写入；WY 元素 prepaint 经
    /// [`Self::advance_pending_scroll`] 逐帧重钉宿主偏移直到稳住再清——仿缩放恢复的
    /// 收敛语义，规避「单次写宿主偏移被后续布局复位」）。
    scroll_target: Cell<Option<usize>>,
    /// 上次重钉写进的 `(目标源码行, 目标绝对偏移)`：prepaint 每帧先读宿主当前偏移，
    /// 与它一致才判「已稳住并清除」；否则重写并更新。
    last_pin: Cell<Option<(usize, f32)>>,
    /// 重钉已施加的帧数：每次 `set_scroll_target` 清零，`advance_pending_scroll` 每帧
    /// +1；超 [`Self::SCROLL_TARGET_MAX_APPLIES`] 强制清除（防宿主偏移被滚动条/布局
    /// 持续复位的病态下永不收敛 → 每帧满载重排，仿 `zoom_applies` 的兜底）。
    scroll_applies: Cell<u32>,
    /// 缩放恢复**收敛状态**：`advance_zoom_restore` 逐帧随 `block_top/height` 细化重新计算
    /// 目标（高度估计→整形真值逐帧收敛），直到目标位移 < [`Self::ZOOM_ANCHOR_EPS`] 或
    /// 超过 [`Self::ZOOM_ANCHOR_MAX_APPLIES`] 帧才消费锚点——避免一次性按未收敛高度
    /// 恢复导致视口继续漂移。
    zoom_last_target: Cell<Option<f32>>,
    zoom_applies: Cell<u32>,
    /// 宿主 WYSIWYG 滚动容器句柄 + 内容顶部内衬（`#live-body` py）。注入后
    /// `advance_zoom_restore` 才能在 element prepaint 逐帧把缩放恢复写回 `ScrollHandle`；
    /// 未注入（独立测试）时缩放锚点直接丢弃。
    host_scroll: RefCell<Option<(gpui::ScrollHandle, f32)>>,
    /// 内容代数：任何文本变更自增；`request_layout` 用它判断帧缓存是否仍有效。
    content_gen: Cell<u64>,
    /// 可视带（窗口坐标→元素局部 y，量程化）：prepaint 用 `bounds.origin.y` +
    /// `viewport_size` 推算并发布，measure 用上一帧的带做窗口化 shape。
    shape_band: Cell<Option<(f32, f32)>>,
    /// **滚动预热**状态（`element::prewarm_band`）：把下一量子带块提前 shape 进
    /// `shape_rows` 缓存（键与 band 无关）→ 带穿越的 6.4ms/band shape 突发被摊平。
    /// 方向由带序号跨界判定；结构编辑清空整形缓存后必须归零（见 `replace_range`）。
    prewarm: RefCell<Prewarm>,
    /// 全篇**基础布局**缓存：`(代, Rc<RefCell<BlockLayout>>)`——rolling/带移动与
    /// **非结构性编辑**都复用（只对受影响块做 Fenwick 点更新），代 ≠ 内容代：
    /// 由 `struct_ver`/图片/数学/宽度组成。结构性变更（换行/围栏/表格等改了块数或
    /// 块序）才整篇重建。`set_theme`/`set_font_size` 把代置失效位强制重算。
    block_layout: RefCell<Option<(u64, Rc<std::cell::RefCell<element::BlockLayout>>)>>,
    /// 内容**结构版本**：`replace_range` 里结构性变更（跨行、代码/表格边界）才递增——
    /// 参与基础布局代；非结构性编辑不递增（基础布局跨键复用，只 patch 受影响块高）。
    struct_ver: Cell<u64>,
    /// 结构性编辑影响的**首个源行**（Pass1 增量高度重估的起点）：编辑线之前的块
    /// 内容未变 → 复用旧布局高度；只有「编辑线起 → 文尾」重新估算（Enter 贴近文尾时
    /// O(变化后缀) 而非全篇）。非结构编辑不参与（无重建）。
    struct_dirty_line: Cell<usize>,
    /// 编辑影响的**最后一个旧源行**（`edit_line + 被删换行数`）：Pass1 用它定位旧尾部
    /// 起点（`j`），从而**复用尾部高度**（编辑下方块内容未变，高度不变）——把结构编辑
    /// 从「dirty_line→文尾」全后缀重估降为「仅变化中段」+ 尾部 Vec 复制。
    struct_old_end_line: Cell<usize>,
    /// 编辑的源行净增（插入换行数 − 被删换行数）：尾部块 src_line 整体平移量。
    struct_line_delta: Cell<i64>,
    /// 结构性编辑被编辑块的**旧块下标**（编辑前）与**块数净增**（编辑后 − 编辑前）：
    /// `fold_plan` deltas 增量平移用（块下标 ≥ 旧块下标之后整体 + 块数净增）。
    struct_blk_old: Cell<usize>,
    struct_blk_span_delta: Cell<i64>,
    /// 结构性编辑待消费标志：结构性分支置位、`fold_plan` 消费后清除——防止增量平移
    /// 误用陈旧编辑信息（非结构编辑/宽度变化恰在结构编辑之后、其间无重建时）。
    struct_edit_pending: Cell<bool>,
    /// 内容**派生版本**——`derived()` 缓存键：结构性编辑、编辑行 region 签名变化才
    /// 递增；纯文本键入（非结构、签名不变、不在数学块内）不递增 → `derived` Rc
    /// 跨键复用，不再每键对全篇 `math_regions`/`alert_*` 重扫（7MB/结构密集文档
    /// 「输入卡死」的主因，旧实现每键 2 次全篇扫描 ≈1s）。
    regions_ver: Cell<u64>,
    /// 折叠/重排计划缓存：`(key, marker_key, Rc<FoldPlan>)`——与可视带无关（内容、caret
    /// reveal、数学就绪、宽度、结构版本决定），滚动/带移动复用；build_frame 每档不再
    /// 重建 ~万行 Hash 集合 + 逐块二分（大文档滚动「卡顿明显」：~40ms/档）。
    /// `marker_key` = regions_ver^mgen^fold_ver：marker 身份在结构编辑间未变 → deltas
    /// 可增量平移复用（`fold_plan_incremental`）。
    fold_plan: RefCell<Option<(u64, u64, Rc<element::FoldPlan>)>>,
    /// 标题折叠视图态版本：`toggle_heading_fold_at_line` 翻转 `folded_headings` 时递增
    /// （纳入 `fold_plan` 缓存键，滚动不带折叠态重算）。
    fold_ver: Cell<u64>,
    /// `$` 出现次数（内容级增量计数；`derived` 的数学预筛用，免全篇重扫）。
    math_dollar_cnt: Cell<usize>,
    /// `[!` 出现次数（`derived` 的警报/callout 预筛用，免全篇重扫）。
    alert_marker_cnt: Cell<usize>,
    /// 最近一次编辑受影响（需重估高度）的块下标；`build_frame` 消费后清空。
    /// `None` = 无（或结构性编辑 → 整篇重建，不进此表）。
    pending_patch_blocks: RefCell<Option<Vec<usize>>>,
    /// 内容派生扫描缓存：`(regions_ver, Rc<RefCell<WyDerived>>)`——滚动/带移动与纯文本
    /// 键入复用；结构性/签名变化重建。`RefCell` 供编辑期对数学块 `source` 做局部修补
    /// （`replace_range`，避免键入大文档数学块内部也整篇重建）。
    derived: RefCell<Option<(u64, Rc<RefCell<WyDerived>>)>>,
    /// 行整形内容缓存（按块下标；键含 `epoch`/`mgen`）：滚动/带重建复用已整形块，
    /// 不再整带重 shape + 代码块重跑 syntect（大文档滚动「每档 65ms」的主因）。
    /// 非结构性编辑只作废被编辑块，结构性/换源/字号变化整体清空。
    shape_rows: RefCell<RowShapeCache>,
    /// 含 `.notdef`（glyph id=0，Windows 冷启动回退未命中）的块 → 自愈状态机。
    /// 见 [`NotdefRetry`] 文档「冷却一帧破 LineLayoutCache」：Pass2 命中/入缓存前都
    /// 扫 .notdef，对坏块走「冷却一帧 → 新鲜平台重排」直至 DWrite 回退解析成功。
    notdef_retry: RefCell<NotdefRetry>,
    /// （仅测试）自上次读取以来 Pass2 实际发生的行整形次数（缓存未命中/光标行）。
    #[cfg(test)]
    reshapes: Cell<u64>,
    /// （仅测试）自上次读取以来 D1 预热实际发生的行整形次数（`prewarm_band` 计数）。
    #[cfg(test)]
    prewarm_shapes: Cell<u64>,
    /// （仅测试）Pass2 notdef 冷却帧数（复用旧几何、未调用 gpui shape）。
    #[cfg(test)]
    coolings: Cell<u64>,
    /// 图片异步加载缓存（gpui `ImageAssetLoader` 实体；命中返回 `Arc<RenderImage>`）。
    image_cache: gpui::Entity<gpui::RetainAllImageCache>,
    /// 独立图片行 src → 逻辑显示尺寸 `(w, h)`（按加载后的宽高比换算）。
    /// `None`(外层) = 尚未尝试加载；`Some(None)` = 加载失败（一直显示 alt 兜底）；
    /// `Some(Some(..))` = 已加载。供 shape（假布局尺寸）与 paint（占位）共用。
    image_sizes: RefCell<std::collections::HashMap<SharedString, Option<(f32, f32)>>>,
    /// 图片加载完成代数：任何图片从「占位尺寸」变成「真实尺寸」自增——
    /// 纳入帧缓存键，新图到达后必须重建整帧（否则缓存总高还是旧尺寸）。
    images_gen: Cell<u64>,
    /// 用户拖拽调整的图片宽（px，按 src）：`None`（地图缺项）= 未调整 → 自然宽
    /// `min(avail, IMAGE_MAX_W)`。值已钳在 `[IMAGE_MIN_W, IMAGE_MAX_W]` 内。
    /// 改变 → `images_gen` 自增（帧/整形缓存失效 → 图片按新宽 + 原宽高比重排）。
    image_widths: RefCell<std::collections::HashMap<SharedString, f32>>,
    /// 进行中的图片调整条拖拽（右中边缘）。`Some` = 按住拖动中（随鼠标改宽，不改源码）。
    image_resize_drag: RefCell<Option<ImageResizeDrag>>,
    /// LaTeX → 排版结果缓存（参考 zorite `<MathStore>` 按公式源码做键）。
    /// `Loading` 排版中 / `Ready` 就绪（位图 + 逻辑 px 尺寸）/ `Failed` 失败（回退 Unicode）。
    math_slots: Rc<RefCell<std::collections::HashMap<SharedString, MathSlot>>>,
    /// 排版文字色；主题切换 → 清缓存重排（同 zorite `MathStore::set_color`）。
    math_color: RefCell<Option<Hsla>>,
    /// 排版代数：任何公式 Loading→Ready/Failed 自增，纳入帧缓存键（同 `images_gen`）。
    math_gen: Cell<u64>,
    /// mermaid 图逐块滚轮缩放：键 = 围栏块首源码行。
    mermaid_zoom: std::cell::RefCell<std::collections::HashMap<usize, f32>>,
    /// mermaid 缩放代数：缩放变化自增，纳入帧缓存键 + 基础布局代（同 `math_gen`，强迫重排）。
    mermaid_zoom_gen: Cell<u64>,
    /// 当前获得焦点的 mermaid 卡（`src_line`）：**点击卡片**置位、点击其它处清除。
    /// 仅聚焦的 mermaid 用滚轮缩放（`on_scroll_wheel` 只消费聚焦卡上的滚轮），未聚焦时
    /// 滚轮一律放行给宿主正常滚动——避免「往下滚遇到 mermaid 就缩图」。视图本地态，不进撤销栈。
    focused_mermaid: Option<usize>,
    /// 代码块高亮解析缓存（后台 syntect 整块解析；`request_code_parse` 置 Pending、
    /// `sync_code_parse` 后台跑 `parse_code_lines` 后落 Done）。解析就绪由
    /// `on_code_parse_ready` **定向作废该代码块的行整形缓存条目**（非全局代数）→ 下帧
    /// 重建用高亮行替换占位纯文本行。
    code_parse: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<CodeParseKey, CodeParseState>>>,
    /// 本帧登记的待后台解析队列 `((语言, 代码), 块 src_line)`（`sync_code_parse` 每帧
    /// 清空消费）。`src_line` 用于完成时的**带内门控**（块还在带内才立即重建帧上色）。
    code_parse_need: RefCell<Vec<(CodeParseKey, usize)>>,
    /// 数学显示行数缓存（见字段声明）。
    math_line_map: RefCell<std::collections::HashMap<String, usize>>,
    /// 行内公式键（`"inline:…"`）→ 包含它的行 src_line（`sync_math` claim 时登记）。
    /// `on_math_ready` 完成时**定向作废**这些行整形缓存条目——替代 mgen 进全局 shape_key
    /// 的整带 miss（`$$` 显示块在 Pass2 被 blocked、不入 `shape_rows`，无需作废）。
    /// 换源清空（行号随源失效）；轻微过期只多作废一两个无关块（无害，正确性不受影响）。
    inline_math_rows: RefCell<std::collections::HashMap<SharedString, Vec<usize>>>,
    /// 图片相对路径解析的基准目录（打开文件时由应用传入其父目录，与预览一致）。
    base_dir: Option<std::path::PathBuf>,
    /// IME 组合（marked text）的源字节区间。空 = 无组合进行中。
    /// 关键：组合的每次更新都**替换**这段（而非追加到后面）——不追踪它就会把
    /// 上一次组合的字符重复插入（输入“你好”→ “nnini'hni'hao你好”的坏字符）。
    marked: Option<Range<usize>>,
    /// 文本选区的固定锚点（源字节）。`Some(anchor)` 时选区 = `anchor..caret`（caret
    /// 为活动端）；`None` = 无选区（仅光标）。拖拽/ Shift+方向 产生，Ctrl+C/A/V 消费。
    sel_anchor: Option<usize>,
    /// A4：目录缓存 `(content_gen, 条目)`——内容变更才重扫 Doc 块（侧边栏目录每帧
    /// 渲染消费，`toc_entries` 按此 O(1) 命中，避免大文档每帧 O(块数) 扫描）。
    toc_cache: RefCell<Option<TocCacheEntry>>,
    /// 鼠标左键拖选进行中（on_mouse_move 延伸选区）。
    dragging: bool,
    /// 预览独有能力「搜索跳转高亮」（自 [`MarkdownPreview::highlight_search`] 搬入）：
    /// （源码行, 小写查询词）；`Some` 时 paint 只给该行中命中词的源区间画背景色，
    /// [`crate::config::timing::HIGHLIGHT_MS`] 后自动消失（`highlight_seq` 防旧定时器误清）。
    highlight_search: Option<(usize, String)>,
    /// 高亮代序列号：每次发起/清除高亮 +1，定时器只清除同代高亮。
    highlight_seq: u64,
    /// 代码块横向滚动偏移（px；键 = 代码块首源码行）。长代码行 nowrap 超宽时
    /// 由横向滚动条/拖动驱动（paint 平移 + 命中/光标/选区反向加回）。纯视图状态，
    /// 不写源码、不进撤销栈。
    code_h_scroll: RefCell<std::collections::HashMap<usize, f32>>,
    /// 代码块横向滚动条拖拽进行中：`Some((块首源码行, 指针相对滑块左缘偏移))`。
    code_h_drag: RefCell<Option<(usize, f32)>>,
    /// 表格横向滚动偏移（px；键 = 表格块首源码行）。超宽表格（`total_w` > 内容宽）
    /// 由底部横向滚动条驱动（paint 平移 + 命中/光标反向加回，参考代码块 `code_h_scroll`；
    /// 对齐 preview `markdown/view` 的表格溢出横向滚动）。纯视图状态，不写源码、不进撤销栈。
    table_h_scroll: RefCell<std::collections::HashMap<usize, f32>>,
    /// 表格横向滚动条拖拽进行中：`Some((块首源码行, 指针相对滑块左缘偏移))`。
    table_h_drag: RefCell<Option<(usize, f32)>>,
    /// 已折叠的标题区块（视图本地状态，键 = 标题行 trim 后的文本，与 `live` 一致；
    /// markdown 没有标题折叠语法，折叠态**不写回源码**、不进撤销栈）。点击标题行
    /// 右缘的折叠箭头翻转（`▸` 展开 / `▾` 折叠）；折叠后该 section 的 body 行
    /// 折叠成 0 高（光标严格落在 body 行内才 reveal）。
    folded_headings: RefCell<std::collections::HashSet<String>>,
    /// 当前文档中每个可编辑块的**区域**（代码内容行/表格表头+数据行）与**围栏行**
    /// （代码 ``` 标记行、表格分隔行，只读）。导航/退格/删除据此避免光标落进
    /// 只读围栏（否则在隐藏围栏上输入会破坏结构）。随文本变更重建。
    block_regions: Vec<BlockRegion>,
    /// 撤销栈（pop → 应用于当前内容）；重做栈。**精确逆操作**（净一次编辑 / 单元），
    /// 深度 [`UNDO_LIMIT`]；**B2 合并**：连续同意图且邻接的编辑并入当前打开的
    /// 单元（`open_unit`），撤销把净逆操作经增量管道打回——不再整体重建模型。
    undo_stack: Vec<UndoUnit>,
    redo_stack: Vec<UndoUnit>,
    /// 正在累积的撤销单元（最后一批并入它的编辑尚未固化；下次非合并编辑或 undo/redo
    /// 时关闭并入栈）。
    open_unit: Option<UndoUnit>,
    /// 上一个编辑（意图 + 源字节区间）：`replace_range` 用它判定是否与当前单元合并成
    /// 同一撤销事务（连续打字 / 相邻退格 / 同点删除）；光标移动（`set_caret`/方向键
    /// `moved`）与整篇内容替换打断。
    last_edit: Option<(EditKind, usize, usize)>,
    /// 当前悬浮的图片行 `src_line`（`on_mouse_move` 更新）。非 `None` 时该图片右上角
    /// 浮现「编辑」铅笔按钮（仅可编辑模式绘制/响应）。纯视图态，不进撤销栈。
    hover_image: Cell<Option<usize>>,
    /// 点击图片**本体**时置位的选中图片行 `src_line`。非 `None` 时该图片画 accent
    /// 描边 + 编辑按钮常显（不依赖指针悬浮）。与 `hover_image` 不同：选中态独立于
    /// 指针，点击其它文本区 / 换源 / 只读 时清除；Backspace/Delete 删除整张图片行。
    /// 纯视图态，不进撤销栈。
    selected_image: Cell<Option<usize>>,
    /// 图片编辑卡片（打开时在图片旁渲染悬浮表单：链接 + 文本两个输入框）。
    /// `None` = 未打开。打开期间输入框获得焦点（IME 走 `InputState`，与文档输入互不干扰）。
    image_edit: Option<ImageEditPopup>,
    /// 图片编辑卡片的**窗口锚点**（卡片 TopLeft，window 坐标）：paint 期用「本帧
    /// `bounds.origin` + 本帧 frame」同代配对算出并缓存，render 只读它摆卡片。
    /// 绝不能由 render 直接拿 frame/last_bounds 现算——`end_selection` 清帧后
    /// `ensure_frame_for_input` 会用兜底 `px(800)` 宽重建一帧、随后 prepaint 又按真实宽
    /// 重建，render 期每次读到的帧宽不同 → 卡片横向左右跳一帧（「突然往左闪烁」）。
    /// `None` = 无卡片 / 锚点未就绪。
    image_edit_anchor: Cell<Option<gpui::Point<Pixels>>>,
}

/// 编辑意图（决定撤销合并规则，纯无定时器确定性判定，同 editor/undo.rs）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditKind {
    /// 纯字符插入（无换行、邻接续打 → 合并）。
    Typing,
    /// 退格（纯删除、首尾相接 → 合并）。
    Backspace,
    /// 正向删除（纯删除、同起点延伸 → 合并）。
    Delete,
    /// 原子编辑（Enter/Tab/粘贴/程序化写/任务框/折叠 → 永不合并并断层）。
    Atomic,
}

/// 可编辑块区域（与块级编辑映射配套；围栏行不可落点）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlockRegion {
    /// 代码围栏：内容行 `lo..=hi` 连续；围栏行 = `lo-1`（开）、`hi+1`（闭）。
    Code { lo: usize, hi: usize },
    /// GFM 表格：表头行 + 数据行 `dlo..=dhi`；分隔行（围栏）= `dlo-1`。
    Table { header: usize, dlo: usize, dhi: usize },
}

/// 图片编辑卡片：两个单行输入框（链接 + 文本）锚定在目标图片旁。
struct ImageEditPopup {
    /// 目标图片行的源码行号。
    src_line: usize,
    /// 图片链接（`![alt](src)` 的 src）输入框。
    link: gpui::Entity<crate::ui::input::InputState>,
    /// 图片文本（alt / 题注）输入框。
    text: gpui::Entity<crate::ui::input::InputState>,
}

/// 图片调整条拖拽：按住右中边缘的条，随鼠标横向移动改图片宽（保持宽高比）。
/// `grab_x` = 按下时指针的窗口 x；拖动中新宽 = `start_w + (mouse_x − grab_x)`。
#[derive(Clone)]
struct ImageResizeDrag {
    /// 目标图片的 src。
    src: gpui::SharedString,
    /// 按下瞬间的图片显示宽（px）。
    start_w: f32,
    /// 按下瞬间指针的窗口 x（px）。
    grab_x: f32,
}

/// 一个 LaTeX 公式的排版产物（键 = 公式源码 LaTeX；参考 zorite 的 `MathStore::Slot`）。
#[derive(Clone)]
pub(crate) enum MathSlot {
    /// 排版已开始，位图未回（paint 回退 Unicode 文本；结束自增 `math_gen`）。
    Loading,
    /// 已就绪：位图 + 逻辑（pre-DPR）px 尺寸。
    Ready(Arc<RenderImage>, f32, f32),
    /// 排版失败（回退 Unicode 文本，不再重试）。
    Failed,
}

/// 数学文字色（单一源：位图着色 = Unicode 回退文本色 = 渲染 fallback 一致）。
/// 取 `config::math::MATH_COLOR` 的紫（业界公式感），暗色主题提亮。
pub(crate) fn math_color(theme: &WyTheme) -> Hsla {
    let mut c: Hsla = gpui::rgb(crate::config::math::MATH_COLOR).into();
    if theme.is_dark {
        c.l = (c.l + crate::config::math::MATH_COLOR_DARK_LIFT).clamp(0.05, 1.);
    }
    c
}

/// 行内（`$…$`）公式的排版槽键：与块公式（键 = latex 原样）分开，行内用紧凑
/// `INLINE_PAD` 光栅（否则小公式也被 PAD 顶高、撑行）。`latex` 从不以 `inline:` 起头，
/// 不冲突。
pub(crate) fn inline_math_key(latex: &str) -> SharedString {
    format!("inline:{latex}").into()
}

/// 基础布局代：`(struct_ver, igen, mermaid_zoom, shaping 宽)` 折成一个 u64——**结构性内容**/
/// 图片/宽度 任一变 → 整篇重建；非结构性编辑不改 `struct_ver` → 布局跨键复用，只做
/// Fenwick 点更新。mermaid 缩放（`zgen`）也进代：缩放只影响单张卡片高，但块高存在基础
/// 布局数组里，必须整篇重建一次才一致（缩放是低频操作，可接受；不复用 `mgen` 的稀疏
/// delta 路线）。
/// **不含 `mgen`**：数学排版就绪只改**卡片高**（走 `fold_plan` 稀疏 delta），基础布局里
/// 数学块的 base 是普通段估算高，与就绪无关——把 `mgen` 放这里会让每个公式光栅完成都
/// 触发全 139k 块 Pass1 重建。纯算术混淆（不做 SipHash：15.9 万块逐块键构造会吃掉
/// ~200ms）。
pub(crate) fn height_epoch(struct_ver: u64, igen: u64, width_bits: u32, zgen: u64) -> u64 {
    let a = struct_ver ^ igen.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let b = (width_bits as u64).wrapping_mul(0x517C_C1B7_2722_0A95) ^ zgen.wrapping_mul(0x243F_6A88_85A3_08D3);
    let x = a ^ b.rotate_left(32);
    x ^ x >> 33
}

/// 与 element prepaint 同式的可视带量程（q=512）：缩放锚定偏移写回后据此推导新带，
/// 保证 `prepare_font_zoom` 重建的帧与 measure/prepaint 一致（避免跨界二次重建与
/// `shape_band` 瞬态不一致）。`top` = 元素局部 y 的视口顶部（内容坐标）。
pub(crate) fn zoom_band_for_top(top: f32, viewport_h: Pixels) -> (f32, f32) {
    let q = 512.;
    let vh = viewport_h.as_f32().max(1.);
    ((((top - vh) / q).floor() * q).max(0.), ((top + 2. * vh) / q).ceil() * q)
}

/// 一次「内容派生」的全篇扫描结果。纯 `content` 函数 → 按 `content_gen` 缓存，滚动 /
/// 可视带移动（内容不变）直接复用 `Rc`，**不重扫全篇**——大文档每帧卡顿的主因之一
/// （`math_regions` 7MB 全篇扫描 ~400ms、`alert_*` ~30ms，随每帧 paint / 每档 rebuild
/// 重复执行）。
pub(crate) struct WyDerived {
    /// `$$…$$` 数学块区域（首源码行 → 卡片）。
    pub math_regions: Vec<crate::markdown::parse::inline_style::MathRegion>,
    /// 可折叠 callout 区域 `(行区间, 是否折叠)`。
    pub alert_fold_regions: Vec<(Range<usize>, bool)>,
    /// 每个警报区域的 `AlertKind`（marker + body 行），paint 引述条着色用。
    /// Fx 哈希：结构编辑 `shift_derived` 全量平移重插时省 SipHash（大文档几百到
    /// 数千条 ≈ Enter 路径 1-2ms）。
    pub alert_region_kinds: rustc_hash::FxHashMap<usize, crate::markdown::parse::recognition::AlertKind>,
}

/// 一次全篇的行内公式清单扫描 `(源码行, 槽键)`——已被「可见带懒光栅」取代（`sync_math`
/// 改从窗口化 `frame.rows` 拿带内公式，见约定）。此函数保留一则可应急回退。
fn alert_kind_tag(k: crate::markdown::parse::recognition::AlertKind) -> u8 {
    use crate::markdown::parse::recognition::AlertKind::*;
    match k {
        Note => 0,
        Tip => 1,
        Important => 2,
        Warning => 3,
        Caution => 4,
    }
}

/// 一行内容对 `derived`（数学块 / 警报区域）的**相关投影签名**——纯局部判定该行的
/// region 结构是否可能变化，与 `math_regions` / `alert_regions` 的逐行判据逐条对应：
/// 围栏、`$$`、单行 `$$…$$`（**不含内部文本**——内部变化走编辑期的数学 source 修补）、
/// 警报/引述、对齐标记、行内 `$` 有无。编辑前/后签名相同 → 非结构性编辑不会改变任何
/// 区域 → `derived` Rc 跨键复用（不再全篇重扫）；不同 → 整篇重建。
fn region_line_sig(line: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if line.trim_start().starts_with("```") {
        h.write_u8(1);
    } else {
        let t = line.trim();
        if t == "$$" {
            h.write_u8(2);
        } else if crate::markdown::parse::inline_style::one_line_math(line).is_some() {
            h.write_u8(3);
        } else if let Some(p) = crate::markdown::parse::inline_style::blockquote_prefix(line) {
            match crate::markdown::parse::recognition::alert_prefix(&line[p..]) {
                Some((kind, _, fold)) => {
                    h.write_u8(4);
                    alert_kind_tag(kind).hash(&mut h);
                    fold.hash(&mut h);
                }
                None => h.write_u8(5),
            }
        } else if let Some(a) = crate::markdown::parse::inline_style::math_align_marker(line) {
            h.write_u8(6);
            use crate::markdown::parse::inline_style::MathAlign::*;
            match a {
                Left => h.write_u8(0),
                Center => h.write_u8(1),
                Right => h.write_u8(2),
            }
        } else {
            h.write_u8(7);
            line.as_bytes().contains(&b'$').hash(&mut h);
        }
    }
    h.finish()
}

/// 一整行（块）的**整形内容缓存**（内容寻址，键 = 块内容指纹 `block_content_id`）。
/// 滚动/带重建时复用已整形块，不再把可视带整批重 shape（`shape_line` ）并为每个代码块
/// 重跑 syntect——结构密集代码文档「滚动每档 65ms」的主因（对齐 `view` 的行虚拟化复用
/// + `editor` 「只重排变更区」的原则）。
///
/// - 键 = 内容指纹（kind/跨度/首末行文本）^ 换代键 `key`（宽度/mermaid 缩放）：
///   **不随块下标/行号位移**——结构编辑（Enter/合并）后位移但内容未变的块指纹不变 →
///   命中旧行，免整带重排（旧实现按块下标键，结构编辑让后缀块下标全位移 → 整带 miss
///   重排 ~6ms debug）；内容真正变化的块指纹变 → 自然 miss。
/// - 内容变化靠指纹覆盖；数学就绪/图片尺寸给特定块换高 → `invalidate_ids` 定向作废；
///   换源/主题字号变化 → 整体 `clear`。
/// - 结构性编辑（块数/块序变）/ 换源 / 主题字号变化 → 整体 `clear`。
/// - caret 严格落在**含行内公式的文本行**时会 reveal（shape 依赖光标）→ 该行不缓存
///   （调用方 `caret_dep` 判定）。
/// - 容量上限见 [`crate::config::limits::SHAPED_ROW_CACHE_CAP`]，超出按最近最少使用淘汰。
struct RowShapeCache {
    entries: std::collections::HashMap<u64, RowShapeEntry>,
    cap: usize,
    clock: u32,
}

struct RowShapeEntry {
    key: u64,
    last_seen: u32,
    row: std::rc::Rc<element::ShapedRow>,
}

impl RowShapeCache {
    fn new(cap: usize) -> Self {
        Self { entries: std::collections::HashMap::new(), cap, clock: 0 }
    }
    /// 命中：更新访问钟并返回 `Rc`（浅克隆即可取得行，heavy 数据全是 Arc/Rc）。
    fn get(&mut self, id: u64, key: u64) -> Option<std::rc::Rc<element::ShapedRow>> {
        match self.entries.get_mut(&id) {
            Some(e) if e.key == key => {
                e.last_seen = self.clock;
                self.clock = self.clock.wrapping_add(1);
                Some(e.row.clone())
            }
            _ => None,
        }
    }
    /// 该块存在**但键已换代**（宽度/mermaid 缩放变化把旧行逐出）——与"从未缓存"区分：
    /// 换代重排时旧 `settled/verified/attempts` 判决对应的是上一代结果，应作废重来。
    fn key_mismatch(&self, id: u64, key: u64) -> bool {
        self.entries.get(&id).is_some_and(|e| e.key != key)
    }
    /// 插入（新行）；容量满时淘汰 last_seen 最小的条目。返回缓存的 `Rc`。
    fn put(&mut self, id: u64, key: u64, row: element::ShapedRow) -> std::rc::Rc<element::ShapedRow> {
        if self.entries.len() >= self.cap && !self.entries.contains_key(&id)
            && let Some(old) = self.entries.values().map(|e| e.last_seen).min()
                && let Some(k) = self
                    .entries
                    .iter()
                    .find(|(_, e)| e.last_seen == old)
                    .map(|(k, _)| *k)
                {
                    self.entries.remove(&k);
                }
        let rc = std::rc::Rc::new(row);
        self.entries.insert(
            id,
            RowShapeEntry { key, last_seen: self.clock, row: rc.clone() },
        );
        self.clock = self.clock.wrapping_add(1);
        rc
    }
    /// 逐个作废（数学就绪/内容变化的受影响块；键 = 内容指纹）。
    fn invalidate_ids(&mut self, ids: &[u64]) {
        for &b in ids {
            self.entries.remove(&b);
        }
    }
    fn clear(&mut self) {
        self.entries.clear();
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// 含 `.notdef`（回退未命中）块的**真实平台重排次数**上限。gpui 的 `LineLayoutCache`
/// 把平台 `layout_line` 结果按 `(text, font, runs)` 键缓存、与 wrap-width 无关——扰动
/// 宽度永远打不到 DWrite（115c131 的失败根因）。因此自愈改走「冷却一帧」：每次真实
/// 重排后让该键一帧不被引用 → 两帧缓存自然淘汰 → 下一次重排才是新键、真打到平台。
/// 连续 `NOTDEF_RETRY_ATTEMPTS` 次真实平台重排仍坏 → 判定字符确实无字形（正确显示
/// tofu），落入 `settled` 从此不再重排空转。冷启动 DWrite 首次 shape 回退字形可能给
/// .notdef、第二次同 (font,size) 即正常——3 帧内自愈；给 15 次余量（≈0.5s）。
pub(crate) const NOTDEF_RETRY_ATTEMPTS: u32 = 15;

/// `.notdef`（回退未命中）坏块的自愈状态机（被 [`element::build_frame`] Pass2 驱动）。
/// 每次「真实平台重排」出 notdef 后登记一次并转冷却；冷却帧**不调用 gpui shape**——
/// 让 `LineLayoutCache` 里该 (text,font,runs) 键自然过期（两帧缓存只在当前/上一帧存
/// 活），下一次重排才是全新键、真的重新打到 DWrite。回退解析成功（或任何冷却行恢复）
/// 即整体标记「回退已温」，此后 notdef = 确无字形、直接入缓存不再空转。
#[derive(Default)]
pub(crate) struct NotdefRetry {
    /// 块 → 已做的真实平台重排次数（达到 [`NOTDEF_RETRY_ATTEMPTS`] 上限 → `settled`）。
    pub attempts: rustc_hash::FxHashMap<usize, u32>,
    /// 处于冷却帧的块（下一帧不 shape，复用 `last` 几何）。
    pub cooling: rustc_hash::FxHashSet<usize>,
    /// 已判定「确无字形」的块：命中缓存时不再当 miss 重排（稳定，避免空转）。
    pub settled: rustc_hash::FxHashSet<usize>,
    /// 冷却帧用于继续显示的上一份 notdef 行（几何与新鲜重排一致，不闪跳）。
    pub last: rustc_hash::FxHashMap<usize, std::rc::Rc<element::ShapedRow>>,
    /// 已做过「回退字形校验重排」的块：冷启动首次 shape 用到回退字体（❗⚠✅ 等）时，
    /// DWrite 可能给非零但错误的占位字形（非 .notdef，glyph id≠0 检测不到）→ 冷却一次
    /// 强制第二次真实重排并缓存那次结果；标记后直接走正常缓存。
    pub verified: rustc_hash::FxHashSet<usize>,
}

impl NotdefRetry {
    /// 单块状态整体作废（内容编辑/前缀保留外块）。
    fn clear_block(&mut self, block: usize) {
        self.attempts.remove(&block);
        self.cooling.remove(&block);
        self.settled.remove(&block);
        self.verified.remove(&block);
        self.last.remove(&block);
    }

    /// 「换代」作废：块被**新代**键重新整形前，清掉上一代的判决（attempts/settled/
    /// verified/last），**保留**冷却位——冷却帧仍复用上一份几何，换代重排不闪跳。
    /// 作用：图片解码完成触发 `igen` 换代重建时，若某块曾在 DWrite 退化期（解码卡顿
    /// 窗口）被误判 `settled`（预算烧光）或 `verified`（非零占位字形），换代后重新
    /// 校验/重试即可自愈，不再整次运行永久豆腐块。
    fn clear_stale(&mut self, block: usize) {
        self.attempts.remove(&block);
        self.settled.remove(&block);
        self.verified.remove(&block);
        self.last.remove(&block);
    }

    /// 前缀保留：结构编辑后下标 < `keep` 的块未位移，重试/冷却/settled 状态保留。
    fn retain_prefix(&mut self, keep: usize) {
        if keep == 0 {
            self.clear();
            return;
        }
        self.attempts.retain(|&k, _| k < keep);
        self.cooling.retain(|&k| k < keep);
        self.settled.retain(|&k| k < keep);
        self.verified.retain(|&k| k < keep);
        self.last.retain(|&k, _| k < keep);
    }

    fn clear(&mut self) {
        self.attempts.clear();
        self.cooling.clear();
        self.settled.clear();
        self.verified.clear();
        self.last.clear();
    }
}

/// 冷启动回退自愈诊断：`WY_NOTDEF_DEBUG=1` 时把首启探针/重试/给出的过程打到 stderr，
/// 用于在真实 Windows 首启复现 `.notdef` 豆腐块问题时拿到确切证据（不开启则零开销）。
#[inline]
pub(crate) fn notdef_debug(args: std::fmt::Arguments<'_>) {
    if std::env::var_os("WY_NOTDEF_DEBUG").is_some() {
        eprintln!("[WY-NOTDEF] {args}");
    }
}

/// 进程级 warm 状态（一次/进程）：`WARM_DONE` 置位后，各窗格 paint 顶首先查
/// [`warm_emoji_is_done`] 快速短路，**不再进入** [`warm_emoji_rasterizer`] 本体。
static WARM_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static WARM_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static WARM_SIZES: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
const WARM_MAX_ATTEMPTS: usize = 60;
/// 撤销/重做栈深度上限（超出丢最旧）。
const UNDO_LIMIT: usize = 200;

/// warm 是否已完成：各窗格 paint 前调用，完成后直接跳过 warm，零重复访问。
pub(crate) fn warm_emoji_is_done() -> bool {
    WARM_DONE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Windows 冷启动 color-emoji 光栅预热（一次/进程，**共享给所有文本窗格**：预览/
/// WYSIWYG/源码编辑区）。实测：DWrite 在冷进程里**首次**光栅化彩色 COLR 字形时
/// `rasterize_color` 失败，gpui 回退为单色（彩色 emoji 无单色轮廓 → 空白/方框），且
/// `sprite_atlas` 按 (font,glyph,size) 把坏结果钉住整次运行（字符在文本里、复制正常、
/// shape 正常——就是画不出）。各窗格在自己 `paint` 的**顶首**（画自己任何文字之前）
/// 都调本函数；**先画的窗格**先在此把 DWrite/D3D 彩字管线打热（冷僻字号 7px、首个
/// 可能失败、其后即成功），之后任何窗格的真实 emoji 首帧即正确，不再被某个先画窗格
/// 毒化。探针字体取窗口缺省文本样式即可——回退按字符独立，emoji 字符必路由到回退彩
/// 字字体。
pub(crate) fn warm_emoji_rasterizer(window: &mut Window, tag: &'static str) {
    // 冷启动 COLR 自愈：DWrite 彩色光栅在冷进程首次使用时失败/返回空白。平台层
    //（gpui_windows rasterize_glyph）已保证「空白不写 atlas 缓存」（返回 Err 即不入缓存），
    // 因此 paint_emoji 的 `.is_ok()` 从此“诚实”——失败 = 真的没彩出来。这里就借助它做
    // 帧驱动重试：**单帧内磨完全部字号**（WYSIWYG/编辑器/预览真实字号不同：分别取自
    // theme.font_size / font_size_px / window.text_style，默认 DEFAULT_FONT_SIZE=14），
    // 常规+粗体各自 shape 一遍合并 emoji 后统一屏幕外光栅；全部成功即 DONE，正常冷启动
    // 1 帧收工、不再被逐帧 window.refresh() 拖住启动；任一字号失败 → 强制下一帧整批重试
    //（失败不入缓存、自然自愈），直到全成功或帧数上限；给出总帧数上限，防 GPU/驱动
    // 彻底不行时无限自旋。
    if WARM_DONE.load(std::sync::atomic::Ordering::Relaxed) {
        // 高频重绘下每帧都打会刷屏 → 仅 WY_WARM_DEBUG=1 时打，且每进程**只打一次**
        //（「哪个窗格命中已完成的守卫」这一诊断目标达成后，默认路径不再喷日志）。
        static LOGGED_SKIP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var_os("WY_WARM_DEBUG").is_some()
            && !LOGGED_SKIP.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!("[WY-WARM] already done -> skip ({tag})");
        }
        return;
    }

    let sizes = WARM_SIZES.get_or_init(|| {
        let body_size = match window.text_style().font_size {
            gpui::AbsoluteLength::Pixels(p) => p.as_f32(),
            gpui::AbsoluteLength::Rems(r) => r.to_pixels(window.rem_size()).as_f32(),
        };
        let mut v: Vec<f32> = vec![body_size, 13.0, 14.0, 15.0, 16.0, 17.0, 18.0, 20.0, 24.0];
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.dedup();
        // 仅保留合理范围，避免把冷僻/超大字号也磨进去。
        v.retain(|s| (10.0..=32.0).contains(s));
        v
    });

    let probe = "⚠✅❗❌❤️🔥  🚀✨★☆➡  😀😅🤔💡📌";
    let body = window.text_style().font();
    let mut body_bold = body.clone();
    body_bold.weight = gpui::FontWeight::BOLD;
    // 标题（## 等）形体 = 正文**粗体**（element.rs `text_style_for` 对 Heading 置 BOLD）：
    // emoji 回退落到的 FontId 可能与常规体不同 → atlas 键 (font,glyph,size) 不同 → 必须把
    // 粗体也一起磨热，否则「标题里的 ❗ 首次真实光栅」冷失败被钉成空白（图片后的标题恰是
    // 首见者——图片只是恰好排前面，非图片自身问题）。
    let bases: Vec<gpui::Font> = vec![body.clone(), body_bold];

    // —— 单帧内磨完全部字号；任一字号失败即整批下一帧重试（保留 COLR 冷自愈）——
    let started = std::time::Instant::now();
    let mut all_ok = true;
    let mut font_ids: Vec<u32> = Vec::new();
    // 先 shape 完再画：shape 拿不可变借用，画要走 window 可变借用，分开时段避免 E0502。
    // 常规 + 粗体分别 shape，合并 emoji（含各自字号）后再统一屏幕外 paint。
    let mut emojis: Vec<(gpui::FontId, gpui::GlyphId, f32)> = Vec::new();
    {
        let ts = window.text_system();
        for &fs in sizes.iter() {
            for font in &bases {
                let run = [gpui::TextRun {
                    len: probe.len(),
                    font: font.clone(),
                    ..Default::default()
                }];
                let line = ts.shape_line(probe.into(), px(fs), &run, None);
                emojis.extend(line.runs.iter().flat_map(|r| {
                    r.glyphs
                        .iter()
                        .filter(|g| g.is_emoji)
                        .map(move |g| (r.font_id, g.id, fs))
                }));
            }
        }
    }
    for (font_id, glyph_id, fs) in emojis {
        let fid = font_id.0 as u32;
        if !font_ids.contains(&fid) {
            font_ids.push(fid);
        }
        // 屏幕外绘制：仅触发 atlas 光栅化（图元被裁剪、无可见痕迹）。
        if window
            .paint_emoji(point(px(-1e7), px(-1e7)), font_id, glyph_id, px(fs))
            .is_err()
        {
            all_ok = false;
        }
    }
    if all_ok {
        WARM_DONE.store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[WY-WARM] all {:?} sizes ok in {:.0}ms (base={:?}) done-by={tag}",
            sizes,
            started.elapsed().as_millis(),
            body.family
        );
    } else {
        // 仍有字号没磨热（COLR 冷 / 突发失败）：失败不入缓存 → 必须强制下一帧整批重试，
        // 否则该字号真实首光栅冷失败被原生 atlas 钉成豆腐块（旧逐帧版即靠这自愈）。
        let attempt = WARM_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if attempt < WARM_MAX_ATTEMPTS {
            eprintln!(
                "[WY-WARM] partial fail(attempt={attempt}) sizes={sizes:?} font_ids={font_ids:?} from={tag} -> refresh",
            );
            window.refresh();
        } else {
            // 到上限仍不行 → 放弃自旋，等用户滚动/其它重绘触发（原生仍可能自愈）。
            WARM_DONE.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// 代码块高亮解析缓存键：`(语言名, 代码全文)`。`parse_code_lines` 是纯计算
/// （与预览共用全局 `SYNTAX_SET`/`SYNTAX_CACHE`，不依赖 window/平台）→ 可在
/// background executor 跑；就绪后 `on_code_parse_ready` **定向作废该代码块的整形缓存**
/// （不参与全局 shape_key）→ 该块按下帧重建用高亮行替换占位纯文本行，其余块保持命中
/// （对齐 `math_gen` 只影响本身块的语义，避免代码块内每键打字把整带缓存全量 miss）。
pub(crate) type CodeParseKey = (SharedString, SharedString);

/// [`CodeParseKey`] 对应的缓存条目（后台线程写、主线程读）。
#[derive(Clone)]
pub(crate) enum CodeParseState {
    /// 解析在途（防重复发起）。
    Pending,
    /// 已解析；`None` = 该语言无高亮（按纯文本渲染）。
    Done(Option<crate::syntax::CodeLineRuns>),
}

/// [`WyEditor::code_parse_lookup`] 的返回（shape 期用）。
pub(crate) enum CodeParseLookup {
    /// 已就绪：直接用其着色（`None` = 纯文本）。
    Ready(Option<crate::syntax::CodeLineRuns>),
    /// 未就绪：登记后台解析，本帧先按纯文本渲染（就绪后自动上色）。
    NotReady,
}

/// 跨帧滚动预热进度（见 [`element::prewarm_band`]）：方向只由带序号跨界
/// （`band.top/512` 取整变化）决定，量子内小滚不换向；预算按块数硬上限；
/// `bias` = 目标带内滑动窗起点，每帧推进、回绕兜底（保证总有进度、永不卡死）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prewarm {
    pub dir: i8,
    /// 基础预算（每帧预热块数的下限；慢滚/静止时用它维持对下一带的渐进填充）。
    pub blocks_per_frame: usize,
    pub bias: usize,
    pub last_band_idx: Option<i32>,
}

impl Default for Prewarm {
    fn default() -> Self {
        Self { dir: 0, blocks_per_frame: 3, bias: 0, last_band_idx: None }
    }
}

impl Prewarm {
    /// **速度自适应预算**：`vel` = 本帧带序号的跳跃量（= 滚动速度，带/帧）。
    ///
    /// 快滚时视口每帧跨多个 512px 量子带，固定 3 块/帧的预热追不上 → 落点带仍冷、Pass2
    /// 每带穿越补一次 ~6.4ms 整形突发（方波帧耗时 = “粘滞”来源）。按速度放大预算：
    /// - `vel=0`（静止/带内微滚）→ 只用基础预算，维持对下一带的渐进填充不空转；
    /// - `vel=1-2`（匀速滚）→ 预算 ~2 帧盖满一档（~20 块），连续带穿越变缓存命中；
    /// - `vel≥4`（拖条/猛滑）→ 预算接近一档块数，单帧即把下一落点带填满。
    ///
    /// 硬上限 [`Self::MAX_BUDGET`]（≈一档短段落块数）兜底最坏帧内整形量；预算单位是
    /// “本帧新 shape 的块数”，与 `prewarm_band` 的 `done` 计数同式。
    pub(crate) const MAX_BUDGET: usize = 24;
    pub(crate) fn effective_budget(&self, vel: usize) -> usize {
        (self.blocks_per_frame.max(1) + vel.saturating_mul(4)).min(Self::MAX_BUDGET)
    }
}

/// 后台装载结果：整篇 O(n) 解析（Doc::open + 派生标记计数）的**纯输出**。`Send`，
/// 可在 `background_executor` 任意线程构建、再回 UI 线程 [`WyEditor::install_open`]。
pub(crate) struct WyOpenResult {
    pub(crate) doc: Doc,
    pub(crate) math_dollar_cnt: usize,
    pub(crate) alert_marker_cnt: usize,
}

impl WyEditor {
    /// 缩放锚点收敛判定：相邻两帧恢复目标位移小于此值（px）即视为高度已稳定并消费锚点。
    pub(crate) const ZOOM_ANCHOR_EPS: f32 = 0.5;
    /// 缩放锚点收敛兜底：最多逐帧施加这么多次后强制消费（防高度持续细化的极端文档上
    /// 锚点永不收敛、宿主每帧覆盖用户滚动）。
    pub(crate) const ZOOM_ANCHOR_MAX_APPLIES: u32 = 30;

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ts = window.text_style();
        let fs: Pixels = ts.font_size.to_pixels(window.rem_size());
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(fs)));
        Self {
            doc: Doc::open(SharedString::default()),
            focus: cx.focus_handle(),
            caret: 0,
            loaded: false,
            editable: true,
            theme: WyTheme::from_style(&p, ts.font(), fs),
            frame: RefCell::new(None),
            last_bounds: Cell::new(None),
            zoom_anchor: RefCell::new(None),
            viewport_top_px: Cell::new(0.0),
            scroll_target: Cell::new(None),
            last_pin: Cell::new(None),
            scroll_applies: Cell::new(0),
            zoom_last_target: Cell::new(None),
            zoom_applies: Cell::new(0),
            host_scroll: RefCell::new(None),
            content_gen: Cell::new(0),
            shape_band: Cell::new(None),
            prewarm: RefCell::new(Prewarm::default()),
            block_layout: RefCell::new(None),
            struct_ver: Cell::new(0),
            struct_dirty_line: Cell::new(0),
            struct_old_end_line: Cell::new(0),
            struct_line_delta: Cell::new(0),
            struct_blk_old: Cell::new(0),
            struct_blk_span_delta: Cell::new(0),
            struct_edit_pending: Cell::new(false),
            regions_ver: Cell::new(0),
            fold_ver: Cell::new(0),
            fold_plan: RefCell::new(None),
            pending_patch_blocks: RefCell::new(None),
            math_dollar_cnt: Cell::new(0),
            alert_marker_cnt: Cell::new(0),
            derived: RefCell::new(None),
            shape_rows: RefCell::new(RowShapeCache::new(
                crate::config::limits::SHAPED_ROW_CACHE_CAP,
            )),
            // 含 `.notdef`（元字形，glyph id=0）的坏排板块：Windows 冷启动时回退字体
            // （❗⚠✅ 等主字体缺的字形）首次解析可能给出 .notdef，且坏的 ShapedRow 若被
            // 行缓存钉住整次运行不消失。此表登记「待新鲜重排」的块并驱动机器（见
            // [`NotdefRetry`]）：Pass2 命中判定/入缓存前都扫 .notdef，命中坏缓存视作
            // miss、出新 notdef 不入缓存，走「冷却一帧→重排」直至 DWrite 回退解析成功。
            notdef_retry: RefCell::new(NotdefRetry::default()),
            #[cfg(test)]
            reshapes: Cell::new(0),
            #[cfg(test)]
            prewarm_shapes: Cell::new(0),
            #[cfg(test)]
            coolings: Cell::new(0),
            image_cache: gpui::RetainAllImageCache::new(cx),
            image_sizes: RefCell::new(std::collections::HashMap::new()),
            images_gen: Cell::new(0),
            image_widths: RefCell::new(std::collections::HashMap::new()),
            image_resize_drag: RefCell::new(None),
            math_slots: Rc::new(RefCell::new(std::collections::HashMap::new())),
            math_color: RefCell::new(None),
            math_gen: Cell::new(0),
            mermaid_zoom: RefCell::new(std::collections::HashMap::new()),
            mermaid_zoom_gen: Cell::new(0),
            focused_mermaid: None,
            code_parse: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            code_parse_need: RefCell::new(Vec::new()),
            // 数学**显示行数**缓存（键=去定界符源）：`math_card_height` 回退分支按行数
            // 估高——`math_display_lines` 会对每个源做 LaTeX→Unicode 转换，3000 个数学
            // 块每次 fold_plan 重建重复转换 = ~38ms（B3）；缓存后 O(1) 复用。内容变更
            // 清缓存。
            math_line_map: RefCell::new(std::collections::HashMap::new()),
            inline_math_rows: RefCell::new(std::collections::HashMap::new()),
            base_dir: None,
            marked: None,
            sel_anchor: None,
            dragging: false,
            toc_cache: RefCell::new(None),
            highlight_search: None,
            highlight_seq: 0,
            code_h_scroll: RefCell::new(std::collections::HashMap::new()),
            code_h_drag: RefCell::new(None),
            table_h_scroll: RefCell::new(std::collections::HashMap::new()),
            table_h_drag: RefCell::new(None),
            folded_headings: RefCell::new(std::collections::HashSet::new()),
            block_regions: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            open_unit: None,
            last_edit: None,
            hover_image: Cell::new(None),
            selected_image: Cell::new(None),
            image_edit: None,
            image_edit_anchor: Cell::new(None),
        }
    }

    /// 覆盖主题。
    #[allow(dead_code)]
    pub fn set_theme(&mut self, theme: WyTheme) {
        self.theme = theme;
        *self.frame.borrow_mut() = None;
        // 基础布局存的是 px 真值，主题/字号变化量纲改变 → 代置失效位（否则带外沿用
        // 旧字号的固定 px 高，行高与带内新字号不一致，滚动跳变）。
        *self.block_layout.borrow_mut() = None;
        // 整形行内容同样依赖主题（字号/颜色/字体）→ 整体清空。
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
    }

    /// 显式设置字号（应用把全局字号同步进来；变更新帧缓存）。不读窗口 text_style，
    /// 保证 measure/prepaint 两阶段一致。字号不只影响正文/代码尺寸，标题也按
    /// `StyleParams::heading_metrics` 等比缩放（见 `WyTheme::set_font_size`）。
    pub fn set_font_size(&mut self, size: Pixels) {
        if self.theme.font_size == size {
            return;
        }
        // 缩放锚点：重排前记录视口顶部内容点，帧按新字号重建后由 element prepaint
        // 自驱动滚回（`advance_zoom_restore`）——Ctrl+滚轮缩放不带动滚动位置，
        // 对齐 view/editor。
        self.capture_zoom_anchor();
        self.theme.set_font_size(size);
        *self.frame.borrow_mut() = None;
        // 同 `set_theme`：字号变 → 基础布局（px）失效。
        *self.block_layout.borrow_mut() = None;
        // 字号变 → 行整形（宽/高）也失效。
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
    }

    /// 从全局主题**重建整份调色主题**（应用「切换黑白主题」后调用）：WyTheme 在
    /// `new()` 时由 [`StyleParams::from_theme_with_font`] 一次性派生并缓存（主题切换
    /// 后如不重建，预览/WYSIWYG 的文字/代码块/链接等颜色会保持旧主题色）。正文字体与
    /// 字号沿用本主题既有的（创建时从 `window.text_style()` 确定性取用），只重建色板。
    /// `set_theme` 会清空帧/布局/整形缓存，下一帧按新色板重画。
    pub(crate) fn rebuild_theme_from_global(&mut self, cx: &mut gpui::App) {
        let fs = self.theme.font_size;
        let body_font = self.theme.font.clone();
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(fs)));
        self.set_theme(WyTheme::from_style(&p, body_font, fs));
    }

    /// 切只读/可编辑门控（A1：MD 右栏预览用）。只读 = 输入/IME/删除/撤销/粘贴短路
    /// （`replace_range` 硬门控）、不画光标；保留光标移动、点选/复制、链接点击、折叠、
    /// 搜索高亮、表格/代码横向滚动条。切只读时丢弃 IME 组合态、收拢选区为光标（避免
    /// 只读态下残留活动选区误导）。已处目标态则 no-op（不触发帧失效）。
    pub fn set_editable(&mut self, editable: bool) {
        if self.editable == editable {
            return;
        }
        self.editable = editable;
        if !editable {
            self.marked = None;
            self.sel_anchor = None;
            self.dragging = false;
            // 只读：无编辑能力 → 收起图片编辑卡片与悬浮按钮态、图片选中态。
            self.image_edit = None;
            self.image_edit_anchor.set(None);
            self.hover_image.set(None);
            self.selected_image.set(None);
        }
        *self.frame.borrow_mut() = None;
    }

    /// 当前是否可编辑（只读预览 = `false`）。
    pub fn is_editable(&self) -> bool {
        self.editable
    }

    /// A4：目录条目 `(源码行, 标题级别, 标题文本)`——与 `MarkdownPreview::toc_entries`
    /// 同构，首字段语义 = **源码行**（统一组件宿主用 `scroll_to_line` 跳转）。遍历 Doc
    /// 块（O(块数)，按 `content_gen` 缓存，仅内容变更重扫；侧边栏目录渲染低频消费）。
    pub(crate) fn toc_entries(&self) -> TocEntries {
        if let Some((g, t)) = &*self.toc_cache.borrow()
            && *g == self.content_gen.get()
        {
            return t.clone();
        }
        let n = self.doc.block_count();
        let mut out = Vec::with_capacity(n.min(1024));
        for i in 0..n {
            let Some(m) = self.doc.block_meta(i) else { continue };
            if let crate::markdown::parse::RowKind::Heading(level) = m.kind {
                let t = self.line_text(m.src_line as usize);
                let clean = t.trim().trim_start_matches('#').trim_start();
                out.push((m.src_line as usize, level, SharedString::from(clean)));
            }
        }
        *self.toc_cache.borrow_mut() = Some((self.content_gen.get(), out.clone()));
        out
    }

    /// A4：目录/搜索跳转 —— 把（只读）预览滚动到**源码第 `src_line` 行**所在块。
    /// 镜像 `prepare_font_zoom` 渲染期重建套路：先以当前（或默认带建的）窗口化帧估块顶
    /// → 定位目标可视带 → 在目标带重建窗口化帧读**精确**块顶 → 写回宿主 `ScrollHandle`
    ///（内容坐标 top = −offset.y − pad_y → offset = −(top + pad_y)），并发布新带给下帧。
    pub(crate) fn scroll_to_line(&mut self, src_line: usize, window: &mut Window) {
        let _ = self.scroll_to_line_impl(src_line, window);
    }

    /// [`Self::scroll_to_line_impl`] 的精确落地（含宿主偏移写入），返回写入的目标
    /// **绝对偏移** `-(exact_top + pad_y)`；空文档/无宿主时返回 `f32::NAN`。编辑区⇄
    /// 预览同步的 `advance_pending_scroll` 复用同一落点，并以返回值逐帧核对是否稳住。
    fn scroll_to_line_impl(&mut self, src_line: usize, window: &mut Window) -> f32 {
        let n = self.doc.block_count();
        if n == 0 || self.doc.len() == 0 {
            return f32::NAN;
        }
        if src_line >= self.doc.line_count() {
            return f32::NAN;
        }
        let block = self.doc.block_index_for_line(src_line).min(n - 1);
        let width = self.frame.borrow().as_ref().map(|f| f.width).unwrap_or(px(800.));
        // 确保基础布局已建（未建则先建一次再读估计块顶；带外只取估算用于定位）。
        if self.frame.borrow().is_none() {
            let band = self.shape_band().unwrap_or((0., 5000.));
            let f = element::build_frame(&*self, window, width, band);
            *self.frame.borrow_mut() = Some(std::rc::Rc::new(f));
        }
        let est_top = self
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.block_top(block))
            .unwrap_or(0.);
        let band = zoom_band_for_top(est_top, window.viewport_size().height);
        if self.shape_band() != Some(band) {
            self.set_shape_band(Some(band));
        }
        let frame = element::build_frame(&*self, window, width, band);
        let exact_top = frame.block_top(block);
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        if let Some((vh, pad_y)) = self.host_scroll.borrow().clone() {
            let target = -(exact_top + pad_y);
            vh.set_offset(gpui::point(gpui::px(0.), gpui::px(target)));
            return target;
        }
        f32::NAN
    }

    /// 编辑区⇄预览滚动同步：把目标块**源码行**记入 `scroll_target`（宿主渲染期写入，
    /// 只存 Cell 不做任何布局/滚动）。WyEditor 元素 prepaint 经 [`Self::advance_pending_scroll`]
    /// 逐帧重钉宿主偏移直到稳住——提前在宿主 render 期调 `scroll_to_line` 会被本帧
    /// 布局复位，必须借 WY 自己的重钉循环（同缩放恢复收敛）。
    pub(crate) fn set_scroll_target(&self, src_line: usize) {
        self.scroll_target.set(Some(src_line));
        self.last_pin.set(None);
        self.scroll_applies.set(0);
    }

    /// 是否有待落地的滚动目标（prepaint 据此驱动重钉 + 刷帧）。
    pub(crate) fn scroll_target_active(&self) -> bool {
        self.scroll_target.get().is_some()
    }

    /// 编辑区⇄预览滚动同步的**重钉兜底**：最多逐帧施加这么多次后强制清除（防宿主
    /// 偏移被滚动条/布局持续复位的病态下永不收敛 → 每帧 `window.refresh()` + 重排
    /// 满载忙循环；对齐缩放恢复 [`Self::ZOOM_ANCHOR_MAX_APPLIES`]）。
    pub(crate) const SCROLL_TARGET_MAX_APPLIES: u32 = 30;

    /// 同步重钉在「复用态」下判定「用户已接管本窗滚动」的偏移阈值（px）：宿主当前偏移
    /// 偏离我方上次钉定目标超过该值 = 用户切过来滚了（一节滚轮 ≈ 54px+，绝无歧义）。
    /// 一旦判定即放弃重钉，避免与用户滚轮互搏（面板交接处上下跳动的直接根因）。
    pub(crate) const USER_TAKEOVER_PX: f32 = 8.0;

    /// 推进待落地滚动（WyEditor 元素 prepaint 每帧调用）：目标块**精确**顶经
    /// `scroll_to_line_impl` 写回宿主句柄。先读宿主当前偏移，若与上次写入的目标一致
    /// （说明布局未复位、已稳住）则清除目标停止重钉，否则重写并继续。
    ///
    /// - **后续帧复用 `last_pin` 缓存目标只写不重建**（首帧经 `scroll_to_line_impl`
    ///   已在目标带整形真值）：确认帧零成本，避免大文档上每帧 band 重建放大 CPU。
    /// - **[`Self::SCROLL_TARGET_MAX_APPLIES`] 兜底**：超限强制清除，绝不无限重钉。
    /// - 宿主未注入（live2 / 元素卸载期）：**丢弃残留目标**，宿主回归后按陈旧块号
    ///   重钉（仿缩放恢复宿主缺失即弃锚点）。
    pub(crate) fn advance_pending_scroll(&mut self, window: &mut Window) {
        let Some(sl) = self.scroll_target.get() else {
            return;
        };
        let applies = self.scroll_applies.get();
        if applies + 1 >= Self::SCROLL_TARGET_MAX_APPLIES {
            // 超限兜底：强制停止（最后一次 `scroll_to_line_impl` 已把精确目标写进宿主）。
            self.scroll_target.set(None);
            self.last_pin.set(None);
            self.scroll_applies.set(0);
            return;
        }
        self.scroll_applies.set(applies + 1);
        let Some((vh, _pad_y)) = self.host_scroll.borrow().clone() else {
            self.scroll_target.set(None);
            self.last_pin.set(None);
            self.scroll_applies.set(0);
            return;
        };
        let cur = (-vh.offset().y).as_f32();
        // 复用上次钉定目标（`last_pin` 与当前 `sl` 同源）则直接收敛，不再重建；
        // 首帧 / 目标块变化则经 `scroll_to_line_impl` 精确重建。
        let reused = match self.last_pin.get() {
            Some((psl, pt)) if psl == sl => Some(pt),
            _ => None,
        };
        let target = match reused {
            Some(pt) => pt,
            None => self.scroll_to_line_impl(sl, window),
        };
        if !target.is_finite() {
            self.scroll_target.set(None);
            self.last_pin.set(None);
            self.scroll_applies.set(0);
            return;
        }
        if reused.is_some() {
            // 复用态的收敛判定（不重建，避免大文档每帧 band 重建放大 CPU）：
            let diff = cur - target;
            if diff.abs() < 1.5 {
                // 上一帧写入的目标在再次写入前仍被宿主持有 → 布局未复位，稳定落地。
                self.scroll_target.set(None);
                self.last_pin.set(None);
                self.scroll_applies.set(0);
                return;
            }
            if diff.abs() > Self::USER_TAKEOVER_PX {
                // 宿主已偏离目标超过「用户接管」阈值 = 用户在两边面板间切着滚（或其它
                // 控制流）抢走了本窗滚动 → 我方同步重钉立即放弃，**绝不与用户滚轮互搏**。
                // 这是「编辑区滚完切去预览区滚 / 反方向」上下跳动的直接根因：在途重钉
                // 每帧把用户刚滚的分量改写回目标。放弃后由 app 层 sync 正常接管（预览
                // 动了 → 驱动编辑器；两侧收敛由幂等回拉完成）。
                self.scroll_target.set(None);
                self.last_pin.set(None);
                self.scroll_applies.set(0);
                return;
            }
        }
        vh.set_offset(gpui::point(gpui::px(0.), gpui::px(target)));
        self.last_pin.set(Some((sl, target)));
    }

    /// 字号变化的**渲染期精确锚定**：在宿主 render（prepaint 读宿主滚动偏移**之前**）
    /// 按新字号重建窗口化帧，并把新布局里锚点块（缩放前视口顶部内容点）的精确位置
    /// 立即写回宿主 `ScrollHandle`——重排帧一开始就精确定位，不产生「旧偏移 × 新高度」
    /// 的中间错位帧（消除放大/缩小时的向上/向下跳动）。
    ///
    /// prepaint 的 stale 判定（rgen/igen/mgen/width/band）不含字号，帧已在此重建 →
    /// 本帧 prepaint 复用该帧不二次重建。锚点保留给 `advance_zoom_restore` 做带边缘/
    /// 细化的小幅收敛（带内块 build_frame 已整形真值，通常立即收敛）。
    pub(crate) fn prepare_font_zoom(&mut self, new_fs: Pixels, window: &mut Window) {
        if self.theme.font_size == new_fs {
            return;
        }
        // 1) 用旧帧捕捉缩放锚点（宿主已推入 viewport_top）。
        self.capture_zoom_anchor();
        // 2) 记住当前宽/带，应用新字号并作废字号相关缓存（同 set_font_size）。
        let width = self
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.width)
            .unwrap_or(px(800.));
        let band = self.shape_band().unwrap_or((0., 5000.));
        self.theme.set_font_size(new_fs);
        *self.block_layout.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
        // 3) 渲染期重建窗口化帧 → 拿到新布局的精确块 top/height。
        let frame = element::build_frame(&*self, window, width, band);
        // 4) 锚点块在新布局里的精确位置 = 恢复目标（元素局部 y）。
        let target = {
            let anchor = *self.zoom_anchor.borrow();
            match anchor {
                Some((block, frac)) => {
                    let count = frame.block_count();
                    let block = block.min(count.saturating_sub(1));
                    let h = frame.block_height(block).max(1.);
                    frame.block_top(block) + frac * h
                }
                None => 0.0,
            }
        };
        // 5) 写回宿主滚动句柄，并按「锚定后的内容顶」推导新可视带（与 element prepaint
        //    的 band 计算同式）——若带跨界，直接以新带再建一帧并发布给 `shape_band`，
        //    让本帧 measure/prepaint 复用的都是同一帧：既避免「prepare 用旧带 / prepaint
        //    用新带」的冗余重建，也消除两者之间 `shape_band` 的瞬态不一致。
        let host = self.host_scroll.borrow().clone();
        let final_band = if let Some((vh, pad_y)) = host {
            vh.set_offset(gpui::point(gpui::px(0.), gpui::px(-(target + pad_y))));
            zoom_band_for_top(target + pad_y, window.viewport_size().height)
        } else {
            band
        };
        let frame = if final_band != band {
            self.set_shape_band(Some(final_band));
            element::build_frame(&*self, window, width, final_band)
        } else {
            frame
        };
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
    }

    /// 宿主每帧推入当前视口顶部 px（元素局部 y，>=0）。缩放锚点 `capture_zoom_anchor`
    /// 以此为准；宿主用 `#live-scroll` 的 `ScrollHandle::offset()` 减去 #live-body 顶部
    /// 内衬（见 app::panes）得到。
    pub(crate) fn set_viewport_top(&self, top_px: f32) {
        self.viewport_top_px.set(top_px.max(0.));
    }

    /// 第 `top_px`（元素局部 y）落在的块 + 块内比例：`(块下标, (top − 块顶)/块高)`。
    /// 供缩放锚点捕捉与（测试）断言缩放后视口顶部内容点不变。
    pub(crate) fn block_of_px(&self, top_px: f32) -> Option<(usize, f32)> {
        let f = self.frame.borrow().clone()?;
        let n = f.block_count();
        if n == 0 {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if f.block_top(mid) <= top_px {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let block = lo.saturating_sub(1).min(n - 1);
        let h = f.block_height(block).max(1.);
        Some((block, ((top_px - f.block_top(block)) / h).clamp(0.0, 1.0)))
    }

    /// 当前视口顶部落点块的**起始源码行**（0 基；帧未就绪返回 `None`）。编辑区⇄
    /// 预览滚动同步用——预览侧同步键 = 该行所在块的起点，与 [`Self::block_start_of_line`]
    /// 同口径（都以块对齐：块内微滚动不拉动对端）。
    pub(crate) fn top_source_line(&self) -> Option<usize> {
        let f = self.frame.borrow().clone()?;
        let (block, _) = self.block_of_px(self.viewport_top_px.get())?;
        Some(f.base.borrow().src_line(block) as usize)
    }

    /// 源码行 → 所在块的**起始源码行**（0 基；块按 `src_line` 升序连续覆盖，越界归
    /// 首/末块；0 块时原样返回）。滚动同步把「编辑器顶行」归一成块起点，与预览顶
    /// 块比较——同一段落/代码块内微滚动双方键不变，不互相拉动。
    pub(crate) fn block_start_of_line(&self, line: usize) -> usize {
        let n = self.doc.block_count();
        if n == 0 {
            return line;
        }
        let ix = self.doc.block_index_for_line(line).min(n.saturating_sub(1));
        self.doc.block_meta(ix).map(|m| m.src_line as usize).unwrap_or(line)
    }

    /// 重排前捕捉缩放锚点：当前视口顶部 px → 落在的块 + 块内比例。
    fn capture_zoom_anchor(&self) {
        if let Some(a) = self.block_of_px(self.viewport_top_px.get()) {
            *self.zoom_anchor.borrow_mut() = Some(a);
        }
    }

    /// 宿主把 WYSIWYG 宿主滚动容器（`ScrollHandle`）与内容顶部内衬注入；`None` =
    /// 非宿主（独立测试/未进入 live）时缩放恢复不驱动滚轮。句柄与内衬已被当前值覆盖
    /// 时直接早退——宿主每帧都调，避免反复 RefCell 写 + Rc 克隆。
    pub(crate) fn set_host_scroll(&mut self, handle: Option<gpui::ScrollHandle>, pad_y: f32) {
        if handle.is_some()
            && self.host_scroll.borrow().as_ref().is_some_and(|(_, p)| *p == pad_y)
        {
            return;
        }
        *self.host_scroll.borrow_mut() = handle.map(|h| (h, pad_y));
    }

    /// 当前宿主滚动句柄（供元素 prepaint 写回缩放恢复目标）。
    pub(crate) fn host_scroll(&self) -> Option<(gpui::ScrollHandle, f32)> {
        self.host_scroll.borrow().clone()
    }

    /// 推进（并最终消费）缩放恢复：返回 `Some((目标局部 y, done))`，`done` = 是否已收敛
    /// 消费锚点。**由 WyEditor 自身在 element prepaint 每帧驱动**（宿主应用只负责注入
    /// 句柄 + 推视口顶 + 改字号）——因为字号变化后 frame 逐帧把「估计高」细化为「整形
    /// 真值」，`block_top/height` 缓慢变动；必须在本编辑器自己的重排帧里逐帧重钉，直到
    /// 目标位移 < [`Self::ZOOM_ANCHOR_EPS`] 或超过 [`Self::ZOOM_ANCHOR_MAX_APPLIES`] 帧
    /// 才停（防极端文档高度持续细化时锚点永不收敛、永远覆盖用户滚动）。
    ///
    /// 帧未就绪返回 `None`（下帧再取）；未注入宿主句柄时直接丢弃锚点（不进死循环）。
    pub(crate) fn advance_zoom_restore(&self) -> Option<(f32, bool)> {
        // 先查锚点（绝大多数帧为 None，最早退出）；再查宿主句柄，缺则丢弃锚点（不进死循环）。
        let anchor = (*self.zoom_anchor.borrow())?;
        if self.host_scroll.borrow().is_none() {
            *self.zoom_anchor.borrow_mut() = None;
            self.zoom_last_target.set(None);
            self.zoom_applies.set(0);
            return None;
        }
        let f = self.frame.borrow().clone()?;
        let (block, frac) = anchor;
        let count = f.block_count();
        let block = block.min(count.saturating_sub(1));
        let h = f.block_height(block).max(1.);
        let target = f.block_top(block) + frac * h;
        let applies = self.zoom_applies.get();
        let done = if let Some(prev) = self.zoom_last_target.get() {
            (target - prev).abs() < Self::ZOOM_ANCHOR_EPS
                || applies + 1 >= Self::ZOOM_ANCHOR_MAX_APPLIES
        } else {
            false
        };
        self.zoom_last_target.set(Some(target));
        self.zoom_applies.set(applies + 1);
        if done {
            *self.zoom_anchor.borrow_mut() = None;
            self.zoom_last_target.set(None);
            self.zoom_applies.set(0);
        }
        Some((target, done))
    }

    // —— `.notdef`（回退未命中）坏行自愈 ——
    // Windows 冷启动时 DWrite 对主字体缺的字形（❗⚠✅ 等，需走回退字体）首次解析可能
    // 直接给出 .notdef（glyph id=0，空心矩形）。若这个坏 `ShapedRow` 被行缓存钉住，
    // 整次运行都不消失。实测确认的根因（115c131 失败点）：gpui 的 `LineLayoutCache`
    // 把平台 `layout_line` 结果按 `(text, font, runs)` 键缓存，**与 wrap-width 无关**——
    // 只扰动宽度永远打不到 DWrite，重试 N 帧 → 给出 → 豆腐块钉住。这里改为：命中/入
    // 缓存前扫 .notdef → 坏行不入缓存，进入「冷却一帧 → 新鲜重排」循环（冷却帧不调用
    // gpui shape，让被钉的键自然过期），每次重排都真正打到平台，直到回退解析成功；
    // 连续 [`NOTDEF_RETRY_ATTEMPTS`] 次仍坏才判定字符确无字形（tofu 即正确显示）。
    //
    // **不要**把「回退已温」当作「.notdef = 确无字形」的短路：探针只覆盖少数 (font,size)
    // 组合，真实行的字号/字体可能未探到而首排仍 .notdef（实测首排坏、次排好）——所以
    // 一律走重试，由同位（font,size）的若干次真实重排自行恢复，绝不静默缓存成豆腐块。

    /// 冷却帧取上一份 notdef 行几何（不调用 gpui shape → 被钉的 LineLayoutCache 键
    /// 本帧不被引用、帧末自然淘汰）。返回 `Some` 表示本块处于冷却帧、应直接复用返回行。
    pub(crate) fn cooling_row(&self, block: usize) -> Option<std::rc::Rc<element::ShapedRow>> {
        let mut m = self.notdef_retry.borrow_mut();
        if !m.cooling.remove(&block) {
            return None;
        }
        m.last.remove(&block)
    }

    /// 登记一次 notdef（推进真实平台重排计数 + 置冷却）。返回 `true` 表示已达
    /// [`NOTDEF_RETRY_ATTEMPTS`] 上限 → 判定字符确无字形（settled），本次结果应入缓存。
    pub(crate) fn cool_start(&self, block: usize) -> bool {
        let mut m = self.notdef_retry.borrow_mut();
        let cnt = m.attempts.get(&block).copied().unwrap_or(0) + 1;
        if cnt >= NOTDEF_RETRY_ATTEMPTS {
            m.attempts.remove(&block);
            m.cooling.remove(&block);
            m.last.remove(&block);
            m.settled.insert(block);
            true
        } else {
            m.attempts.insert(block, cnt);
            m.cooling.insert(block);
            false
        }
    }

    /// notdef 后保存本帧几何（供随后的冷却帧 [`Self::cooling_row`] 显示复用）。
    pub(crate) fn cool_keep_last(&self, block: usize, row: std::rc::Rc<element::ShapedRow>) {
        self.notdef_retry.borrow_mut().last.insert(block, row);
    }

    /// 该块是否已判定「确无字形」（settled）：是 → 命中含 notdef 的缓存不再当 miss。
    pub(crate) fn notdef_settled(&self, block: usize) -> bool {
        self.notdef_retry.borrow().settled.contains(&block)
    }

    pub(crate) fn clear_notdef(&self, block: usize) {
        self.notdef_retry.borrow_mut().clear_block(block);
    }

    /// 换代自愈：块被新代键重排前清掉上一代判决（attempts/settled/verified/last），
    /// 保留冷却。由 Pass2 在检测到 `RowShapeCache` 键换代时调用（见 build_frame）。
    pub(crate) fn clear_stale_verdicts(&self, block: usize) {
        self.notdef_retry.borrow_mut().clear_stale(block);
    }

    /// 该块是否已做过「回退字形校验重排」（见 [`NotdefRetry::verified`]）。
    pub(crate) fn fallback_verified(&self, block: usize) -> bool {
        self.notdef_retry.borrow().verified.contains(&block)
    }

    pub(crate) fn mark_fallback_verified(&self, block: usize) {
        self.notdef_retry.borrow_mut().verified.insert(block);
    }

    /// 回退校验冷却：只置冷却标志（不占 notdef 重试次数）——下一帧不 shape、让被钉的
    /// LineLayoutCache 键淘汰，再下一帧带新键真打到平台拿到稳定回退字形。
    pub(crate) fn verify_cool(&self, block: usize) {
        self.notdef_retry.borrow_mut().cooling.insert(block);
    }

    /// 设置初始内容。
    pub fn with_text(mut self, text: &str) -> Self {
        self.set_source_text(text);
        self
    }

    /// 当前完整源文本（宿主写回用；rope 单真源派生）。
    pub fn text(&self) -> SharedString {
        self.doc.text_shared()
    }

    /// 显示语义行数（`str::lines()` 口径，与状态栏一致）——增量索引，O(1)，不物化全文。
    pub fn line_count(&self) -> usize {
        self.doc.line_count().max(1)
    }

    /// 标量字符数（`chars().count()` 口径，状态栏显示用）——增量维护，O(1)。
    pub fn char_count(&self) -> usize {
        self.doc.char_count()
    }

    /// （仅测试）`doc.text_shared` 被调次数——B 步断言 handler 查询/编辑不再物化全文。
    #[cfg(test)]
    pub(crate) fn text_shared_calls(&self) -> u64 {
        self.doc.text_shared_calls()
    }

    /// 聚焦句柄（元素命中/宿主聚焦用）。
    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 覆盖源并重建文档模型（load 路径：一次性 O(n)）。rope COW 为唯一源，不再旁挂
    /// 一份 `content` SharedString（每键整串复制的旧成本由此消掉）。
    ///
    /// 大文档（App load）不直接走这里：整篇 O(n) 解析挪到后台（[`Self::open_from_rope`]），
    /// 完成后再 [`Self::install_open`] 装配——本函数只保留给同步路径（小文档/测试）。
    pub fn set_source_text(&mut self, text: &str) {
        let ss = SharedString::from(text);
        self.install_open(Self::open_from_str(ss));
    }

    /// 是否已装入完整源（见 [`Self::loaded`]）：`false` = App 大文档后台解析在途，
    /// 编辑器内容为空，宿主读写文本必须回退源码编辑区。
    pub(crate) fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// 从 `SharedString` 完整构建（同步路径；`set_source_text` 直接用它）。
    ///
    /// 派生扫描的**标记计数器**（全篇各一次 O(n)，之后增量维护 O(编辑)）——`derived()`
    /// 预筛用计数，避免每次内容变更对 7MB 全篇 `contains('$')` 重扫（debug 下 ~100ms/键）。
    pub(crate) fn open_from_str(text: SharedString) -> WyOpenResult {
        let _t = crate::prof::begin();
        let doc = Doc::open(text.clone());
        crate::prof::end("wy.open.doc", _t);
        WyOpenResult {
            doc,
            math_dollar_cnt: text.as_ref().bytes().filter(|&b| b == b'$').count(),
            alert_marker_cnt: text.as_ref().matches("[!").count(),
        }
    }

    /// 从编辑器 `Rope`（O(1) COW 快照）完整构建——App 大文档 load 的后台段：
    /// `Doc::open` 6.4MB 解析（~200ms）与标记计数全在这里，UI 线程只做快照 + 装配。
    pub(crate) fn open_from_rope(rope: Rope) -> WyOpenResult {
        let _t = crate::prof::begin();
        let ss = SharedString::from(rope.to_string());
        crate::prof::end("wy.open.to_string", _t);
        Self::open_from_str(ss)
    }

    /// 装配后台结果（UI 线程）：整篇重活已发生（`WyOpenResult`），这里只剩换源后的
    /// 状态重置/缓存失效（除 `rebuild_block_regions` 为 O(块) 外全 O(1)/微量）。
    pub(crate) fn install_open(&mut self, r: WyOpenResult) {
        self.doc = r.doc;
        let _t = crate::prof::begin();
        self.rebuild_block_regions();
        crate::prof::end("wy.set_source.regions", _t);
        self.caret = self.caret.min(self.doc.len());
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.open_unit = None;
        self.last_edit = None;
        self.focused_mermaid = None;
        // 换源：图片编辑卡片/悬浮态/选中态随行号失效 → 关闭。
        self.image_edit = None;
        self.image_edit_anchor.set(None);
        self.hover_image.set(None);
        self.selected_image.set(None);
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        self.math_dollar_cnt.set(r.math_dollar_cnt);
        self.alert_marker_cnt.set(r.alert_marker_cnt);
        self.struct_ver.set(0);
        self.struct_dirty_line.set(0);
        self.struct_old_end_line.set(0);
        self.struct_line_delta.set(0);
        self.struct_blk_old.set(0);
        self.struct_blk_span_delta.set(0);
        self.struct_edit_pending.set(false);
        self.math_line_map.borrow_mut().clear();
        // 换源：行的 src_line 全变 → 行内公式→行的登记作废（重新 claim 时再登记）。
        self.inline_math_rows.borrow_mut().clear();
        // 全新内容：派生/数学清单/基础布局全量失效（新代 → 下帧全篇重建）。
        *self.block_layout.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
        self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
        *self.pending_patch_blocks.borrow_mut() = None;
        // 换源：搜索高亮行号/内容作废（同预览换源清高亮）。
        self.clear_highlight();
        self.loaded = true;
    }

    /// 设置图片相对路径解析的基准目录（打开文件时由应用传入其父目录，与预览一致）。
    pub fn set_base_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.base_dir = dir;
        // 基准目录变化 → 重新解析图片尺寸（此前按旧基准解析的作废；真实尺寸由图片行
        // 首次进带惰性重读，见 `lazy_ensure_image_size`）。
        self.image_sizes.borrow_mut().clear();
        self.images_gen.set(self.images_gen.get().wrapping_add(1));
        *self.frame.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
    }

    /// 解析图片源：URL / Windows 盘符绝对路径 / 已绝对路径原样返回，其余基于
    /// 基准目录拼成绝对路径（与预览 `view::MarkdownPreview::resolve_image_src` 一致）。
    fn resolve_image_src(&self, src: &str) -> String {
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
        match &self.base_dir {
            Some(base) => base.join(src).to_string_lossy().to_string(),
            None => src.to_string(),
        }
    }

    /// 图片加载资源（解析后的路径/URL → gpui 加载器资源）。
    pub(crate) fn image_load_resource(&self, src: &str) -> gpui::Resource {
        let resolved = self.resolve_image_src(src);
        if resolved.starts_with("http://")
            || resolved.starts_with("https://")
            || resolved.starts_with("file://")
        {
            gpui::Resource::Uri(resolved.into())
        } else {
            gpui::Resource::Path(std::sync::Arc::<std::path::Path>::from(
                std::path::PathBuf::from(resolved),
            ))
        }
    }

    /// 打开文档后预读图片行的固有尺寸（与预览一致：本地文件按宽高比显示，
    /// 首帧即用真实行高，避免「占位 → 真实」跳动）。只读文件头，O(行数，廉价)。
    ///
    /// 从原始源码行直接取 src（`parse::parse_image_line`），不物化 [`DocRow`]——
    /// 逐块 `rows_in_block_window(i, i)` 会命中 `parse_block_window` 的「末块含尾
    /// 换行 ≠ 块数」边界（debug 断言），且为读个 src 物化整行纯属浪费。
    /// A-4 冷开惰性化：图片行**首次进带 shape** 时按需读一次真实尺寸并入缓存。
    /// 替代旧 `preseed_image_sizes`（set_source_text/set_base_dir 时同步读**全篇**图片
    /// 尺寸——6.4MB/3456 图参考文档 ~130ms 阻塞首帧；且 O(图片数) 随文档图片量线性
    /// 膨胀）。`image_sizes` 是持久缓存 → 之后进带 O(1)；HTTP/已达尺寸/读取失败均短路，
    /// paint 的异步 asset 加载照常（带内图尺寸已知后只画、不重复校正）。
    pub(crate) fn lazy_ensure_image_size(&self, src: &str) {
        if self.image_sizes.borrow().get(src).is_some() {
            return;
        }
        let resolved = self.resolve_image_src(src);
        if !resolved.starts_with("http://")
            && !resolved.starts_with("https://")
            && !resolved.starts_with("file://")
            && let Some((w, h)) = crate::markdown::view::dimensions::image_dimensions(
                std::path::Path::new(&resolved),
            ) {
            self.image_sizes
                .borrow_mut()
                .insert(src.into(), Some((w as f32, h as f32)));
        }
    }

    /// （仅测试）整篇源文本（O(n) 复制）。产线元素访问一律走 [`Self::line_text`] / doc。
    #[cfg(test)]
    pub(crate) fn content(&self) -> String {
        self.doc.text_shared().to_string()
    }
    /// 第 `line` 行的源码文本（O(行长)；`line_str` 同源）。
    pub(crate) fn line_text(&self, line: usize) -> String {
        self.doc.line_str(line)
    }
    pub(crate) fn doc(&self) -> &Doc {
        &self.doc
    }
    pub(crate) fn caret(&self) -> usize {
        self.caret
    }
    /// 全文字节长（O(1)，rope 单真源）。
    pub(crate) fn len(&self) -> usize {
        self.doc.len()
    }

    /// 直接放置光标（源字节偏移）。命中/测试/编程式定位共用。
    pub(crate) fn set_caret(&mut self, off: usize) {
        self.caret = off.min(self.len());
        self.sel_anchor = None;
        // 光标移动 → 打断撤销合并组（同 editor/undo.rs 的 cooperative=false）。
        self.last_edit = None;
        *self.frame.borrow_mut() = None;
    }

    /* ---------- 选区 ---------- */

    /// 当前选区（归一化源字节区间）；无选区或零宽返回 `None`。
    pub(crate) fn selection(&self) -> Option<Range<usize>> {
        match self.sel_anchor {
            Some(a) if a != self.caret => {
                let s = a.min(self.caret).min(self.len());
                let e = a.max(self.caret).min(self.len());
                if s < e {
                    Some(s..e)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// 选中的源文本（复制/Ctrl+C）。
    fn selected_text(&self) -> String {
        self.selection()
            .map(|r| self.doc.slice_str(r))
            .unwrap_or_default()
    }

    /// 鼠标按下：锚点 = 光标处（开始拖选）。
    pub(crate) fn begin_selection_at(&mut self, off: usize) {
        let off = off.min(self.len());
        self.caret = off;
        self.sel_anchor = Some(off);
        self.dragging = true;
        *self.frame.borrow_mut() = None;
    }

    /// 拖拽中延伸选区（活动端跟随鼠标）。
    pub(crate) fn extend_selection_to(&mut self, off: usize) {
        if !self.dragging {
            return;
        }
        let off = off.min(self.len());
        self.caret = off;
        if self.sel_anchor.is_none() {
            self.sel_anchor = Some(off);
        }
        *self.frame.borrow_mut() = None;
    }

    /// 鼠标松开结束拖选。
    pub(crate) fn end_selection(&mut self) {
        self.dragging = false;
        // 零宽（纯点击）→ 视为无选区。
        if self.sel_anchor == Some(self.caret) {
            self.sel_anchor = None;
        }
        *self.frame.borrow_mut() = None;
    }

    /// 全选。
    pub(crate) fn select_all(&mut self) {
        self.sel_anchor = Some(0);
        self.caret = self.len();
        *self.frame.borrow_mut() = None;
    }

    /* ---------- 搜索跳转高亮（预览独有能力，搬入统一组件） ---------- */

    /// 搜索跳转后高亮「源码第 `line` 行」（0 基）内**第一个命中 `query` 的文本**，
    /// 只给搜索词本身着色（不吃整行），[`crate::config::timing::HIGHLIGHT_MS`] 后
    /// 自动消失（与 `MarkdownPreview::highlight_search` 同款语义）。对小程序否则
    /// 仅登记状态，paint 逐个源区间在用搜索色调绘制（复用选区几何）。
    pub fn highlight_search(&mut self, line: usize, query: &str, cx: &mut Context<Self>) {
        self.highlight_search = Some((line, query.to_lowercase()));
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        let seq = self.highlight_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    crate::config::timing::HIGHLIGHT_MS,
                ))
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

    /// 解析搜索高亮为**绝对源字节区间**（命中词在 `line` 源码内首次出现的字节范围）。
    /// 无高亮 / 行内无命中返回 `None`。paint 与测试共用（区间语义同选区）。
    pub(crate) fn search_highlight_range(&self) -> Option<std::ops::Range<usize>> {
        let (line, query) = self.highlight_search.as_ref()?;
        let text = self.line_text(*line);
        let range = crate::markdown::view::find_text_range(&text, query)?;
        Some(self.line_start(*line) + range.start..self.line_start(*line) + range.end)
    }

    /// 复制：写入平台剪贴板（由 `on_action(Copy)` 装配）。
    fn copy(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.selected_text()));
    }

    /// 剪切：复制 + 删除选区。
    fn cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("cut");
        self.copy(window, cx);
        let range = self.selection().unwrap_or(self.caret..self.caret);
        self.replace_range(range, "", cx);
    }

    /// 粘贴：用剪贴板文本替换选区（若为空则插入光标处）。
    fn paste(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("paste");
        let text = cx
            .read_from_clipboard()
            .map(|i| i.text().unwrap_or_default())
            .unwrap_or_default();
        let range = self.selection().unwrap_or(self.caret..self.caret);
        if text.is_empty() && range.start == range.end {
            return;
        }
        self.replace_range(range, &text, cx);
    }

    /// 直输/提交（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked，
    /// 再清掉 marked——否则下一次组合会把已提交文本当旧 marked 再次替换。
    pub(crate) fn replace_text_utf16(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        // A1 只读门控：直输短路（`replace_range` 亦硬门控；这里提前返回省一次 op 账）。
        if !self.editable {
            return;
        }
        crate::prof::op_start("type");
        let r = match range {
            // 显式 UTF-16 区间（macOS/部分平台）走行粒度索引换算，O(log n + 行长)。
            Some(r) => self.doc.utf16_to_byte(r.start)..self.doc.utf16_to_byte(r.end),
            None => self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection().unwrap_or(self.caret..self.caret)),
        };
        self.replace_range(r, text, cx);
        self.marked = None;
    }

    /// IME 组合更新（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked
    /// （没有 marked 时才落普通选区/光标）——否则 Windows 每次组合更新都不带区间，
    /// 新旧组合文本会一前一后堆叠出「nnini'hni'hao你好」式垃圾串。
    pub(crate) fn replace_and_mark_utf16(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        // A1 只读门控：IME 组合短路（不置 marked、不改文本）。
        if !self.editable {
            return;
        }
        crate::prof::op_start("ime");
        let byte_range = match range {
            Some(r) => self.doc.utf16_to_byte(r.start)..self.doc.utf16_to_byte(r.end),
            None => self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection().unwrap_or(self.caret..self.caret)),
        };
        self.replace_range(byte_range.clone(), new_text, cx);
        if new_text.is_empty() {
            // 组合被取消：清掉 marked，光标回组合起点。
            self.marked = None;
            self.caret = byte_range.start.min(self.len());
        } else {
            self.marked = Some(
                byte_range.start
                    ..byte_range.start
                        + new_text.len().min(self.len() - byte_range.start),
            );
            // `new_selected_range` 是**相对 new_text 起点**的 UTF-16 偏移，不是文档
            // 绝对偏移（AppKit setMarkedText 的 selectedRange 语义）。
            let caret = match new_selected_range {
                Some(sel) => byte_range.start + input::offset_from_utf16(new_text, sel.start).min(new_text.len()),
                None => byte_range.start + new_text.len(),
            };
            self.caret = caret.min(self.len());
        }
        cx.notify();
    }

    /* ---------- 点击 / 鼠标 ---------- */

    /// 鼠标按下命中 → 源偏移（经 frame + byte_map）；左键开始拖选并抢焦点。
    /// 先试交互性命中：任务框翻转（C1）、警报折叠箭头（C3）、行内链接（C5）。
    pub(crate) fn on_mouse_down(
        &mut self,
        ev: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ev.button != gpui::MouseButton::Left {
            return;
        }
        // 输入命中前保证帧存在（缺帧当场重建，避免展开后立刻再点折叠时命中落空）。
        if !self.ensure_frame_for_input(window) {
            return;
        }
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        // 任何点击都先清除旧图片选中（图片本体/编辑按钮分支随后重新置位）：
        // 点文本/任务框/链接等 → 图片不再保持选中态。
        self.selected_image.set(None);
        // **mermaid 卡点击 → 聚焦**：聚焦后滚轮才缩放该卡（交互见图文，未聚焦时滚轮
        // 一律正常滚动）。点击**任何其它地方**（含选中/链接/任务框）→ 清除聚焦。
        if let Some(mline) = element::mermaid_card_hit(self, f, ev.position) {
            drop(marker);
            self.focus.focus(window, cx);
            self.focused_mermaid = Some(mline);
            cx.notify();
            return;
        }
        self.focused_mermaid = None;
        // 交互性命中（任务框/折叠箭头/链接）单双击都响应，展开→立即再点也能折叠回。
        if !ev.modifiers.shift && !ev.modifiers.control {
            // 图片编辑铅笔按钮：仅可编辑模式——点击 → 打开/切换/关闭编辑卡片
            // （不落光标、不进入选区）。只读预览不响应（不画按钮、这里也不拦截）。
            if self.editable
                && let Some(line) = element::image_edit_button_hit_at(self, f, ev.position)
            {
                drop(marker);
                self.open_image_edit(line, window, cx);
                return;
            }
            // 图片调整条（右中边缘）：按住 → 开始拖拽改宽（保持宽高比、不改源码）。
            // 在「点图本体选中」之前判定（条在图片盒内）。
            if self.editable
                && let Some((line, grab_x)) =
                    element::image_resize_handle_hit_at(self, f, ev.position)
            {
                let (src, start_w) = f
                    .rows
                    .iter()
                    .find(|r| {
                        r.src_line == line && r.kind == crate::markdown::ir::IrKind::Image
                    })
                    .and_then(|r| r.image.as_ref())
                    .map(|i| (i.src.clone(), i.w))
                    .unwrap_or((gpui::SharedString::default(), 0.));
                drop(marker);
                if src.is_empty() {
                    return;
                }
                self.focus.focus(window, cx);
                self.selected_image.set(Some(line));
                self.begin_image_resize(src, start_w, grab_x);
                *self.frame.borrow_mut() = None;
                cx.notify();
                return;
            }
            // 点击图片**本体**（图片盒，不含下方题注行）→ 选中该图：不落光标进题注
            //（修掉「点图 → 光标跳到图片下方描述文本」）。选中后图片画 accent 描边 +
            // 编辑按钮常显；Backspace/Delete 删除整张图片行。
            if self.editable
                && let Some(line) = element::image_box_hit_at(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                self.select_image(line, cx);
                return;
            }
            // 表格横向滚动条（内容超宽的表格底部轨道）：点在**滑块**上 → 拖动；
            // 点在**轨道**上 → 滑块直接跳到点击处（view 的 gpui-component 语义）。
            if let Some((blk, on_thumb, grab)) =
                element::table_hscrollbar_hit(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                if on_thumb {
                    *self.table_h_drag.borrow_mut() = Some((blk, grab));
                    self.table_h_drag_apply(ev.position, cx);
                } else {
                    self.table_bar_jump_to(blk, ev.position);
                    cx.notify();
                }
                return;
            }
            // 代码块横向滚动条（内容超宽的块底部轨道）：点在**滑块**上 → 拖动；
            // 点在**轨道**上 → 滑块直接跳到点击处（view 的 gpui-component 语义）。
            if let Some((blk, on_thumb, grab)) =
                element::code_hscrollbar_hit(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                if on_thumb {
                    *self.code_h_drag.borrow_mut() = Some((blk, grab));
                    self.code_h_drag_apply(ev.position, cx);
                } else {
                    // 跳转：滑块中心对准点击点（·`rel = (px − bar_x − thumb_w/2)/travel`）。
                    self.code_bar_jump_to(blk, ev.position);
                    cx.notify();
                }
                return;
            }
            // C1: 点击任务框 → 翻转 `[ ]` ↔ `[x]`（写源码，走撤销栈）。
            if let Some((line, _)) = element::checkbox_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                self.toggle_checkbox_at_line(line, cx);
                return;
            }
            // C3: 点击警报折叠箭头 → 翻转 `-`/`+`（折叠/展开 callout 主体）。
            if let Some((line, _)) = element::alert_chevron_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                self.toggle_alert_fold_at_line(line, cx);
                return;
            }
            // 标题折叠箭头 → 翻转视图本地折叠态（`## Goals` body 行折叠/展开）。
            if let Some((line, _)) = element::heading_chevron_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                self.toggle_heading_fold_at_line(line, cx);
                return;
            }
            // C5: 点击行内链接 → 打开（安全 URL 直开系统浏览器；wiki/tag → OpenPage）。
            if let Some(hit) = element::link_hit_at(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                match hit {
                    LinkHit::Url(u) if recognition::is_safe_external_url(&u) => {
                        cx.open_url(&u);
                    }
                    LinkHit::Url(u) | LinkHit::Page(u) => {
                        cx.emit(WyEvent::OpenPage(u));
                    }
                    LinkHit::BlockRef(id) => {
                        cx.emit(WyEvent::OpenPage(format!("#^{id}")));
                    }
                }
                return;
            }
            // A1/B2: 数学交互——
            // 数学块卡片：**单击不改光标**（否则 caret 进块被 reveal、整块回显源码字符串，
            // 即用户看到的「单击变回字符串」）；**双击**进块编辑（reveal 源码可改）。
            // 行内公式：**双击** spacer → 光标放进 `$…$` 内（shape 期 reveal 可编辑）。
            let dbl = ev.click_count >= 2;
            if let Some(block_start) = element::math_card_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                if dbl {
                    // 光标放进 `$$` 行**内容内 1 字节**（不是行首 0 列）：
                    // ①块 reveal ②单行 `$$…$$` 的行内 splice reveal 同时满足
                    // → 两种情况（多行块 / 单行块）双击都直接还原成源码字符串
                    // （放行首 0 列时 `splice_math_text` 的 `c > span.start` 不成立，
                    // 单行块会被 splice 成小号行内公式，需再双击一次才还原）。
                    self.set_caret(block_start + 1);
                    cx.notify();
                }
                return;
            }
            if dbl
                && let Some(src) = element::inline_math_hit_at(self, f, ev.position) {
                    drop(marker);
                    self.focus.focus(window, cx);
                    self.set_caret(src.start + 1);
                    cx.notify();
                    return;
                }
        }
        let off = element::source_offset_at(self, f, ev.position);
        drop(marker);
        self.focus.focus(window, cx);
        self.begin_selection_at(off);
        cx.notify();
    }

    /// 拖拽中（左键已按下）。
    pub(crate) fn on_mouse_move(&mut self, ev: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        // 图片调整条拖拽优先：按住条 → 随鼠标横向改宽（不参与选区拖选/悬浮）。
        if self.image_resize_drag.borrow().is_some() {
            self.apply_image_resize(ev.position.x, cx);
            return;
        }
        // 代码/表格横向滚动条拖动优先（不参与选区拖选）。
        if self.code_h_drag.borrow().is_some() {
            self.code_h_drag_apply(ev.position, cx);
            return;
        }
        if self.table_h_drag.borrow().is_some() {
            self.table_h_drag_apply(ev.position, cx);
            return;
        }
        // 未拖选：更新图片悬浮态（命中图片盒 → 该图右上角浮现编辑按钮）。
        // 拖选中不更新（指针离开图片也不应隐藏按钮，避免拖到别处时误闪烁）。
        if !self.dragging {
            self.update_hover(ev.position, cx);
            // 图片调整条 hover → 即时换 resize 光标（同 `on_window_mouse_move`）。
            if self.notify_when_hover_grip(ev.position) {
                cx.notify();
            }
            return;
        }
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let off = element::source_offset_at(self, f, ev.position);
        drop(marker);
        self.extend_selection_to(off);
        *self.frame.borrow_mut() = None;
        cx.notify();
    }

    /// 更新图片悬浮态（命中图片盒 → `hover_image`）。未命中（含指针移出元素）置 `None`。
    fn update_hover(&mut self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let hovered = element::image_box_hit_at(self, f, pos);
        drop(marker);
        if self.hover_image.get() != hovered {
            self.hover_image.set(hovered);
            cx.notify();
        }
    }

    /// 窗口级 mouse-move 的悬浮更新（图片编辑按钮亮显）：元素在宿主 `#live-vscrollbar-overlay`
    /// 等顶层叠层之下时，元素级 `on_mouse_move` 会被遮罩吞掉（与滚轮同因），走
    /// `window.on_mouse_event`（Bubble）由元素 paint 期注册驱动。指针在本元素
    /// `last_bounds` 内才更新 `hover_image`，移出即清除。拖选中（鼠标按下）由元素级
    /// 拖选事件驱动，这里跳过。
    pub(crate) fn on_window_mouse_move(
        &mut self,
        ev: &gpui::MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        // 图片调整条拖拽优先（顶层叠层会吞元素级 move，拖动期间走这里兜底）。
        if self.image_resize_drag.borrow().is_some() {
            self.apply_image_resize(ev.position.x, cx);
            return;
        }
        if self.dragging {
            return;
        }
        let Some(b) = self.last_bounds() else {
            return;
        };
        let inside_image = b.contains(&ev.position);
        if inside_image {
            self.update_hover(ev.position, cx);
        } else if self.hover_image.get().is_some() {
            self.hover_image.set(None);
            cx.notify();
        }
        // 指针进入图片调整条（右中边缘）→ 立即重绘：paint 登记窗口级 resize 光标，
        // `finish_frame` 按新帧当场解析——否则要等下一次输入/点击光标才变
        //（「hover 不变 → 先点才显示」）。
        if self.notify_when_hover_grip(ev.position) {
            cx.notify();
        }
    }

    /// 指针落在图片调整条（当前帧，可编辑）→ `true`（调用方 `notify` 触发一次重绘，
    /// 让 resize 光标即时生效）。
    fn notify_when_hover_grip(&self, pos: gpui::Point<Pixels>) -> bool {
        if !self.editable {
            return false;
        }
        let marker = self.frame.borrow();
        marker
            .as_ref()
            .and_then(|f| element::image_resize_handle_hit_at(self, f, pos))
            .is_some()
    }

    /// 代码块滚动条**轨道点击跳转**：滑块中心对准点击点（view 语义：不在滑块上
    /// 按轨道即跳转，而不是像旧实现那样点空白处无反应）。
    fn code_bar_jump_to(&self, blk: usize, pos: gpui::Point<Pixels>) {
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f
            .rows
            .iter()
            .find(|r| r.src_line == blk && !r.multi_offsets.is_empty())
        else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::code_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        // 滑块中心对准点击点：center = p_local → thumb_x = p_local − thumb_w/2。
        let rel = ((p_local - bar_x - thumb_w / 2.0) / travel).clamp(0., 1.);
        drop(marker);
        self.set_code_h_scroll(blk, rel * max_scroll);
    }

    /// 把指针位置应用到正在拖动的代码块横向滚动条（保持**抓取点相对滑块左缘**：
    /// `thumb_x = px - grab`，按下瞬间 scroll 不变、拖动时滑块钉住抓取点 1:1 跟随）。
    fn code_h_drag_apply(&self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((blk, grab)) = *self.code_h_drag.borrow() else {
            return;
        };
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f.rows.iter().find(|r| r.src_line == blk && !r.multi_offsets.is_empty())
        else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::code_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - grab) / travel).clamp(0., 1.);
        drop(marker);
        self.set_code_h_scroll(blk, rel * max_scroll);
        cx.notify();
    }

    /// 表格滚动条**轨道点击跳转**：滑块中心对准点击点（与 [`Self::code_bar_jump_to`] 同语义）。
    fn table_bar_jump_to(&self, blk: usize, pos: gpui::Point<Pixels>) {
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f.rows.iter().find(|r| r.src_line == blk && r.table.is_some()) else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::table_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - thumb_w / 2.0) / travel).clamp(0., 1.);
        drop(marker);
        self.set_table_h_scroll(blk, rel * max_scroll);
    }

    /// 把指针位置应用到正在拖动的表格横向滚动条（抓取点相对滑块左缘，1:1 跟随）。
    fn table_h_drag_apply(&self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((blk, grab)) = *self.table_h_drag.borrow() else {
            return;
        };
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f.rows.iter().find(|r| r.src_line == blk && r.table.is_some()) else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::table_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - grab) / travel).clamp(0., 1.);
        drop(marker);
        self.set_table_h_scroll(blk, rel * max_scroll);
        cx.notify();
    }

    /// 鼠标松开：结束拖选 / 图片调整条 / 代码块 & 表格横向滚动条拖动。
    pub(crate) fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        self.end_selection();
        self.end_image_resize();
        *self.code_h_drag.borrow_mut() = None;
        *self.table_h_drag.borrow_mut() = None;
        cx.notify();
    }

    /// 滚轮：**仅在光标悬停在 mermaid 卡片上时**拦截（普通滚轮 = 整卡缩放），其余位置
    /// 返回 `false` 放行给宿主滚动/`zoom_overlay`。由元素 paint 期 `window.on_mouse_event`
    /// （bubble）驱动——元素在宿主 `zoom_overlay`（顶层兄弟）之下，元素级 `.on_scroll_wheel`
    /// 会被兄弟遮罩吞掉，必须走窗口级事件监听。
    ///
    /// 缩放走 `zoom_mermaid_at`：先捕捉视口锚点、重排后再写回宿主 `ScrollHandle`——页面
    /// **不同步滚动**（缩放改的是卡高，内容随锚点原地钉住）。
    pub(crate) fn on_scroll_wheel(
        &mut self,
        ev: &gpui::ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if ev.modifiers.control {
            return false; // Ctrl 放行给宿主 zoom_overlay（全局字号缩放）。
        }
        let Some(b) = self.last_bounds() else {
            return false;
        };
        if !b.contains(&ev.position) {
            return false;
        }
        let local_y = f32::from(ev.position.y) - f32::from(b.origin.y);
        let hit_mermaid = self
            .frame
            .borrow()
            .as_ref()
            .and_then(|f| {
                f.rows.iter().find(|r| {
                    local_y >= f32::from(r.top) && local_y < f32::from(r.top) + f32::from(r.height)
                })
            })
            .filter(|r| matches!(&r.extra.card, Some(element::BlockCard::Mermaid)))
            .map(|r| r.src_line);
        let Some(block) = hit_mermaid else {
            return false;
        };
        // **仅聚焦的 mermaid 消费滚轮**（点击卡片后才可缩放）：未聚焦 → 放行宿主正常
        // 滚动（否则鼠标往下滚遇到 mermaid 会变成缩图）。
        if self.focused_mermaid != Some(block) {
            return false;
        }
        let delta_y = match ev.delta {
            gpui::ScrollDelta::Lines(l) => l.y,
            gpui::ScrollDelta::Pixels(p) => f32::from(p.y),
        };
        let step = delta_y.clamp(-1.0, 1.0);
        if step == 0.0 {
            return false;
        }
        self.zoom_mermaid_at(block, step, window, cx);
        true
    }

    /// mermaid 卡滚轮缩放 + **视口锚定**：改缩放（`mermaid_zoom_gen` 自增 → 帧/块高按新
    /// 比例失效）→ 渲染期重建窗口化帧 → 把「缩放前视口顶部的内容点」在新布局里的位置
    /// 写回宿主 `ScrollHandle`（同 `prepare_font_zoom` 的锚定恢复，一次性：mermaid 块
    /// 估计高与整形高同式 `×zoom`，无收敛过程）。未注入宿主句柄（独立测试）时只重建帧。
    pub(crate) fn zoom_mermaid_at(
        &self,
        block: usize,
        step: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 1) 重排前捕捉缩放锚点（当前视口顶部 → 块 + 块内比例）。
        self.capture_zoom_anchor();
        // 2) 应用新缩放（zgen++ → 帧缓存与基础布局代都失效）。
        let z = (self.mermaid_zoom(block) + step * 0.12).clamp(0.5, 3.0);
        self.set_mermaid_zoom(block, z);
        // 3) 渲染期重建窗口化帧 → 新布局的精确块 top/height。
        let width = self.frame.borrow().as_ref().map(|f| f.width).unwrap_or(px(800.));
        let band = self.shape_band().unwrap_or((0., 5000.));
        let frame = element::build_frame(self, window, width, band);
        // 4) 锚点块在新布局里的精确位置 = 恢复目标（元素局部 y）。
        let target = {
            let anchor = *self.zoom_anchor.borrow();
            match anchor {
                Some((blk, frac)) => {
                    let count = frame.block_count();
                    let blk = blk.min(count.saturating_sub(1));
                    let h = frame.block_height(blk).max(1.);
                    if count > 0 {
                        frame.block_top(blk) + frac * h
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            }
        };
        // 5) 写回宿主滚动句柄并按锚定后内容顶推导新带（与 element prepaint 同式）。
        let host = self.host_scroll.borrow().clone();
        let final_band = if let Some((vh, pad_y)) = host {
            vh.set_offset(gpui::point(gpui::px(0.), gpui::px(-(target + pad_y))));
            zoom_band_for_top(target + pad_y, window.viewport_size().height)
        } else {
            band
        };
        let frame = if final_band != band {
            self.set_shape_band(Some(final_band));
            element::build_frame(self, window, width, final_band)
        } else {
            frame
        };
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        cx.notify();
    }


    /// 代码块横向滚动偏移（键 = 代码块首源码行）。视图本地状态（不写源码）。
    pub(crate) fn code_h_scroll_of(&self, src_line: usize) -> f32 {
        self.code_h_scroll.borrow().get(&src_line).copied().unwrap_or(0.)
    }
    /// 设置代码块横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    pub(crate) fn set_code_h_scroll(&self, src_line: usize, v: f32) {
        let v = v.max(0.);
        if v <= 0.01 {
            self.code_h_scroll.borrow_mut().remove(&src_line);
        } else {
            self.code_h_scroll.borrow_mut().insert(src_line, v);
        }
    }

    /// 表格横向滚动偏移（px；键 = 表格块首源码行）。
    pub(crate) fn table_h_scroll_of(&self, src_line: usize) -> f32 {
        self.table_h_scroll.borrow().get(&src_line).copied().unwrap_or(0.)
    }
    /// 设置表格横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    pub(crate) fn set_table_h_scroll(&self, src_line: usize, v: f32) {
        let v = v.max(0.);
        if v <= 0.01 {
            self.table_h_scroll.borrow_mut().remove(&src_line);
        } else {
            self.table_h_scroll.borrow_mut().insert(src_line, v);
        }
    }

    /// 记录本帧元素在窗口中的 bounds（paint 调用；鼠标命中换算用）。
    pub(crate) fn set_last_bounds(&self, bounds: Option<gpui::Bounds<Pixels>>) {
        self.last_bounds.set(bounds);
    }
    pub(crate) fn last_bounds(&self) -> Option<gpui::Bounds<Pixels>> {
        self.last_bounds.get()
    }

    /// 居中阅读栏的**水平移位**（px）：`(元素可用宽 − 帧布局宽) ÷ 2`。
    ///
    /// 帧宽被 measure 钳到 ≤ 72ch（`READING_COL_ENABLED`），元素仍占满面板——内容列整列
    /// （含 gutter/负缩进/引述条）需在面板内水平居中。所有把窗口坐标换算回**内容区内部**
    /// 坐标系（`to_local`/`source_offset_at`/横向滚动条拖拽）都必须叠减该偏移；paint/hitbox
    /// 把元素屏幕原点叠加该偏移。内容区内部的几何（`padding`/`x_off`/光标/选区）一律不变，
    /// 拖拽/点击/光标对齐因此天然保持。未启用或窗口窄于列宽时 = 0（不破坏原语义）。
    pub(crate) fn reading_col_shift(&self, elem_w: f32) -> f32 {
        let fw = self
            .frame
            .borrow()
            .as_ref()
            .map(|f| f32::from(f.width))
            .unwrap_or(elem_w);
        ((elem_w - fw) / 2.).max(0.)
    }

    /// 内容列在**窗口坐标**中的屏幕原点（元素屏幕原点 + 阅读栏水平移位）。
    /// open（非 paint 期）需要与帧同代摆卡片/命中时用（paint 期用传入的 `bounds.origin`
    /// 现算，这里供 `image_edit_anchor` 等静态换算路径复用同一语义）。
    fn content_origin(&self) -> gpui::Point<Pixels> {
        let (origin, elem_w) = match self.last_bounds() {
            Some(b) => (b.origin, f32::from(b.size.width)),
            None => (gpui::point(px(0.), px(0.)), 0.),
        };
        let shift = self.reading_col_shift(elem_w);
        gpui::point(origin.x + px(shift), origin.y)
    }

    pub(crate) fn content_gen(&self) -> u64 {
        self.content_gen.get()
    }
    pub(crate) fn set_shape_band(&self, band: Option<(f32, f32)>) {
        self.shape_band.set(band);
    }
    pub(crate) fn shape_band(&self) -> Option<(f32, f32)> {
        self.shape_band.get()
    }
    /// 内容结构版本（基础布局代组成；结构性编辑才递增）。
    pub(crate) fn struct_ver(&self) -> u64 {
        self.struct_ver.get()
    }
    /// 结构编辑影响的首个源行（Pass1 增量前缀复用边界）。
    pub(crate) fn struct_dirty_line(&self) -> usize {
        self.struct_dirty_line.get()
    }
    /// 编辑影响的最后一个旧源行（Pass1 定位旧尾部起点用）。
    pub(crate) fn old_edit_end_line(&self) -> usize {
        self.struct_old_end_line.get()
    }
    /// 编辑的源行净增（插入换行 − 被删换行）。
    pub(crate) fn edit_line_delta(&self) -> i64 {
        self.struct_line_delta.get()
    }
    /// 派生版本（`derived()` 缓存键；供回归断言「纯文本键入不重建派生」）。
    pub(crate) fn regions_ver(&self) -> u64 {
        self.regions_ver.get()
    }
    /// 标题折叠视图态版本（`fold_plan` 缓存键组成部分）。
    pub(crate) fn fold_ver(&self) -> u64 {
        self.fold_ver.get()
    }
    /// 当前缓存的折叠计划 Rc（滚动预热读取同一份带无关缓存；测试断言「仅移动光标/
    /// 纯文本键入不重建折叠计划」也用它）。
    pub(crate) fn fold_plan_rc(&self) -> Option<Rc<element::FoldPlan>> {
        self.fold_plan.borrow().as_ref().map(|(_, _, p)| p.clone())
    }

    /// 取走待修补（重估高度）的块下标（编辑后一帧用；无则 None）。
    pub(crate) fn take_pending_patch_blocks(&self) -> Option<Vec<usize>> {
        self.pending_patch_blocks.borrow_mut().take()
    }

    /// （仅测试）自上次读取以来 Pass2 实际整形次数（缓存未命中 / 光标行），
    /// 供回归断言「滚动/同带重建复用缓存、编辑只重排受影响块」。
    #[cfg(test)]
    pub(crate) fn take_reshapes(&self) -> u64 {
        let v = self.reshapes.get();
        self.reshapes.set(0);
        v
    }
    /// （仅测试）行整形缓存当前条目数。
    #[cfg(test)]
    pub(crate) fn shape_rows_len(&self) -> usize {
        self.shape_rows.borrow().len()
    }
    /// （仅测试）非结构性编辑后：被编辑块（内容变了 → 指纹变）应从缓存剔除。
    #[cfg(test)]
    pub(crate) fn shape_rows_has(&self, block: usize) -> bool {
        let cid = frame::block_content_id(&self.doc, block);
        self.shape_rows.borrow().entries.contains_key(&cid)
    }
    /// （仅测试）Pass2 光栅计数（缓存未命中/光标行时自增）。
    #[cfg(test)]
    pub(crate) fn bump_reshapes(&self) {
        self.reshapes.set(self.reshapes.get() + 1);
    }
    /// （仅测试）Pass2 notdef 冷却帧计数（复用旧几何、未调用 gpui shape）。
    #[cfg(test)]
    pub(crate) fn bump_coolings(&self) {
        self.coolings.set(self.coolings.get() + 1);
    }
    /// （仅测试）D1 预热整形计数（`prewarm_band` 每实际 shape 一块自增）。
    #[cfg(test)]
    pub(crate) fn bump_prewarm_shapes(&self) {
        self.prewarm_shapes.set(self.prewarm_shapes.get() + 1);
    }
    /// （仅测试）自上次读取以来 D1 预热实际整形次数。
    #[cfg(test)]
    pub(crate) fn take_prewarm_shapes(&self) -> u64 {
        let v = self.prewarm_shapes.get();
        self.prewarm_shapes.set(0);
        v
    }
    /// 校验基础布局命中当前（代, 块数）：命中返回 `(false, 新布局)` 供复用；
    /// 未命中建出空布局（build_frame 随即填估算/或 patch）并缓存，返回 `(true, 新)`
    /// 及第三个元素 = **旧布局**（结构编辑增量 Pass1 里「编辑线之前未变前缀」的高度
    /// 复用；首次打开/宽度变化旧布局为空 → `None`）。
    pub(crate) fn ensure_block_layout(
        &self,
        n_blocks: usize,
        epoch: u64,
    ) -> (
        bool,
        BlockLayoutRef,
        Option<BlockLayoutRef>,
    ) {
        if let Some((e, l)) = &*self.block_layout.borrow()
            && *e == epoch && l.borrow().n() == n_blocks {
                return (false, l.clone(), None);
            }
        let old = self.block_layout.borrow().as_ref().map(|(_, l)| l.clone());
        // 重建分支给 **空** 草稿（O(1)）：Pass1 紧随其后整表重建真正填充——不先建一份
        // 全量零点 Fenwick/line_block 又立刻被覆盖（双重 O(n)，Enter/结构编辑每键 ~0.8ms）。
        let l = Rc::new(std::cell::RefCell::new(element::BlockLayout::empty()));
        *self.block_layout.borrow_mut() = Some((epoch, l.clone()));
        (true, l, old)
    }

    /// 折叠/重排计划（缓存）：`math_start` / `math_body` / `fold_lines` / `deltas`——
    /// 全部**与可视带无关且不含 caret**（键 = `epoch ^ regions_ver ^ mgen ^ fold_ver`），
    /// 滚动/带移动/**光标移动/纯文本键入**都复用同一 `Rc`。caret 的 reveal-on-caret 由
    /// [`Self::fold_reveal`] 按带现算叠加（最多一个数学块 + 一个折叠体）。
    /// 旧实现把 caret 放进键——每次光标动/每键都整算（结构密集大文档 ~40-100ms → 输入/
    /// 方向键「巨大延迟」）；更早是每档在 `build_frame` 里重算（滚动卡）。
    pub(crate) fn fold_plan(
        &self,
        base: Rc<std::cell::RefCell<element::BlockLayout>>,
        epoch: u64,
        mgen: u64,
    ) -> Rc<element::FoldPlan> {
        let regions_ver = self.regions_ver();
        let fold_ver = self.fold_ver();
        let key = epoch
            ^ regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
        // marker 身份（不含结构代）：结构编辑后 marker 集合未变 → deltas 可平移复用。
        let marker_key = regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
        if let Some((k, _m, p)) = &*self.fold_plan.borrow()
            && *k == key {
                return p.clone();
            }
        let derived = self.derived();
        // ———（一）marker 集合：从已增量平移的 `derived` 重建（O(#区域)；math/alert 区域
        // 在 `update_derived_after_edit` 已行号平移，marker 无需再平移）。标题折叠态仅在
        // 有折叠时才全文扫描。
        let (math_start, math_body, fold_lines, fold_bodies) = {
            let d = derived.borrow();
            let math_regions = &d.math_regions;
            // 预分配容量（大文档 3000 数学块：未预分配每次重建多次 rehash ≈20ms）。
            let _t_sets = crate::prof::begin();
            let mut math_start: rustc_hash::FxHashMap<usize, usize> =
                rustc_hash::FxHashMap::with_capacity_and_hasher(
                    math_regions.len(),
                    std::default::Default::default(),
                );
            let mut math_body: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::with_capacity_and_hasher(
                math_regions.len() * 2,
                std::default::Default::default(),
            );
            for (idx, reg) in math_regions.iter().enumerate() {
                math_start.insert(reg.range.start, idx);
                math_body.extend(reg.range.start + 1..reg.range.end);
            }
            let mut fold_lines: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
            let mut fold_bodies: Vec<Range<usize>> = Vec::new();
            for (rnge, folded) in &d.alert_fold_regions {
                if !*folded {
                    continue;
                }
                fold_bodies.push(rnge.clone());
                fold_lines.extend(rnge.start + 1..rnge.end);
            }
            crate::prof::end("wy.fp.sets", _t_sets);
            let _t_head = crate::prof::begin();
            {
                let folded = self.folded_headings.borrow();
                if !folded.is_empty() {
                    let content = self.doc.text_shared();
                    for rnge in inline_style::heading_fold_regions(&content, &folded) {
                        fold_bodies.push(rnge.clone());
                        fold_lines.extend(rnge.start + 1..rnge.end);
                    }
                }
            }
            crate::prof::end("wy.fp.head", _t_head);
            (math_start, math_body, fold_lines, fold_bodies)
        };
        // ———（二）deltas：marker 身份未变 + 结构编辑待消费 → 增量平移（免 O(n) 双数组
        // 标记 + 0..n 扫描）；否则整建（见 [`Self::fold_plan_deltas_full`]）。
        if let Some(plan) = self.fold_plan_deltas_shifted(
            &derived,
            base.clone(),
            key,
            marker_key,
            &math_start,
            &math_body,
            &fold_lines,
            &fold_bodies,
        ) {
            return plan;
        }
        self.fold_plan_deltas_full(
            &derived,
            base,
            key,
            marker_key,
            math_start,
            math_body,
            fold_lines,
            fold_bodies,
        )
    }

    /// deltas **增量平移**（B5）：marker 集合已重建（O(#区域)，`fp.sets`），deltas 免
    /// O(n) 双数组标记 + 0..n 扫描（`fp.dyn/fp.tail` ~3ms debug），改为平移旧计划：
    /// 块下标 < 旧编辑块的不变、> 旧编辑块的整体 + 块数净增；被编辑块旧条目丢弃，按新
    /// markers + 新 base 就地重算编辑邻域（折叠/数学 body → -height、数学 start →
    /// 卡高-height；body 优先）。
    /// 仅在：marker 身份与缓存一致、确有结构编辑待消费、无标题折叠态时走此路径；任何
    /// 偏离回退 `None` 走整建（正确 > 快）。
    #[allow(clippy::too_many_arguments)] // 折叠计划产出的正交输入
    fn fold_plan_deltas_shifted(
        &self,
        derived: &Rc<std::cell::RefCell<WyDerived>>,
        base: Rc<std::cell::RefCell<element::BlockLayout>>,
        key: u64,
        marker_key: u64,
        math_start: &rustc_hash::FxHashMap<usize, usize>,
        math_body: &rustc_hash::FxHashSet<usize>,
        fold_lines: &rustc_hash::FxHashSet<usize>,
        fold_bodies: &[Range<usize>],
    ) -> Option<Rc<element::FoldPlan>> {
        if !self.struct_edit_pending.get() || !self.folded_headings.borrow().is_empty() {
            return None;
        }
        let old_plan = {
            let old = self.fold_plan.borrow();
            let (_, cm, _p) = old.as_ref()?;
            if *cm != marker_key {
                return None;
            }
            _p.clone()
        };
        let ed_blk_old = self.struct_blk_old.get();
        let blk_delta = self.struct_blk_span_delta.get();
        // 平移旧 deltas（块下标）；被编辑块旧条目丢弃，由编辑邻域重算补充。
        let mut deltas: Vec<(usize, f32)> = Vec::with_capacity(old_plan.deltas.len() + 2);
        for &(ix, v) in old_plan.deltas.iter() {
            if ix < ed_blk_old {
                deltas.push((ix, v));
            } else if ix > ed_blk_old {
                deltas.push(((ix as i64 + blk_delta).max(0) as usize, v));
            }
        }
        // 编辑邻域（受影响的新块区间）就地重算。
        let base_br = base.borrow();
        let n = base_br.n();
        let ed_blk_new = self
            .doc
            .block_index_for_line(self.struct_dirty_line())
            .min(n.saturating_sub(1));
        let member_delta = |ix: usize| -> Option<f32> {
            let m = self.doc.block_meta(ix)?;
            let s0 = m.src_line as usize;
            let s1 = (s0 + m.aux.max(1) as usize).min(self.doc.line_starts().len());
            let mut line = s0;
            while line < s1 {
                if fold_lines.contains(&line) || math_body.contains(&line) {
                    return Some(-base_br.height(ix));
                }
                line += 1;
            }
            let mut line = s0;
            while line < s1 {
                if let Some(&ridx) = math_start.get(&line) {
                    let card_h = {
                        let d = derived.borrow();
                        f32::from(element::math_card_height(self, &d.math_regions[ridx]))
                    };
                    return Some(card_h - base_br.height(ix));
                }
                line += 1;
            }
            None
        };
        let lo = ed_blk_new.saturating_sub(1);
        let hi = (ed_blk_new + blk_delta.max(0) as usize + 2).min(n);
        for ix in lo..hi {
            if let Some(v) = member_delta(ix) {
                if let Some(pos) = deltas.iter().position(|&(b, _)| b == ix) {
                    deltas.remove(pos);
                }
                deltas.push((ix, v));
            }
        }
        deltas.sort_unstable_by_key(|&(b, _)| b);
        deltas.dedup_by_key(|&mut (b, _)| b);
        let delta_sum: f32 = deltas.iter().map(|&(_, v)| v).sum();
        let plan = Rc::new(element::FoldPlan {
            math_start: math_start.clone(),
            math_body: math_body.clone(),
            fold_lines: fold_lines.clone(),
            fold_bodies: fold_bodies.to_vec(),
            deltas: deltas.into_boxed_slice().into(),
            delta_sum,
        });
        self.struct_edit_pending.set(false); // 消费本次平移
        *self.fold_plan.borrow_mut() = Some((key, marker_key, plan.clone()));
        Some(plan)
    }

    /// deltas **整建**：marker 集合（已重建）→「命中标记 + 块值」双数组（块下标 ∈
    /// [0,n)，填表 O(1)、尾随按 0..n 升序扫出——免 30k 项全排序；见 `fp.dyn/fp.tail`）。
    #[allow(clippy::too_many_arguments)] // 与 fold_plan_deltas_shifted 同源的正交输入
    fn fold_plan_deltas_full(
        &self,
        derived: &Rc<std::cell::RefCell<WyDerived>>,
        base: Rc<std::cell::RefCell<element::BlockLayout>>,
        key: u64,
        marker_key: u64,
        math_start: rustc_hash::FxHashMap<usize, usize>,
        math_body: rustc_hash::FxHashSet<usize>,
        fold_lines: rustc_hash::FxHashSet<usize>,
        fold_bodies: Vec<Range<usize>>,
    ) -> Rc<element::FoldPlan> {
        let math_regions = &derived.borrow().math_regions;
        let _t_dyn = crate::prof::begin();
        let base_br = base.borrow();
        let nb = base_br.n();
        let mut marked: Vec<bool> = vec![false; nb];
        let mut vals: Vec<f32> = vec![0.0; nb];
        for &srcline in fold_lines.iter() {
            let ix = base_br.block_for_line(srcline);
            marked[ix] = true;
            vals[ix] = -base_br.height(ix);
        }
        for &srcline in math_body.iter() {
            let ix = base_br.block_for_line(srcline);
            marked[ix] = true;
            vals[ix] = -base_br.height(ix);
        }
        for (&srcline, &ridx) in math_start.iter() {
            let ix = base_br.block_for_line(srcline);
            if !marked[ix] {
                let card_h = f32::from(element::math_card_height(self, &math_regions[ridx]));
                marked[ix] = true;
                vals[ix] = card_h - base_br.height(ix);
            }
        }
        crate::prof::end("wy.fp.dyn", _t_dyn);
        let _t_tail = crate::prof::begin();
        let mut deltas_vec: Vec<(usize, f32)> = Vec::new();
        for ix in 0..nb {
            if marked[ix] {
                deltas_vec.push((ix, vals[ix]));
            }
        }
        let delta_sum: f32 = deltas_vec.iter().map(|&(_, v)| v).sum();
        crate::prof::end("wy.fp.tail", _t_tail);
        let plan = Rc::new(element::FoldPlan {
            math_start,
            math_body,
            fold_lines,
            fold_bodies,
            deltas: deltas_vec.into_boxed_slice().into(),
            delta_sum,
        });
        self.struct_edit_pending.set(false);
        *self.fold_plan.borrow_mut() = Some((key, marker_key, plan.clone()));
        plan
    }

    /// caret 所在区域「展开显示源码」（reveal-on-caret）覆盖，按带现算（O(区域数) 线性扫
    ///   加少量块定位）——最多一个数学块 + 一个折叠体。`build_frame` 用它把全折叠基准里
    /// 该区域的行恢复成可编辑源码显示。
    pub(crate) fn fold_reveal(
        &self,
        caret_line: usize,
        plan: &element::FoldPlan,
        base: &std::cell::RefCell<element::BlockLayout>,
    ) -> element::RevealPlan {
        let mut reveal = element::RevealPlan::empty();
        let base_br = base.borrow();
        let block_ix_of = |srcline: usize| -> usize {
            base_br.block_index_for_src_line(srcline).unwrap_or(0)
        };
        let zero_ix = |zeroed: &mut Vec<usize>, ix: usize| {
            if !zeroed.contains(&ix) {
                zeroed.push(ix);
            }
        };
        // 数学块：caret 在 [start, end) → 整块还原源码（不卡片、body 不折叠 0 高）。
        {
            let d = self.derived();
            let d = d.borrow();
            if let Some(reg) = d
                .math_regions
                .iter()
                .find(|r| caret_line >= r.range.start && caret_line < r.range.end)
            {
                let rng = reg.range.clone();
                reveal.ranges.push(rng.clone());
                zero_ix(&mut reveal.zeroed, block_ix_of(rng.start));
                for l in rng.start + 1..rng.end {
                    zero_ix(&mut reveal.zeroed, block_ix_of(l));
                }
            }
        }
        // 折叠体：caret 在 body `[start+1, end)` → 该体行还原源码（不折叠成 0）。
        if let Some(rnge) = plan
            .fold_bodies
            .iter()
            .find(|r| caret_line > r.start && caret_line < r.end)
        {
            let rng = rnge.clone();
            reveal.ranges.push(rng.clone());
            for l in rng.start + 1..rng.end {
                zero_ix(&mut reveal.zeroed, block_ix_of(l));
            }
        }
        // 被归零的静态 Δ 之和（total 减去）。
        if !reveal.zeroed.is_empty() {
            for &ix in &reveal.zeroed {
                if let Some((_, v)) = plan.deltas.iter().find(|(i, _)| *i == ix) {
                    reveal.reveal_delta_sum += v;
                }
            }
        }
        reveal
    }

    /// 内容派生扫描缓存（全篇一遍，按 `regions_ver` 缓存）——`build_frame` / paint 用，
    /// 避免滚动/每帧重复全篇扫描（大文档卡顿主因之一）。**标记计数器预筛**（增量维护，
    /// 见 `replace_range`）：无 `$` → `math_regions` 空；无 `[!` → 两个 alert 扫描空。
    /// 7MB 大文档 `math_regions` 全篇行切分+扫描 ~390ms——**纯文本键入**（region 签名
    /// 不变）不递增 `regions_ver` → 跨键复用 `Rc`，编辑每键省掉这些全篇扫描
    /// （结构密集文档「输入卡死」的根因，旧实现每键重扫 ~1s）。
    pub(crate) fn derived(&self) -> Rc<RefCell<WyDerived>> {
        let ver = self.regions_ver.get();
        if let Some((g, d)) = &*self.derived.borrow()
            && *g == ver {
                return d.clone();
            }
        let content = self.doc.text_shared();
        // 警报区域单遍扫描，两个公开查询由它派生（旧实现 `alert_fold_regions` +
        // `alert_region_kinds` 各扫一遍全篇——重复 O(n)）。
        let alerts = if self.alert_marker_cnt.get() > 0 {
            inline_style::alert_regions(&content)
        } else {
            Vec::new()
        };
        let alert_fold_regions = alerts
            .iter()
            .filter_map(|(r, _, fold)| fold.map(|f| (r.clone(), f)))
            .collect();
        let mut alert_region_kinds: rustc_hash::FxHashMap<usize, crate::markdown::parse::recognition::AlertKind> =
            rustc_hash::FxHashMap::with_capacity_and_hasher(alerts.len() * 2, Default::default());
        for (r, k, _) in &alerts {
            for l in r.clone() {
                alert_region_kinds.insert(l, *k);
            }
        }
        let d = Rc::new(RefCell::new(WyDerived {
            math_regions: if self.math_dollar_cnt.get() > 0 {
                inline_style::math_regions(&content)
            } else {
                Vec::new()
            },
            alert_fold_regions,
            alert_region_kinds,
        }));
        *self.derived.borrow_mut() = Some((ver, d.clone()));
        d
    }

    /// 图片加载缓存实体（元素在 paint 里 `load` 用）。
    pub(crate) fn image_cache(&self) -> gpui::Entity<gpui::RetainAllImageCache> {
        self.image_cache.clone()
    }

    /// `src` 的当前显示尺寸：`None` = 未尝试加载（发起加载/占位）；
    /// `Some(None)` = 加载失败；`Some(Some((w, h)))` = 已加载的真实宽高比尺寸。
    pub(crate) fn image_size(&self, src: &str) -> Option<Option<(f32, f32)>> {
        self.image_sizes.borrow().get(src).copied()
    }

    /// 记录 `src` 的显示尺寸；尺寸变化（占位→真实）自增 `images_gen` 使帧缓存失效。
    pub(crate) fn set_image_size(&self, src: SharedString, size: Option<(f32, f32)>) {
        let changed = self.image_sizes.borrow().get(&src).copied() != Some(size);
        if changed {
            self.image_sizes.borrow_mut().insert(src, size);
            self.images_gen.set(self.images_gen.get().wrapping_add(1));
        }
    }

    pub(crate) fn images_gen(&self) -> u64 {
        self.images_gen.get()
    }

    /// 用户拖拽调整的图片宽（px）；未调整返回 `None`（走自然宽）。
    pub(crate) fn image_width(&self, src: &str) -> Option<f32> {
        self.image_widths.borrow().get(src).copied()
    }

    /// 记录用户拖拽调整的图片宽（px，已钳制）；变化 → `images_gen` 自增使帧/整形缓存
    /// 失效（下一帧图片按新宽 + 原宽高比重排）。不改源码。
    pub(crate) fn set_image_width(&self, src: gpui::SharedString, w: f32) {
        let clamped = w
            .clamp(
                crate::config::markdown::IMAGE_MIN_W,
                crate::config::markdown::IMAGE_MAX_W,
            )
            .max(1.);
        if self.image_widths.borrow().get(&src).copied() != Some(clamped) {
            self.image_widths.borrow_mut().insert(src, clamped);
            self.images_gen.set(self.images_gen.get().wrapping_add(1));
        }
    }

    /// 当前进行中的图片调整条拖拽的 src（测试）。
    #[cfg(test)]
    pub(crate) fn image_resize_drag_src(&self) -> Option<gpui::SharedString> {
        self.image_resize_drag
            .borrow()
            .as_ref()
            .map(|d| d.src.clone())
    }
    /// 正在拖拽图片调整条（paint 期据此保持 resize 光标）。
    pub(crate) fn image_resizing(&self) -> bool {
        self.image_resize_drag.borrow().is_some()
    }

    /// 命中调整条 → 开始拖拽：记录按下时的图片宽 + 指针窗口 x，并选中该图。
    fn begin_image_resize(&mut self, src: gpui::SharedString, start_w: f32, grab_x: f32) {
        *self.image_resize_drag.borrow_mut() = Some(ImageResizeDrag { src, start_w, grab_x });
    }

    /// 拖拽中应用新宽：新宽 = `start_w + (mouse_x − grab_x)`（钳制），写回覆写表；
    /// 帧/整形缓存失效 → 图片随鼠标实时缩放（保持宽高比）。
    fn apply_image_resize(&mut self, mouse_x: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(d) = self.image_resize_drag.borrow().clone() else {
            return;
        };
        let new_w = d.start_w + (f32::from(mouse_x) - d.grab_x);
        self.set_image_width(d.src.clone(), new_w);
        *self.frame.borrow_mut() = None;
        cx.notify();
    }

    /// 结束调整条拖拽。
    fn end_image_resize(&mut self) {
        self.image_resize_drag.borrow_mut().take();
    }

    /* ---------- 数学排版（LaTeX → RenderImage；参考 zorite 的 MathStore） ---------- */

    /// 公式排版槽（元素 build/paint 只读查询）。
    pub(crate) fn math_slots(
        &self,
    ) -> Rc<RefCell<std::collections::HashMap<SharedString, MathSlot>>> {
        self.math_slots.clone()
    }

    /// 排版完成代数（Loading→Ready/Failed 自增，纳入帧缓存键）。
    pub(crate) fn math_gen(&self) -> u64 {
        self.math_gen.get()
    }

    /* ---------- Mermaid 图：滚轮逐块缩放（缩放值随块源码行存，重排用） ---------- */

    /// 某 mermaid 围栏块的缩放（缺省 1.0）。
    pub(crate) fn mermaid_zoom(&self, src_line: usize) -> f32 {
        self.mermaid_zoom.borrow().get(&src_line).copied().unwrap_or(1.0)
    }

    /// 设置某围栏块缩放并自增缩放代（纳入帧缓存/基础布局代 → 整帧+块高按新比例重建）。
    pub(crate) fn set_mermaid_zoom(&self, src_line: usize, z: f32) {
        self.mermaid_zoom.borrow_mut().insert(src_line, z);
        self.mermaid_zoom_gen.set(self.mermaid_zoom_gen.get().wrapping_add(1));
    }

    /// mermaid 缩放代数（进帧缓存键 + 基础布局代）。
    pub(crate) fn mermaid_zoom_gen(&self) -> u64 {
        self.mermaid_zoom_gen.get()
    }

    /// 当前获得焦点的 mermaid 卡（`src_line`）；`None` = 无（滚轮在 mermaid 上将继续滚动）。
    pub(crate) fn focused_mermaid(&self) -> Option<usize> {
        self.focused_mermaid
    }

    /// 数学源码的**显示行数**（card 回退估高用）：`math_display_lines` 会对每源做
    /// LaTeX→Unicode 转换，3000 块每次 `fold_plan` 重建重复转换 ≈38ms；按源缓存后
    /// O(1) 复用（同一源显示行数确定不变）。
    pub(crate) fn math_line_count(&self, src: &str) -> usize {
        if let Some(&n) = self.math_line_map.borrow().get(src) {
            return n;
        }
        let n = element::math_display_lines(src).len();
        if self.math_line_map.borrow().len() < 10_000 {
            self.math_line_map.borrow_mut().insert(src.to_string(), n);
        }
        n
    }

    /* ---------- 代码块高亮：后台 syntect 整块解析（把 debug 下 ~300ms/块移出主线程） ---------- */

    /// 查代码块高亮解析缓存：就绪返回高亮 run，否则按 NotReady 处理。
    pub(crate) fn code_parse_lookup(&self, lang: &str, code: &str) -> CodeParseLookup {
        let m = self.code_parse.lock().unwrap();
        let key = (lang.into(), code.into());
        match m.get(&key) {
            Some(CodeParseState::Done(runs)) => CodeParseLookup::Ready(runs.clone()),
            _ => CodeParseLookup::NotReady,
        }
    }

    /// shape 期登记一个需要后台解析的代码块（幂等：已在途/已就绪则跳过）。
    /// `src_line` 供完成时的带内门控使用。
    pub(crate) fn request_code_parse(&self, lang: &str, code: &str, src_line: usize) {
        let key: CodeParseKey = (lang.into(), code.into());
        {
            let mut m = self.code_parse.lock().unwrap();
            if !m.contains_key(&key) {
                m.insert(key.clone(), CodeParseState::Pending);
            }
        }
        // 记录**每个**请求块的 src_line：同一内容键可被多个内容完全相同的代码块请求
        // （sync_code_parse 按键分组、就绪时对全部同内容块定向作废缓存，否则滚回带内
        // 第二个相同副本因 request 去重永不重上色、永久保持纯文本）。
        let mut need = self.code_parse_need.borrow_mut();
        if !need.iter().any(|(k, s)| k == &key && *s == src_line) {
            need.push((key, src_line));
        }
    }

    /// 消费本帧登记的代码块后台解析请求（paint 期调用，对齐 `sync_math` 的可见带
    /// 懒解析）。syntect 整块解析是纯计算 → background executor 跑，完成后写缓存
    /// 并定向作废该块整形缓存 + 失效帧 → 主线程下一帧用高亮行重建该块（不再
    /// ~300ms 阻塞滚动/整形帧）。
    pub(crate) fn sync_code_parse(&mut self, cx: &mut Context<Self>) {
        let jobs = std::mem::take(&mut *self.code_parse_need.borrow_mut());
        if jobs.is_empty() {
            return;
        }
        // 按内容键分组（同一键可由多个相同代码块请求）：每个键只 spawn 一个解析任务，
        // 完成时对全部同内容块的 src_line 一并定向作废再上色。
        let mut groups: Vec<(CodeParseKey, Vec<usize>)> = Vec::new();
        for (key, src_line) in jobs {
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, lines)) if !lines.contains(&src_line) => lines.push(src_line),
                Some(_) => {}
                None => groups.push((key, vec![src_line])),
            }
        }
        for ((lang, code), src_lines) in groups {
            let cache = self.code_parse.clone();
            cx.spawn(async move |this, cx| {
                // 内层 async move 需要 'static 自有值：克隆一份进去，外层保留原值插缓存。
                let lang2 = lang.clone();
                let code2 = code.clone();
                let result: Option<crate::syntax::CodeLineRuns> = cx
                    .background_executor()
                    .spawn(async move {
                        crate::syntax::parse_code_lines(lang2.as_ref(), code2.as_ref())
                    })
                    .await;
                {
                    let mut m = cache.lock().unwrap();
                    m.insert((lang.clone(), code.clone()), CodeParseState::Done(result));
                }
                let _ = this.update(cx, |e, ecx| e.on_code_parse_ready(&src_lines, ecx));
            })
            .detach();
        }
    }

    /// 一个/多个代码块高亮就绪：**定向作废这些块的行整形缓存条目**（替代旧
    /// 「`code_parse_gen` 恒自增 + 进全局 shape_key」：全局键在代码块内每键打字（内容变 →
    /// 新内容解析完成 → gen++）时会让**整带所有块**（含无关段落）全量 miss 重排，真实
    /// 窗口顶部密集带 24-34ms/键、`bf.shape` 6-8ms）。改动后仅这些代码块（的旧纯文本
    /// 缓存行）被删，本帧/滚回带内由 Pass2 以新高亮重排该块，其余块保持命中。
    /// `src_lines` 可能是多个（内容完全相同的重复代码块共享同一解析键）。仅当任一
    /// src_line 仍在本帧带内（`frame.rows` 含它）才立即 `frame=None + notify` 强制本帧
    /// 换高亮——带外完成只删缓存、不打断帧（对齐 `on_math_ready` 的带内门控，消除多块
    /// 并发完成时的重建 churn）。
    fn on_code_parse_ready(&mut self, src_lines: &[usize], cx: &mut Context<Self>) {
        let block_indices: Vec<usize> =
            src_lines.iter().map(|&s| self.doc.block_index_for_line(s)).collect();
        let ids: Vec<u64> =
            block_indices.iter().map(|&b| frame::block_content_id(&self.doc, b)).collect();
        self.shape_rows.borrow_mut().invalidate_ids(&ids);
        let in_band = self
            .frame
            .borrow()
            .as_ref()
            .is_some_and(|f| f.rows.iter().any(|r| src_lines.contains(&r.src_line)));
        if in_band {
            *self.frame.borrow_mut() = None;
            cx.notify();
        }
    }

    /// 对**当前带内可见**公式发起离屏排版——按「只渲染窗口可见区域」原则（对齐
    /// `view`/`editor`）：不再开篇全量 claim 全篇 ~1.3 万公式。旧实现每个公式后台
    /// 光栅 10-20ms（debug），全量累计数百 CPU-秒 → 打满后台线程、UI 帧被拖慢 →
    /// 「还是很卡顿」。
    ///
    /// 消费**窗口化 `frame.rows`**（已整形的带内行：`extra.card = Math{latex}` 是块
    /// 卡片、`extra.inline_maths[].latex` 是行内公式）——滚入带的公式由 shape/splice
    /// 带出 → 这里按需 claim → 就绪后 `mgen++` 重建帧（卡片/行内 spacer 换真位图）。
    /// 幂等：已占用槽（含 Loading/Failed）不重复发起；主题色变化清缓存强制重排。
    /// 每次 paint 前调一次成本 = iterate `frame.rows`（≤ 带内行数）+ contains_key。
    pub(crate) fn sync_math(&mut self, cx: &mut Context<Self>, frame: &element::Frame) {
        let color = math_color(&self.theme);
        let color_changed = self.math_color.borrow().as_ref() != Some(&color);
        if color_changed {
            self.math_color.borrow_mut().replace(color);
            self.math_slots.borrow_mut().clear();
        }
        // 收集带内块 + 行内公式源（键 = 去定界符后的源码，同 zorite 的 MathStore）。
        // (槽键, 是否行内, 排版边距, 要排版的 LaTeX)：块用 `MATH_PAD`；行内用
        // `INLINE_MATH_PAD` 且键带 `inline:` 前缀 + `\textstyle` 包层（大算子转行内式，
        // 只压算子不缩正文——否则 `\int_0^1` 整图缩放会把 `x^2 dx` 也变小）。
        let mut sources: Vec<(SharedString, bool, f32, String, usize)> = Vec::new();
        for row in &frame.rows {
            let src_line = row.src_line;
            if let Some(element::BlockCard::Math { latex, .. }) = &row.extra.card {
                let s = latex.trim();
                if !s.is_empty() && s != "$$" {
                    sources.push((
                        s.to_string().into(),
                        false,
                        crate::config::math::MATH_PAD,
                        s.to_string(),
                        src_line,
                    ));
                }
            }
            for m in &row.extra.inline_maths {
                let lx = m.key.as_ref().strip_prefix("inline:").unwrap_or(m.key.as_ref());
                if !lx.is_empty() {
                    sources.push((
                        m.key.clone(),
                        true,
                        crate::config::math::INLINE_MATH_PAD,
                        lx.to_string(),
                        src_line,
                    ));
                }
            }
        }
        for (key, is_inline, pad, latex, src_line) in sources {
            // 行内公式登记「所在行」→ 完成时定向作废该行整形缓存（见 `inline_math_rows`）。
            if is_inline {
                let mut rows = self.inline_math_rows.borrow_mut();
                let v = rows.entry(key.clone()).or_default();
                if !v.contains(&src_line) {
                    v.push(src_line);
                }
                drop(rows);
            }
            let mut slots = self.math_slots.borrow_mut();
            if slots.contains_key(key.as_ref()) {
                continue;
            }
            slots.insert(key.clone(), MathSlot::Loading);
            drop(slots);
            // 离屏排版（纯 CPU；与 zorite `ensure_math_loaded` 同构）。
            let slots = self.math_slots.clone();
            let color2 = color;
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        // 行内先试 `\textstyle …`（大算子转上下标、只压算子）；包层解析
                        // 失败（如某些环境命令）则回退原样排版，避免把好公式降级成 Unicode。
                        if is_inline {
                            let ts = format!("\\textstyle {latex}");
                            math_typeset::render_latex_with_pad(
                                &ts,
                                crate::config::math::MATH_FONT_SIZE,
                                crate::config::math::MATH_DPR,
                                color2,
                                pad,
                            )
                            .or_else(|| {
                                math_typeset::render_latex_with_pad(
                                    &latex,
                                    crate::config::math::MATH_FONT_SIZE,
                                    crate::config::math::MATH_DPR,
                                    color2,
                                    pad,
                                )
                            })
                        } else {
                            math_typeset::render_latex_with_pad(
                                &latex,
                                crate::config::math::MATH_FONT_SIZE,
                                crate::config::math::MATH_DPR,
                                color2,
                                pad,
                            )
                        }
                        .map(|r| (r.image, r.width, r.height))
                    })
                    .await;
                let slot = match result {
                    Some((img, w, h)) => MathSlot::Ready(img, w, h),
                    None => MathSlot::Failed,
                };
                let slots2 = slots.clone();
                let key2 = key.clone();
                // 完成时自增 `math_gen`（on_math_ready）使帧/折叠计划按**真实位图高**重建：
                // 这里 key 必已以 `Loading` 占位 → `insert` 返回 `Some(Loading)`，用
                // `map_or(false, …Loading)` 判定“排版完成”（旧的 `.is_none()` 恒假——
                // 占位符永远在场，mgen 从不自增 → 卡片永远停在回退高 42px，68px 位图
                // 越出灰底——本 bug 自 RaTeX 接入起即存在）。
                let became_ready = slots2
                    .borrow_mut()
                    .insert(key.clone(), slot)
                    .is_some_and(|old| matches!(old, MathSlot::Loading));
                if became_ready {
                    let _ = this.update(cx, |e, ecx| e.on_math_ready(&key2, ecx));
                }
            })
            .detach();
        }
    }

    /// 一个公式排版完成：自增代数 → 帧缓存失效（下次 measure 以真实高度重建）。
    /// **定向作废**：行内公式（`"inline:…"`）→ 删除包含它的行整形缓存条目（行内
    /// spacer 宽/撑高依赖 slot 由 Loading→Ready 换代，不删则滚回带内命中旧行永不换
    /// 真图）。`$$` 显示块行在 Pass2 被 blocked、不入 `shape_rows`，无需作废。替代旧
    /// 「`mgen` 进全局 shape_key」——每个公式完成会让整带所有块（含无关段落）全 miss
    /// 重排一次（headless 实测 67/68 行整带重排）。
    /// **带门控**：完成的公式不在当前带内 → 只置代数 + 定向作废（保持候选高度一致，
    /// 滚动回带时由自然重建带上），不立即失效帧/重绘——滚过一片公式区时几十个后台光栅
    /// 完成不再各自打断一帧（消除数学密集段的滚动/空转卡顿）。在带内才立即失效（本帧换真图）。
    fn on_math_ready(&mut self, key: &str, cx: &mut Context<Self>) {
        if key.starts_with("inline:") {
            let mut lines: Vec<usize> = Vec::new();
            if let Some(v) = self.inline_math_rows.borrow().get(key) {
                lines.extend(v.iter().copied());
            }
            if let Some(f) = self.frame.borrow().as_ref() {
                for r in &f.rows {
                    if r.extra.inline_maths.iter().any(|m| m.key.as_ref() == key) {
                        lines.push(r.src_line);
                    }
                }
            }
            if !lines.is_empty() {
                lines.sort_unstable();
                lines.dedup();
                let blocks: Vec<usize> =
                    lines.into_iter().map(|s| self.doc.block_index_for_line(s)).collect();
                let ids: Vec<u64> =
                    blocks.iter().map(|&b| frame::block_content_id(&self.doc, b)).collect();
                self.shape_rows.borrow_mut().invalidate_ids(&ids);
            }
        }
        let in_band = self.frame.borrow().as_ref().is_some_and(|f| {
            f.rows.iter().any(|r| {
                if let Some(element::BlockCard::Math { latex, .. }) = &r.extra.card
                    && latex.trim() == key {
                        return true;
                    }
                r.extra.inline_maths.iter().any(|m| m.key.as_ref() == key)
            })
        });
        self.math_gen.set(self.math_gen.get().wrapping_add(1));
        if in_band {
            *self.frame.borrow_mut() = None;
            cx.notify();
        }
    }

    /* ---------- 光标 ↔ 源辅助 ---------- */

    /// 源字节偏移 → (源行, 行内字节偏移)。用 `Doc::line_starts` 二分（O(log n)），
    /// 不再每帧线性扫整篇源文本（7MB 大文档每帧 ~100ms+）。
    pub(crate) fn row_col(&self, off: usize) -> (usize, usize) {
        let off = off.min(self.len());
        let ls = self.doc.line_starts();
        // 最后一个 `line_starts[i] <= off` 的行。
        let line = ls.partition_point(|&st| st <= off).saturating_sub(1);
        let start = ls.get(line).unwrap_or(0);
        (line, off - start)
    }

    /// 源行 → 该行起始字节（O(log n)，`line_starts` 二分）。
    pub(crate) fn line_start(&self, line: usize) -> usize {
        self.doc
            .line_starts()
            .get(line)
            .unwrap_or(self.len())
    }

    /// 源行 → 行尾字节（不含换行符；`line_starts[line+1]-1` = 行尾 `\n` 位置）。
    /// 末行（无换行）返回内容尾。`CRLF` 下返回 `\n`（含 `\r`，与原文语义一致）。
    pub(crate) fn line_end(&self, line: usize) -> usize {
        let ls = self.doc.line_starts();
        let start = ls.get(line).unwrap_or(self.len());
        match ls.get(line + 1) {
            Some(next) => (next.saturating_sub(1)).min(self.len()).max(start),
            None => self.len(),
        }
    }

    /// 该行的粗略 `RowKind`（行首启发，实体内部导航用；渲染以 `DocRow` 为准）。
    fn kind_for_line(&self, line: usize) -> crate::markdown::parse::RowKind {
        use crate::markdown::parse::RowKind;
        let start = self.line_start(line);
        let end = self.line_end(line);
        let raw = self.doc.slice_str(start..end);
        let t = raw.trim_start();
        if t.is_empty() {
            RowKind::Blank
        } else if t.starts_with("```") {
            RowKind::Code
        } else if t.starts_with('|') {
            RowKind::Table
        } else if t.starts_with('#') {
            RowKind::Heading(t.chars().take_while(|c| *c == '#').count() as u8)
        } else if t.starts_with("![") {
            RowKind::Image
        } else if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
            RowKind::Bullet
        } else if t == "---" || t == "***" || t == "___" {
            RowKind::Rule
        } else if t.starts_with('>') {
            RowKind::Quote
        } else {
            RowKind::Paragraph
        }
    }

    /// 该行「可编辑段」的源局部区间 `(lo, hi)`：`lo` = 正文源偏移，`hi` = 行尾。
    /// 非文本行 → `None`（只读/不可编辑）。代码内容行 / 表格表头与数据行整行可编辑
    /// （围栏标记行 / 表格分隔行仍 `None`）。独立图片行 → alt（`![` 之后到 `](` 之前）。
    fn editable_segment(&self, line: usize) -> Option<(usize, usize)> {
        let start = self.line_start(line);
        let end = self.line_end(line);
        if end <= start {
            return None;
        }
        if self.in_block_content(line) {
            return Some((0, end - start));
        }
        let raw = self.doc.slice_str(start..end);
        // 独立图片行：可编辑段 = alt（`![` 之后到 `](` 之前），题注行可编辑。
        if self.kind_for_line(line) == crate::markdown::parse::RowKind::Image {
            return self.image_alt_segment(line);
        }
        let seg = crate::markdown::parse::text_segment(&raw, self.kind_for_line(line))?;
        Some((seg.0, raw.len()))
    }

    /// 独立图片行的可编辑段（源局部区间，相对行首）：alt 内容 `[lo, hi)`。
    /// 非图片行 / 解析失败 → `None`。与 `DocRow.byte_map`（图片题注映射）同构。
    fn image_alt_segment(&self, line: usize) -> Option<(usize, usize)> {
        if self.kind_for_line(line) != crate::markdown::parse::RowKind::Image {
            return None;
        }
        let start = self.line_start(line);
        let end = self.line_end(line);
        if end <= start {
            return None;
        }
        let raw = self.doc.slice_str(start..end);
        let lead = raw.len() - raw.trim_start().len();
        let r = crate::markdown::parse::parse_ref(raw.trim_start())?;
        let lo = lead + 2;
        Some((lo, lo + r.alt.len()))
    }

    /* ---------- 可编辑块区域（代码/表格 + 围栏行判定） ---------- */

    /// 从块索引重建每个可编辑块区域（代码内容行 / 表格表头+数据行）。文本变更后调用。
    /// 结构编辑后**派生区域行号平移**（B3 增量，替代整篇 `inline_style::math_regions`/
    /// `alert_regions` 重扫）：编辑线**之上**（`end <= edit_line`）区域完全不变；**之下**
    /// （`start > edit_line`）整体平移 `Δ`（source 内容未变，保留）。若派生未缓存或
    /// 有区域**跨过**编辑线（内容变了，无法仅平移）→ 返回 `false`，调用方回退整篇
    /// 重建（`regions_ver++`，`derived()` 懒重建）。
    fn shift_derived_after_edit(&mut self, edit_line: usize, line_delta: i64) -> bool {
        let cached = self.derived.borrow_mut();
        let Some((_, rc)) = cached.as_ref() else {
            return false;
        };
        let mut d = rc.borrow_mut();
        // 跨线区域：编辑线落在区域内 → 内容/边界变了，无法纯平移。
        if d
            .math_regions
            .iter()
            .any(|r| r.range.start <= edit_line && edit_line < r.range.end)
            || d
                .alert_fold_regions
                .iter()
                .any(|(r, _)| r.start <= edit_line && edit_line < r.end)
        {
            return false;
        }
        let shift = |x: usize| -> usize { (x as i64 + line_delta).max(0) as usize };
        for reg in &mut d.math_regions {
            if reg.range.start > edit_line {
                reg.range.start = shift(reg.range.start);
                reg.range.end = shift(reg.range.end);
            }
        }
        for (r, _) in &mut d.alert_fold_regions {
            if r.start > edit_line {
                *r = shift(r.start)..shift(r.end);
            }
        }
        if line_delta != 0 {
            let old = std::mem::take(&mut d.alert_region_kinds);
            d.alert_region_kinds =
                rustc_hash::FxHashMap::with_capacity_and_hasher(old.len(), Default::default());
            for (l, k) in old {
                d.alert_region_kinds.insert(shift(l), k);
            }
        }
        true
    }

    fn rebuild_block_regions(&mut self) {
        use crate::markdown::parse::RowKind;
        let n = self.doc.block_count();
        let mut regions: Vec<BlockRegion> = Vec::new();
        for i in 0..n {
            let Some(m) = self.doc.block_meta(i) else {
                continue;
            };
            match m.kind {
                RowKind::Code => {
                    let lo = m.src_line as usize + 1;
                    let hi = lo + m.aux.max(1) as usize - 1;
                    regions.push(BlockRegion::Code { lo, hi });
                }
                RowKind::Table => {
                    let header = m.src_line as usize;
                    let dlo = header + 2;
                    let dhi = header + 1 + m.aux.max(1) as usize;
                    regions.push(BlockRegion::Table { header, dlo, dhi });
                }
                _ => {}
            }
        }
        self.block_regions = regions;
    }

    /// B4：结构编辑后的 `block_regions` **增量更新**——替代每键 O(块数) 整表重建
    /// [`Self::rebuild_block_regions`]（6MB/10 万块 debug ~5.9ms/键）。
    ///
    /// 结构编辑是**局部**的：只影响「编辑线所在块」及其直接后继（Enter 分裂、退格合并、
    /// 围栏跨界至多涉及这两块）。做法：把既有区域按**所属块**（区域回到块 = 围栏开行/
    /// 表头行，再经 `line_block` 稠密查）分类——窗口 `[edit_blk, edit_blk+2)` 内 → 丢弃、
    /// 由新 doc 重算；窗口之前 → 原样保留（块内容/行号未变）；窗口之后 → 整行号平移
    /// `line_delta`。序列仍按 src_line 升序（上方保留 < 窗口新派生 < 下方平移）。
    /// 首建/整篇重置仍走 [`Self::rebuild_block_regions`]。
    fn update_block_regions_after_edit(&mut self, edit_line: usize, line_delta: i64) {
        use crate::markdown::parse::RowKind;
        let doc = &self.doc;
        let n = doc.block_count();
        let span = |r: &BlockRegion| -> (usize, usize) {
            match r {
                BlockRegion::Code { lo, hi } => (*lo, *hi),
                BlockRegion::Table { header, dlo, dhi } => (*header.min(dlo), *dhi),
            }
        };
        let shift = |r: &BlockRegion, d: i64| -> BlockRegion {
            let sh = |x: usize| (x as i64 + d).max(0) as usize;
            match r {
                BlockRegion::Code { lo, hi } => BlockRegion::Code {
                    lo: sh(*lo),
                    hi: sh(*hi),
                },
                BlockRegion::Table { header, dlo, dhi } => BlockRegion::Table {
                    header: sh(*header),
                    dlo: sh(*dlo),
                    dhi: sh(*dhi),
                },
            }
        };
        // 1) 旧区域按**行号**分类（不查新 doc 块：编辑线下方的行号已平移，反查会错位）：
        //    完全在编辑线上方 → 保留（内容/行号未变）；完全在下方 → 整行号平移；
        //    跨编辑线（含编辑线）→ 只可能是被编辑块的区域 → 丢弃，交给 2) 重算。
        let mut kept: Vec<BlockRegion> = Vec::with_capacity(self.block_regions.len());
        for &r in &self.block_regions {
            let (mn, mx) = span(&r);
            if mx < edit_line {
                kept.push(r);
            } else if mn > edit_line {
                kept.push(shift(&r, line_delta));
            }
            // else：跨编辑线 → 由窗口重算
        }
        // 2) 新 doc 重算受影响窗口块 [edit_blk, edit_blk+2)（Enter 分裂 / 删除合并 / 围栏
        //    ——至多涉及编辑线所在块及其直接后继两 3 块）；块序升序，按首行插回排序序列。
        let edit_blk = doc.block_index_for_line(edit_line).min(n.saturating_sub(1));
        let win_hi = (edit_blk + 2).min(n);
        for i in edit_blk..win_hi {
            let Some(m) = doc.block_meta(i) else { continue };
            let r = match m.kind {
                RowKind::Code => {
                    let lo = m.src_line as usize + 1;
                    let hi = lo + m.aux.max(1) as usize - 1;
                    BlockRegion::Code { lo, hi }
                }
                RowKind::Table => {
                    let header = m.src_line as usize;
                    let dlo = header + 2;
                    let dhi = header + 1 + m.aux.max(1) as usize;
                    BlockRegion::Table { header, dlo, dhi }
                }
                _ => continue,
            };
            let rl = span(&r).0;
            let pos = kept.partition_point(|k| span(k).1 < rl);
            kept.insert(pos, r);
        }
        self.block_regions = kept;
    }

    /// `line` 所在的可编辑块区域（代码内容行 / 表格表头+数据行）。
    fn region_content(&self, line: usize) -> Option<BlockRegion> {
        self.block_regions.iter().copied().find(|r| match r {
            BlockRegion::Code { lo, hi } => line >= *lo && line <= *hi,
            BlockRegion::Table { header, dlo, dhi } => {
                line == *header || (line >= *dlo && line <= *dhi)
            }
        })
    }

    /// `line` 是否为可编辑块区域的**围栏行**（代码 ``` 标记行 / 表格分隔行；只读）。
    fn is_fence_line(&self, line: usize) -> bool {
        self.block_regions.iter().any(|r| match r {
            BlockRegion::Code { lo, hi } => {
                (line + 1 == *lo) || (line > 0 && line - 1 == *hi)
            }
            BlockRegion::Table { dlo, .. } => line + 1 == *dlo,
        })
    }

    /// `line` 是否落在可编辑块区域的**内容行**内（可编辑）。
    fn in_block_content(&self, line: usize) -> bool {
        self.region_content(line).is_some()
    }

    /// 表格内容行：取离 `col` 最近的单元格内容边界 `(start, end)`（相对行首）。
    /// 非表格行/无格 → None。
    fn table_cell_bounds(&self, line: usize, col: usize) -> Option<(usize, usize)> {
        if !matches!(self.region_content(line), Some(BlockRegion::Table { .. })) {
            return None;
        }
        let raw = self.line_text(line);
        let cells = crate::markdown::parse::split_table_cells_mapped(&raw);
        if cells.is_empty() {
            return None;
        }
        let idx = (0..cells.len())
            .min_by_key(|&i| {
                let r = &cells[i].0;
                if col < r.start {
                    r.start - col
                } else { col.saturating_sub(r.end) }
            })
            .unwrap_or(0);
        let r = &cells[idx].0;
        Some((r.start, r.end))
    }

    /// 表格内容行内的 caret 钳到最近单元格内容区间 `[start, end]`（不落在管道/空白上）。
    fn clamp_caret_table(&self, line: usize, col: usize) -> Option<usize> {
        let (s, e) = self.table_cell_bounds(line, col)?;
        Some(self.line_start(line) + col.clamp(s, e.max(s)))
    }

    /// 从 `line` 向下找第一个**非围栏**行（可含当前行；跳过连续围栏行）。
    fn prev_render_line(&self, line: usize) -> usize {
        let mut l = line;
        while l > 0 && self.is_fence_line(l) {
            l -= 1;
        }
        l
    }

    /// 从 `line` 向上找第一个**非围栏**行（可含当前行；跳过连续围栏行）。
    fn next_render_line(&self, line: usize) -> usize {
        let total = self.doc.line_starts().len();
        let mut l = line;
        while l + 1 < total && self.is_fence_line(l) {
            l += 1;
        }
        l
    }

    /* ---------- 编辑操作（源字节域） ---------- */

    /// 核心编辑：替换 `[start, end)` → `text`，重建模型、移光标、通知。
    /// 任意替换都会收敛选区（无选区）与 IME 组合区间（替换后由输入处理器重设）。
    /// **所有编辑**（输入/剪贴板/退格/任务框翻转/警报折叠）都从这里过——每笔真实
    /// 内容变更先记入撤销栈（D1）；**B2 事务合并**：连续同意图且邻接的编辑并入同一
    /// 撤销单元（连续打字/相邻退格/同点删除），只在「新事务起点」push 快照。
    pub fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        // 只读门控（A1）：`set_editable(false)` 后一律短路——输入/IME/粘贴/退格/删除/
        // Enter/撤销/重做等一切内容变更都汇聚到本函数，这里硬门控即杜绝只读预览改动。
        if !self.editable {
            return;
        }
        let len = self.len();
        let r = range.start.min(len)..range.end.min(len);
        if r.start == r.end && text.is_empty() {
            return;
        }
        let _t_rr = crate::prof::begin();
        // 编辑前被替换区间（cross_newline / 标记计数 / 撤销意图用；O(被删长)）。
        let removed = self.doc.slice_str(r.clone());
        let inserted_len = text.len();
        // —— B2 撤销单元 + 事务合并（无定时器确定性判定，同 editor/undo.rs）——
        self.record_undo_unit(&r, &removed, text);
        // 原地 rope 编辑（`Doc::edit`：COW remove+insert + 增量块扫描 + 增量行起始）——
        // 不再每键拼一份 6MB 整串再分配/复制（旧 `next` String + Arc 包装是 6MB 文档
        // ~5-6ms/键的根因）；`content` 字段随之取消，rope 为唯一真源。
        let edit_line = self.row_col(r.start).0;
        // 编辑前该行内容（region 签名对比用；非结构性编辑行边界不变）。
        let old_line = self.doc.line_str(edit_line);
        let prev_count = self.doc.block_count();
        // 编辑前被编辑块的几何（kind/src_line/aux）——用于判定是否需要降级为结构编辑：
        // 代码/表格**内容行**的非换行键入块数/几何都变 → 走非结构路径（Patch + 局部
        // 作废整形缓存），免整篇 Pass1 高度重建 + fold_plan 重建 + block_regions 整表
        // 扫描（旧实现 `in_code_table` 一律 structural → 表内每键 ~140ms）。
        let edit_blk = self.doc.block_index_for_line(edit_line);
        let prev_geo = self.doc.block_meta(edit_blk).map(|m| (m.kind, m.src_line, m.aux));
        // 编辑前被编辑块的内容指纹（整形缓存**内容寻址**键）：编辑后内容已变 → 剔除
        // 旧指纹行（否则旧条目滞留到被 LRU 淘汰；测试与内存卫生都要它即时消失）。
        let old_cid = frame::block_content_id(&self.doc, edit_blk);
        self.doc.edit(r.clone(), text);
        let cross_newline = text.contains('\n') || removed.contains('\n') || text.contains('\r');
        let breaks_fence = text.contains("```") || text.trim_start().starts_with('|');
        let count_same = self.doc.block_count() == prev_count;
        // 几何不变 = 新块 meta 与编辑前 (kind, src_line, aux) 逐项一致；不一致（块被
        // 吞并/分裂/变成表格/围栏等）必须结构重建。
        let geo_same = match (self.doc.block_meta(edit_blk), prev_geo) {
            (Some(nm), Some(om)) => nm.kind == om.0 && nm.src_line == om.1 && nm.aux == om.2,
            _ => false,
        };
        let structural = cross_newline || breaks_fence || !count_same || !geo_same;
        // Pass1 增量重估定位 + 整形/预热缓存作废（结构 vs 非结构两路，见 `invalidate_after_edit`）。
        self.invalidate_after_edit(edit_line, edit_blk, prev_count, structural, old_cid, &removed, text);
        // —— 内容派生（derived）的增量判定（详见 `update_derived_after_edit`）——
        self.update_derived_after_edit(edit_line, &old_line, structural, text, &removed);

        self.caret = (r.start + inserted_len).min(self.len());
        self.sel_anchor = None;
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        crate::prof::end("wy.replace_range", _t_rr);
        cx.emit(WyEvent::Changed);
        cx.notify();
    }

    /// B2 撤销单元 + 事务合并（无定时器确定性判定，同 `editor/undo.rs`）：
    /// 「同意图且邻接」→ 并入当前打开的单元（净 `insert`/`delete` 累积成一次替换），
    /// 否则关闭上一个单元并入栈并开新单元；200 深可覆盖远多于 200 字符的内容。
    /// 返回是否并入上一事务。
    fn record_undo_unit(&mut self, r: &Range<usize>, removed: &str, text: &str) -> bool {
        let inserted_len = text.len();
        let intent = if removed.is_empty() && !text.is_empty() && !text.contains('\n') {
            EditKind::Typing
        } else if !removed.is_empty() && text.is_empty() {
            // 纯删除：退格（r.end == 光标）vs 正向删除（r.start == 光标）。
            if self.caret == r.end {
                EditKind::Backspace
            } else {
                EditKind::Delete
            }
        } else {
            EditKind::Atomic
        };
        let merges = match self.last_edit {
            Some((k, s, _e)) if k == intent => match intent {
                // 纯插入区间零宽：上一编辑的**插入终点** = 其起点 + 插入长。
                EditKind::Typing => r.start == s + inserted_len,
                EditKind::Backspace => r.end == s, // 向后退格首尾相接
                EditKind::Delete => r.start == s,  // 同起点延伸
                EditKind::Atomic => false,
            },
            _ => false,
        };
        if merges {
            // 并入打开单元：净区间/文本按意图累积（逆操作坐标不变形，见 UndoUnit）。
            let u = self
                .open_unit
                .as_mut()
                .expect("能合并必已有打开的单元");
            u.insert.push_str(text);
            match intent {
                EditKind::Backspace => {
                    // 新增删除在左侧：`removed` 前插、区间起点左移（退格序列自右向左）。
                    u.delete = format!("{removed}{}", u.delete);
                    u.range.start = u.range.start.saturating_sub(removed.len());
                }
                // 同点删除延伸：删除文本按序追加。
                EditKind::Delete => u.delete.push_str(removed),
                _ => {} // 纯键入：insert 已追加，delete 保持（纯键入时为空）。
            }
            u.range.end = u.range.start + u.insert.len();
        } else {
            // 关闭上一个打开的单元（撤销单元自此固化）并入栈，开新单元。
            if let Some(prev) = self.open_unit.take() {
                self.push_undo_unit(prev);
            }
            self.open_unit = Some(UndoUnit {
                range: r.start..(r.start + inserted_len),
                insert: text.to_string(),
                delete: removed.to_string(),
                caret_before: self.caret,
                forward_caret: 0, // undo 时再写
            });
        }
        self.redo_stack.clear();
        self.last_edit = Some((intent, r.start, r.start + inserted_len));
        merges
    }

    /// 入撤销栈（深度上限 [`UNDO_LIMIT`]，超出丢最旧）。
    fn push_undo_unit(&mut self, u: UndoUnit) {
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(u);
    }

    /// Pass1 增量重估定位 + 整形/预热缓存作废：编辑行的**前一内容行**之后的块全部未变
    /// （前缀高度复用，不整篇重估）；结构编辑保留 < edit_blk 前缀的整形/重试缓存并归零
    /// 预热，非结构编辑只作废受影响块（块下标稳定）。
    #[allow(clippy::too_many_arguments)] // 编辑管线的正交输入
    fn invalidate_after_edit(
        &mut self,
        edit_line: usize,
        edit_blk: usize,
        prev_count: usize,
        structural: bool,
        old_cid: u64,
        removed: &str,
        text: &str,
    ) {
        // 每轮编辑先清「结构编辑待消费」标志；结构性分支再置位（见字段注释）。
        self.struct_edit_pending.set(false);
        // 内容寻址整形缓存：被编辑块旧指纹行先剔除（内容已变，本帧起按新指纹重排）。
        self.shape_rows.borrow_mut().invalidate_ids(&[old_cid]);
        // Pass1 增量高度重估起点；三段式（前缀 / 变化中段 / 尾部复用）定位用。
        self.struct_dirty_line.set(edit_line);
        self.struct_old_end_line.set(edit_line + removed.matches('\n').count());
        self.struct_line_delta
            .set(text.matches('\n').count() as i64 - removed.matches('\n').count() as i64);
        if structural {
            self.struct_ver.set(self.struct_ver.get().wrapping_add(1));
            *self.pending_patch_blocks.borrow_mut() = None;
            // fold_plan deltas 增量平移的编辑信息：被编辑块旧下标 + 块数净增。
            self.struct_blk_old.set(edit_blk);
            self.struct_blk_span_delta
                .set(self.doc.block_count() as i64 - prev_count as i64);
            self.struct_edit_pending.set(true);
            // 整形缓存**内容寻址**（`frame::block_content_id`）：位移但内容未变的块指纹
            // 不变 → Pass2 自动命中，无需 retain_prefix；只有内容真正变化的块（编辑块，
            // 指纹已变）自然 miss。（旧实现按块下标键，这里曾不得不 retain_prefix 保前缀
            // 丢后缀，Enter 仍整带重排。）notdef 重试表仍按下标：前缀保留、≥ edit 作废。
            let prefix_keep = edit_blk.min(prev_count);
            self.notdef_retry.borrow_mut().retain_prefix(prefix_keep);
            // 结构编辑清空整形缓存 → 预热进度全部作废：归零方向/偏差/带序号，
            // 下一帧由带序号跨界重新判定方向（否则拿旧 dir 在空区间空转）。
            let mut pw = self.prewarm.borrow_mut();
            pw.dir = 0;
            pw.bias = 0;
            pw.last_band_idx = None;
        } else {
            // 非结构性：块下标稳定，记受影响的编辑行块。
            let blk = self.doc.block_index_for_line(edit_line);
            *self.pending_patch_blocks.borrow_mut() = Some(vec![blk]);
            // 该块内容变了 → 指纹自然变 → 整形缓存自生效（无需显式作废）。
            self.notdef_retry.borrow_mut().clear_block(blk);
        }
    }

    /// 内容派生（derived）的增量判定（详见 `region_line_sig`）：更新 math/alert 标记计数；
    /// 结构编辑把派生区域**行号平移**（B3 增量）并重建 block_regions；非结构编辑比对行
    /// 签名，签名不变即区域结构不变 → 走数学块 source 小修补（都不全篇重建）。
    /// 目标：**纯文本键入**（非结构、行签名不变、不在数学块内）不动 `regions_ver`
    /// → `derived` Rc 跨键复用——大文档（尤其结构密集）「输入卡死」的根因是旧实现
    /// 每键对全篇 `math_regions`×2 + `alert_*`×2 + 行内逐行扫 ≈1s。
    /// （公式光栅已走「可见带懒加载」，不再需要另一路全篇数学重扫信号。）
    fn update_derived_after_edit(
        &mut self,
        edit_line: usize,
        old_line: &str,
        structural: bool,
        text: &str,
        removed: &str,
    ) {
        self.math_dollar_cnt.set(
            self.math_dollar_cnt.get()
                + text.bytes().filter(|&b| b == b'$').count()
                - removed.bytes().filter(|&b| b == b'$').count(),
        );
        self.alert_marker_cnt.set(
            self.alert_marker_cnt.get() + text.matches("[!").count() - removed.matches("[!").count(),
        );
        if structural {
            // 行索引 / 块结构变化：派生区域**行号平移**——编辑线之上的区域不变、之下
            // 平移 Δ；有区域跨线 / 派生未缓存才回退整篇重建。可编辑块区域（代码/表格）
            // 下标随之变化 → 一并重建（否则导航判定错位）。
            let line_delta =
                (text.matches('\n').count() as i64) - (removed.matches('\n').count() as i64);
            if !self.shift_derived_after_edit(edit_line, line_delta) {
                self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
            }
            // B4：`block_regions` 增量更新（编辑线附近重算 + 其余平移），不再每键
            // O(块数) 整表扫描（6MB/10 万块 debug ~5.9ms/键）。
            self.update_block_regions_after_edit(edit_line, line_delta);
        } else {
            // 非结构性：代码/表格区域布局不变 → `block_regions` 复用，跳过
            // `rebuild_block_regions` 的 O(块数) 整表扫描（6MB/6 万块 debug ~2.3ms/键，
            // 是「输入"仍卡"」里仅次于整串复制的第二大头；`structural` 判定已保证
            // 非结构性编辑既不跨行、也不在代码/表格块内 → 区域必然不变）。
            let new_line = self.doc.line_str(edit_line);
            if region_line_sig(old_line) != region_line_sig(&new_line) {
                // 该行 region 签名变了（出现/消失 `$`、``` 、`[!` 或 `$$` 边界等）
                // → 区域结构可能变化 → 整篇重建派生。
                self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
            } else {
                let _t_m = crate::prof::begin();
                // 快路径：区域结构不变，只做受影响点的小修补（都不全篇重建）——
                // 编辑行在 `$$…$$` 数学块内 → 重算该块 `source`（卡片 / 排版键正确，
                // 否则离开块后卡片显示旧公式）。
                {
                    let derived = self.derived();
                    let d = derived.borrow();
                    if let Some(idx) = d
                        .math_regions
                        .iter()
                        .position(|reg| edit_line >= reg.range.start && edit_line < reg.range.end)
                    {
                        let rng = d.math_regions[idx].range.clone();
                        let old_src = d.math_regions[idx].source.clone();
                        drop(d);
                        let mut d2 = derived.borrow_mut();
                        let new_source = if rng.end == rng.start + 1 {
                            // 单行 `$$x^2$$`：source = 行内内层。
                            inline_style::one_line_math(&new_line)
                                .map(str::to_string)
                                .unwrap_or(old_src)
                        } else if rng.start + 1 < rng.end.saturating_sub(1) {
                            let hi = rng.end.saturating_sub(1);
                            (rng.start + 1..hi)
                                .map(|l| self.doc.line_str(l))
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            String::new()
                        };
                        if d2.math_regions[idx].source != new_source {
                            d2.math_regions[idx].source = new_source;
                        }
                    }
                }
                crate::prof::end("wy.rr.math", _t_m);
            }
        }
    }

    /// 记录撤销快照并做一次文本替换（任务框 toggle / 警报折叠共用）。
    /// 快照与重做栈的维护在 [`Self::replace_range`] 内统一完成。
    fn apply_edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.replace_range(range, text, cx);
    }

    /// 翻转第 `line` 行（`> [!NOTE]-` / `+`）的折叠符（C3）：`-`→`+` 展开、`+`→`-` 折叠。
    pub(crate) fn toggle_alert_fold_at_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_end(line).max(start);
        let raw = self.doc.slice_str(start..end);
        if let Some((at, cur)) = recognition::alert_fold_char(&raw) {
            let range = start + at..start + at + 1;
            let repl = if cur { "+" } else { "-" };
            if range.end <= end {
                self.apply_edit(range, repl, cx);
            }
        }
    }

    /// 翻转第 `line` 行标题的折叠态（视图本地，不写源码）：`## Goals` body 行折叠。
    /// 折叠时若光标在该 section 的 body 内会被 reveal 弹开（折叠无效），先把光标
    /// 放到标题行末尾（与 live 的 `move_to(end)` 同款）。
    pub(crate) fn toggle_heading_fold_at_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_end(line).max(start);
        let key = self.doc.slice_str(start..end).trim().to_string();
        if key.is_empty() {
            return;
        }
        let was_folded = self.folded_headings.borrow().contains(&key);
        if !was_folded {
            // 折叠前：光标若在 section body 内，先移到标题行末（否则 body 有光标被 reveal）。
            let single = std::collections::HashSet::from([key.clone()]);
            let crow = self.row_col(self.caret()).0;
            if inline_style::heading_fold_regions(&self.doc.text_shared(), &single)
                .iter()
                .any(|r| crow > r.start && crow < r.end)
            {
                self.set_caret(end);
            }
        }
        {
            let mut folded = self.folded_headings.borrow_mut();
            if was_folded {
                folded.remove(&key);
            } else {
                folded.insert(key);
            }
        }
        // 折叠态变化 → `fold_plan` 缓存失效（滚动/带移动间不重算，但折叠变必须重建）。
        self.fold_ver.set(self.fold_ver.get().wrapping_add(1));
        *self.frame.borrow_mut() = None;
        cx.notify();
    }

    /// 翻转第 `line` 行任务框（C1）：`[ ]`↔`[x]`。
    pub(crate) fn toggle_checkbox_at_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_end(line).max(start);
        let raw = self.doc.slice_str(start..end);
        if let Some(new_line) = inline_style::toggle_task_checkbox(&raw) {
            self.apply_edit(start..end, &new_line, cx);
        }
    }

    /* ---------- 图片编辑（悬浮铅笔按钮 + 卡片表单） ---------- */

    /// 当前悬浮的图片行（`Some(src_line)` 时画铅笔按钮）。
    pub(crate) fn hover_image(&self) -> Option<usize> {
        self.hover_image.get()
    }
    /// 当前选中的图片行（`Some(src_line)` 时该图片画 accent 描边 + 编辑按钮常显）。
    pub(crate) fn selected_image(&self) -> Option<usize> {
        self.selected_image.get()
    }
    /// 图片编辑卡片当前锚定的图片行；`None` = 卡片未打开。（测试访问器）
    #[cfg(test)]
    pub(crate) fn image_edit_src_line(&self) -> Option<usize> {
        self.image_edit.as_ref().map(|p| p.src_line)
    }
    /// 卡片是否打开。（测试访问器）
    #[cfg(test)]
    pub(crate) fn image_edit_open(&self) -> bool {
        self.image_edit.is_some()
    }
    /// 卡片的「链接」输入框实体（render / 测试读值）。
    #[cfg(test)]
    pub(crate) fn image_edit_link_input(&self) -> Option<gpui::Entity<crate::ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.link.clone())
    }
    /// 卡片的「文本」输入框实体。
    #[cfg(test)]
    pub(crate) fn image_edit_text_input(&self) -> Option<gpui::Entity<crate::ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.text.clone())
    }
    /// 独立图片行 `src_line` 的当前 `(alt, src)`（`parse::parse_image_line` 反解）；
    /// 非独立图片行返回 `None`。
    fn image_line_alt_src(&self, src_line: usize) -> Option<(String, String)> {
        let raw = self.line_text(src_line);
        crate::markdown::parse::parse_image_line(raw.trim())
    }

    /// 打开 / 切换到 `src_line` 图片行的编辑卡片（仅可编辑模式）。输入框以当前
    /// alt / src 预填，打开后焦点落在「链接」输入框。已在同一行 → 关闭。
    pub(crate) fn open_image_edit(
        &mut self,
        src_line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editable {
            return;
        }
        if self
            .image_edit
            .as_ref()
            .is_some_and(|cur| cur.src_line == src_line)
        {
            self.close_image_edit(cx);
            return;
        }
        let Some((alt, src)) = self.image_line_alt_src(src_line) else {
            return;
        };
        // 打开编辑卡片也视为选中该图片（描边 + 按钮常显，与点图本体同效果）。
        self.selected_image.set(Some(src_line));
        let link = cx.new(|cx| {
            crate::ui::input::InputState::new(window, cx)
                .placeholder("图片链接（URL / 本地相对路径）")
                .default_value(src)
        });
        let text = cx.new(|cx| {
            crate::ui::input::InputState::new(window, cx)
                .placeholder("图片文本（alt 题注）")
                .default_value(alt)
        });
        self.image_edit = Some(ImageEditPopup { src_line, link, text });
        // 计算并缓存卡片锚点：用**当前帧 + 上次 paint 的元素原点**（同代配对）。这保证
        // 打开后的第 1 次 render（在 paint 之前）就有正确几何；此后每帧 paint 刷新。
        // ⚠️ 不再依赖 render 期现算 frame——`end_selection` 清帧后 ensure 用 800px 宽
        // 重建会让 render 读到一个宽度错误的帧，卡片横向跳一帧。
        if let Some(f) = self.frame.borrow().as_ref().cloned() {
            let origin = self.content_origin();
            self.image_edit_anchor.set(self.image_edit_anchor_at(origin, &f));
        }
        // 打开后立刻聚焦「链接」输入框（用户可直接键入/IME）。
        if let Some(p) = &self.image_edit {
            let fh = p.link.read(cx).focus_handle(cx);
            fh.focus(window, cx);
        }
        cx.notify();
    }

    /// 图片编辑卡片的**窗口锚点**（卡片 TopLeft，window 坐标）：内容区水平居中 +
    /// 顶对齐目标图片行的当前屏幕 y。帧行坐标是**文档局部**（`row.top` 含整篇偏移），
    /// 窗口坐标 = 元素屏幕原点（`origin`）+ 帧局部。`origin` 必须与 `f` 同代（paint 期
    /// 用 `bounds.origin` + 本帧 frame，open 期用 `last_bounds` + 保持的帧），否则换帧
    /// 就闪。缺卡片/找不到图片行 → `None`。
    pub(crate) fn image_edit_anchor_at(
        &self,
        origin: gpui::Point<Pixels>,
        f: &element::Frame,
    ) -> Option<gpui::Point<Pixels>> {
        let popup = self.image_edit.as_ref()?;
        let card_w = element::IMAGE_EDIT_CARD_W;
        let content_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
        let l = f32::from(f.padding) + ((content_w - card_w) / 2.).max(0.);
        let row_top = f
            .rows
            .iter()
            .find(|r| {
                r.src_line == popup.src_line && r.kind == crate::markdown::ir::IrKind::Image
            })
            .map(|r| f32::from(r.top))
            .unwrap_or_else(|| {
                let blk = f.block_index_for_src_line(popup.src_line).unwrap_or(0);
                f.block_top(blk)
            });
        Some(point(
            px(f32::from(origin.x) + l),
            px(f32::from(origin.y) + row_top),
        ))
    }

    /// 设置图片编辑卡片锚点（`element::WyEditorElement::paint` 每帧刷新；`None` 清空）。
    pub(crate) fn set_image_edit_anchor(&self, anchor: Option<gpui::Point<Pixels>>) {
        self.image_edit_anchor.set(anchor);
    }
    /// 当前缓存的卡片锚点（测试访问器）。
    #[cfg(test)]
    pub(crate) fn image_edit_anchor(&self) -> Option<gpui::Point<Pixels>> {
        self.image_edit_anchor.get()
    }

    /// 关闭图片编辑卡片（不写源码）。
    pub(crate) fn close_image_edit(&mut self, cx: &mut Context<Self>) {
        if self.image_edit.take().is_some() {
            self.image_edit_anchor.set(None);
            cx.notify();
        }
    }

    /// 点击图片本体 → 选中该图：清除文本选区/拖选，选中态独立于指针（移开后编辑
    /// 按钮仍在，画 accent 描边）。再次点击同一图保持选中（不产生文本选区）。
    fn select_image(&mut self, line: usize, cx: &mut Context<Self>) {
        self.selected_image.set(Some(line));
        self.sel_anchor = None;
        self.dragging = false;
        *self.frame.borrow_mut() = None;
        cx.notify();
    }

    /// 删除选中的整张图片行（含换行；末行无换行只删内容）。撤销栈统一
    /// （`apply_edit`）。删除后关闭编辑卡片、清除悬浮/选中态。
    fn delete_selected_image(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_start(line + 1);
        self.selected_image.set(None);
        self.image_edit = None;
        self.hover_image.set(None);
        if end > start {
            self.apply_edit(start..end, "", cx);
        } else {
            self.sel_anchor = None;
            *self.frame.borrow_mut() = None;
            cx.notify();
        }
    }

    /// 保存图片编辑卡片：以输入框值改写 `![alt](src)` → `![新文本](新链接)`。
    /// 写入走 `replace_range`（撤销栈统一），随后关闭卡片。
    pub(crate) fn save_image_edit(&mut self, cx: &mut Context<Self>) {
        let Some(popup) = self.image_edit.as_ref() else {
            return;
        };
        let src_line = popup.src_line;
        let link_text = popup.link.read(cx).value().to_string();
        let text_text = popup.text.read(cx).value().to_string();
        let saved = !link_text.is_empty();
        self.apply_image_line_edit(src_line, text_text, if saved { link_text } else { "".into() }, cx);
        self.close_image_edit(cx);
    }

    /// 程序化改写第 `src_line` 行独立图片的 alt 与 src（`![alt](src)`）。
    /// 空 `src` 视为保留原 src。供图片编辑卡片保存 / 程序化测试调用。
    pub(crate) fn apply_image_line_edit(
        &mut self,
        src_line: usize,
        new_alt: String,
        new_src: String,
        cx: &mut Context<Self>,
    ) {
        let start = self.line_start(src_line);
        let end = self.line_end(src_line).max(start);
        let raw = self.doc.slice_str(start..end);
        let (_, src) = match crate::markdown::parse::parse_image_line(raw.trim()) {
            Some(pair) => pair,
            None => return,
        };
        let src = if new_src.is_empty() { src } else { new_src };
        let indent = raw.len() - raw.trim_start().len();
        let new_line = format!(
            "{}![{}]({})",
            &raw[..indent],
            new_alt.replace(']', "\\]").replace('\n', " "),
            src.replace('(', "\\(").replace(')', "\\)").replace('\n', " ")
        );
        self.apply_edit(start..end, &new_line, cx);
    }

    /// 图片编辑卡片的悬浮浮层（打开时返回 `Some`）：`deferred(anchored(Local))` 锚定在
    /// 目标图片顶部、内容区水平居中。浮层逃逸滚动容器裁剪/命中，随内容滚动跟手（对齐
    /// `ui::menu` 的 Dropdown 浮层约定）。
    fn image_edit_card_element(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use gpui::{
            AnchoredPositionMode, Anchor, Edges, InteractiveElement, IntoElement, ParentElement,
            Styled,
        };
        use crate::ui::button::ButtonVariants as _;
        let popup = self.image_edit.as_ref()?;
        // 卡片几何（**窗口坐标**，供 `anchored(Window)` 定位）。**直接读 paint 期缓存好的
        // 锚点**（`image_edit_anchor`：同代 origin+frame 算出的窗口 TopLeft）——render 期
        // 现算 frame 会读到 `end_selection` 清帧后 `ensure_frame_for_input` 兜底 800 宽
        // 重建的临时帧（与真实宽不同），卡片横向跳一帧（往左闪烁）。锚点未就绪才回退到
        // 逐帧现算；再没有帧则本帧不画卡片（绝不画角落）。
        let anchor = match self.image_edit_anchor.get() {
            Some(a) => a,
            None => {
                let f = self.frame.borrow();
                let f = f.as_ref()?;
                let origin = self.content_origin();
                self.image_edit_anchor_at(origin, f)?
            }
        };
        let (left, top) = (f32::from(anchor.x), f32::from(anchor.y));
        let ed = cx.entity();
        let ed_save = ed.clone();
        let ed_close = ed.clone();
        let colors = crate::ui::theme::Theme::global(cx).colors;
        let link_input = popup.link.clone();
        let text_input = popup.text.clone();
        let card = gpui::div()
            .id("wy-image-edit-card")
            .debug_selector(|| "wy-image-edit-card".into())
            // 点击**卡片外**任意处 → 关闭卡片（控件内点击不外发，不误关）。
            .on_mouse_down_out(move |_: &gpui::MouseDownEvent, _: &mut Window, cx: &mut gpui::App| {
                ed_close.update(cx, |e, cx| e.close_image_edit(cx));
            })
            .w(px(element::IMAGE_EDIT_CARD_W))
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .occlude()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap_y(px(8.))
            .child(
                gpui::div()
                    .text_size(px(13.))
                    .text_color(colors.fg)
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("编辑图片"),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_y(px(4.))
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(colors.fg_muted)
                            .child("链接"),
                    )
                    // 输入框样式：内凹底色 + 圆角描边，标识「可输入」区域（区别于卡片
                    // 表面与纯文字）。聚焦态边框提亮在保存时由卡片统一不变，此处只供样式。
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .h(px(28.))
                            .w_full()
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.bg)
                            .child(crate::ui::input::Input::new(&link_input)),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_y(px(4.))
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(colors.fg_muted)
                            .child("文本"),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .h(px(28.))
                            .w_full()
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.bg)
                            .child(crate::ui::input::Input::new(&text_input)),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_row()
                    .gap_x(px(8.))
                    .justify_end()
                    .child(
                        crate::ui::button::Button::new("wy-image-edit-cancel")
                            .label("取消")
                            .ghost()
                            .h(px(26.))
                            .px(px(12.))
                            .on_click(move |_, _: &mut Window, cx: &mut gpui::App| {
                                ed.update(cx, |e, cx| e.close_image_edit(cx));
                            }),
                    )
                    .child(
                        crate::ui::button::Button::new("wy-image-edit-save")
                            .label("保存")
                            .primary()
                            .h(px(26.))
                            .px(px(12.))
                            .on_click(move |_, _: &mut Window, cx: &mut gpui::App| {
                                ed_save.update(cx, |e, cx| e.save_image_edit(cx));
                            }),
                    ),
            );
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position_mode(AnchoredPositionMode::Window)
                    .anchor(Anchor::TopLeft)
                    .position(gpui::point(px(left), px(top)))
                    .snap_to_window_with_margin(Edges::all(px(8.)))
                    .child(card),
            )
            .with_priority(100)
            .into_any_element(),
        )
    }

    /// 输入命中前确保帧存在（帧被置空后、下一次渲染还没重建时，点击才会落空——
    /// 这正是「展开后立刻再点折叠」偶发失效的来源）。缺帧则当场按当前可视带重建。
    fn ensure_frame_for_input(&mut self, window: &mut Window) -> bool {
        if self.frame.borrow().is_some() {
            return true;
        }
        let width = {
            let w = self.frame.borrow().as_ref().map(|f| f.width).unwrap_or(px(800.));
            if f32::from(w) <= 1. {
                return false;
            }
            w
        };
        let band = self.shape_band().unwrap_or((0., 5000.));
        let frame = element::build_frame(self, window, width, band);
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        true
    }

    /// 撤销（D1）：把当前打开的撤销单元固化入栈，弹出上一单元并沿增量管道打回其
    /// **净逆操作**（不再整体重建模型）；被撤销的单元（带撤销瞬间光标）进重做栈。
    pub(crate) fn undo(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("undo");
        if let Some(u) = self.open_unit.take() {
            self.push_undo_unit(u);
        }
        if let Some(mut u) = self.undo_stack.pop() {
            // 重做恢复位定格为「按下撤销瞬间」的光标（同 `editor/undo.rs`）。
            u.forward_caret = self.caret;
            self.redo_stack.push(u.clone());
            let target = u.caret_before;
            self.apply_raw_edit(u.range.clone(), &u.delete, target, cx);
        }
        self.last_edit = None;
    }

    /// 重做（D1）。
    pub(crate) fn redo(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("redo");
        if let Some(u) = self.open_unit.take() {
            self.push_undo_unit(u);
        }
        if let Some(u) = self.redo_stack.pop() {
            self.undo_stack.push(u.clone());
            let target = u.forward_caret;
            // 前向：当前文档在 `[range.start, range.start+delete.len())` 处是 `delete`，
            // 用 `insert` 换回（撤销的逆）。
            let r = u.range.start..(u.range.start + u.delete.len());
            self.apply_raw_edit(r, &u.insert, target, cx);
        }
        self.last_edit = None;
    }

    /// A3：只读右栏预览的**增量桥**——把左栏源码编辑区同一次增删（`CodeEditor::take_last_edit`
    /// 的 `(start, old_end, head)` 轨迹）重放过来。复用 undo/redo 的增量管线
    /// `apply_raw_edit`（`Doc::edit` 增量块扫描 + Pass1 三段式 + derived 小修补 + 单块整形
    /// 作废），**不写撤销栈**（预览不参与撤销）、光标保持原位（只读光标不可见勿动）、
    /// **绕过 `editable` 门控**（桥是宿主同步写，非用户编辑；用户的一切编辑入口仍被 A1
    /// 短路）。同步保持：装配源与编辑区同源，此后逐键重放 → 预览恒等于编辑区文本。
    pub fn preview_apply_edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let len = self.len();
        let r = range.start.min(len)..range.end.min(len);
        if r.start == r.end && text.is_empty() {
            return;
        }
        let cur = self.caret;
        self.apply_raw_edit(r, text, cur, cx);
    }

    /// 撤销/重做共用增量编辑：与 `replace_range` 完全同款（`Doc::edit` 增量块扫描 +
    /// Pass1 三段式重估 + derived 小修补 + 单块整形作废 + 结构判定），**唯二区别**：
    /// 不写撤销单元（本调用由 undo/redo 发起）、光标按目标位放置。UndoUnit 的
    /// `range`/`insert`/`delete` 组合即自带精确坐标，直接复用这套增量管线——
    /// 大文档撤销从「整体重建 6.4MB」降到「一次局部增量编辑」。
    fn apply_raw_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        target_caret: usize,
        cx: &mut Context<Self>,
    ) {
        let len = self.len();
        let r = range.start.min(len)..range.end.min(len);
        if r.start == r.end && text.is_empty() {
            return;
        }
        let _t_rr = crate::prof::begin();
        let removed = self.doc.slice_str(r.clone());
        let edit_line = self.row_col(r.start).0;
        let old_line = self.doc.line_str(edit_line);
        let prev_count = self.doc.block_count();
        let edit_blk = self.doc.block_index_for_line(edit_line);
        let prev_geo = self.doc.block_meta(edit_blk).map(|m| (m.kind, m.src_line, m.aux));
        // 与 `replace_range` 同：剔除被编辑块的旧内容指纹行（内容寻址缓存）。
        let old_cid = frame::block_content_id(&self.doc, edit_blk);
        self.doc.edit(r.clone(), text);
        let cross_newline = text.contains('\n') || removed.contains('\n') || text.contains('\r');
        let breaks_fence = text.contains("```") || text.trim_start().starts_with('|');
        let count_same = self.doc.block_count() == prev_count;
        let geo_same = match (self.doc.block_meta(edit_blk), prev_geo) {
            (Some(nm), Some(om)) => nm.kind == om.0 && nm.src_line == om.1 && nm.aux == om.2,
            _ => false,
        };
        let structural = cross_newline || breaks_fence || !count_same || !geo_same;
        self.invalidate_after_edit(edit_line, edit_blk, prev_count, structural, old_cid, &removed, text);
        self.update_derived_after_edit(edit_line, &old_line, structural, text, &removed);
        self.caret = target_caret.min(self.len());
        self.sel_anchor = None;
        self.marked = None;
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        crate::prof::end("wy.replace_range", _t_rr);
        cx.emit(WyEvent::Changed);
        cx.notify();
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        // 选中的图片对象：Backspace 删除整张图片行（含换行；记撤销栈）。
        if let Some(line) = self.selected_image.get() {
            self.delete_selected_image(line, cx);
            return;
        }
        crate::prof::op_start("backspace");
        // 有选区 → 直接删选区（等价退格删除选中文字）。
        if let Some(range) = self.selection() {
            return self.replace_range(range, "", cx);
        }
        let at = self.caret.min(self.len());
        if at == 0 {
            return;
        }
        // 围栏/单元格边界守卫：退格不得删到代码块首内容行行首（会吃掉 ``` 围栏的
        // `\n`）或表格单元格内容起点（会吃掉 `| `）。
        let (bl, bcol) = self.row_col(at);
        if self.backspace_blocked(bl, bcol) {
            return;
        }
        // 上一字符起点（rope char boundary 回溯，O(chars)，不复制前缀整串）。
        let prev = self.doc.rope().floor_char_boundary(at - 1);
        self.replace_range(prev..at, "", cx);
    }

    /// 退格是否被边界守卫拦截（代码首内容行行首 / 表格单元格内容起点 / 图片 alt 起点）。
    fn backspace_blocked(&self, line: usize, col: usize) -> bool {
        match self.region_content(line) {
            Some(BlockRegion::Code { lo, .. }) => line == lo && col == 0,
            Some(BlockRegion::Table { .. }) => {
                self.table_cell_bounds(line, col).is_some_and(|(s, _)| col <= s)
            }
            None => {
                // 图片行 alt：起点即 `![` 边界，退格不得吃到图片标记（行会变非图片）。
                self.image_alt_segment(line).is_some_and(|(lo, _)| col <= lo)
            }
        }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        // 选中的图片对象：Delete 删除整张图片行（含换行；记撤销栈）。
        if let Some(line) = self.selected_image.get() {
            self.delete_selected_image(line, cx);
            return;
        }
        crate::prof::op_start("delete");
        if let Some(range) = self.selection() {
            return self.replace_range(range, "", cx);
        }
        let at = self.caret.min(self.len());
        if at >= self.len() {
            return;
        }
        // 围栏/单元格边界守卫：删除不得删到代码块末内容行行尾（会把闭合围栏并进
        // 内容行）或表格单元格内容尾（会吃掉 `|`）。
        let (dl, dcol) = self.row_col(at);
        if self.delete_blocked(dl, dcol) {
            return;
        }
        // 下一字符尾（rope char boundary，O(chars)）。
        let end = self.doc.rope().ceil_char_boundary(at + 1);
        self.replace_range(at..end, "", cx);
    }

    /// 删除是否被边界守卫拦截（代码末内容行行尾 / 表格单元格内容尾 / 图片 alt 尾）。
    fn delete_blocked(&self, line: usize, col: usize) -> bool {
        match self.region_content(line) {
            Some(BlockRegion::Code { hi, .. }) => {
                let len = self.line_end(line) - self.line_start(line);
                line == hi && col >= len
            }
            Some(BlockRegion::Table { .. }) => {
                self.table_cell_bounds(line, col).is_some_and(|(_, e)| col >= e)
            }
            None => {
                // 图片行 alt：尾即 `](` 边界，删除不得吃到闭合中括号（行会变非图片）。
                self.image_alt_segment(line).is_some_and(|(_, hi)| col >= hi)
            }
        }
    }

    fn newline(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("newline");
        // 表格内换行会拆坏表结构（MVP 先 no-op）；代码块内换行合法（分行）。
        // 图片行 alt（`![` 之后到 `](` 之前）内换行会拆坏图片语法 → 同样 no-op
        // （光标在 alt 内或未越过 alt 尾；越过整行尾（`)(src)` 之后）才放行）。
        let (nl, col) = self.row_col(self.caret);
        if matches!(self.region_content(nl), Some(BlockRegion::Table { .. }))
            || self.image_alt_segment(nl).is_some_and(|(_lo, hi)| col <= hi)
        {
            return;
        }
        let range = self.selection().unwrap_or(self.caret..self.caret);
        self.replace_range(range, "\n", cx);
    }

    /* ---------- 光标移动 ---------- */

    fn move_left(&mut self) {
        let (line, col) = self.row_col(self.caret);
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if col > lo {
                // 段内退一字符（rope char boundary，不复制前缀整串）。
                self.caret = self
                    .doc
                    .rope()
                    .floor_char_boundary(self.caret.saturating_sub(1))
                    .max(seg_start);
            } else if self.caret >= seg_start && self.caret < seg_end {
                // 段首（光标坐在标记后/代码内容行行首）→ 回上一**可视**行段尾
                // （跳过围栏行，光标绝不落进只读围栏）。
                self.caret = if line > 0 {
                    let p = self.prev_render_line(line - 1);
                    let e = self.line_end(p);
                    self.editable_segment(p)
                        .map_or(self.line_start(p), |(_l, h)| self.line_start(p) + h)
                        .min(e)
                        .max(self.line_start(p))
                } else {
                    0
                };
            }
        } else if line > 0 {
            let p = self.prev_render_line(line - 1);
            self.caret = self.line_end(p).max(self.line_start(p));
        } else if self.caret > 0 {
            self.caret -= 1;
        }
        self.moved();
    }

    fn move_right(&mut self) {
        let (line, _col) = self.row_col(self.caret);
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if self.caret < seg_end {
                let n = self.doc.rope().ceil_char_boundary(self.caret + 1);
                self.caret = n.min(seg_end).max(seg_start + 1);
            } else {
                // 段尾 → 下一**可视**行段首（跳过围栏行）。
                self.caret = self.next_segment_start(line);
            }
        } else {
            self.caret = self.next_segment_start(line);
        }
        self.moved();
    }

    fn next_segment_start(&self, line: usize) -> usize {
        let total = self.doc.line_starts().len();
        if line + 1 < total {
            // 下一行是围栏（代码块边界）→ 跳到围栏之后的内容/段落行段首。
            let n = if self.is_fence_line(line + 1) {
                self.next_render_line(line + 1)
            } else {
                line + 1
            };
            self.editable_segment(n)
                .map_or(self.line_end(n), |(lo, _)| self.line_start(n) + lo)
        } else {
            self.len()
        }
    }

    fn move_up(&mut self) {
        let (line, _) = self.row_col(self.caret);
        self.caret = if line > 0 {
            self.place_in_line(self.up_target(line), self.visual_col())
        } else {
            0
        };
        self.moved();
    }

    /// 光标向上移动的落点行（块内逐内容行；跨围栏/块边进出）。
    fn up_target(&self, line: usize) -> usize {
        match self.region_content(line) {
            // 代码/表格内容行内部：上一内容行；已是块首内容行 → 跳出到上一可视行。
            Some(BlockRegion::Code { lo, .. }) if line > lo => line - 1,
            Some(BlockRegion::Table { dlo, .. }) if line > dlo => line - 1,
            Some(_) | None => self.prev_render_line(line - 1),
        }
    }

    fn move_down(&mut self) {
        let (line, _) = self.row_col(self.caret);
        let total = self.doc.line_starts().len();
        self.caret = if line + 1 < total {
            self.place_in_line(self.down_target(line), self.visual_col())
        } else {
            self.len()
        };
        self.moved();
    }

    /// 光标向下移动的落点行（块内逐内容行；跨围栏/块边进出）。
    fn down_target(&self, line: usize) -> usize {
        match self.region_content(line) {
            // 代码/表格内容行内部：下一内容行；已是块末内容行 → 跳出到下一可视行。
            Some(BlockRegion::Code { hi, .. }) if line < hi => line + 1,
            // 表头 → 跳过分隔行，进入首数据行。
            Some(BlockRegion::Table { header, .. }) if line == header => {
                self.next_render_line(line + 1)
            }
            Some(BlockRegion::Table { dhi, .. }) if line < dhi => line + 1,
            Some(_) | None => {
                let nxt = line + 1;
                if self.is_fence_line(nxt) {
                    self.next_render_line(nxt)
                } else {
                    nxt
                }
            }
        }
    }

    /// 光标行内“可视列”（段内渲染列）——上下保持列。
    fn visual_col(&self) -> usize {
        let (line, col) = self.row_col(self.caret);
        if let Some((lo, _)) = self.editable_segment(line) {
            col.saturating_sub(lo)
        } else {
            0
        }
    }

    /// 目标行按“可视列”落位（限段）。
    fn place_in_line(&self, line: usize, col: usize) -> usize {
        let ls = self.line_start(line);
        let le = self.line_end(line);
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = ls + lo;
            let seg_end = ls + hi;
            let cap = (seg_end.saturating_sub(seg_start + 1)).min(80);
            (seg_start + col.min(cap)).min(seg_end).min(le)
        } else {
            le.max(ls)
        }
    }

    fn moved(&mut self) {
        // 方向键移动 → 打断撤销合并组。
        self.last_edit = None;
        // 表格内容行内：光标钳到最近单元格内容区间（不落在管道/空白上，避免输入破坏表）。
        let (line, col) = self.row_col(self.caret);
        if let Some(c) = self.clamp_caret_table(line, col) {
            self.caret = c;
        }
        *self.frame.borrow_mut() = None;
    }
}

/* ---------- 键盘 / IME 输入 ---------- */

/// 键上下文（Render 的 div 挂载；键位由 [`bind_keys`] 装配）。
pub const CONTEXT: &str = "wy-editor";

macro_rules! wy_action {
    ($name:ident, $s:literal) => {
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name;
        impl gpui::Action for $name {
            fn boxed_clone(&self) -> Box<dyn gpui::Action> {
                Box::new(self.clone())
            }
            fn partial_eq(&self, other: &dyn gpui::Action) -> bool {
                (other as &dyn std::any::Any).downcast_ref::<Self>().is_some()
            }
            fn name(&self) -> &'static str {
                $s
            }
            fn name_for_type() -> &'static str {
                $s
            }
            fn build(_: serde_json::Value) -> anyhow::Result<Box<dyn gpui::Action>> {
                Ok(Box::new(Self))
            }
        }
    };
}
wy_action!(Left, "wy-left");
wy_action!(Right, "wy-right");
wy_action!(Up, "wy-up");
wy_action!(Down, "wy-down");
wy_action!(Backspace, "wy-backspace");
wy_action!(Delete, "wy-delete");
wy_action!(Enter, "wy-enter");
wy_action!(SelectAll, "wy-select-all");
wy_action!(Copy, "wy-copy");
wy_action!(Cut, "wy-cut");
wy_action!(Paste, "wy-paste");
wy_action!(Undo, "wy-undo");
wy_action!(Redo, "wy-redo");

/// 装配键位（入口：`main.rs` 与其它键位一并安装）。
pub fn bind_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(CONTEXT)),
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(CONTEXT)),
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
    ])
}


impl Focusable for WyEditor {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl gpui::EventEmitter<WyEvent> for WyEditor {}

impl gpui::Render for WyEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{CursorStyle, InteractiveElement, ParentElement, Styled};
        // 图片编辑卡片浮层：在元素树根上**最后**叠加（最后绘制、命中最靠前），
        // 锚定在目标图片旁渲染（打开时才有）。卡片几何取自 paint 期缓存的
        // `image_edit_anchor`（同代 origin+frame），不在此现算帧（避免读到 ensure
        // 兜底 800 宽重建的临时帧而横跳）。
        let image_edit_overlay = self.image_edit_card_element(window, cx);
        let mut root = gpui::div()
            .relative()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Left, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_left()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Right, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_right()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Up, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_up()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Down, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_down()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Backspace, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.backspace(cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Delete, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.delete(cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Enter, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.newline(cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &SelectAll, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.select_all()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Copy, window: &mut Window, cx: &mut Context<WyEditor>| {
                    this.copy(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Cut, window: &mut Window, cx: &mut Context<WyEditor>| {
                    this.cut(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Paste, window: &mut Window, cx: &mut Context<WyEditor>| {
                    this.paste(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Undo, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.undo(cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Redo, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.redo(cx)
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this: &mut WyEditor, ev: &gpui::MouseDownEvent, window: &mut Window, cx: &mut Context<WyEditor>| {
                    this.on_mouse_down(ev, window, cx)
                }),
            )
            .on_mouse_move(
                cx.listener(|this: &mut WyEditor, ev: &gpui::MouseMoveEvent, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.on_mouse_move(ev, cx)
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this: &mut WyEditor, ev: &gpui::MouseUpEvent, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.on_mouse_up(ev, cx)
                }),
            )
            .child(element::WyEditorElement {
                editor: cx.entity(),
            });
        if let Some(overlay) = image_edit_overlay {
            root = root.child(overlay);
        }
        root
    }
}
#[cfg(test)]
impl WyEditor {
    pub(crate) fn test_theme_fg(&self) -> gpui::Hsla {
        self.theme.text
    }
    pub(crate) fn test_theme_code_bg(&self) -> gpui::Hsla {
        self.theme.code_bg
    }
    pub(crate) fn test_theme_link(&self) -> gpui::Hsla {
        self.theme.link
    }
    pub(crate) fn test_theme_is_dark(&self) -> bool {
        self.theme.is_dark
    }
}
