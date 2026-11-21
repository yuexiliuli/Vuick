//! parser 的全部数据型：worker 消息载荷 / 渲染请求与结果 / 文本与图片抽取 / 书签与注释 /
//! 重着色刷新条目 / 网格预熟几何。纯类型定义（+ 少量盒几何纯函数），不含任何 pdfium 调用。
//!
//! 可见性约定（模块重组）：对外 API 均为 `pub`（经根模块 `pub use types::*` 重导出）；
//! 仅库内部跨簇消费的项（`PageDims`/`OpenPayload`/`CharBox`/`resolve_char_box`）为
//! `pub(crate)`，不对外泄漏。

/// 页尺寸对（宽, 高）。
pub(crate) type PageDims = Vec<(f32, f32)>;
/// 打开成功载荷：页尺寸 + 书签条目。
pub(crate) type OpenPayload = (PageDims, Vec<PdfOutlineEntry>);

/// 打开的文档在本进程内的唯一编号。
pub type DocId = u64;

/// 渲染请求：某文档 × 页 × 栅格化倍率（PDF 点 → px 的乘数）。
#[derive(Debug, Clone, Copy)]
pub struct RenderRequest {
    pub page: usize,
    pub scale: f32,
}

/// 瓦片渲染请求：把某页切成 [TILE_SIZE × TILE_SIZE] 的网格，栅格化其中一格。
///
/// - `scale` = 当前显示密度（px/pt，见 [`crate::pdf::layout::render_scale`]）：瓦片按
///   **显示物理像素** 1:1 渲染，无论是高 DPI 屏还是 Ctrl+滚轮放大，文字/图形都不经
///   GPU 上采样——这是"整页 4× 缩放位图 → 发糊"的根治手段之一。
/// - `col / row` 决定页位图内的哪一格；worker 只分配一格大小的 BGRA 缓冲
///   （~512²×4 ≈ 1MB），整页分摊到视口可见的几片里，内存不再随缩放爆炸。
#[derive(Debug, Clone, Copy)]
pub struct TileRequest {
    pub page: usize,
    pub scale: f32,
    pub tile_size: u32,
    pub row: u32,
    pub col: u32,
}

/// 一块瓦片的渲染结果：像素 + 在页位图中的几何（含出血裁剪，夹到页内）。
///
/// view 层用 `x0/y0/w/h` **原样**定位瓦片（与 worker 同宗同一套数学，见
/// [`crate::pdf::layout::tile_rect`]），再用 `full_w/full_h` 换算卡片显示比例。
#[derive(Debug)]
pub struct TileResult {
    pub page: usize,
    pub row: u32,
    pub col: u32,
    /// 瓦片在页位图中的左上角 x（含出血）。
    pub x0: u32,
    /// 瓦片在页位图中的左上角 y。
    pub y0: u32,
    /// 瓦片位图实际宽（含出血，夹到页内）。
    pub w: u32,
    /// 瓦片位图实际高。
    pub h: u32,
    /// 页位图全宽（= ceil(页pt宽 × scale)）。
    pub full_w: u32,
    /// 页位图全高（= ceil(页pt高 × scale)）。
    pub full_h: u32,
    /// 紧凑 BGRA 像素（`w*h*4` 字节）或 **R8 luma**（`w*h*1` 字节，`luma == true` 时；
    /// pdfium 反字节序后即 BGRA）。
    pub bgra: Vec<u8>,
    /// W1：渲染该瓦片时 worker 侧该文档的**重着色世代号**（`SetRecolor` 按文档递增世代，
    /// 见 `run` 循环）。由 `run` 循环按 doc 回填；view 侧 `collect_tile_results` 丢弃
    /// 世代 ≠ 当前的到达结果——杜绝「CPU 回退路径切档后，旧色重栅结果晚到覆盖新参数」的
    /// 乱序窗口（GPU 门控下结果恒为 raw，着色在绘制时，不受影响）。构造留 0 占位待回填。
    pub recolor_epoch: u64,
    /// P2：`bgra` 是否为 **R8 单通道 luma**（1BPP，`w*h*1`）——GPU 重着色 + FullBand +
    /// `ink=Some` 门控激活时 worker 编码成 luma（-4× 无损）。view 侧按此选包装路径。
    pub luma: bool,
}

