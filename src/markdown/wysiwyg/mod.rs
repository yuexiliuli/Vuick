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
    /// 块「真实高度（px）」缓存（关键：带宽滚动时带外块沿用**已整形过的真值**，
    /// 而非每次回到估算——否则行高在估算/真实间跳动，滚动时文字忽大忽小）。
    /// 键 = `(gen, src_line, kind_tag, width_bits)` 的 hash；值 = 该块整形的实际高度 px。
    /// **必须存 px 而不是「可视行数」**：行数重建 (`para_line × n`) 只对普通段落
    /// 成立，标题（`heading_line` + 8px pad）、行内公式撑高行（`sub_heights[0]` > lh）
    /// 重建会矮/高不实 → 带内/带外 total 不一致 → 滚动到文档先后端反复 clamp 跳变。
    /// 字号/主题变化会改 px 量纲 → `set_theme`/`set_font_size` 必须清此缓存。
    height_cache: RefCell<std::collections::HashMap<u64, f32>>,
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
            height_cache: RefCell::new(std::collections::HashMap::new()),
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
        // 高度缓存存的是 px 真值，主题/字号变化量纲改变 → 必须清（否则带外沿用
        // 旧字号的固定 px 高，行高与带内新字号不一致，滚动跳变）。
        self.height_cache.borrow_mut().clear();
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
        // 同 `set_theme`：字号变 → 高度缓存（px）失效。
        self.height_cache.borrow_mut().clear();
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
    pub fn set_source_text(&mut self, text: &str) {
        self.content = SharedString::from(text);
        self.doc = Doc::open(self.content.clone());
        self.rebuild_block_regions();
        self.preseed_image_sizes();
        self.caret = self.caret.min(self.content.len());
        self.undo_stack.clear();
        self.redo_stack.clear();
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
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
    pub(crate) fn cached_height(&self, key: u64) -> Option<f32> {
        self.height_cache.borrow().get(&key).copied()
    }
    pub(crate) fn cache_height(&self, key: u64, height_px: f32) {
        self.height_cache.borrow_mut().insert(key, height_px);
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
    /// `math_scanned_gen` 挡掉同代重复扫描。
    pub(crate) fn sync_math(&mut self, cx: &mut Context<Self>) {
        let color = math_color(&self.theme);
        let cgen = self.content_gen.get();
        let color_changed = self.math_color.borrow().as_ref() != Some(&color);
        if color_changed {
            self.math_color.borrow_mut().replace(color);
            self.math_slots.borrow_mut().clear();
        }
        if !color_changed && self.math_scanned_gen.get() == cgen {
            return;
        }
        self.math_scanned_gen.set(cgen);

        // 收集块 + 行内公式 LaTeX 源码（键 = 去定界符后的源码，同 zorite 的 MathStore）。
        let content = self.content.as_ref().to_string();
        // (槽键, 是否行内, 排版边距, 要排版的 LaTeX)：块用 `MATH_PAD`；行内用
        // `INLINE_MATH_PAD` 且键带 `inline:` 前缀 + `\textstyle` 包层（大算子转行内式，
        // 只压算子不缩正文——否则 `\int_0^1` 整图缩放会把 `x^2 dx` 也变小）。
        let mut sources: Vec<(SharedString, bool, f32, String)> = Vec::new();
        for reg in inline_style::math_regions(&content) {
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
        for line in content.split('\n') {
            for span in inline_style::inline_math_spans(line) {
                let lx = inline_style::inline_math_latex(line, &span).trim();
                if !lx.is_empty() {
                    sources.push((
                        inline_math_key(lx),
                        true,
                        crate::config::math::INLINE_MATH_PAD,
                        lx.to_string(),
                    ));
                }
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

    /// 源字节偏移 → (源行, 行内字节偏移)。
    pub(crate) fn row_col(&self, off: usize) -> (usize, usize) {
        let s = self.content.as_ref();
        let off = off.min(s.len());
        let mut line = 0;
        let mut last = 0;
        for (i, b) in s.bytes().enumerate() {
            if i >= off {
                break;
            }
            if b == b'\n' {
                line += 1;
                last = i + 1;
            }
        }
        (line, off - last)
    }

    /// 源行 → 该行起始字节。
    pub(crate) fn line_start(&self, line: usize) -> usize {
        let s = self.content.as_ref();
        let mut start = 0;
        let mut n = 0;
        for (i, b) in s.bytes().enumerate() {
            if n == line {
                return i;
            }
            if b == b'\n' {
                n += 1;
                start = i + 1;
            }
        }
        start
    }

    /// 源行 → 行尾字节（不含换行符）。
    pub(crate) fn line_end(&self, line: usize) -> usize {
        let start = self.line_start(line);
        match self.content.as_ref()[start..].find('\n') {
            Some(i) => start + i,
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
        let mut next = String::with_capacity(s.len() - r.len() + text.len());
        next.push_str(&s[..r.start]);
        next.push_str(text);
        next.push_str(&s[r.end..]);
        self.content = SharedString::from(next);
        self.doc = Doc::open(self.content.clone());
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
