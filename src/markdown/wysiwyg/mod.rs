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
//!   范式）；行内样式来自 `DocRow.runs`（`parser::RunKind`）。
//! - 非文本块已按类型渲染：代码围栏（等宽 + 语法高亮 + 圆角面板，**内容行可编辑**）、
//!   GFM 表格（网格，列宽自适应）、引述（灰底 + 左侧指示条）、独立图片行
//!   （`AnyImageCache` 异步加载，占位/失败回退 alt）；样式统一抄自预览
//!   [`super::view::style::StyleParams`]。
//! - 输入：普通文本/IME 组合命中（`marked` 区间追踪，修复重复串）、点击定位、
//!   鼠标拖选 + Ctrl+C/X/V/A（选区替换/删除）。
//! - 现状声明（留待逐步补齐）：代码块内容行、表格**单元格**已可编辑（代码走块级
//!   `byte_map` + `multi_offsets`；表格走逐行逐格的 `DocRow.table_rows` → `table_edit`
//!   单元格排版，含对齐位移、渲染列↔源字节映射；导航跳过只读围栏/分隔行，退格/
//!   删除有单元格边界守卫，表格内回车 no-op）。图片 alt 仍只读（可复用文本行映射）。
//!   选区面板高亮尚未覆盖表格单元格（源码级复制/删除已可用）。仅作「预览编辑
//!   测试」入口，验证「本项目流程能编辑 + 正确映射回源」。

mod element;
mod math_typeset;

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Context, EntityInputHandler, FocusHandle, Focusable, Font, Hsla, Pixels, RenderImage, SharedString,
    UTF16Selection, Window, font, px,
};

use super::doc::Doc;
use crate::markdown::recognition::{self, LinkHit};
use crate::markdown::inline_style;
use crate::markdown::view::style::StyleParams;