/// 渲染结果：页面 **BGRA 像素字节** + 位图像素 (宽, 高)。
///
/// `bgra` 直接来自 pdfium 的原生 BGRA 缓冲（`FPDFBitmap_GetBuffer_as_vec`），
/// 主线程用 `Frame::from_raw` 直接包裹即上传 → **没有任何像素循环或字节重排**。
/// （缩略/中清/全尺寸均为纯内存，无 PNG/WebP 编码与落盘。）
#[derive(Debug)]
pub struct RenderResult {
    /// 原始 BGRA 像素（`w*h*4` 字节），主线程 `Frame::from_raw` 直接包裹即上传。
    /// 注意：pdfium 的 stride 可能大于 `w*4`（对齐填充），本字段为**去 stride 后**
    /// 的紧凑一行式缓冲；`width/height` 为位图逻辑尺寸。
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// W1：渲染该结果时 worker 侧该文档的**重着色世代号**（同 [`TileResult::recolor_epoch`]，
    /// 由 `run` 循环按 doc 回填）。view 侧 `collect_cover_results` 丢弃世代 ≠ 当前的到达
    /// 结果。构造留 0 占位待回填。
    pub recolor_epoch: u64,
    /// P2：`bgra` 是否为 **R8 单通道 luma**（1BPP，`width*height*1`）——GPU 门控激活时
    /// worker 编码成 luma（-4× 无损）。view 侧按此选包装路径。
    pub luma: bool,
}

/// 重着色刷新的位图来源类别（对应 view 层四个槽位类：整页封面 / 瓦片 / 中清 / 缩略）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RawKind {
    Cover,
    Tile,
    Mid,
    Thumb,
}

/// 重着色刷新回传：某条**已缓存未着色原图**按当前参数重着色后的显示副本（连同定位
/// 元数据与密度标尺）。view 层按 `kind` 替换对应槽位图片（密度守卫与各 `collect_*`
/// 同宗：瓦片比对 `full_w/full_h` 与 `scale`，封面比对 `scale`，中清/缩略只换图）。
#[derive(Debug)]
pub struct RecolorEntry {
    pub kind: RawKind,
    pub page: usize,
    pub row: u32,
    pub col: u32,
    /// 瓦片在页位图中的左上角 x（含出血）。
    pub x0: u32,
    /// 瓦片在页位图中的左上角 y。
    pub y0: u32,
    /// 位图实际宽（含出血）。
    pub w: u32,
    /// 位图实际高。
    pub h: u32,
    /// 页位图全宽（= ceil(页pt宽 × scale)）。
    pub full_w: u32,
    /// 页位图全高。
    pub full_h: u32,
    /// 该原图的栅格化密度（px/pt；view 侧密度守卫用）。
    pub scale: f32,
    /// 按当前重着色参数着色后的 BGRA（`None` 直通时 = 原图副本）。
    pub bgra: Vec<u8>,
}

/// 重着色刷新可精确定位的一条显示位图（kind + 页 + 格号）：**可见集定义** = view 侧按
/// 「显示槽真实需要的图片」枚举出的 key 集合（在位封面 + 在位瓦片 + 中清/缩略占位）；
/// worker 侧据此从 raw 缓存逐条现算回传——**只刷可见集**，窗外 / 已淘汰但仍在 raw 缓存
/// 的条目不参与本次回传（保留在 raw 缓存，用户滚回需要时再走常规渲染现算，互不污染）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecolorKey {
    pub kind: RawKind,
    pub page: usize,
    pub row: u32,
    pub col: u32,
}

/// 页内单个字符的文本层数据：Unicode + 包围盒。
///
/// 坐标语义：与 pdfium 一致（PDF 用户空间，原点在页**左下角**，Y 轴向上），单位
/// 为 PDF 点；到屏幕坐标的 Y 翻转与缩放由 view 层的映射（layout.rs，P1）负责。
/// 盒字段均已夹到页内 [0, width] × [0, height]。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfTextChar {
    /// Unicode 码点（pdfium 无法解码时为 `U+FFFD`；可能为不可见字符）。
    pub ch: char,
    /// 包围盒左缘（PDF 点）。
    pub left: f32,
    /// 包围盒下缘（PDF 点）。
    pub bottom: f32,
    /// 包围盒右缘（PDF 点）。
    pub right: f32,
    /// 包围盒上缘（PDF 点）。
    pub top: f32,
    /// 盒退化（tight 与 loose 均为 0 宽 0 高）：命中测试应**降权**此字符（复制仍可用）。
    pub synthetic: bool,
    /// 字符基线原点（PDF 点）。同一行（同一文本对象 run）的 `origin_y` 一致——
    /// P1 用它做"基线对齐"校验（映射后同行 `origin_y` 方差 < 1px，防选区高亮锯齿）。
    pub origin_x: f32,
    pub origin_y: f32,
}

/// 一页的文本层数据（**自包含**、可跨线程传给阅读器；复制/命中测试/搜索用）。
#[derive(Debug, Clone, PartialEq)]
pub struct PageText {
    /// 所属页号（0 基）。
    pub page_index: usize,
    /// 页面宽度（PDF 点）。
    pub width: f32,
    /// 页面高度（PDF 点）。
    pub height: f32,
    /// 按 pdfium 文本索引顺序排列的字符（≈文档内定义顺序，选中复制按此拼接）。
    pub chars: Vec<PdfTextChar>,
    /// 盒退化字符数（诊断用；正常应为 0）。
    pub synthetic_count: usize,
}

/// 页内单个光栅图片（raster Image 对象）的包围盒与自然分辨率。
///
/// 坐标语义与 [`PdfTextChar`] 一致：PDF 用户空间（原点在页**左下角**、Y 轴向上），
/// 单位 PDF 点；到屏幕坐标的翻转与缩放由 view 层映射负责。**自包含**、可跨线程传给
/// 阅读器（供图片命中测试 / 选中描框 / 右键「复制图片」取数）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfImageBox {
    /// 所属页号（0 基）。
    pub page_index: usize,
    /// 页对象集合内的对象下标（`PdfPage::objects().get(obj_index)` 可再取回该对象）。
    pub obj_index: usize,
    /// 包围盒左缘（PDF 点）。
    pub x0: f32,
    /// 包围盒下缘（PDF 点）。
    pub y0: f32,
    /// 包围盒右缘（PDF 点）。
    pub x1: f32,
    /// 包围盒上缘（PDF 点）。
    pub y1: f32,
    /// 图片**自然宽**（嵌入分辨率，位图像素；`PdfPageImageObject::width()`）。
    pub natural_w: u32,
    /// 图片**自然高**（嵌入分辨率，位图像素；`PdfPageImageObject::height()`）。
    pub natural_h: u32,
}

impl PdfImageBox {
    /// 盒是否可用于命中测试（正宽高 + 有限值）。
    ///
    /// 跨簇可见性（模块重组）：`extract::extract_page_images` 调用 → 升 `pub(crate)`。
    pub(crate) fn valid(self) -> bool {
        self.x1 - self.x0 > 0.0
            && self.y1 - self.y0 > 0.0
            && self.x0.is_finite()
            && self.x1.is_finite()
            && self.y0.is_finite()
            && self.y1.is_finite()
    }