/// 编辑器变更事件（宿主订阅，内容镜像回源码编辑区/保存流程）。
#[derive(Clone, PartialEq, Debug)]
pub enum WyEvent {
    Changed,
    /// 点击内部链接（wiki/tag/block-ref，`[[…]]`/`#tag`/`((id))`）→ 宿主打开对应页面。
    OpenPage(String),
}

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
    /// [`crate::markdown::view::style::typography_for_size`]，不在此重复定义）。
    pub(crate) fn set_font_size(&mut self, size: Pixels) {
        self.font_size = size;
        let (pl, cs, cl) =
            crate::markdown::view::style::typography_for_size(f32::from(size).max(9.));
        self.para_line = pl;
        self.code_size = cs;
        self.code_line = cl;
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
    /// 第 n 级标题字号（预览 `heading_metrics`：h1=20 h2=18 h3/4=16 其余 15）。
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

/// 撤销/重做快照（整份源文本 + 光标）。
#[derive(Clone)]
struct Snapshot {
    text: SharedString,
    caret: usize,
}

/// 项目管线的所见即所得编辑器：`Doc` 为唯一数据源，`caret` 是**源字节偏移**。
pub struct WyEditor {
    content: SharedString,
    doc: Doc,
    focus: FocusHandle,
    caret: usize,
    theme: WyTheme,
    /// 本帧行网格（measure→prepaint→paint 复用；**窗口化**：仅带内块被真正 shape，
    /// 带外块用 `BlockMeta` 估算高度占位，避免大文档整篇每帧 shape 卡死）。
    frame: RefCell<Option<std::rc::Rc<element::Frame>>>,
    /// 本帧元素在窗口中的布局 bounds（paint 写入；鼠标命中时全局坐标 → 元素局部）。
    last_bounds: Cell<Option<gpui::Bounds<Pixels>>>,
    /// 内容代数：任何文本变更自增；`request_layout` 用它判断帧缓存是否仍有效。
    content_gen: Cell<u64>,
    /// 可视带（窗口坐标→元素局部 y，量程化）：prepaint 用 `bounds.origin.y` +
    /// `viewport_size` 推算并发布，measure 用上一帧的带做窗口化 shape。
    shape_band: Cell<Option<(f32, f32)>>,
    /// 全篇**基础布局**缓存：`(代, Rc<RefCell<BlockLayout>>)`——rolling/带移动与
    /// **非结构性编辑**都复用（只对受影响块做 Fenwick 点更新），代 ≠ 内容代：
    /// 由 `struct_ver`/图片/数学/宽度组成。结构性变更（换行/围栏/表格等改了块数或
    /// 块序）才整篇重建。`set_theme`/`set_font_size` 把代置失效位强制重算。
    block_layout: RefCell<Option<(u64, Rc<std::cell::RefCell<element::BlockLayout>>)>>,
    /// 内容**结构版本**：`replace_range` 里结构性变更（跨行、代码/表格边界）才递增——
    /// 参与基础布局代；非结构性编辑不递增（基础布局跨键复用，只 patch 受影响块高）。
    struct_ver: Cell<u64>,
    /// 内容**派生版本**——`derived()` 缓存键：结构性编辑、编辑行 region 签名变化才
    /// 递增；纯文本键入（非结构、签名不变、不在数学块内）不递增 → `derived` Rc
    /// 跨键复用，不再每键对全篇 `math_regions`/`alert_*` 重扫（7MB/结构密集文档
    /// 「输入卡死」的主因，旧实现每键 2 次全篇扫描 ≈1s）。
    regions_ver: Cell<u64>,
    /// 数学相关版本——`sync_math` 的守卫键：仅当编辑**确实**可能改变公式内容时递增
    /// （`$`/围栏 编辑、数学块 source 修补、含 `$` 行单行修补）；纯文本键入不递增 →
    /// paint 不再每键全篇重扫行内公式。
    math_ver: Cell<u64>,
    /// 行内公式清单 `(源码行, 槽键)`——`sync_math` 直接消费（迭代槽键、不再全篇
    /// 重扫）。随编辑增量维护：结构/签名变化整表重建、行内含 `$` 的行被编辑单行重扫。
    inline_math: Rc<RefCell<Vec<(usize, SharedString)>>>,
    /// 标题折叠视图态版本：`toggle_heading_fold_at_line` 翻转 `folded_headings` 时递增
    /// （纳入 `fold_plan` 缓存键，滚动不带折叠态重算）。
    fold_ver: Cell<u64>,
    /// 折叠/重排计划缓存：`(key, Rc<FoldPlan>)`——与可视带无关（内容、caret reveal、
    /// 数学就绪、宽度、结构版本决定），滚动/带移动复用；build_frame 每档不再重建
    /// ~万行 Hash 集合 + 逐块二分（大文档滚动「卡顿明显」：~40ms/档）。
    fold_plan: RefCell<Option<(u64, Rc<element::FoldPlan>)>>,
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
    /// 图片异步加载缓存（gpui `ImageAssetLoader` 实体；命中返回 `Arc<RenderImage>`）。
    image_cache: gpui::Entity<gpui::RetainAllImageCache>,
    /// 独立图片行 src → 逻辑显示尺寸 `(w, h)`（按加载后的宽高比换算）。
    /// `None`(外层) = 尚未尝试加载；`Some(None)` = 加载失败（一直显示 alt 兜底）；
    /// `Some(Some(..))` = 已加载。供 shape（假布局尺寸）与 paint（占位）共用。
    image_sizes: RefCell<std::collections::HashMap<SharedString, Option<(f32, f32)>>>,
    /// 图片加载完成代数：任何图片从「占位尺寸」变成「真实尺寸」自增——
    /// 纳入帧缓存键，新图到达后必须重建整帧（否则缓存总高还是旧尺寸）。
    images_gen: Cell<u64>,
    /// LaTeX → 排版结果缓存（参考 zorite `<MathStore>` 按公式源码做键）。
    /// `Loading` 排版中 / `Ready` 就绪（位图 + 逻辑 px 尺寸）/ `Failed` 失败（回退 Unicode）。
    math_slots: Rc<RefCell<std::collections::HashMap<SharedString, MathSlot>>>,
    /// 排版文字色；主题切换 → 清缓存重排（同 zorite `MathStore::set_color`）。
    math_color: RefCell<Option<Hsla>>,
    /// 最近一次已扫描的内容代数（`sync_math` 挡同代重复扫描）。
    math_scanned_gen: Cell<u64>,
    /// 排版代数：任何公式 Loading→Ready/Failed 自增，纳入帧缓存键（同 `images_gen`）。
    math_gen: Cell<u64>,
    /// 图片相对路径解析的基准目录（打开文件时由应用传入其父目录，与预览一致）。
    base_dir: Option<std::path::PathBuf>,
    /// IME 组合（marked text）的源字节区间。空 = 无组合进行中。
    /// 关键：组合的每次更新都**替换**这段（而非追加到后面）——不追踪它就会把
    /// 上一次组合的字符重复插入（输入“你好”→ “nnini'hni'hao你好”的坏字符）。
    marked: Option<Range<usize>>,
    /// 文本选区的固定锚点（源字节）。`Some(anchor)` 时选区 = `anchor..caret`（caret
    /// 为活动端）；`None` = 无选区（仅光标）。拖拽/ Shift+方向 产生，Ctrl+C/A/V 消费。
    sel_anchor: Option<usize>,
    /// 鼠标左键拖选进行中（on_mouse_move 延伸选区）。
    dragging: bool,
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
    /// 撤销栈（旧状态 → pop 后恢复）；重做栈。整份源文本快照（简单可靠，文档不大）。
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
}

/// 可编辑块区域（与块级编辑映射配套；围栏行不可落点）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockRegion {
    /// 代码围栏：内容行 `lo..=hi` 连续；围栏行 = `lo-1`（开）、`hi+1`（闭）。
    Code { lo: usize, hi: usize },
    /// GFM 表格：表头行 + 数据行 `dlo..=dhi`；分隔行（围栏）= `dlo-1`。
    Table { header: usize, dlo: usize, dhi: usize },
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

/// 基础布局代：`(struct_ver, igen, mgen, shaping 宽)` 折成一个 u64——**结构性内容**
/// /图片/数学/宽度 任一变 → 整篇重建；非结构性编辑不改 `struct_ver` → 布局跨键复用，
/// 只做 Fenwick 点更新（编辑每键省掉旧实现在内容代变化时对全篇的估计重扫 ~400ms）。
/// 纯算术混淆（不做 SipHash：15.9 万块逐块键构造会吃掉 ~200ms）。
pub(crate) fn height_epoch(struct_ver: u64, igen: u64, mgen: u64, width_bits: u32) -> u64 {
    let a = struct_ver ^ igen.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    let b = (width_bits as u64).wrapping_mul(0x517C_C1B7_2722_0A95);
    let x = a ^ b.rotate_left(32);
    x ^ x >> 33
}

/// 一次「内容派生」的全篇扫描结果。纯 `content` 函数 → 按 `content_gen` 缓存，滚动 /
/// 可视带移动（内容不变）直接复用 `Rc`，**不重扫全篇**——大文档每帧卡顿的主因之一
/// （`math_regions` 7MB 全篇扫描 ~400ms、`alert_*` ~30ms，随每帧 paint / 每档 rebuild
/// 重复执行）。
pub(crate) struct WyDerived {
    /// `$$…$$` 数学块区域（首源码行 → 卡片）。
    pub math_regions: Vec<crate::markdown::inline_style::MathRegion>,
    /// 可折叠 callout 区域 `(行区间, 是否折叠)`。
    pub alert_fold_regions: Vec<(Range<usize>, bool)>,
    /// 每个警报区域的 `AlertKind`（marker + body 行），paint 引述条着色用。
    pub alert_region_kinds: std::collections::HashMap<usize, crate::markdown::recognition::AlertKind>,
}

/// 一次全篇的行内公式清单扫描 `(源码行, 槽键)`——`sync_math` 消费，免每次 paint / 每次
/// 数学相关编辑对全篇逐行 `$` 字节扫（旧实现每键一次 ≈400ms）。
fn inline_inventory(content: &str) -> Vec<(usize, SharedString)> {
    let mut out = Vec::new();
    for (ix, line) in content.split('\n').enumerate() {
        for span in crate::markdown::inline_style::inline_math_spans(line) {
            let lx = crate::markdown::inline_style::inline_math_latex(line, &span).trim();
            if !lx.is_empty() {
                out.push((ix, inline_math_key(lx)));
            }
        }
    }
    out
}

fn alert_kind_tag(k: crate::markdown::recognition::AlertKind) -> u8 {
    use crate::markdown::recognition::AlertKind::*;
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
        } else if crate::markdown::inline_style::one_line_math(line).is_some() {
            h.write_u8(3);
        } else if let Some(p) = crate::markdown::inline_style::blockquote_prefix(line) {
            match crate::markdown::recognition::alert_prefix(&line[p..]) {
                Some((kind, _, fold)) => {
                    h.write_u8(4);
                    alert_kind_tag(kind).hash(&mut h);
                    fold.hash(&mut h);
                }
                None => h.write_u8(5),
            }
        } else if let Some(a) = crate::markdown::inline_style::math_align_marker(line) {
            h.write_u8(6);
            use crate::markdown::inline_style::MathAlign::*;
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

impl WyEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ts = window.text_style();
        let fs: Pixels = ts.font_size.to_pixels(window.rem_size());
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(fs)));
        Self {
            content: SharedString::default(),
            doc: Doc::open(SharedString::default()),
            focus: cx.focus_handle(),
            caret: 0,
            theme: WyTheme::from_style(&p, ts.font(), fs),
            frame: RefCell::new(None),
            last_bounds: Cell::new(None),
            content_gen: Cell::new(0),
            shape_band: Cell::new(None),
            block_layout: RefCell::new(None),
            struct_ver: Cell::new(0),
            regions_ver: Cell::new(0),
            math_ver: Cell::new(0),
            inline_math: Rc::new(RefCell::new(Vec::new())),
            fold_ver: Cell::new(0),
            fold_plan: RefCell::new(None),
            pending_patch_blocks: RefCell::new(None),
            math_dollar_cnt: Cell::new(0),
            alert_marker_cnt: Cell::new(0),
            derived: RefCell::new(None),
            image_cache: gpui::RetainAllImageCache::new(cx),
            image_sizes: RefCell::new(std::collections::HashMap::new()),
            images_gen: Cell::new(0),
            math_slots: Rc::new(RefCell::new(std::collections::HashMap::new())),
            math_color: RefCell::new(None),
            math_scanned_gen: Cell::new(0),
            math_gen: Cell::new(0),
            base_dir: None,
            marked: None,
            sel_anchor: None,
            dragging: false,
            code_h_scroll: RefCell::new(std::collections::HashMap::new()),
            code_h_drag: RefCell::new(None),
            table_h_scroll: RefCell::new(std::collections::HashMap::new()),
            table_h_drag: RefCell::new(None),
            folded_headings: RefCell::new(std::collections::HashSet::new()),
            block_regions: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
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
    }

    /// 显式设置字号（应用把全局字号同步进来；变更新帧缓存）。不读窗口 text_style，
    /// 保证 measure/prepaint 两阶段一致。字号只影响正文/代码尺寸，标题按
    /// [`StyleParams::heading_metrics`] 固定；缩放系数见 `typography_for_size`。
    pub fn set_font_size(&mut self, size: Pixels) {
        if self.theme.font_size == size {
            return;
        }
        self.theme.set_font_size(size);
        *self.frame.borrow_mut() = None;
        // 同 `set_theme`：字号变 → 基础布局（px）失效。
        *self.block_layout.borrow_mut() = None;
    }

    /// 设置初始内容。
    pub fn with_text(mut self, text: &str) -> Self {
        self.set_source_text(text);
        self
    }

    /// 当前完整源文本（宿主写回用）。
    pub fn text(&self) -> SharedString {
        self.content.clone()
    }

    /// 聚焦句柄（元素命中/宿主聚焦用）。
    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 覆盖源并重建文档模型。
    pub     fn set_source_text(&mut self, text: &str) {
        self.content = SharedString::from(text);
        self.doc = Doc::open(self.content.clone());
        self.rebuild_block_regions();
        self.preseed_image_sizes();
        self.caret = self.caret.min(self.content.len());
        self.undo_stack.clear();
        self.redo_stack.clear();
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        // 派生扫描的**标记计数器**（全篇一次 O(n)，之后增量维护 O(编辑)）——
        // derived() 预筛用计数，避免每次内容变更对 7MB 全篇 `contains('$')` 重扫
        // （debug 下 ~100ms/键）。
        self.math_dollar_cnt.set(text.bytes().filter(|&b| b == b'$').count());
        self.alert_marker_cnt.set(text.matches("[!").count());
        self.struct_ver.set(0);
        // 全新内容：派生/数学清单/基础布局全量失效（新代 → 下帧全篇重建）。
        *self.block_layout.borrow_mut() = None;
        self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
        if self.math_dollar_cnt.get() > 0 {
            *self.inline_math.borrow_mut() = inline_inventory(text);
        }
        self.math_ver.set(self.math_ver.get().wrapping_add(1));
        *self.pending_patch_blocks.borrow_mut() = None;
    }

    /// 设置图片相对路径解析的基准目录（打开文件时由应用传入其父目录，与预览一致）。
    pub fn set_base_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.base_dir = dir;
        // 基准目录变化 → 重新解析图片尺寸（此前按旧基准解析的作废）。
        self.image_sizes.borrow_mut().clear();
        self.images_gen.set(self.images_gen.get().wrapping_add(1));
        self.preseed_image_sizes();
        *self.frame.borrow_mut() = None;
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
    /// 从原始源码行直接取 src（`parser::parse_image_line`），不物化 [`DocRow`]——
    /// 逐块 `rows_in_block_window(i, i)` 会命中 `parse_block_window` 的「末块含尾
    /// 换行 ≠ 块数」边界（debug 断言），且为读个 src 物化整行纯属浪费。
    fn preseed_image_sizes(&self) {
        let n = self.doc.block_count();
        for i in 0..n {
            let Some(meta) = self.doc.block_meta(i) else {
                continue;
            };
            if meta.kind != crate::markdown::parser::RowKind::Image {
                continue;
            }
            let line = self.doc.line_str(meta.src_line as usize);
            let Some((_, src)) = crate::markdown::parser::parse_image_line(line.trim()) else {
                continue;
            };
            let key: SharedString = src.clone().into();
            if self.image_sizes.borrow().contains_key(&key) {
                continue;
            }
            let resolved = self.resolve_image_src(&src);
            if !resolved.starts_with("http://")
                && !resolved.starts_with("https://")
                && !resolved.starts_with("file://")
            {
                if let Some((w, h)) = crate::markdown::view::dimensions::image_dimensions(
                    std::path::Path::new(&resolved),
                ) {
                    self.image_sizes
                        .borrow_mut()
                        .insert(key, Some((w as f32, h as f32)));
                }
            }
        }
    }

    /// 内部数据（元素读取）。
    pub(crate) fn content(&self) -> &str {
        self.content.as_ref()
    }
    pub(crate) fn doc(&self) -> &Doc {
        &self.doc
    }
    pub(crate) fn caret(&self) -> usize {
        self.caret
    }

    /// 直接放置光标（源字节偏移）。命中/测试/编程式定位共用。
    pub(crate) fn set_caret(&mut self, off: usize) {
        self.caret = off.min(self.content.len());
        self.sel_anchor = None;
        *self.frame.borrow_mut() = None;
    }

    /* ---------- 选区 ---------- */

    /// 当前选区（归一化源字节区间）；无选区或零宽返回 `None`。
    pub(crate) fn selection(&self) -> Option<Range<usize>> {
        match self.sel_anchor {
            Some(a) if a != self.caret => {
                let s = a.min(self.caret).min(self.content.len());
                let e = a.max(self.caret).min(self.content.len());
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
            .map(|r| self.content.as_ref()[r].to_string())
            .unwrap_or_default()
    }

    /// 鼠标按下：锚点 = 光标处（开始拖选）。
    pub(crate) fn begin_selection_at(&mut self, off: usize) {
        let off = off.min(self.content.len());
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
        let off = off.min(self.content.len());
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
        self.caret = self.content.len();
        *self.frame.borrow_mut() = None;
    }

    /// 复制：写入平台剪贴板（由 `on_action(Copy)` 装配）。
    fn copy(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.selected_text()));
    }

    /// 剪切：复制 + 删除选区。
    fn cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(window, cx);
        let range = self.selection().unwrap_or(self.caret..self.caret);
        self.replace_range(range, "", cx);
    }

    /// 粘贴：用剪贴板文本替换选区（若为空则插入光标处）。
    fn paste(&mut self, _: &mut Window, cx: &mut Context<Self>) {
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
        let r = match range {
            Some(r) => range_from_utf16(self.content.as_ref(), r),
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
        let byte_range = match range {
            Some(r) => range_from_utf16(self.content.as_ref(), r),
            None => self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection().unwrap_or(self.caret..self.caret)),
        };
        self.replace_range(byte_range.clone(), new_text, cx);
        if new_text.is_empty() {
            // 组合被取消：清掉 marked，光标回组合起点。
            self.marked = None;
            self.caret = byte_range.start.min(self.content.len());
        } else {
            self.marked = Some(
                byte_range.start..byte_range.start + new_text.len().min(self.content.len() - byte_range.start),
            );
            // `new_selected_range` 是**相对 new_text 起点**的 UTF-16 偏移，不是文档
            // 绝对偏移（AppKit setMarkedText 的 selectedRange 语义）。
            let caret = match new_selected_range {
                Some(sel) => byte_range.start + offset_from_utf16(new_text, sel.start).min(new_text.len()),
                None => byte_range.start + new_text.len(),
            };
            self.caret = caret.min(self.content.len());
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
        // 交互性命中（任务框/折叠箭头/链接）单双击都响应，展开→立即再点也能折叠回。
        if !ev.modifiers.shift && !ev.modifiers.control {
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
                        let _ = cx.open_url(&u);
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
            if dbl {
                if let Some(src) = element::inline_math_hit_at(self, f, ev.position) {
                    drop(marker);
                    self.focus.focus(window, cx);
                    self.set_caret(src.start + 1);
                    cx.notify();
                    return;
                }
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
        // 代码/表格横向滚动条拖动优先（不参与选区拖选）。
        if self.code_h_drag.borrow().is_some() {
            self.code_h_drag_apply(ev.position, cx);
            return;
        }
        if self.table_h_drag.borrow().is_some() {
            self.table_h_drag_apply(ev.position, cx);
            return;
        }
        if !self.dragging {
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
        let p_local = f32::from(pos.x) - origin_x;
        let travel = (bar_w - thumb_w).max(1.);
        // 滑块中心对准点击点：center = p_local → thumb_x = p_local − thumb_w/2。
        let rel = ((p_local - bar_x - thumb_w / 2.0) / travel).clamp(0., 1.);
        drop(marker);
        self.set_code_h_scroll(blk, rel * max_scroll);
    }

    /// 把指针位置应用到正在拖动的代码块横向滚动条（保持**抓取点相对滑块左缘**：
    /// `thumb_x = px - grab`，按下瞬间 scroll 不变、拖动时滑块钉住抓取点 1:1 跟随）。
    fn code_h_drag_apply(&self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((blk, grab)) = self.code_h_drag.borrow().clone() else {
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
        let p_local = f32::from(pos.x) - origin_x;
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
        let p_local = f32::from(pos.x) - origin_x;
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - thumb_w / 2.0) / travel).clamp(0., 1.);
        drop(marker);
        self.set_table_h_scroll(blk, rel * max_scroll);
    }

    /// 把指针位置应用到正在拖动的表格横向滚动条（抓取点相对滑块左缘，1:1 跟随）。
    fn table_h_drag_apply(&self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((blk, grab)) = self.table_h_drag.borrow().clone() else {
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
        let p_local = f32::from(pos.x) - origin_x;
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - grab) / travel).clamp(0., 1.);
        drop(marker);
        self.set_table_h_scroll(blk, rel * max_scroll);
        cx.notify();
    }

    /// 鼠标松开：结束拖选 / 代码块 & 表格横向滚动条拖动。
    pub(crate) fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        self.end_selection();
        *self.code_h_drag.borrow_mut() = None;
        *self.table_h_drag.borrow_mut() = None;
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
    /// 派生版本（`derived()` 缓存键；供回归断言「纯文本键入不重建派生」）。
    pub(crate) fn regions_ver(&self) -> u64 {
        self.regions_ver.get()
    }
    /// 数学重扫版本（`sync_math` 守卫键；供回归断言「纯文本键入不重扫数学」）。
    #[cfg(test)]
    pub(crate) fn math_ver(&self) -> u64 {
        self.math_ver.get()
    }
    /// 行内公式清单快照（增量维护；供回归断言单行修补正确性）。
    #[cfg(test)]
    pub(crate) fn inline_math_entries(&self) -> Vec<(usize, SharedString)> {
        self.inline_math.borrow().clone()
    }

    /// 标题折叠视图态版本（`fold_plan` 缓存键组成部分）。
    pub(crate) fn fold_ver(&self) -> u64 {
        self.fold_ver.get()
    }

    /// 取走待修补（重估高度）的块下标（编辑后一帧用；无则 None）。
    pub(crate) fn take_pending_patch_blocks(&self) -> Option<Vec<usize>> {
        self.pending_patch_blocks.borrow_mut().take()
    }
    /// 校验基础布局命中当前（代, 块数）：命中返回 `(false, Rc)` 供复用；未命中
    /// 建出空布局（build_frame 随即 O(n) 填估算/或 patch）并缓存，返回 `(true, Rc)`。
    pub(crate) fn ensure_block_layout(
        &self,
        n_blocks: usize,
        epoch: u64,
    ) -> (bool, Rc<std::cell::RefCell<element::BlockLayout>>) {
        if let Some((e, l)) = &*self.block_layout.borrow() {
            if *e == epoch && l.borrow().n() == n_blocks {
                return (false, l.clone());
            }
        }
        let l = Rc::new(std::cell::RefCell::new(element::BlockLayout::new(
            vec![0.0; n_blocks],
            vec![0u32; n_blocks],
        )));
        *self.block_layout.borrow_mut() = Some((epoch, l.clone()));
        (true, l)
    }

    /// 折叠/重排计划（缓存）：`math_start` / `math_body` / `fold_lines` / `deltas`——
    /// 全部**与可视带无关**（由内容、caret reveal、数学就绪、宽度、结构版本决定），
    /// 滚动/带移动复用同一 `Rc`。旧实现把这些放在 `build_frame` 里每档重算（结构密集
    /// 大文档 ~15k 行 Hash 集合 + 逐块二分 ≈40ms/档 → 滚动「卡顿明显」）。
    /// `pass1_delta = 基础布局本档是否刚整篇重建`（重建过的 epoch ≠ 缓存键 → 自会
    /// 重建；此处仅防御性传 `base` 供 deltas 读当前高度）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fold_plan(
        &self,
        base: Rc<std::cell::RefCell<element::BlockLayout>>,
        epoch: u64,
        caret_line: usize,
    ) -> Rc<element::FoldPlan> {
        let key = epoch
            ^ self
                .regions_ver()
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (caret_line as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ self.fold_ver().wrapping_mul(0x1656_67B1_9E37_79F9);
        if let Some((k, p)) = &*self.fold_plan.borrow() {
            if *k == key {
                return p.clone();
            }
        }
        let content = self.content.as_ref();
        let derived = self.derived();
        {
            let d = derived.borrow();
            let math_regions = &d.math_regions;
            let mut math_start: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            let mut math_body: std::collections::HashSet<usize> = std::collections::HashSet::new();
            for (idx, reg) in math_regions.iter().enumerate() {
                if caret_line >= reg.range.start && caret_line < reg.range.end {
                    continue; // reveal-on-caret：光标在块内 → 不折叠，显示源码
                }
                math_start.insert(reg.range.start, idx);
                math_body.extend(reg.range.start + 1..reg.range.end);
            }
            // 可折叠 callout（`> [!NOTE]-`）：body 行折叠成 0 高。
            let mut fold_lines: std::collections::HashSet<usize> = std::collections::HashSet::new();
            for (rnge, folded) in &d.alert_fold_regions {
                if !*folded || (caret_line >= rnge.start + 1 && caret_line < rnge.end) {
                    continue;
                }
                fold_lines.extend(rnge.start + 1..rnge.end);
            }
            // 标题折叠（视图本地状态）。
            {
                let folded = self.folded_headings.borrow();
                if !folded.is_empty() {
                    for rnge in inline_style::heading_fold_regions(content, &folded) {
                        if caret_line >= rnge.start + 1 && caret_line < rnge.end {
                            continue;
                        }
                        fold_lines.extend(rnge.start + 1..rnge.end);
                    }
                }
            }
            // 动态高度覆盖（折叠 body→0 高、数学卡片→卡高）。
            let mut dyn_map: std::collections::HashMap<usize, f32> =
                std::collections::HashMap::new();
            {
                let base_br = base.borrow();
                let src_lines = base_br.src_lines();
                let src_len = src_lines.len();
                let block_ix_of = |srcline: usize| -> usize {
                    src_lines
                        .partition_point(|&s| (s as usize) <= srcline)
                        .saturating_sub(1)
                        .min(src_len.saturating_sub(1))
                };
                for &srcline in fold_lines.iter() {
                    let ix = block_ix_of(srcline);
                    dyn_map.insert(ix, -base_br.height(ix));
                }
                for &srcline in math_body.iter() {
                    let ix = block_ix_of(srcline);
                    dyn_map.insert(ix, -base_br.height(ix));
                }
                for (&srcline, &ridx) in math_start.iter() {
                    let ix = block_ix_of(srcline);
                    let card_h =
                        f32::from(element::math_card_height(self, &math_regions[ridx]));
                    dyn_map.entry(ix).or_insert(card_h - base_br.height(ix));
                }
            }
            let mut deltas_vec: Vec<(usize, f32)> = dyn_map.into_iter().collect();
            deltas_vec.sort_unstable_by_key(|&(ix, _)| ix);
            let plan = Rc::new(element::FoldPlan {
                math_start,
                math_body,
                fold_lines,
                deltas: deltas_vec.into_boxed_slice().into(),
            });
            *self.fold_plan.borrow_mut() = Some((key, plan.clone()));
            plan
        }
    }

    /// 内容派生扫描缓存（全篇一遍，按 `regions_ver` 缓存）——`build_frame` / paint 用，
    /// 避免滚动/每帧重复全篇扫描（大文档卡顿主因之一）。**标记计数器预筛**（增量维护，
    /// 见 `replace_range`）：无 `$` → `math_regions` 空；无 `[!` → 两个 alert 扫描空。
    /// 7MB 大文档 `math_regions` 全篇行切分+扫描 ~390ms——**纯文本键入**（region 签名
    /// 不变）不递增 `regions_ver` → 跨键复用 `Rc`，编辑每键省掉这些全篇扫描
    /// （结构密集文档「输入卡死」的根因，旧实现每键重扫 ~1s）。
    pub(crate) fn derived(&self) -> Rc<RefCell<WyDerived>> {
        let ver = self.regions_ver.get();
        if let Some((g, d)) = &*self.derived.borrow() {
            if *g == ver {
                return d.clone();
            }
        }
        let content = self.content.as_ref();
        // 警报区域单遍扫描，两个公开查询由它派生（旧实现 `alert_fold_regions` +
        // `alert_region_kinds` 各扫一遍全篇——重复 O(n)）。
        let alerts = if self.alert_marker_cnt.get() > 0 {
            inline_style::alert_regions(content)
        } else {
            Vec::new()
        };
        let alert_fold_regions = alerts
            .iter()
            .filter_map(|(r, _, fold)| fold.map(|f| (r.clone(), f)))
            .collect();
        let mut alert_region_kinds = std::collections::HashMap::new();
        for (r, k, _) in &alerts {
            for l in r.clone() {
                alert_region_kinds.insert(l, *k);
            }
        }
        let d = Rc::new(RefCell::new(WyDerived {
            math_regions: if self.math_dollar_cnt.get() > 0 {
                inline_style::math_regions(content)
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

    /// 扫描当前内容里的 `$$…$$` 块 + `$…$` 行内公式，对尚未排版的发起离屏排版；
    /// 幂等（已占用槽的不重复发起），主题色变化清缓存。每次 paint 前调一次，
    /// `math_scanned_gen` 挡掉同代重复扫描。块公式消费 `derived()`（缓存），行内公式
    /// 消费增量维护的行内清单——**不再每次全篇逐行重扫**（旧实现每键 2 次全篇扫描）。
    pub(crate) fn sync_math(&mut self, cx: &mut Context<Self>) {
        let color = math_color(&self.theme);
        let color_changed = self.math_color.borrow().as_ref() != Some(&color);
        if color_changed {
            self.math_color.borrow_mut().replace(color);
            self.math_slots.borrow_mut().clear();
        }
        // 守卫键 = `math_ver`：仅数学相关编辑（`$`/围栏、数学 source 修补、含 `$` 行
        // 单行修补）才递增；纯文本键入 / 滚动不重扫。主题色变化无条件重排。
        let mver = self.math_ver.get();
        if !color_changed && self.math_scanned_gen.get() == mver {
            return;
        }
        self.math_scanned_gen.set(mver);

        // 收集块 + 行内公式 LaTeX 源码（键 = 去定界符后的源码，同 zorite 的 MathStore）。
        // (槽键, 是否行内, 排版边距, 要排版的 LaTeX)：块用 `MATH_PAD`；行内用
        // `INLINE_MATH_PAD` 且键带 `inline:` 前缀 + `\textstyle` 包层（大算子转行内式，
        // 只压算子不缩正文——否则 `\int_0^1` 整图缩放会把 `x^2 dx` 也变小）。
        let mut sources: Vec<(SharedString, bool, f32, String)> = Vec::new();
        {
            let d = self.derived();
            for reg in d.borrow().math_regions.iter() {
                let s = reg.source.trim();
                if !s.is_empty() && s != "$$" {
                    sources.push((
                        s.to_string().into(),
                        false,
                        crate::config::math::MATH_PAD,
                        s.to_string(),
                    ));
                }
            }
        }
        {
            let inv = self.inline_math.borrow();
            for (_, key) in inv.iter() {
                let ks = key.as_ref();
                let lx = ks.strip_prefix("inline:").unwrap_or(ks);
                sources.push((
                    key.clone(),
                    true,
                    crate::config::math::INLINE_MATH_PAD,
                    lx.to_string(),
                ));
            }
        }
        for (key, is_inline, pad, latex) in sources {
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
                let should_bump = slots2.borrow_mut().insert(key.clone(), slot).is_none();
                if should_bump {
                    let _ = this.update(cx, |e, ecx| e.on_math_ready(ecx));
                }
            })
            .detach();
        }
    }

    /// 一个公式排版完成：自增代数 → 帧缓存失效（下次 measure 以真实高度重建）。
    fn on_math_ready(&mut self, cx: &mut Context<Self>) {
        self.math_gen.set(self.math_gen.get().wrapping_add(1));
        *self.frame.borrow_mut() = None;
        cx.notify();
    }

    /* ---------- 光标 ↔ 源辅助 ---------- */

    /// 源字节偏移 → (源行, 行内字节偏移)。用 `Doc::line_starts` 二分（O(log n)），
    /// 不再每帧线性扫整篇源文本（7MB 大文档每帧 ~100ms+）。
    pub(crate) fn row_col(&self, off: usize) -> (usize, usize) {
        let s = self.content.as_ref();
        let off = off.min(s.len());
        let ls = self.doc.line_starts();
        // 最后一个 `line_starts[i] <= off` 的行。
        let line = ls.partition_point(|&st| st <= off).saturating_sub(1);
        let start = ls.get(line).copied().unwrap_or(0);
        (line, off - start)
    }

    /// 源行 → 该行起始字节（O(log n)，`line_starts` 二分）。
    pub(crate) fn line_start(&self, line: usize) -> usize {
        self.doc
            .line_starts()
            .get(line)
            .copied()
            .unwrap_or(self.content.len())
    }

    /// 源行 → 行尾字节（不含换行符；`line_starts[line+1]-1` = 行尾 `\n` 位置）。
    /// 末行（无换行）返回内容尾。`CRLF` 下返回 `\n`（含 `\r`，与原文语义一致）。
    pub(crate) fn line_end(&self, line: usize) -> usize {
        let ls = self.doc.line_starts();
        let start = ls.get(line).copied().unwrap_or(self.content.len());
        match ls.get(line + 1) {
            Some(&next) => (next.saturating_sub(1)).min(self.content.len()).max(start),
            None => self.content.len(),
        }
    }

    /// 该行的粗略 `RowKind`（行首启发，实体内部导航用；渲染以 `DocRow` 为准）。
    fn kind_for_line(&self, line: usize) -> crate::markdown::parser::RowKind {
        use crate::markdown::parser::RowKind;
        let start = self.line_start(line);
        let end = self.line_end(line);
        let raw = &self.content.as_ref()[start..end];
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
    /// （围栏标记行 / 表格分隔行仍 `None`）。
    fn editable_segment(&self, line: usize) -> Option<(usize, usize)> {
        let start = self.line_start(line);
        let end = self.line_end(line);
        if end <= start {
            return None;
        }
        if self.in_block_content(line) {
            return Some((0, end - start));
        }
        let raw = &self.content.as_ref()[start..end];
        let seg = crate::markdown::parser::text_segment(raw, self.kind_for_line(line))?;
        Some((seg.0, raw.len()))
    }

    /* ---------- 可编辑块区域（代码/表格 + 围栏行判定） ---------- */

    /// 从块索引重建每个可编辑块区域（代码内容行 / 表格表头+数据行）。文本变更后调用。
    fn rebuild_block_regions(&mut self) {
        use crate::markdown::parser::RowKind;
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
        let raw = &self.content.as_ref()[self.line_start(line)..self.line_end(line)];
        let cells = crate::markdown::parser::split_table_cells_mapped(raw);
        if cells.is_empty() {
            return None;
        }
        let idx = (0..cells.len())
            .min_by_key(|&i| {
                let r = &cells[i].0;
                if col < r.start {
                    r.start - col
                } else if col > r.end {
                    col - r.end
                } else {
                    0
                }
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
        let total = self.content.as_ref().matches('\n').count() + 1;
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
    /// 内容变更先记入撤销栈（D1）。
    pub fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let s = self.content.as_ref();
        let r = range.start.min(s.len())..range.end.min(s.len());
        if r.start == r.end && text.is_empty() {
            return;
        }
        self.undo_stack.push(Snapshot { text: self.content.clone(), caret: self.caret });
        if self.undo_stack.len() > 200 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
        let prev_len = s.len();
        let mut next = String::with_capacity(prev_len - r.len() + text.len());
        next.push_str(&s[..r.start]);
        next.push_str(text);
        next.push_str(&s[r.end..]);
        // 增量重建 Doc：rope COW 编辑 + `scan_blocks_incremental_rope`（mid-line 插入
        // ~0.3ms）+ 增量行起始——替代旧 `Doc::open`（scan_blocks+build_ir+行表 ~2s /
        // 键，7MB 大文档「一编辑就卡死」的根因）。结构性变更自动回退（仍只一次，
        // 且已省掉 build_ir 的 ~800ms）。
        // 同时按「是否结构性」更新基础布局：非结构性编辑 → `struct_ver` 不变、记
        // 待修补块（build_frame 只对该块 Fenwick 点更新高度，省掉整篇重估 ~400ms）；
        // 结构性（跨行/代码表格/围栏破坏块序）→ `struct_ver` 递增 → 整篇重建。
        let edit_line = self.row_col(r.start).0;
        // 编辑前该行内容（region 签名对比用；非结构性编辑行边界不变）。
        let old_line = {
            let ls = self.line_start(edit_line);
            let le = self.line_end(edit_line).max(ls).min(s.len());
            s[ls..le].to_string()
        };
        let prev_count = self.doc.block_count();
        let in_code_table = {
            let blk = self.doc.block_index_for_line(edit_line);
            matches!(
                self.doc.block_kind_at_index(blk),
                Some(crate::markdown::parser::RowKind::Code)
                    | Some(crate::markdown::parser::RowKind::Table)
            )
        };
        let r_old = r.clone();
        let inserted_len = text.len();
        // 只 build 一次新缓冲：`reopen` 与 `self.content` 共享（免 6.4MB 再拷一次/键）。
        let next_ss = SharedString::from(next);
        self.doc.reopen(&next_ss, prev_len, r_old, inserted_len);
        let cross_newline =
            text.contains('\n') || s[r.start..r.end].contains('\n') || text.contains('\r');
        let breaks_fence = text.contains("```") || text.trim_start().starts_with('|');
        let count_same = self.doc.block_count() == prev_count;
        let structural = cross_newline || in_code_table || breaks_fence || !count_same;
        if structural {
            self.struct_ver.set(self.struct_ver.get().wrapping_add(1));
            *self.pending_patch_blocks.borrow_mut() = None;
        } else {
            // 非结构性：块下标稳定，记受影响的编辑行块。
            let blk = self.doc.block_index_for_line(edit_line);
            *self.pending_patch_blocks.borrow_mut() = Some(vec![blk]);
        }
        let removed = &s[r.start..r.end];
        self.math_dollar_cnt.set(
            self.math_dollar_cnt.get()
                + text.bytes().filter(|&b| b == b'$').count()
                - removed.bytes().filter(|&b| b == b'$').count(),
        );
        self.alert_marker_cnt.set(
            self.alert_marker_cnt.get() + text.matches("[!").count() - removed.matches("[!").count(),
        );
        self.content = next_ss;

        // —— 内容派生（derived / 行内清单 / 数学重扫）的增量判定（详见 `region_line_sig`）——
        // 目标：**纯文本键入**（非结构、行签名不变、不在数学块内）不动 `regions_ver`/`math_ver`
        // → `derived` Rc 跨键复用、paint 不重扫——大文档（尤其结构密集）「输入卡死」的根因
        // 是旧实现每键对全篇 `math_regions`×2 + `alert_*`×2 + 行内逐行扫 ≈1s。
        if structural {
            // 行索引 / 块结构变化 → 区域行号平移、source 变化 → 整篇重建派生 + 行内清单。
            self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
            if self.math_dollar_cnt.get() > 0 {
                *self.inline_math.borrow_mut() = inline_inventory(self.content.as_ref());
            }
            self.math_ver.set(self.math_ver.get().wrapping_add(1));
        } else {
            let new_line = self.doc.line_str(edit_line);
            if region_line_sig(&old_line) != region_line_sig(&new_line) {
                // 该行 region 签名变了（出现/消失 `$`、``` 、`[!` 或 `$$` 边界等）
                // → 区域结构可能变化 → 整篇重建派生 + 行内清单 + 数学重扫。
                self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
                if self.math_dollar_cnt.get() > 0 {
                    *self.inline_math.borrow_mut() = inline_inventory(self.content.as_ref());
                }
                self.math_ver.set(self.math_ver.get().wrapping_add(1));
            } else {
                // 快路径：区域结构不变，只做受影响点的小修补（都不全篇重建）。
                let mut math_dirty = false;
                // (a) 编辑行在 `$$…$$` 数学块内 → 重算该块 `source`（卡片 / 排版键正确，
                //     否则离开块后卡片显示旧公式）。
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
                            math_dirty = true;
                        }
                    }
                }
                // (b) 行内含 `$`（新或旧）→ 单行重扫行内公式清单（增量维护，非全篇）。
                if old_line.contains('$') || new_line.contains('$') {
                    let mut inv = self.inline_math.borrow_mut();
                    inv.retain(|(l, _)| *l != edit_line);
                    for span in inline_style::inline_math_spans(&new_line) {
                        let lx = inline_style::inline_math_latex(&new_line, &span).trim();
                        if !lx.is_empty() {
                            inv.push((edit_line, inline_math_key(lx)));
                        }
                    }
                    math_dirty = true;
                }
                if math_dirty {
                    // 数学内容确实变了（块 source / 行内清单）→ 下次 paint 消费新清单重扫一次。
                    self.math_ver.set(self.math_ver.get().wrapping_add(1));
                }
            }
        }

        self.rebuild_block_regions();
        self.caret = (r.start + text.len()).min(self.content.len());
        self.sel_anchor = None;
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        cx.emit(WyEvent::Changed);
        cx.notify();
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
        let raw = self.content.as_ref()[start..end].to_string();
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
        let key = self.content.as_ref()[start..end].trim().to_string();
        if key.is_empty() {
            return;
        }
        let was_folded = self.folded_headings.borrow().contains(&key);
        if !was_folded {
            // 折叠前：光标若在 section body 内，先移到标题行末（否则 body 有光标被 reveal）。
            let single = std::collections::HashSet::from([key.clone()]);
            let crow = self.row_col(self.caret()).0;
            if inline_style::heading_fold_regions(&self.content, &single)
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
        let raw = self.content.as_ref()[start..end].to_string();
        if let Some(new_line) = inline_style::toggle_task_checkbox(&raw) {
            self.apply_edit(start..end, &new_line, cx);
        }
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

    /// 撤销（D1）：弹出上一状态恢复；被撤销的状态进重做栈。
    pub(crate) fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.undo_stack.pop() {
            self.redo_stack.push(Snapshot { text: self.content.clone(), caret: self.caret });
            self.restore_snapshot(s, cx);
        }
    }

    /// 重做（D1）。
    pub(crate) fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.redo_stack.pop() {
            self.undo_stack.push(Snapshot { text: self.content.clone(), caret: self.caret });
            self.restore_snapshot(s, cx);
        }
    }

    /// 恢复快照：重建模型 + 光标，通知宿主内容已变（返回 Changed，供镜像保存）。
    fn restore_snapshot(&mut self, s: Snapshot, cx: &mut Context<Self>) {
        self.content = s.text;
        self.doc = Doc::open(self.content.clone());
        self.rebuild_block_regions();
        self.caret = s.caret.min(self.content.len());
        self.sel_anchor = None;
        self.marked = None;
        *self.frame.borrow_mut() = None;
        // 快照是全新内容：标记计数 / 派生版本 / 数学清单 / 基础布局全量失效
        // （与 `set_source_text` 同款；旧实现漏刷新这些，撤销到不同结构文档会
        // 命中陈旧派生/布局）。
        self.math_dollar_cnt.set(self.content.as_ref().bytes().filter(|&b| b == b'$').count());
        self.alert_marker_cnt.set(self.content.matches("[!").count());
        self.struct_ver.set(self.struct_ver.get().wrapping_add(1));
        *self.pending_patch_blocks.borrow_mut() = None;
        *self.block_layout.borrow_mut() = None;
        self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
        if self.math_dollar_cnt.get() > 0 {
            *self.inline_math.borrow_mut() = inline_inventory(self.content.as_ref());
        }
        self.math_ver.set(self.math_ver.get().wrapping_add(1));
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        cx.emit(WyEvent::Changed);
        cx.notify();
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        // 有选区 → 直接删选区（等价退格删除选中文字）。
        if let Some(range) = self.selection() {
            return self.replace_range(range, "", cx);
        }
        let s = self.content.as_ref();
        let at = self.caret.min(s.len());
        if at == 0 {
            return;
        }
        // 围栏/单元格边界守卫：退格不得删到代码块首内容行行首（会吃掉 ``` 围栏的
        // `\n`）或表格单元格内容起点（会吃掉 `| `）。
        let (bl, bcol) = self.row_col(at);
        if self.backspace_blocked(bl, bcol) {
            return;
        }
        let start = s[..at].char_indices().last().map(|(i, _)| i).unwrap_or(0);
        self.replace_range(start..at, "", cx);
    }

    /// 退格是否被边界守卫拦截（代码首内容行行首 / 表格单元格内容起点）。
    fn backspace_blocked(&self, line: usize, col: usize) -> bool {
        match self.region_content(line) {
            Some(BlockRegion::Code { lo, .. }) => line == lo && col == 0,
            Some(BlockRegion::Table { .. }) => {
                self.table_cell_bounds(line, col).is_some_and(|(s, _)| col <= s)
            }
            None => false,
        }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if let Some(range) = self.selection() {
            return self.replace_range(range, "", cx);
        }
        let s = self.content.as_ref();
        let at = self.caret.min(s.len());
        if at >= s.len() {
            return;
        }
        // 围栏/单元格边界守卫：删除不得删到代码块末内容行行尾（会把闭合围栏并进
        // 内容行）或表格单元格内容尾（会吃掉 `|`）。
        let (dl, dcol) = self.row_col(at);
        if self.delete_blocked(dl, dcol) {
            return;
        }
        let end = at + s[at..].chars().next().unwrap_or(' ').len_utf8();
        self.replace_range(at..end, "", cx);
    }

    /// 删除是否被边界守卫拦截（代码末内容行行尾 / 表格单元格内容尾）。
    fn delete_blocked(&self, line: usize, col: usize) -> bool {
        match self.region_content(line) {
            Some(BlockRegion::Code { hi, .. }) => {
                let len = self.line_end(line) - self.line_start(line);
                line == hi && col >= len
            }
            Some(BlockRegion::Table { .. }) => {
                self.table_cell_bounds(line, col).is_some_and(|(_, e)| col >= e)
            }
            None => false,
        }
    }

    fn newline(&mut self, cx: &mut Context<Self>) {
        // 表格内换行会拆坏表结构（MVP 先 no-op）；代码块内换行合法（分行）。
        let (nl, _) = self.row_col(self.caret);
        if matches!(self.region_content(nl), Some(BlockRegion::Table { .. })) {
            return;
        }
        let range = self.selection().unwrap_or(self.caret..self.caret);
        self.replace_range(range, "\n", cx);
    }

    /* ---------- 光标移动 ---------- */

    fn move_left(&mut self) {
        let (line, col) = self.row_col(self.caret);
        let s = self.content.as_ref();
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if col > lo {
                // 段内退一字符。
                self.caret = s[..self.caret]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0)
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
        let s = self.content.as_ref();
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if self.caret < seg_end {
                let n = self.caret + s[self.caret..].chars().next().map_or(1, |c| c.len_utf8());
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
        let total = self.content.as_ref().matches('\n').count() + 1;
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
            self.content.len()
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
        let total = self.content.as_ref().matches('\n').count() + 1;
        self.caret = if line + 1 < total {
            self.place_in_line(self.down_target(line), self.visual_col())
        } else {
            self.content.len()
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

/* ---------- UTF16 ↔ 字节（IME/选择器用） ---------- */

fn offset_to_utf16(s: &str, offset: usize) -> usize {
    s[..offset.min(s.len())].encode_utf16().count()
}

fn offset_from_utf16(s: &str, utf16: usize) -> usize {
    let mut count = 0usize;
    for (i, c) in s.char_indices() {
        if count >= utf16 {
            return i;
        }
        count += c.len_utf16();
    }
    s.len()
}

fn range_to_utf16(s: &str, r: Range<usize>) -> Range<usize> {
    offset_to_utf16(s, r.start)..offset_to_utf16(s, r.end)
}

fn range_from_utf16(s: &str, r: Range<usize>) -> Range<usize> {
    offset_from_utf16(s, r.start)..offset_from_utf16(s, r.end)
}

impl EntityInputHandler for WyEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let s = self.content.as_ref();
        let range = range_from_utf16(s, range_utf16);
        *actual_range = Some(range_to_utf16(s, range.clone()));
        Some(s[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let s = self.content.as_ref();
        match self.selection() {
            Some(r) => Some(UTF16Selection {
                range: range_to_utf16(s, r),
                reversed: self.caret < self.sel_anchor.unwrap_or(self.caret),
            }),
            None => Some(UTF16Selection {
                range: range_to_utf16(s, self.caret..self.caret),
                reversed: false,
            }),
        }
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|r| range_to_utf16(self.content.as_ref(), r.clone()))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_utf16(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_and_mark_utf16(range, new_text, new_selected_range, cx);
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        let r = range_from_utf16(self.content.as_ref(), range_utf16);
        self.caret = r.end.min(self.content.len());
        self.sel_anchor = Some(r.start.min(self.content.len()));
        *self.frame.borrow_mut() = None;
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: gpui::Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        let v = caret_vis();
        // `caret_vis` 是元素局部坐标（`paint` 时相对元素原点记录），需叠回元素在
        // 窗口中的原点，IME/输入法候选窗才落在光标处（否则整体偏移元素原点）。
        // 组合时画在标记文本上（从 marked 起点到本次输入终点），否则画光标竖条。
        let r = range_from_utf16(self.content.as_ref(), range_utf16);
        let w = if self.marked.as_ref().map_or(false, |m| r == *m) && r.end > r.start {
            // 组合候选：让候选窗落在整个标记文本宽度上，而不是零宽竖条。
            // 真实几何很难按字符溯源，这里用“应盖住的位置”转成可见块（至少一字符宽）。
            px(24.)
        } else {
            px(2.)
        };
        Some(gpui::Bounds::new(
            gpui::point(element_bounds.origin.x + v.x, element_bounds.origin.y + v.top),
            gpui::size(w, v.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        // 真实命中：经 frame 反查源偏移（平台 IME/辅助功能用，坐标元素局部）。
        let marker = self.frame.borrow();
        let f = marker.as_ref().map(|r| &**r)?;
        let off = element::source_offset_at_local(self, f, point);
        drop(marker);
        Some(offset_to_utf16(self.content.as_ref(), off))
    }

    fn text_length_utf16(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.content.as_ref().encode_utf16().count())
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        true
    }
}

impl Focusable for WyEditor {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl gpui::EventEmitter<WyEvent> for WyEditor {}

impl gpui::Render for WyEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{CursorStyle, InteractiveElement, ParentElement, Styled};
        gpui::div()
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
            })
    }
}