    /// 页内 PDF 点是否落在盒内（含 1pt 级容差，与注释命中 [`ANNOT_HIT_TOL_PT`]
    /// 同一精神——点框线边缘也算命中）。
    pub fn contains_pt_tol(&self, px: f32, py: f32, tol: f32) -> bool {
        px >= self.x0 - tol && px <= self.x1 + tol && py >= self.y0 - tol && py <= self.y1 + tol
    }
}

/// 从图片对象按自然分辨率取回的**紧凑 BGRA** 像素（纯 `w*h*4`、无 stride）。
/// 拷贝到剪贴板（CF_DIB）与测试对拍共用。
#[derive(Debug, Clone, PartialEq)]
pub struct PdfImagePixels {
    /// BGRA（B、G、R、A 字节序）紧凑像素；`width × height × 4` 字节。
    pub bgra: Vec<u8>,
    /// 位图逻辑宽（px）。
    pub width: u32,
    /// 位图逻辑高（px）。
    pub height: u32,
}

/// 字符包围盒（PDF 点）。仅 worker 内部临时结构。
///
/// 跨簇可见性（模块重组）：`extract` 子模块（`char_box_from_rect` / 夹取到页内）直接
/// 读写字段 → 结构与字段升 `pub(crate)`（纯可见性升格，字段语义未变）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CharBox {
    pub(crate) left: f32,
    pub(crate) bottom: f32,
    pub(crate) right: f32,
    pub(crate) top: f32,
}

impl CharBox {
    /// 跨可见性（模块重组）：`parser/tests.rs` 引用 → 升 `pub(crate)`。
    pub(crate) const ZERO: CharBox = CharBox {
        left: 0.0,
        bottom: 0.0,
        right: 0.0,
        top: 0.0,
    };

    /// 盒是否可用于命中测试（正宽高 + 有限值）。
    fn valid(self) -> bool {
        self.right - self.left > 0.0
            && self.top - self.bottom > 0.0
            && self.left.is_finite()
            && self.right.is_finite()
            && self.bottom.is_finite()
            && self.top.is_finite()
    }
}

/// 字符盒回退策略：**tight（首选）→ loose（回退）→ 标记 synthetic**。
///
/// `FPDFText_GetCharBox` 对空格、零宽连接符、部分 CJK 标点等可能返回 0 宽 0 高
/// （或负尺寸）的退化盒；此时用 `FPDFText_GetLooseCharBox` 兜底。若连 loose 都退化，
/// 只能保留（标记 synthetic）供复制，并让命中测试跳过它。
///
/// 跨簇可见性（模块重组）：`extract` 子模块（`extract_page_text`）调用 → 升 `pub(crate)`。
pub(crate) fn resolve_char_box(tight: Option<CharBox>, loose: Option<CharBox>) -> (CharBox, bool) {
    match tight {
        Some(t) if t.valid() => (t, false),
        _ => match loose {
            Some(l) if l.valid() => (l, false),
            l => (l.unwrap_or(CharBox::ZERO), true),
        },
    }
}

/// 一条 PDF 书签（目录项）：标题 + 层级 + 目标页码。
///
/// 由工作线程在打开文档时从 pdfium 书签树提取为**自包含**结构（不借用 pdfium），
/// 可跨线程传到 [`PdfViewer`] 展示为目录。
///
/// [`PdfViewer`]: super::super::view::PdfViewer
#[derive(Debug, Clone)]
pub struct PdfOutlineEntry {
    /// 书签标题。
    pub title: String,
    /// 树层级：顶层为 0，每深入一层 +1（用于缩进）。
    pub level: usize,
    /// 目标页码（0 基）。无目标页码时用 `usize::MAX`（不跳转）。
    pub page_index: usize,
}

/// 写回注释的类型：映 PDF 标准 annotation 子类型（Highlight / StrikeOut / Underline）。
///
/// 数值 = pdfium 的 `FPDF_ANNOT_*` 常量（对照本机 pdfium_7881 bindgen 已核实：
/// `FPDF_ANNOT_HIGHLIGHT=9`、`FPDF_ANNOT_UNDERLINE=10`、`FPDF_ANNOT_STRIKEOUT=12`；
/// FPDFPage_CreateAnnot 的 `subtype` 实参即此 C 枚举值）。Phase B 由 UI 提供类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotKind {
    Highlight,
    Strikeout,
    Underline,
}

impl AnnotKind {
    /// FPDF_ANNOT_* 子类型数值（见类型注释来源）。
    ///
    /// 跨簇可见性（模块重组）：`writeback::apply_annotations_raw` 调用 → 升 `pub(crate)`。
    pub(crate) fn fpdf_subtype(self) -> i32 {
        match self {
            AnnotKind::Highlight => 9,
            AnnotKind::Underline => 10,
            AnnotKind::Strikeout => 12,
        }
    }

    /// 默认标准着色（R,G,B，0-255）：高亮柔和黄 / 删除线红 / 下划线蓝。
    /// Phase B 由「复制/高亮/删除线/下划线」弹卡的 UI 定制，此处仅默认值。
    ///
    /// 跨簇可见性（模块重组）：`writeback::apply_annotations_raw` 调用 → 升 `pub(crate)`。
    pub(crate) fn default_color(self) -> (u32, u32, u32) {
        match self {
            AnnotKind::Highlight => (255, 230, 120),
            AnnotKind::Strikeout => (230, 60, 60),
            AnnotKind::Underline => (60, 120, 230),
        }
    }
}

/// 一个写回选区矩形的 PDF 点坐标（y 向上、原点页左下；与 [`PdfTextChar`] 同坐标语义，
/// 由 view/layout 层在调用前把屏幕选区换算成 PDF 点，worker 不做坐标数学）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadPt {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// 一条待写回的注释：页号 + 类型 + 若干 quad 矩形（同一页可跨行成多个矩形）。
#[derive(Debug, Clone)]
pub struct AnnotateItem {
    pub page_index: usize,
    pub kind: AnnotKind,
    pub quads: Vec<QuadPt>,
}

/// 重开文档后读回的注释摘要（类型 + 每个 quad 的 PDF 点坐标）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationInfo {
    pub kind: AnnotKind,
    pub quads: Vec<QuadPt>,
}

/// 网格**带宽预熟几何**（本线 M1+T6）：view 进入网格 / 卡宽变化 / 滚动重定中心时随
/// [`super::WorkerMsg::SetGridPrewarm`] 下发 worker——worker 据此做「以视口中心页为核心的水平
/// 带宽」预熟，任何时候都知道哪些页近/中/远：
/// - **近带**（`near`，闭区间 = 视图中心行 ±[`config::pdf::GRID_NBAND_ROWS`] 行的页区间，
///   完整 ⊇ view 可视带 + 保留窗 GRID_ROW_BUFFER+GRID_KEEP_ROWS=±4 行）→ 预熟**全清 + 低清
///   双档**（近带恒有图、idle 升级全清命中缓存）；
/// - **中带**（`window` 内 beyond `near`，= ±(近+中)行）→ 预熟**仅低清**（滚回即显、不占
///   全清内存）；
/// - **更远**（`window` 外）→ 不预熟（首曝按需由 view 派发，风暴低清兜底）。
///
/// `center_page` = 当前视口中心行首页（外向扫描的对称中心）。窗口尺寸恒与文档总页数
/// **无关**（固定行数）→ 长文档预熟有界常驻几十 MB、无整本 LRU 滑动窗抖动/永不完成。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridPrewarmGeom {
    /// 视口中心行首页下标（外向扫描对称中心；0 基）。
    pub center_page: usize,
    /// 中带外沿闭区间 `[lo, hi]`（含近+中带全部页；`window.0..=window.1`）。
    pub window: (usize, usize),
    /// 近带闭区间 `[lo, hi]`（⊆ `window`；近带页预熟全清+低清）。其余 `window` 内页仅低清。
    pub near: (usize, usize),
}
