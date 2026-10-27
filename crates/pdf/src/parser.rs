//! PDF 底层原语（pdfium-render）：定位 DLL、进程级单工作线程、多文档逐页栅格化。
//!
//! pdfium 是 native 内核（Edge 同源），单页栅格化快，首次浏览即流畅。
//!
//! ## pdfium-render 的关键约束（0.9.x，已实测确认）
//!
//! - **整进程只能 `Pdfium::new()` 一次**：绑定的 bindings 存在 crate 内部一个全局
//!   `OnceCell` 里，第二次 `Pdfium::new()` 直接 `assert!(BINDINGS.get().is_none())`
//!   崩溃；第二次 `bind_to_library()` 返回 `PdfiumLibraryBindingsAlreadyInitialized`。
//!   因此**不能**为每个阅读器各开一个线程并各自绑定 pdfium。
//! - `Pdfium` 在 `thread_safe` 特性（默认开启）下 `Send + Sync`，但任何一次
//!   `FPDF_*` 调用都要经过这个全局 bindings；真正多线程并发栅格化会触碰 pdfium
//!   内部（字体/页面缓存）竞态，本模块保持**单工作线程**串行渲染。
//!
//! ## 本模块架构
//!
//! **进程级单例后台线程** [`shared_worker()`]：第一次用到时启动；工作线程在自己的
//! 作用域内持有一个 `Pdfium`（唯一一次 `new`）与一个 `HashMap<DocId, PdfDocument>`，
//! 通过 mpsc 接收 `Open / Dims / Render / Close` 命令。所有 [`PdfViewer`] 拿到的
//! [`PdfWorker`] 只是这个单例句柄的克隆（`Arc`），真正干活始终是同一线程——既绕开了
//! "只能绑定一次"，也让多文档共存（各阅读器各占一个 `DocId`）。
//!
//! [`PdfViewer`]: super::view::PdfViewer

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::raw::{c_int, c_ulong, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};

use anyhow::{anyhow, Context as _, Result};
use pdfium_render::prelude::*;

/// 进程级单例：pdfium 动态库路径（只定位一次）。
static PDFIUM_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// 页尺寸对（宽, 高）。
type PageDims = Vec<(f32, f32)>;
/// 打开成功载荷：页尺寸 + 书签条目。
type OpenPayload = (PageDims, Vec<PdfOutlineEntry>);

/// pdfium 版本号文件名（与 `resources/pdfium/` 内实际文件、`build.rs` 保持一致）。
///
/// 用**带版本号**的文件名（`pdfium-<版本>.dll`）替代裸 `pdfium.dll`：
/// - 一眼可辨当前内置的 pdfium 快照版本；
/// - 升级 pdfium 时新旧文件可并存，随时回滚，不用覆盖。
const PDFIUM_VERSIONED_NAME: &str = "pdfium-154.0.8035.0.dll";

/// 内嵌的 pdfium 字节（仅当 `build.rs` 探测到 dll 存在时启用 `embed_pdfium` cfg）。
///
/// 通过编译期 `include_bytes!` 把 dll 打包进可执行文件，实现**单文件分发**：
/// 运行时把字节解出到临时目录再动态加载（Windows 的 `libloading` 必须从磁盘加载 dll，
/// 无法直接内存加载）。若本机没有该 dll（未触发 `embed_pdfium`），则退化为仅磁盘查找。
///
/// 路径由 `build.rs` 通过 `cargo:rustc-env=PDFIUM_DLL_PATH` 注入（指向
/// `resources/pdfium/pdfium-<版本>.dll`）。
#[cfg(embed_pdfium)]
const EMBEDDED_PDFIUM: &[u8] = include_bytes!(env!("PDFIUM_DLL_PATH"));

/// 在临时目录解出内嵌的 pdfium 并返回其路径。
///
/// 文件命名为 `gpui-editor/pdfium-<版本>.dll`（仅首次写入；内容与 `include_bytes!`
/// 一致时直接复用，退出后由临时目录清理，不污染项目目录）。
#[cfg(embed_pdfium)]
fn extract_embedded_pdfium() -> PathBuf {
    let dir = std::env::temp_dir().join("gpui-editor");
    let path = dir.join(PDFIUM_VERSIONED_NAME);
    // 已存在且长度一致 → 直接复用（避免每次启动重写 7MB；长度校验足够，跨版本名唯一）
    let reuse = path.is_file() && path.metadata().map(|m| m.len() as usize).unwrap_or(0) == EMBEDDED_PDFIUM.len();
    if reuse {
        return path;
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, EMBEDDED_PDFIUM);
    path
}

/// 定位 pdfium 动态库（按优先级，**只用带版本号的文件**，不退回裸 `pdfium.dll`）：
/// 1. 环境变量 `GPUI_EDITOR_PDFIUM`：绝对路径直接指定（最优先，逃生口；支持任意文件名）
/// 2. 可执行文件同目录：`<版本号>.dll`（旁置分发/就地升级，可覆盖内嵌版）
/// 3. 项目 `resources/pdfium/`：`<版本号>.dll`（开发树自带）
/// 4. 内嵌到 exe 的 pdfium（`embed_pdfium` cfg）：解出到临时目录（单文件分发兜底）
fn locate_dll() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("GPUI_EDITOR_PDFIUM") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Ok(pb);
        }
    }
    #[allow(unused)]
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(dir.join(PDFIUM_VERSIONED_NAME));
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        // P4：crate 位于 crates/pdf，开发树自带的 pdfium 在工作区根 resources/pdfium/（上溯两级）。
        candidates.push(
            PathBuf::from(manifest)
                .join("../../resources/pdfium")
                .join(PDFIUM_VERSIONED_NAME),
        );
    }
    #[cfg(embed_pdfium)]
    candidates.push(extract_embedded_pdfium());
    #[cfg(not(embed_pdfium))]
    let _ = &candidates; // 无内嵌：candidates 仅磁盘候选

    candidates
        .into_iter()
        .find(|c| c.is_file())
        .ok_or_else(|| anyhow::anyhow!(
            "无法找到 pdfium 动态库 {PDFIUM_VERSIONED_NAME}：\
             请把它放在可执行文件旁或 resources/pdfium/，或用 GPUI_EDITOR_PDFIUM=<路径> 指定"
        ))
}

fn pdfium_dll_path() -> Result<&'static PathBuf> {
    let cached = PDFIUM_PATH.get_or_init(|| locate_dll().map_err(|e| e.to_string()));
    match cached {
        Ok(p) => Ok(p),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

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
    /// 页位图全高。
    pub full_h: u32,
    /// 紧凑 RGBA 像素（`w*h*4` 字节；pdfium 反字节序后即 RGBA）。
    pub bgra: Vec<u8>,
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

/// 字符包围盒（PDF 点）。仅 worker 内部临时结构。
#[derive(Debug, Clone, Copy, PartialEq)]
struct CharBox {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

impl CharBox {
    const ZERO: CharBox = CharBox {
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
fn resolve_char_box(tight: Option<CharBox>, loose: Option<CharBox>) -> (CharBox, bool) {
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
/// [`PdfViewer`]: super::view::PdfViewer
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
    fn fpdf_subtype(self) -> i32 {
        match self {
            AnnotKind::Highlight => 9,
            AnnotKind::Underline => 10,
            AnnotKind::Strikeout => 12,
        }
    }

    /// 默认标准着色（R,G,B，0-255）：高亮柔和黄 / 删除线红 / 下划线蓝。
    /// Phase B 由「复制/高亮/删除线/下划线」弹卡的 UI 定制，此处仅默认值。
    fn default_color(self) -> (u32, u32, u32) {
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

/// FPDF_ANNOT_FLAG_PRINT：注释随打印输出（去掉了默认的 INVISIBLE/HIDDEN 显隐影响，
/// 默认 0 = 可见 + 可打印位置 1）。
const FPDF_ANNOT_FLAG_PRINT: c_int = 4;
/// FPDFANNOT_COLORTYPE_Color：注释描边/填充色（0）。
const FPDFANNOT_COLORTYPE_COLOR: u32 = 0;

/// 写回落盘临时文件序号（同目录并发写回防撞名；worker 单线程串行本不应并发，兜底）。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

enum WorkerMsg {
    Open(
        DocId,
        Vec<u8>,
        mpsc::Sender<Result<OpenPayload>>,
    ),
    #[allow(dead_code)] // 回归测试渲染整页（文本对齐、贴片比对）使用
    Render(DocId, RenderRequest, mpsc::Sender<Option<RenderResult>>),
    /// 瓦片渲染（主内容路径）：只栅格化一页的一格（~512px）。与全尺寸 Render 同一
    /// 优先级（视口内容优先），worker 内实现见 [`render_tile`]。
    RenderTile(DocId, TileRequest, mpsc::Sender<Option<TileResult>>),
    /// L0.5 中清页渲染（~520px × DPI，~5ms/张）。优先级低于全尺寸 Render、高于缩略图
    /// Thumb：快速拖动时用中清页保证可辨识度，同时比全清便宜 ~6×。
    /// `scale_factor` 使目标宽度按显示 DPI 推导（DPI>1 时预览仍锐利，不经 GPU 上采样发糊）。
    Mid(DocId, usize, f32, mpsc::Sender<Option<RenderResult>>),
    /// L0 缩略图渲染（低清晰度、~1ms/张）。优先级从高到低：Render/RenderTile > Mid >
    /// Thumb > Text——拖动期间先铺中清/缩略占位，瓦片随后补齐。
    Thumb(DocId, usize, mpsc::Sender<Option<RenderResult>>),
    Close(DocId),
    /// 请求某页文本层数据（懒加载：仅选中/复制/搜索时调用）。**低优先级**：
    /// worker 调度保证绝不排在栅格化（Render）之后执行。
    Text(DocId, usize, mpsc::Sender<Option<PageText>>),
    /// 请求文档级检索（懒加载，低优先级；全页文本层扫描）。
    Search(DocId, String, mpsc::Sender<Option<PdfSearchResults>>),
    /// **写回注释并落盘**：worker 内把内存 bytes 载入「自有 raw 句柄」
    /// （`FPDF_LoadMemDocument64`）→ 在 raw doc 上建注释 → `FPDF_SaveAsCopy`
    /// （自制 `FPDF_FILEWRITE`，先写同目录临时文件再原子替换 `output_path`）。
    /// 成功回传实际写入的注释条数，并更新内存 bytes（后续操作基于新内容）。
    /// viewer 收到成功后走 Close→Open 同 path 流程重载（见 view 的 modify 相关方法）。
    Annotate(DocId, Vec<AnnotateItem>, PathBuf, mpsc::Sender<Result<usize>>),
    /// 插入空白页并落盘：`at_index` = 新页插入位置（0 基；= page_count 即末尾追加）。
    /// 成功回传**新页数**。
    InsertBlankPage(DocId, usize, f32, f32, PathBuf, mpsc::Sender<Result<usize>>),
    /// 删除页面并落盘（index 越界或仅剩 1 页 → Err）。成功回传**新页数**。
    DeletePage(DocId, usize, PathBuf, mpsc::Sender<Result<usize>>),
    /// 把单页提取成独立 PDF 文件（raw 句柄：`FPDF_CreateNewDocument` + `FPDF_ImportPages`）。
    ExtractPage(DocId, usize, PathBuf, mpsc::Sender<Result<()>>),
    /// 显式把当前内存 bytes 重新编码落盘到 `path`（无内容修改的另存/刷新）。
    Save(DocId, PathBuf, mpsc::Sender<Result<()>>),
    /// **内存级**插入空白页（待保存模型）：只改 worker 内 stores 内存工作副本（新镜像
    /// 用 [`save_raw_doc_to_bytes`] 产出），**不碰磁盘**。成功回传 [`MemOpResult`]
    /// （before=操作前镜像 / after=新镜像 / count=新页数）。与 [`WorkerMsg::InsertBlankPage`]
    /// 区别：后者落盘到 `output_path`，本消息仅内存。
    MemOpInsert(DocId, usize, f32, f32, mpsc::Sender<Result<MemOpResult>>),
    /// **内存级**删除页面（待保存模型）：守卫同磁盘版（越界 / 仅剩 1 页 → Err），
    /// 只改 stores、不碰磁盘。成功回传 [`MemOpResult`]。
    MemOpDelete(DocId, usize, mpsc::Sender<Result<MemOpResult>>),
    /// 用给定 bytes 直接替换 stores 镜像（待保存模型撤销「页面操作」用；本消息不做任何
    /// pdfium 操作、零开销）。回传替换后镜像长度（存在性校验）。
    MemRestoreBytes(DocId, Vec<u8>, mpsc::Sender<Result<usize>>),
    /// 读回 stores 当前内存镜像（撤销快照 / 测试 MD5 / shell 另存共用；None=文档不存在）。
    MemGetBytes(DocId, mpsc::Sender<Option<Vec<u8>>>),
    /// 读回某页注释（only-read，pdfium-render 高层 `page.annotations()`，供测试断言）。
    /// 低优先级，与 Text/Search 同位（不抢栅格化）。
    ReadAnnotations(DocId, usize, mpsc::Sender<Result<Vec<AnnotationInfo>>>),
}

/// 进程级单例工作线程句柄。所有 [`PdfViewer`] 通过克隆它共享同一个 pdfium 线程。
#[derive(Clone)]
pub struct PdfWorker {
    tx: mpsc::Sender<WorkerMsg>,
}

/// 全局单例：第一次调用 [`open_and_dims`](PdfWorker::open_and_dims) 时启动并持有。
static SHARED: OnceLock<PdfWorker> = OnceLock::new();

/// 取进程级单例工作线程的句柄；首次调用时启动线程并完成唯一一次 pdfium 绑定。
fn shared_worker() -> Result<&'static PdfWorker> {
    if let Some(w) = SHARED.get() {
        return Ok(w);
    }
    let dll = pdfium_dll_path()?.clone();
    let (tx, rx) = mpsc::channel::<WorkerMsg>();
    std::thread::Builder::new()
        .name("pdfium-render".into())
        .spawn(move || PdfWorker::run(rx, dll))
        .context("无法启动 pdfium 渲染线程")?;
    Ok(SHARED.get_or_init(|| PdfWorker { tx }))
}

impl PdfWorker {
    /// 打开文档并取每页尺寸（结构解析，快）。成功则返回 (DocId, dims)，
    /// 便于调用方同时得到文档句柄与页面布局数据。书签目录由 [`Self::open_and_dims_full`] 取。
    pub fn open_and_dims(bytes: Vec<u8>) -> Result<(Self, DocId, PageDims)> {
        let (w, id, dims, _outlines) = Self::open_and_dims_full(bytes)?;
        Ok((w, id, dims))
    }

    /// 打开文档并取每页尺寸 + 书签目录（结构解析，快）。
    /// 成功则返回 (DocId, dims, outlines)，便于调用方同时得到文档句柄与页面布局、目录。
    pub fn open_and_dims_full(
        bytes: Vec<u8>,
    ) -> Result<(Self, DocId, PageDims, Vec<PdfOutlineEntry>)> {
        let worker = shared_worker()?;
        let (tx, rx) = mpsc::channel();
        let doc_id = next_doc_id();
        let _ = worker.tx.send(WorkerMsg::Open(doc_id, bytes, tx));
        let (dims, outlines) = match rx.recv() {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(anyhow::anyhow!("{e}")),
            Err(_) => return Err(anyhow::anyhow!("pdfium 工作线程已退出")),
        };
        Ok((worker.clone(), doc_id, dims, outlines))
    }

    /// 请求渲染某页（异步）。生产用于「首屏整页锐化封面」（廉价密度一渲铺满首屏，
    /// 见 `view.rs` 的封面调度），测试亦用于回归/基准比对。返回接收结果的通道。
    pub fn request_render(&self, doc_id: DocId, req: RenderRequest) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::Render(doc_id, req, tx));
        rx
    }

    /// 请求渲染某页的一格瓦片（异步、主内容路径）。返回接收结果的通道。
    pub fn request_tile(&self, doc_id: DocId, req: TileRequest) -> mpsc::Receiver<Option<TileResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::RenderTile(doc_id, req, tx));
        rx
    }

    /// 请求某页的 L0 缩略图（异步、低清晰度）。worker 按目标宽度推导低缩放，
    /// 结果与全尺寸同构（`RenderResult`），供阅读器做拖动占位 / 回翻即时内容。
    pub fn request_thumb(&self, doc_id: DocId, page: usize) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::Thumb(doc_id, page, tx));
        rx
    }

    /// 请求某页的 L0.5 中清页（异步、~520px × DPI）。优先级介于瓦片/全尺寸与缩略图之间：
    /// 快速拖动时用中清页保证可辨识度。`scale_factor` 参与目标宽度推导（高 DPI 屏上
    /// 预览仍锐利）。
    pub fn request_mid(
        &self,
        doc_id: DocId,
        page: usize,
        scale_factor: f32,
    ) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::Mid(doc_id, page, scale_factor, tx));
        rx
    }

    /// 关闭文档，释放 pdfium 侧资源（阅读器关闭时调用）。
    pub fn close(&self, doc_id: DocId) {
        let _ = self.tx.send(WorkerMsg::Close(doc_id));
    }

    /// 请求某页文本层数据（异步、低优先级）。返回接收结果的通道。
    ///
    /// 只应在用户开始选中/搜索时按需调用（懒加载：不交互则零开销）。
    /// 调用方须在结果到达后自行判断是否仍需要：滚动/缩放已使页面过期时直接丢弃
    /// （P2 会绑定 generation 校验，此处只建立契约）。
    pub fn request_text(&self, doc_id: DocId, page: usize) -> mpsc::Receiver<Option<PageText>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::Text(doc_id, page, tx));
        rx
    }

    /// 请求文档级检索（异步、低优先级；全页文本层扫描，命中上限 [`SEARCH_MAX_HITS`]）。
    /// 返回接收结果的通道；文档不存在 → `None`。
    pub fn search(&self, doc_id: DocId, needle: String) -> mpsc::Receiver<Option<PdfSearchResults>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::Search(doc_id, needle, tx));
        rx
    }

    // ---- Phase A：写回 / 页操作 / 注释读回（同步等 worker 结果；FPDF 调用全在 worker 线程）----

    /// 把一批注释写回 PDF 文件并落盘（worker 内全流程；成功后更新内存镜像）。
    /// 成功回传实际写入的注释条数；`output_path` 为用户当前打开的源文件路径。
    pub fn annotate(
        &self,
        doc_id: DocId,
        items: Vec<AnnotateItem>,
        output_path: PathBuf,
    ) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::Annotate(doc_id, items, output_path, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 在 `at_index` 处插入一页空白页并落盘（`at_index` 0 基；= 当前页数即末尾追加；
    /// viewer 侧「当前页后插入」传 `page_index + 1`）。成功回传**新页数**。
    pub fn insert_blank_page(
        &self,
        doc_id: DocId,
        at_index: usize,
        width_pt: f32,
        height_pt: f32,
        output_path: PathBuf,
    ) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::InsertBlankPage(doc_id, at_index, width_pt, height_pt, output_path, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 删除第 `index` 页并落盘（仅剩 1 页 / 越界 → Err）。成功回传**新页数**。
    pub fn delete_page(&self, doc_id: DocId, index: usize, output_path: PathBuf) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::DeletePage(doc_id, index, output_path, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 把第 `index` 页提取成独立 PDF 文件（`out_path` 原子替换）。
    pub fn extract_page_to(
        &self,
        doc_id: DocId,
        index: usize,
        out_path: PathBuf,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::ExtractPage(doc_id, index, out_path, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 显式把当前内存 bytes 重新编码落盘到 `path`（无内容修改的另存/刷新）。
    pub fn save(&self, doc_id: DocId, path: PathBuf) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::Save(doc_id, path, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    // ---- 待保存模型（P1）：内存工作副本操作——只改 stores、不碰磁盘 ----

    /// **内存级**插入空白页（`at_index` 0 基；= 当前页数即末尾追加）：只改 worker 内
    /// 内存工作副本并回传 [`MemOpResult`]（before/after/count），**不写盘**。
    /// viewer 拿到 `after` 做内存级重载（页面操作待存语义）。
    pub fn mem_insert_blank_page(
        &self,
        doc_id: DocId,
        at_index: usize,
        width_pt: f32,
        height_pt: f32,
    ) -> Result<MemOpResult> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::MemOpInsert(doc_id, at_index, width_pt, height_pt, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// **内存级**删除第 `index` 页（仅剩 1 页 / 越界 → Err）：只改 stores、不写盘。
    pub fn mem_delete_page(&self, doc_id: DocId, index: usize) -> Result<MemOpResult> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::MemOpDelete(doc_id, index, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 用给定 bytes 直接替换 stores 内存镜像（撤销「页面操作」用；零 pdfium 操作）。
    pub fn mem_restore_bytes(&self, doc_id: DocId, bytes: Vec<u8>) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::MemRestoreBytes(doc_id, bytes, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 读回 stores 当前内存镜像（`None` = 文档不存在；测试 MD5 / 撤销快照 / 另存用）。
    pub fn mem_bytes(&self, doc_id: DocId) -> Option<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::MemGetBytes(doc_id, tx));
        rx.recv().ok().flatten()
    }

    /// 读回某页的注释摘要（类型 + quad 点坐标；只读 pdfium-render 高层 API）。
    /// 供测试断言与 Phase B 展示已写回注释。
    pub fn annotations_for_page(
        &self,
        doc_id: DocId,
        page_index: usize,
    ) -> Result<Vec<AnnotationInfo>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WorkerMsg::ReadAnnotations(doc_id, page_index, tx))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 工作线程主体（进程内唯一）：绑定 pdfium 一次 → 循环处理命令。
    fn run(rx: mpsc::Receiver<WorkerMsg>, dll: PathBuf) {
        let bindings = match Pdfium::bind_to_library(&dll) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("pdfium bind failed: {e}");
                return;
            }
        };
        let pdfium = Pdfium::new(bindings);
        // 借出进程级 bindings（同一原生库句柄）：写回/页操作的低层 FPDF_* 调用都走它，
        // 与 PdfDocument 渲染共用同一 pdfium 实例（单工作线程不变式，见模块头注释）。
        let fpdf: &dyn PdfiumLibraryBindings = PdfiumLibraryBindingsAccessor::bindings(&pdfium);
        // 文档存活期 = `pdfium` 存活期；所有 `PdfDocument<'a>` 都借用上面的 `pdfium`。
        let mut docs: HashMap<DocId, PdfDocument<'_>> = HashMap::new();
        // 各文档在内存中的源 bytes（与 `docs` 同生命周期）：写回/提取/另存时把 bytes
        // 载入**自有 raw 句柄**做修改再落盘，不动正在渲染的 PdfDocument；落盘后同步镜像，
        // 保证后续操作基于最新内容（重开显示由 viewer 的 Close→Open 负责）。
        let mut stores: HashMap<DocId, Vec<u8>> = HashMap::new();
        // 本地优先级队列：按类分桶（render > mid > thumb > 其余），桶内 FIFO——
        // **Render 永不排在 Text 之后**（滚动画帧的栅格化优先于懒加载的文本提取），
        // 同优先级保持到达顺序。替代原「单队 + 每轮 `iter().position()+remove(i)`」的
        // O(n) 扫描抽取；调度语义（严格类优先级 + 同类 FIFO）与原实现完全一致。
        let mut render_q: VecDeque<WorkerMsg> = VecDeque::new();
        let mut mid_q: VecDeque<WorkerMsg> = VecDeque::new();
        let mut thumb_q: VecDeque<WorkerMsg> = VecDeque::new();
        let mut rest_q: VecDeque<WorkerMsg> = VecDeque::new();
        // 在途的全文档检索（分片续跑）：worker 无渲染/占位积压时每轮扫一片（几页/几 ms），
        // 保证检索期间到达的 Render/RenderTile 可以插队先跑——否则一次全量扫描会冻结所有
        // 瓦片/封面（87 页文本 PDF 实测检索 ~440ms，期间渲染被整段堵住 → 随机页先模糊）。
        let mut pending_search: Option<PendingSearch> = None;
        loop {
            // 现积压一次性分桶入队（每桶 FIFO；跨类优先序由下方「先取高优先桶」体现）。
            while let Ok(msg) = rx.try_recv() {
                enqueue_msg(msg, &mut render_q, &mut mid_q, &mut thumb_q, &mut rest_q);
            }
            // 1) 渲染永远最优先：全尺寸 Render / RenderTile（瓦片与整页同为视口内容），
            //    即使渲染消息排在检索/文本之后到，也会先从队列里抽出来执行。
            if let Some(msg) = render_q.pop_front() {
                match msg {
                    WorkerMsg::Render(doc_id, req, tx) => {
                        let result = docs.get(&doc_id).and_then(|doc| render_page(doc, req));
                        let _ = tx.send(result);
                    }
                    WorkerMsg::RenderTile(doc_id, req, tx) => {
                        let result = docs.get(&doc_id).and_then(|doc| render_tile(doc, req));
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("render_q 只入 Render/RenderTile"),
                }
                continue;
            }
            // 2) LOD 占位：Mid（中清）> Thumb（缩略），低于渲染、高于其余。
            if let Some(msg) = mid_q.pop_front() {
                match msg {
                    WorkerMsg::Mid(doc_id, page, sf, tx) => {
                        let result = docs.get(&doc_id).and_then(|doc| render_mid(doc, page, sf));
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("mid_q 只入 Mid"),
                }
                continue;
            }
            if let Some(msg) = thumb_q.pop_front() {
                match msg {
                    WorkerMsg::Thumb(doc_id, page, tx) => {
                        let result = docs.get(&doc_id).and_then(|doc| render_thumb(doc, page));
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("thumb_q 只入 Thumb"),
                }
                continue;
            }
            // 3) 其余 FIFO：Open / Close / Text / Search / 写回·页操作·注释读回。
            //    Search 只登记成分片状态、不在此阻塞；写回类（Annotate/Insert/Delete/Extract/Save）
            //    与注释读回属低频结构操作，放这里不抢栅格化（Render 总是最先出队）。
            if let Some(msg) = rest_q.pop_front() {
                match msg {
                    WorkerMsg::Open(doc_id, bytes, tx) => {
                        let t0 = std::time::Instant::now();
                        // 先克隆一份源 bytes 存内存镜像（写回/提取用），原始 bytes 移给渲染 doc
                        let store_bytes = bytes.clone();
                        let result = match pdfium.load_pdf_from_byte_vec(bytes, None) {
                            Ok(doc) => {
                                let t1 = std::time::Instant::now();
                                // 优先用 FPDF_GetPageSizeByIndexF（页树 MediaBox，**不加载页对象**，
                                // 免去逐页 Resources/字体解析，实测 87 页 284ms → ~5ms）；个别 PDF
                                // 该 API 失败时回退逐页加载量尺寸。
                                let dims: Vec<(f32, f32)> = match doc.pages().page_sizes() {
                                    Ok(sizes) => sizes
                                        .into_iter()
                                        .map(|r| (r.width().value, r.height().value))
                                        .collect(),
                                    Err(_) => doc
                                        .pages()
                                        .iter()
                                        .map(|p| (p.width().value, p.height().value))
                                        .collect(),
                                };
                                let t2 = std::time::Instant::now();
                                let outlines = extract_outline(&doc);
                                let t3 = std::time::Instant::now();
                                if timing() {
                                    eprintln!(
                                        "[{:.1}ms] [OPEN-STRUCT] load={:.1}ms dims({})={:.1}ms outline={:.1}ms total={:.1}ms",
                                        crate::pdf::timing_ms(),
                                        t1.duration_since(t0).as_secs_f64() * 1e3,
                                        dims.len(),
                                        t2.duration_since(t1).as_secs_f64() * 1e3,
                                        t3.duration_since(t2).as_secs_f64() * 1e3,
                                        t3.duration_since(t0).as_secs_f64() * 1e3,
                                    );
                                }
                                docs.insert(doc_id, doc);
                                stores.insert(doc_id, store_bytes);
                                Ok((dims, outlines))
                            }
                            Err(e) => Err(anyhow::anyhow!("PDF 解析失败：{e}")),
                        };
                        let _ = tx.send(result);
                    }
                    WorkerMsg::Close(doc_id) => {
                        docs.remove(&doc_id);
                        stores.remove(&doc_id);
                    }
                    WorkerMsg::Text(doc_id, page, tx) => {
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| extract_page_text(doc, page));
                        let _ = tx.send(result);
                    }
                    WorkerMsg::Search(doc_id, needle, tx) => {
                        if needle.is_empty() {
                            let _ = tx.send(Some(PdfSearchResults {
                                hits: Vec::new(),
                                truncated: false,
                            }));
                        } else if let Some(doc) = docs.get(&doc_id) {
                            // 只登记成分片状态：后续每轮在「步骤 4」分片执行，不整段占用 worker
                            pending_search = Some(PendingSearch {
                                doc_id,
                                needle: needle.chars().collect(),
                                next_page: 0,
                                page_count: doc.pages().len().max(0) as usize,
                                hits: Vec::new(),
                                truncated: false,
                                tx,
                            });
                        } else {
                            let _ = tx.send(None);
                        }
                    }
                    // ---- 写回 / 页操作 / 注释读回（全部 worker 单线程内，见各 handler）----
                    WorkerMsg::Annotate(doc_id, items, out, tx) => {
                        let result = writeback_edit(&mut stores, fpdf, doc_id, &out, |b, raw| {
                            apply_annotations_raw(b, raw, &items)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::InsertBlankPage(doc_id, at, w, h, out, tx) => {
                        let result = writeback_edit(&mut stores, fpdf, doc_id, &out, |b, raw| {
                            let page = unsafe { b.FPDFPage_New(raw, at as c_int, w as f64, h as f64) };
                            if page.is_null() {
                                return Err(anyhow!("插入空白页失败（at={at} {w:.1}x{h:.1}pt）"));
                            }
                            // 新页句柄须 Close 释放（同 FPDF_LoadPage 的对称语义）
                            unsafe { b.FPDF_ClosePage(page) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::DeletePage(doc_id, index, out, tx) => {
                        // 越界/单页守卫以**实际待写 bytes**（stores←raw 句柄页数）为准，而非
                        // `docs`（渲染文档可能在连环操作中滞后于 stores，旧计数会误判/漏判）。
                        let result = writeback_edit(&mut stores, fpdf, doc_id, &out, |b, raw| {
                            let count = unsafe { b.FPDF_GetPageCount(raw) } as usize;
                            if count <= 1 {
                                return Err(anyhow!("不能删除仅剩的 1 页（当前 {count} 页）"));
                            }
                            if index >= count {
                                return Err(anyhow!("页码越界：删除第 {index} 页（共 {count} 页）"));
                            }
                            unsafe { b.FPDFPage_Delete(raw, index as c_int) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::ExtractPage(doc_id, index, out, tx) => {
                        let result = (|| -> Result<()> {
                            let bytes = stores
                                .get(&doc_id)
                                .ok_or_else(|| anyhow!("文档 {doc_id} 不存在"))?
                                .clone();
                            unsafe {
                                let raw_src = load_raw_doc(fpdf, &bytes)?;
                                let raw_dst = fpdf.FPDF_CreateNewDocument();
                                if raw_dst.is_null() {
                                    fpdf.FPDF_CloseDocument(raw_src);
                                    return Err(anyhow!("FPDF_CreateNewDocument 失败"));
                                }
                                // pagerange 为 1 基页码串；index=0 表示插入到新文档开头
                                let range = (index + 1).to_string();
                                let ok = fpdf.FPDF_ImportPages(raw_dst, raw_src, &range, 0);
                                if !fpdf.is_true(ok) {
                                    fpdf.FPDF_CloseDocument(raw_dst);
                                    fpdf.FPDF_CloseDocument(raw_src);
                                    return Err(anyhow!("提取第 {index} 页失败"));
                                }
                                let res = save_raw_doc_to_file(fpdf, raw_dst, &out);
                                fpdf.FPDF_CloseDocument(raw_dst);
                                fpdf.FPDF_CloseDocument(raw_src);
                                res
                            }
                        })();
                        let _ = tx.send(result);
                    }
                    WorkerMsg::Save(doc_id, out, tx) => {
                        let result = (|| -> Result<()> {
                            let bytes = stores
                                .get(&doc_id)
                                .ok_or_else(|| anyhow!("文档 {doc_id} 不存在"))?
                                .clone();
                            unsafe {
                                let raw = load_raw_doc(fpdf, &bytes)?;
                                let res = save_raw_doc_to_file(fpdf, raw, &out);
                                fpdf.FPDF_CloseDocument(raw);
                                res
                            }
                        })();
                        let _ = tx.send(result);
                    }
                    // 待保存模型：内存级页面操作 / 镜像替换——只改 stores，不写盘。
                    WorkerMsg::MemOpInsert(doc_id, at, w, h, tx) => {
                        let result = writeback_edit_mem(&mut stores, fpdf, doc_id, |b, raw| {
                            let page = unsafe { b.FPDFPage_New(raw, at as c_int, w as f64, h as f64) };
                            if page.is_null() {
                                return Err(anyhow!("插入空白页失败（at={at} {w:.1}x{h:.1}pt）"));
                            }
                            unsafe { b.FPDF_ClosePage(page) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemOpDelete(doc_id, index, tx) => {
                        let result = writeback_edit_mem(&mut stores, fpdf, doc_id, |b, raw| {
                            let count = unsafe { b.FPDF_GetPageCount(raw) } as usize;
                            if count <= 1 {
                                return Err(anyhow!("不能删除仅剩的 1 页（当前 {count} 页）"));
                            }
                            if index >= count {
                                return Err(anyhow!("页码越界：删除第 {index} 页（共 {count} 页）"));
                            }
                            unsafe { b.FPDFPage_Delete(raw, index as c_int) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemRestoreBytes(doc_id, bytes, tx) => {
                        let result = if stores.contains_key(&doc_id) {
                            stores.insert(doc_id, bytes);
                            Ok(stores.get(&doc_id).map(|b| b.len()).unwrap_or(0))
                        } else {
                            Err(anyhow!("文档 {doc_id} 不存在（无法恢复镜像）"))
                        };
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemGetBytes(doc_id, tx) => {
                        let r = stores.get(&doc_id).cloned();
                        let _ = tx.send(r);
                    }
                    WorkerMsg::ReadAnnotations(doc_id, page, tx) => {
                        let result = match docs.get(&doc_id) {
                            Some(doc) => read_annotations(doc, page)
                                .ok_or_else(|| anyhow!("文档或第 {page} 页不存在")),
                            None => Err(anyhow!("文档 {doc_id} 不存在")),
                        };
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("rest_q 只入 Open/Close/Text/Search/写回/页操作/注释读回"),
                }
                continue;
            }
            // 4) 无渲染/占位/其他积压时，跑一片在途检索再回主循环（新渲染消息可插队）。
            //    每片至多数页/数 ms，因此检索期间的前台渲染冻结有上界，不再拖到全文扫完。
            if let Some(mut ps) = pending_search.take() {
                search_slice(&mut docs, &mut ps);
                if ps.is_done() {
                    let _ = ps.tx.send(Some(PdfSearchResults {
                        hits: ps.hits,
                        truncated: ps.truncated,
                    }));
                } else {
                    pending_search = Some(ps);
                }
                continue;
            }
            // 5) 真空闲：阻塞等新消息（只有在既无排队消息也无在途检索时才会真阻塞）。
            match rx.recv() {
                Ok(msg) => enqueue_msg(msg, &mut render_q, &mut mid_q, &mut thumb_q, &mut rest_q),
                Err(_) => break, // 所有发送端已关闭（全部阅读器已释放）
            }
        }
    }
}

/// 把工作消息按优先级类分桶（render > mid > thumb > rest），桶内保持到达顺序（FIFO）。
/// 分桶后各桶互不交错：严格类优先级 + 同类 FIFO 与原「单队 + position/remove 扫描」的
/// 调度语义完全一致，只是每轮取舍从 O(n) 扫描降为 O(1) 取桶头。
fn enqueue_msg(
    msg: WorkerMsg,
    render_q: &mut VecDeque<WorkerMsg>,
    mid_q: &mut VecDeque<WorkerMsg>,
    thumb_q: &mut VecDeque<WorkerMsg>,
    rest_q: &mut VecDeque<WorkerMsg>,
) {
    match msg {
        WorkerMsg::Render(..) | WorkerMsg::RenderTile(..) => render_q.push_back(msg),
        WorkerMsg::Mid(..) => mid_q.push_back(msg),
        WorkerMsg::Thumb(..) => thumb_q.push_back(msg),
        _ => rest_q.push_back(msg),
    }
}

// ---- 写回 / 页操作（Phase A）：worker 单线程内的低层 FPDF 调用 ----
//
// 关键原则（单工作线程不变式，勿破坏）：所有 FPDF_* 调用（含 FPDF_SaveAsCopy /
// FPDFPage_CreateAnnot / FPDFPage_New/Delete / FPDF_ImportPages）全部发生在 worker
// 线程内、经进程级 bindings（`fpdf`）进行；viewer 线程只发消息收结果，绝不触碰 FPDF。
//
// 原生句柄生命周期：
//   FPDF_LoadMemDocument64 → FPDF_DOCUMENT（结束须 FPDF_CloseDocument）
//   FPDF_LoadPage → FPDF_PAGE（结束须 FPDF_ClosePage）
//   FPDFPage_CreateAnnot → FPDF_ANNOTATION（结束须 FPDFPage_CloseAnnot）
//   FPDF_CreateNewDocument → FPDF_DOCUMENT（结束须 FPDF_CloseDocument，含失败分支）
// 任一环节失败都在归还前 Close 已打开的更内层句柄（防柄泄漏；参照 pdfium-render drop 语义）。

/// 取 pdfium 最近一次错误码的简短描述（诊断用）。
fn fpdf_last_error(bindings: &dyn PdfiumLibraryBindings) -> String {
    let code = unsafe { bindings.FPDF_GetLastError() };
    match code {
        0 => "未知".into(),
        v => format!("0x{v:x}"),
    }
}

/// 把内存 bytes 载入**自有 raw 句柄**文档（写回/提取的独立副本，不动渲染中 PdfDocument）。
unsafe fn load_raw_doc(
    bindings: &dyn PdfiumLibraryBindings,
    bytes: &[u8],
) -> Result<FPDF_DOCUMENT> {
    let h = unsafe { bindings.FPDF_LoadMemDocument64(bytes, None) };
    if h.is_null() {
        return Err(anyhow!("载入 PDF 失败（FPDF_GetLastError={}）", fpdf_last_error(bindings)));
    }
    Ok(h)
}

/// 自制 `FPDF_FILEWRITE` 回调结构（与 pdfium-render 的 `FpdfFileWriteExt` / bindgen
/// `FPDF_FILEWRITE_` 同内存布局：`version` + `write_block` 前置两字段，pdfium 只读这两
/// 字段；后缀自定义 `file` 字段由回调经指针还原访问）。禁止改动首两字段的类型/顺序。
#[repr(C)]
struct RawFileWrite<'a> {
    version: c_int,
    write_block: Option<unsafe extern "C" fn(*mut FPDF_FILEWRITE, *const c_void, c_ulong) -> c_int>,
    file: &'a mut std::fs::File,
}

/// FPDF_SaveAsCopy 的写块回调：把 pdfium 产出的字节流直接写进目标临时文件（同步立即）。
unsafe extern "C" fn raw_file_write_block(
    this_: *mut FPDF_FILEWRITE,
    data: *const c_void,
    size: c_ulong,
) -> c_int {
    let this = unsafe { &mut *(this_ as *mut RawFileWrite) };
    match this
        .file
        .write_all(unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) })
    {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

/// 写回目标「被占用/只读」时的可操作指引文案（R1 专用）：唯一判据 = 用户可读句子，
/// 供 [`is_writeback_busy_error`] 匹配 `last_modify_error`（改措辞须同步测试与 shell 层）。
const WRITEBACK_BUSY_INSTRUCTION: &str =
    "PDF 正被其他程序打开或文件只读；请关闭占用它的程序后重试";

/// 判断 `last_modify_error` 文案是否属「目标文件被其他程序占用/只读」（shell 层 toast
/// 据此补第二条路：另存为新副本）。
pub fn is_writeback_busy_error(msg: &str) -> bool {
    msg.contains(WRITEBACK_BUSY_INSTRUCTION)
}

/// Windows `rename` 覆盖被占用（无 FILE_SHARE_DELETE）/只读目标返回 ERROR_ACCESS_DENIED
/// （std 映射 `io::ErrorKind::PermissionDenied`）：杀软/预览器可能瞬时锁住 → 短重试；
/// 常驻占用与非「被占用/只读」类错误（磁盘满/网络/目录无权限）不重试、直接上抛。
fn is_access_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5)
}

/// 原子替换前的短重试（R2）：首跑 + ≤2 次重试、间隔 50ms（应对杀软/预览器瞬时锁）。
/// 重试只对「被占用/只读」类错误生效；重试耗尽即最终失败，交 [`map_write_io_error`] 呈现。
fn rename_with_retry(tmp: &Path, target: &Path) -> std::io::Result<()> {
    const EXTRA_RETRIES: usize = 2;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
    for attempt in 0..=EXTRA_RETRIES {
        match std::fs::rename(tmp, target) {
            Ok(()) => return Ok(()),
            Err(e) if is_access_denied(&e) && attempt < EXTRA_RETRIES => {
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("重试循环必然 return")
}

/// 把 rename/写盘 io 错误映射成用户可操作中文（R1）：ERROR_ACCESS_DENIED（被占用/只读）
/// → 专属指引（注明不破坏原文件）；其余 io 错误保原文案、前缀「写回 PDF 失败：」。
fn map_write_io_error(e: std::io::Error) -> String {
    if is_access_denied(&e) {
        format!("{WRITEBACK_BUSY_INSTRUCTION}（原文件未受影响、不会被破坏；修改仍在本应用内）")
    } else {
        format!("写回 PDF 失败：{e}")
    }
}

/// 原子替换提交（R0a）：rename 成功则临时文件已随操作移走；失败则清理临时文件
/// （防 `.pid.seq.tmp.pdf` 点文件残留）并映射错误文案后上抛。
fn commit_rename(tmp: &Path, target: &Path) -> Result<()> {
    match rename_with_retry(tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            Err(anyhow!("{}", map_write_io_error(e)))
        }
    }
}

/// 自制 `FPDF_FILEWRITE` 回调结构（写进 `Vec<u8>` 内存缓冲）：与 [`RawFileWrite`] 同
/// 内存布局（`version` + `write_block` 前置两字段），只把产出字节追加进缓冲而非文件。
/// 供「待保存」内存工作副本的页面操作（插页/删页/撤销恢复）使用——不动磁盘。
#[repr(C)]
struct RawFileWriteVec<'a> {
    version: c_int,
    write_block: Option<unsafe extern "C" fn(*mut FPDF_FILEWRITE, *const c_void, c_ulong) -> c_int>,
    buf: &'a mut Vec<u8>,
}

/// `FPDF_SaveAsCopy` 的写块回调：把 pdfium 产出的字节流直接追加进内存 `Vec<u8>`。
unsafe extern "C" fn raw_file_write_vec_block(
    this_: *mut FPDF_FILEWRITE,
    data: *const c_void,
    size: c_ulong,
) -> c_int {
    let this = unsafe { &mut *(this_ as *mut RawFileWriteVec) };
    this.buf
        .extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) });
    1
}

/// 把 raw 句柄文档重新编码成内存 bytes（等效 [`save_raw_doc_to_file`] 但零磁盘）。
/// 供 worker 内存工作副本（stores）的页面操作产出新镜像，由 viewer 内存级重载。
unsafe fn save_raw_doc_to_bytes(
    bindings: &dyn PdfiumLibraryBindings,
    doc_handle: FPDF_DOCUMENT,
) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut w = RawFileWriteVec {
        version: 1,
        write_block: Some(raw_file_write_vec_block),
        buf: &mut buf,
    };
    let ok = unsafe {
        bindings.FPDF_SaveAsCopy(doc_handle, (&mut w as *mut RawFileWriteVec).cast(), 0)
    };
    drop(w); // 结束对 buf 的借用；回调已同步返回
    if !bindings.is_true(ok) {
        return Err(anyhow!("FPDF_SaveAsCopy 失败（err={}）", fpdf_last_error(bindings)));
    }
    Ok(buf)
}

/// 把 raw 句柄文档保存到 `target`：先写**同目录临时文件**，成功后再原子替换目标
/// （Windows 上 `std::fs::rename` = `MoveFileExW + MOVEFILE_REPLACE_EXISTING`，可覆盖已存
/// 文件）。任一步失败都保留原目标文件完好（先落盘再替换，防中途损坏原文件）。
unsafe fn save_raw_doc_to_file(
    bindings: &dyn PdfiumLibraryBindings,
    doc_handle: FPDF_DOCUMENT,
    target: &Path,
) -> Result<()> {
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::env::temp_dir(),
    };
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    // 同目录 . 前缀隐藏文件，随进程 id + 序号唯一；同名写回/重载无碰撞风险
    let tmp = dir.join(format!(".{}.{seq}.tmp.pdf", std::process::id()));
    let mut file = std::fs::File::create(&tmp)?;
    let mut w = RawFileWrite {
        version: 1,
        write_block: Some(raw_file_write_block),
        file: &mut file,
    };
    let ok = unsafe { bindings.FPDF_SaveAsCopy(doc_handle, (&mut w as *mut RawFileWrite).cast(), 0) };
    drop(w); // 结束对 file 的借用；回调已同步返回
    file.sync_all().ok();
    drop(file); // 释放句柄后才能 rename（Windows 语义）
    if !bindings.is_true(ok) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("FPDF_SaveAsCopy 失败（err={}）", fpdf_last_error(bindings)));
    }
    // rename 成功 → 临时文件随操作移走；失败（被占用/只读/磁盘满等） → commit_rename
    // 内清理临时文件并映射成用户可操作的中文文案（防 `.pid.seq.tmp.pdf` 点文件残留）。
    commit_rename(&tmp, target)
}

/// worker 内“载入 raw 句柄 → edit 修改 → **内存**产出新 bytes → 更新 stores 镜像”。
///
/// 待保存模型（P1）的页面操作：只改 worker 内内存工作副本（stores，`FPDF_SaveAsCopy`
/// 到 `Vec`），**绝不碰磁盘**。`before` = 操作前 stores 快照（撤销恢复用），`after` =
/// 操作后新镜像，两者都由 viewer 持有/消费。
fn writeback_edit_mem(
    stores: &mut HashMap<DocId, Vec<u8>>,
    bindings: &dyn PdfiumLibraryBindings,
    doc_id: DocId,
    edit: impl FnOnce(&dyn PdfiumLibraryBindings, FPDF_DOCUMENT) -> Result<usize>,
) -> Result<MemOpResult> {
    let before = stores
        .get(&doc_id)
        .ok_or_else(|| anyhow!("文档 {doc_id} 不存在（无法内存操作）"))?
        .clone();
    unsafe {
        let raw = load_raw_doc(bindings, &before)?;
        let count = match edit(bindings, raw) {
            Ok(n) => n,
            Err(e) => {
                bindings.FPDF_CloseDocument(raw);
                return Err(e);
            }
        };
        let after = match save_raw_doc_to_bytes(bindings, raw) {
            Ok(b) => b,
            Err(e) => {
                bindings.FPDF_CloseDocument(raw);
                return Err(e);
            }
        };
        bindings.FPDF_CloseDocument(raw);
        stores.insert(doc_id, after.clone());
        Ok(MemOpResult { before, after, count })
    }
}

/// 页面操作的结果载荷（[`WorkerMsg::MemOpInsert`] / [`WorkerMsg::MemOpDelete`]）：含操作前
/// 内存镜像（撤销 / 测试 MD5 用）、操作后新镜像（viewer 内存级重载）与新页数。
#[derive(Debug, Clone)]
pub struct MemOpResult {
    /// 操作前的内存工作副本（撤销恢复用）。
    pub before: Vec<u8>,
    /// 操作后的新内存工作副本（viewer 重载显示用）。
    pub after: Vec<u8>,
    /// 操作后的页数。
    pub count: usize,
}

/// worker 内“载入 raw 句柄 → edit 修改 → 落盘 → 收尾”的统一骨架。
///
/// - `edit` 闭包在 raw 句柄上做修改并返回「结果数」（注释条数 / 新页数）；
/// - 成功路径：`FPDF_SaveAsCopy` 落到 `output_path`（原子替换），然后**更新内存 bytes**
///   镜像（后续操作基于新内容）；
/// - 失败路径：关闭 raw 句柄后原样上抛，目标文件不受影响。
fn writeback_edit(
    stores: &mut HashMap<DocId, Vec<u8>>,
    bindings: &dyn PdfiumLibraryBindings,
    doc_id: DocId,
    output_path: &Path,
    edit: impl FnOnce(&dyn PdfiumLibraryBindings, FPDF_DOCUMENT) -> Result<usize>,
) -> Result<usize> {
    let bytes = stores
        .get(&doc_id)
        .ok_or_else(|| anyhow!("文档 {doc_id} 不存在（无法写回）"))?
        .clone();
    unsafe {
        let raw = load_raw_doc(bindings, &bytes)?;
        let result_out = match edit(bindings, raw) {
            Ok(n) => {
                if let Err(e) = save_raw_doc_to_file(bindings, raw, output_path) {
                    bindings.FPDF_CloseDocument(raw);
                    return Err(e);
                }
                // 读回已写入的新文件 bytes 作内存镜像（后续写回/重载基于新内容）
                if let Ok(new_bytes) = std::fs::read(output_path) {
                    stores.insert(doc_id, new_bytes);
                }
                n
            }
            Err(e) => {
                bindings.FPDF_CloseDocument(raw);
                return Err(e);
            }
        };
        bindings.FPDF_CloseDocument(raw);
        Ok(result_out)
    }
}

/// 在 raw 句柄文档上逐条创建标准注释（Highlight / StrikeOut / Underline）并回填几何。
///
/// 坐标约定：`QuadsPt` 为 PDF 用户空间坐标（y 向上、原点页左下）。PDF 的 QuadPoints
/// 顶点序按 pdfium-render 官方姿势（自底左起逆时针：BL→BR→TR→TL，见其
/// create_squiggly_annotation_under_object）；注释外接矩形 = 全部 quad 的包围盒。
fn apply_annotations_raw(
    bindings: &dyn PdfiumLibraryBindings,
    doc_handle: FPDF_DOCUMENT,
    items: &[AnnotateItem],
) -> Result<usize> {
    for item in items {
        let page = unsafe { bindings.FPDF_LoadPage(doc_handle, item.page_index as c_int) };
        if page.is_null() {
            return Err(anyhow!("写回注释：无法加载第 {} 页", item.page_index));
        }
        let annot = unsafe { bindings.FPDFPage_CreateAnnot(page, item.kind.fpdf_subtype()) };
        if annot.is_null() {
            unsafe { bindings.FPDF_ClosePage(page) };
            return Err(anyhow!(
                "写回注释：FPDFPage_CreateAnnot 失败（第 {} 页）",
                item.page_index
            ));
        }
        // 默认色（Phase B 由 UI 定制）；注释级透明度 /CA：高亮半透明、删除线/下划线不透明
        let (r, g, b) = item.kind.default_color();
        let ca_alpha = if item.kind == AnnotKind::Highlight {
            APOBJ_HIGHLIGHT_A
        } else {
            255
        };
        unsafe {
            bindings.FPDFAnnot_SetColor(annot, FPDFANNOT_COLORTYPE_COLOR, r, g, b, ca_alpha);
        }
        // 外接矩形（FS_RECTF 字段为 left/top/right/bottom，PDF 用户空间 y 向上）
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut bottom = f32::INFINITY;
        let mut top = f32::NEG_INFINITY;
        for q in &item.quads {
            left = left.min(q.x0.min(q.x1));
            right = right.max(q.x0.max(q.x1));
            bottom = bottom.min(q.y0.min(q.y1));
            top = top.max(q.y0.max(q.y1));
        }
        if !(left.is_finite() && right.is_finite() && bottom.is_finite() && top.is_finite()) {
            unsafe { bindings.FPDFPage_CloseAnnot(annot) };
            unsafe { bindings.FPDF_ClosePage(page) };
            return Err(anyhow!("写回注释：quad 坐标非法（第 {} 页）", item.page_index));
        }
        let rect = FS_RECTF {
            left,
            top,
            right,
            bottom,
        };
        unsafe {
            bindings.FPDFAnnot_SetRect(annot, &rect);
            for q in &item.quads {
                let qp = FS_QUADPOINTSF {
                    x1: q.x0, y1: q.y0, // 左下
                    x2: q.x1, y2: q.y0, // 右下
                    x3: q.x1, y3: q.y1, // 右上
                    x4: q.x0, y4: q.y1, // 左上
                };
                bindings.FPDFAnnot_AppendAttachmentPoints(annot, &qp);
            }
            // 默认可见 + 随打印输出（PDF 标准 AnnotationFlag 的 Print 位）
            bindings.FPDFAnnot_SetFlags(annot, FPDF_ANNOT_FLAG_PRINT);
        }
        // 生成外观流（关键，诊断实证）：只设 Rect/QuadPoints/Color 的话，存盘后注释字典里
        // **没有 /AP**（文件里有 /Annots+/Subtype+/QuadPoints 但无 /AP），重新打开渲染时
        // pdfium 对无外观流的注释整页像素零差异；FPDFAnnot_AppendObject 仅支持 ink/stamp
        // 子类型（Highlight/Underline/StrikeOut 都会失败）→ 只能用 `FPDFAnnot_SetAP` 手写
        // 外观内容。实测本机 pdfium(.8035)：SetAP 生成的 /AP/N Form XObject 的 **BBox =
        // 注释矩形（页用户空间坐标）**、无 /Matrix，即内容按**页坐标**绘制（不是 BBox 的
        // 0..W×0..H 局部坐标）——若按局部坐标写会被裁剪在注释矩形外、整页像素零差异。
        let (fr, fg, fb) = item.kind.default_color();
        // 高亮半透明：pdfium 的 SetAP 已自动在 AP 的 /Resources 里生成
        // `ExtGState/GS (CA=.431)`（因为 SetColor 传了 alpha<255），内容里须显式 `/GS gs`
        // 激活它才会半透明（否则 ExtGState 定义但未被引用 = 纯色填充覆盖正文）；不透明类型无 gs。
        let mut content = String::from("q\n");
        if item.kind == AnnotKind::Highlight {
            content.push_str("/GS gs\n");
        }
        content.push_str(&format!(
            "{} {} {} rg\n",
            fr as f32 / 255.0,
            fg as f32 / 255.0,
            fb as f32 / 255.0
        ));
        for q in &item.quads {
            let (qx0, qy0) = (q.x0.min(q.x1), q.y0.min(q.y1));
            let (qx1, qy1) = (q.x0.max(q.x1), q.y0.max(q.y1));
            let (qw, qh) = ((qx1 - qx0).max(1.0), (qy1 - qy0).max(1.0));
            let (ly, lh) = match item.kind {
                // 高亮整块填充；删除线居中细条；下划线贴底——全部用页坐标
                AnnotKind::Highlight => (qy0, qh),
                AnnotKind::Strikeout => {
                    let c = (qy0 + qy1) * 0.5;
                    (c - APOBJ_LINE_H * 0.5, APOBJ_LINE_H)
                }
                AnnotKind::Underline => (qy0 - APOBJ_LINE_H * 0.25, APOBJ_LINE_H),
            };
            content.push_str(&format!("{} {} {} {} re f\n", qx0, ly, qw, lh));
        }
        content.push('Q');
        // FPDF_WIDESTRING（FPDFAnnot_SetAP 只收指针无长度）按 pdfium C 约定必须 C 风格
        // **u16 零结尾**（对照 pdfium-render utils::utf16le 注释「expects C-style null
        // termination」）——补尾零防 pdfium 端 wcslen 越过缓冲区读到随机零（隐性越界）。
        let mut wide: Vec<u16> = content.encode_utf16().collect();
        wide.push(0);
        let ok = unsafe {
            bindings.FPDFAnnot_SetAP(
                annot,
                0, // FPDF_ANNOT_APPEARANCEMODE_NORMAL
                wide.as_ptr(),
            )
        };
        if !bindings.is_true(ok) {
            unsafe {
                bindings.FPDFPage_CloseAnnot(annot);
                bindings.FPDF_ClosePage(page);
            }
            return Err(anyhow!("写回注释：FPDFAnnot_SetAP 失败（第 {} 页）", item.page_index));
        }
        unsafe {
            bindings.FPDFPage_CloseAnnot(annot);
            bindings.FPDF_ClosePage(page);
        }
    }
    Ok(items.len())
}

/// 删除线/下划线细条的视觉厚度（PDF 点，~1px@72dpi；与待存叠加层 `render` 的 ~1.5pt 对齐）。
const APOBJ_LINE_H: f32 = 1.5;
/// 高亮注释注释级透明度（/CA，0-255）：≈0.43 半透明，正文不被遮挡。
const APOBJ_HIGHLIGHT_A: u32 = 110;

/// 读回某页注释（only-read，走 pdfium-render 高层 `page.annotations()`；供测试断言/Phase B）。
/// 只摘 Highlight/Underline/Strikeout 的几何：每个附件点多边形折算成 quad 包围盒（PDF 点）。
fn read_annotations(doc: &PdfDocument<'_>, page_index: usize) -> Option<Vec<AnnotationInfo>> {
    use PdfPageAnnotationType as T;
    let page = doc.pages().get(page_index as i32).ok()?;
    let mut out = Vec::new();
    for a in page.annotations().iter() {
        let kind = match a.annotation_type() {
            T::Highlight => AnnotKind::Highlight,
            T::Underline => AnnotKind::Underline,
            T::Strikeout => AnnotKind::Strikeout,
            _ => continue,
        };
        let mut quads = Vec::new();
        for qp in a.attachment_points().iter() {
            let (l, r) = (qp.left().value.min(qp.right().value), qp.left().value.max(qp.right().value));
            let (b, t) = (qp.bottom().value.min(qp.top().value), qp.bottom().value.max(qp.top().value));
            quads.push(QuadPt {
                x0: l,
                y0: b,
                x1: r,
                y1: t,
            });
        }
        if quads.is_empty() {
            continue;
        }
        out.push(AnnotationInfo { kind, quads });
    }
    Some(out)
}


/// 全局自增文档 ID（进程级原子）。
fn next_doc_id() -> DocId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 从文档书签树提取自包含的目录条目（标题 + 层级 + 目标页码）。
///
/// 深度优先先序展开（pdfium `bookmarks().iter()` 即 DF 先序）。每个书签的层级 =
/// 其父链长度（`parent()` 链递推）；目标页码取书签指向的 destination，
/// 解析失败（如指向外部/无页码）记为 `usize::MAX`。
fn extract_outline(doc: &PdfDocument<'_>) -> Vec<PdfOutlineEntry> {
    let mut out = Vec::new();
    for bookmark in doc.bookmarks().iter() {
        let title = bookmark.title().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let page_index = bookmark
            .destination()
            .and_then(|d| d.page_index().ok().map(|i| i as usize))
            .unwrap_or(usize::MAX);
        // 层级 = 父链长度
        let mut level = 0usize;
        let mut parent = bookmark.parent();
        while let Some(p) = parent {
            level += 1;
            parent = p.parent();
        }
        out.push(PdfOutlineEntry {
            title,
            level,
            page_index,
        });
    }
    out
}

/// 在工作线程内：把单个页面栅格化，返回**紧凑 BGRA** 像素 + 尺寸。
///
/// 关键优化：pdfium 渲染的 bitmap 原生就是 BGRA（`PdfBitmapFormat::BGRA`，stride 恒为
/// `w*4`——见 `PdfBitmap::preferred_stride_bytes` 对 4 通道格式从不补对齐），
/// 直接 `as_raw_bytes()` 拷出即可，**零像素循环、零字节重排**。旧的
/// `as_image()` → `into_rgba8()` 会对每像素做 BGRA→RGBA 再一次 RGBA→BGRA 的浪费。
/// 是否开启 worker 渲染计时日志（`GPUI_EDITOR_PDF_TIMING=1`）：打印每次瓦片/整页渲染的
/// get_page 与 pdfium 光栅化耗时，便于在真实慢 PDF 上定位瓶颈（worker 侧）。
fn timing() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| {
        std::env::var_os("GPUI_EDITOR_PDF_TIMING").is_some_and(|v| v == "1" || v == "true")
    })
}

fn render_page(doc: &PdfDocument<'_>, req: RenderRequest) -> Option<RenderResult> {
    let page = doc.pages().get(req.page as i32).ok()?;
    let (pw, ph) = (page.width().value, page.height().value);
    let w = (pw * req.scale).ceil().max(1.0) as i32;
    let h = (ph * req.scale).ceil().max(1.0) as i32;
    let t1 = std::time::Instant::now();
    let bmp = page.render(w, h, None).ok()?;
    // pdfium-render 的 `PdfBitmap::render()` 用 `PdfBitmapFormat::default()`（= BGRA）建位图，
    // BGRA 的 stride 恒为 `width*4`（无对齐填充，见 preferred_stride_bytes）。
    // `as_raw_bytes()` 直接返回整块原生 BGRA 缓冲 → 主线程 `Frame::from_raw` 直接包裹。
    let bgra = bmp.as_raw_bytes();
    if timing() {
        eprintln!(
            "[{:.1}ms] [PAGE-RENDER] p{} scale={:.2} render={:.3}ms -> {}x{}",
            crate::pdf::timing_ms(),
            req.page,
            req.scale,
            t1.elapsed().as_secs_f64() * 1e3,
            w,
            h,
        );
    }
    Some(RenderResult {
        bgra,
        width: w as u32,
        height: h as u32,
    })
}

/// 在工作线程内：栅格化某页的一格瓦片（SumatraPDF 式分块渲染的主内容路径）。
///
/// 机制（与 pdfium-render 官方 `set_origin` 条带拼接测试同款）：给瓦片大小的
/// BGRA 缓冲，把页面以**整页位图尺寸**为参考缩放到 `full_w × full_h`，再用
/// 负 `set_origin(-x0, -y0)` 把页面左上角推到缓冲右上角——pdfium 渲染时自动
/// 裁剪到缓冲边界，于是只有 `[x0, x0+w) × [y0, y0+h)` 这一格被画进 ~1MB 的小缓冲。
///
/// 内存：每片恒 ≈ TILE_SIZE² × 4，**与缩放倍数无关**；同一页切成 视口可见 的几片，
/// 不再存在"4× 缩放一页 = 64MB 整页位图"的内存/density 上限墙。
fn render_tile(doc: &PdfDocument<'_>, req: TileRequest) -> Option<TileResult> {
    let t0 = std::time::Instant::now();
    let page = doc.pages().get(req.page as i32).ok()?;
    let t_get = std::time::Instant::now();
    let (pw, ph) = (page.width().value, page.height().value);
    let bmp = crate::pdf::layout::page_bitmap_size((pw, ph), req.scale);
    let (x0, y0, w, h) =
        crate::pdf::layout::tile_rect(req.col, req.row, req.tile_size, config::pdf::TILE_BLEED, bmp)?;
    let mut dst = PdfBitmap::empty(w as i32, h as i32, PdfBitmapFormat::BGRA).ok()?;
    let t1 = std::time::Instant::now();
    page.render_into_bitmap_with_config(
        &mut dst,
        &PdfRenderConfig::new()
            .set_fixed_size(bmp.0 as i32, bmp.1 as i32)
            .set_origin(-(x0 as i32), -(y0 as i32)),
    )
    .ok()?;
    let res = TileResult {
        page: req.page,
        row: req.row,
        col: req.col,
        x0,
        y0,
        w,
        h,
        full_w: bmp.0,
        full_h: bmp.1,
        bgra: dst.as_raw_bytes(),
    };
    if timing() {
        let end = std::time::Instant::now();
        eprintln!(
            "[{:.1}ms] [TILE-RENDER] p{} r{}c{} scale={:.2} get_page={:.3}ms render={:.3}ms",
            crate::pdf::timing_ms(),
            req.page,
            req.row,
            req.col,
            req.scale,
            t_get.duration_since(t0).as_secs_f64() * 1e3,
            end.duration_since(t1).as_secs_f64() * 1e3,
        );
    }
    Some(res)
}

/// 在工作线程内：栅格化某页的 **L0 缩略图**（低分辨率，用于拖动期间占位 / 回翻即时内容）。
///
/// 缩放按目标宽度 [`THUMB_WIDTH`](config::pdf::THUMB_WIDTH) 推导并夹在
/// `[0.1, 1.0]`（防止超大页 / 极小页产生荒谬分辨率）；pdfium 下采样极快（~1ms/张）。
fn render_thumb(doc: &PdfDocument<'_>, page_index: usize) -> Option<RenderResult> {
    let page = doc.pages().get(page_index as i32).ok()?;
    let pw = page.width().value.max(1.0);
    let scale = (config::pdf::THUMB_WIDTH / pw).clamp(0.1, 1.0);
    render_page(doc, RenderRequest {
        page: page_index,
        scale,
    })
}

/// 在工作线程内：栅格化某页的 **L0.5 中清页**（~520px × DPI 宽，~5ms/张，用于快速拖动）。
///
/// 缩放按目标宽度 [`MID_WIDTH`](config::pdf::MID_WIDTH) × 显示 `scale_factor` 推导
/// 并夹在 `[0.1, 1.0]`：DPI>1 的屏上目标宽度随之放大，预览不经 GPU 大倍率上采样、保持锐利
/// （避免“字体/图像发糊”）。比全清便宜 ~6×，比 260px 缩略图清晰得多（可辨识标题/段落结构）。
fn render_mid(
    doc: &PdfDocument<'_>,
    page_index: usize,
    scale_factor: f32,
) -> Option<RenderResult> {
    let page = doc.pages().get(page_index as i32).ok()?;
    let pw = page.width().value.max(1.0);
    let target_w = config::pdf::MID_WIDTH * scale_factor.max(1.0);
    let scale = (target_w / pw).clamp(0.1, 1.0);
    render_page(doc, RenderRequest {
        page: page_index,
        scale,
    })
}

/// 打开文档并取每页 (宽, 高)（PDF 点单位）。dll 路径自动定位（进程级缓存）。
/// 用进程级单例工作线程完成，避免二次绑定 pdfium。
/// 解析出的文档会留在工作线程的文档表里（用于测试；生产走 [`PdfWorker::open_and_dims`]）。
#[allow(dead_code)]
pub fn page_dims(bytes: &[u8]) -> Result<Vec<(f32, f32)>> {
    let (_worker, _doc_id, dims) = PdfWorker::open_and_dims(bytes.to_vec())?;
    Ok(dims)
}

fn char_box_from_rect(r: PdfRect) -> CharBox {
    CharBox {
        left: r.left().value,
        bottom: r.bottom().value,
        right: r.right().value,
        top: r.top().value,
    }
}

/// 在工作线程内：抽取单页的完整文本层（Unicode + 每字符包围盒）。
///
/// - 用 [`PdfPageText::chars`](PdfPageText) 遍历 pdfium 文本索引顺序（≈文档内定义顺序，
///   复制时按此拼接，与视觉阅读顺序在复杂排版下可能略有出入——见 pdfium 文档说明）。
/// - 每字符取 `tight_bounds`（`FPDFText_GetCharBox`），退化时按 [`resolve_char_box`]
///   回退 `loose_bounds`（`FPDFText_GetLooseCharBox`）并标记 `synthetic`。
/// - 所有盒夹到页内，保证下游命中测试/高亮不越界。
/// - 页面无文本（扫描件）返回 `Some(PageText { chars: [] })`；页/文档不存在返回 `None`。
fn extract_page_text(doc: &PdfDocument<'_>, page_index: usize) -> Option<PageText> {
    let page = doc.pages().get(page_index as i32).ok()?;
    let (page_w, page_h) = (page.width().value, page.height().value);
    let text = page.text().ok()?;
    let mut chars = Vec::with_capacity(text.len().max(0) as usize);
    let mut synthetic_count = 0usize;
    for c in text.chars().iter() {
        let tight = c.tight_bounds().ok().map(char_box_from_rect);
        let loose = c.loose_bounds().ok().map(char_box_from_rect);
        let (mut b, synthetic) = resolve_char_box(tight, loose);
        if synthetic {
            synthetic_count += 1;
        }
        // 基线原点（PDF 点；失败为 0，仅影响基线校验不参与命中测试）
        let (origin_x, origin_y) = c
            .origin()
            .map(|o| (o.0.value, o.1.value))
            .unwrap_or((0.0, 0.0));
        // 夹到页内（防负坐标/越界；到屏幕的 Y 翻转与缩放由 P1 的映射层完成）
        b.left = b.left.clamp(0.0, page_w);
        b.right = b.right.clamp(0.0, page_w);
        b.bottom = b.bottom.clamp(0.0, page_h);
        b.top = b.top.clamp(0.0, page_h);
        chars.push(PdfTextChar {
            ch: c.unicode_char().unwrap_or('\u{FFFD}'),
            left: b.left,
            bottom: b.bottom,
            right: b.right,
            top: b.top,
            synthetic,
            origin_x,
            origin_y,
        });
    }
    Some(PageText {
        page_index,
        width: page_w,
        height: page_h,
        chars,
        synthetic_count,
    })
}

/// 一次文档级检索的命中：页号 + 页内**原始字符区间**（`[start, end)`，与
/// [`PageText::chars`](PageText) 下标一致）+ 就地生成的**上下文片段**
/// （前/后各 [`SEARCH_SNIPPET_CTX`] 字，侧边栏逐行展示用；不依赖阅读器文本缓存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfSearchHit {
    pub page: usize,
    pub start: usize,
    pub end: usize,
    /// `前5字 + 命中目标 + 后5字`（左右不足一侧带 `…` 省略标记）。
    pub snippet: String,
}

/// 文档级检索结果（**自包含**、可跨线程传给阅读器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfSearchResults {
    /// 全部命中（页号升序，页内按文本顺序；非重叠）。
    pub hits: Vec<PdfSearchHit>,
    /// 命中数是否因超过上限被截断。
    pub truncated: bool,
}

/// 文档检索命中数上限（防超大文档/单字节查询返回海量结果撑爆 UI）。
pub(crate) const SEARCH_MAX_HITS: usize = 1000;
/// 命中上下文片段每侧字数（`前5字 + 目标 + 后5字`）。
pub(crate) const SEARCH_SNIPPET_CTX: usize = 5;

/// 单页文本层内做（ASCII 大小写不敏感、非重叠）子串匹配，返回**原始字符区间**。
///
/// 基于已抽取的 `chars` 重建"可见码点序列"（跳过 NUL——pdfium 生成的填充），
/// 用 `to_ascii_lowercase` 折叠字母（CJK/全角不受影响，仍按精确字匹配）。
/// 返回 `(start, end)`：`start` 为匹配首字符下标，`end` 为匹配末字符下标 +1。
pub(crate) fn search_page_text(text: &PageText, needle: &[char]) -> Vec<(usize, usize)> {
    let m = needle.len();
    if m == 0 || text.chars.is_empty() {
        return Vec::new();
    }
    let hay: Vec<(usize, char)> = text
        .chars
        .iter()
        .enumerate()
        .filter(|(_, c)| c.ch != '\0')
        .map(|(i, c)| (i, c.ch))
        .collect();
    if hay.len() < m {
        return Vec::new();
    }
    let fold = |c: char| c.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + m <= hay.len() {
        let mut matched = true;
        for k in 0..m {
            if fold(hay[i + k].1) != fold(needle[k]) {
                matched = false;
                break;
            }
        }
        if matched {
            out.push((hay[i].0, hay[i + m - 1].0 + 1));
            i += m; // 非重叠：跳过已匹配段
        } else {
            i += 1;
        }
    }
    out
}

/// 全文档检索分片参数：worker 侧每次续跑最多扫 [`SEARCH_SLICE_PAGES`] 页、且耗时不超过
/// [`SEARCH_SLICE_TIMEOUT`]，就回主循环 → 让排队的 Render/RenderTile 插队执行。没有它，
/// 一次全文档同步扫描会在检索期间冻结所有瓦片/封面（87 页文本 PDF 实测检索 ~440ms，
/// 期间到达的渲染被堵到检索扫完全文——正是“随机页先模糊、数秒后清晰”的根因）。
const SEARCH_SLICE_PAGES: usize = 4;
const SEARCH_SLICE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(6);

/// worker 内的一次在途全文档检索（分片续跑）：由 `WorkerMsg::Search` 建立，主循环在
/// 无渲染/占位积压时每轮到 [`search_slice`] 扫一片；文档中途被 Close → 直接取已扫到的
/// 部分结果收尾，绝不长期独占工作线程。
struct PendingSearch {
    doc_id: DocId,
    needle: Vec<char>,
    next_page: usize,
    page_count: usize,
    hits: Vec<PdfSearchHit>,
    truncated: bool,
    tx: mpsc::Sender<Option<PdfSearchResults>>,
}

impl PendingSearch {
    /// 是否已扫完（下一页越界）或命中数已到上限（截断）。
    fn is_done(&self) -> bool {
        self.truncated || self.next_page >= self.page_count
    }
}

/// 单页检索：把该页命中合并进 `hits`；已达上限置 `truncated` 并停止追加。页无效/无文本 → 无命中。
fn search_one_page_into(
    doc: &PdfDocument<'_>,
    page_ix: usize,
    needle: &[char],
    hits: &mut Vec<PdfSearchHit>,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }
    let Some(text) = extract_page_text(doc, page_ix) else {
        return;
    };
    for (s, e) in search_page_text(&text, needle) {
        let snippet = crate::pdf::layout::search_snippet(&text, s, e, SEARCH_SNIPPET_CTX);
        hits.push(PdfSearchHit {
            page: page_ix,
            start: s,
            end: e,
            snippet,
        });
        if hits.len() >= SEARCH_MAX_HITS {
            *truncated = true;
            return;
        }
    }
}

/// 主循环调用的“一片检索”：最多扫 [`SEARCH_SLICE_PAGES`] 页、耗时不超过
/// [`SEARCH_SLICE_TIMEOUT`]，随后返回让其它消息处理。文档被 Close → 置为结束。
fn search_slice(docs: &mut HashMap<DocId, PdfDocument<'_>>, ps: &mut PendingSearch) {
    let start = std::time::Instant::now();
    for _ in 0..SEARCH_SLICE_PAGES {
        if ps.is_done() || start.elapsed() > SEARCH_SLICE_TIMEOUT {
            return;
        }
        match docs.get(&ps.doc_id) {
            Some(doc) => {
                search_one_page_into(doc, ps.next_page, &ps.needle, &mut ps.hits, &mut ps.truncated);
                ps.next_page += 1;
            }
            None => {
                // 文档已被 Close：收尾（返回已扫到的部分结果）
                ps.page_count = ps.next_page;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R1/R4（纯逻辑，不触 pdfium）：错误映射按 io kind 区分——PermissionDenied →
    /// 可操作中文 + 注明不破坏原文件；其余 io 错误保原文案、前缀「写回 PDF 失败：」。
    #[test]
    fn write_io_error_maps_busy_vs_other() {
        let busy = map_write_io_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(is_writeback_busy_error(&busy), "被占用/只读应命中 busy 文案: {busy}");
        assert!(
            busy.contains("PDF 正被其他程序打开或文件只读；请关闭占用它的程序后重试")
                && busy.contains("原文件未受影响"),
            "busy 文案应含指引与不破坏原文件说明: {busy}"
        );
        let other = map_write_io_error(std::io::Error::new(std::io::ErrorKind::Other, "磁盘空间不足"));
        assert!(!is_writeback_busy_error(&other), "非占用错误不应命中 busy: {other}");
        assert!(
            other.starts_with("写回 PDF 失败：") && other.contains("磁盘空间不足"),
            "其余 io 错误应保原文案加前缀: {other}"
        );
        // is_access_denied 的 Windows 裸码分支：ERROR_ACCESS_DENIED = 5
        assert!(is_access_denied(&std::io::Error::from_raw_os_error(5)));
        assert!(!is_access_denied(&std::io::Error::from_raw_os_error(112)));
    }

    /// R2/R4：rename 成功分支与「重试耗尽最终失败」分支（目标为已存在目录 → rename 必败）。
    #[test]
    fn rename_with_retry_succeeds_or_gives_up() {
        let dir = std::env::temp_dir().join(format!(
            "pdf-rename-retry-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录应成功");
        // 成功分支：tmp → target（同目录），rename 应移走 tmp 并落到 target
        let tmp_ok = dir.join("src.pdf");
        let tgt_ok = dir.join("dst.pdf");
        std::fs::write(&tmp_ok, b"hello").expect("写 tmp 应成功");
        rename_with_retry(&tmp_ok, &tgt_ok).expect("正常 rename 应成功");
        assert!(!tmp_ok.exists() && tgt_ok.is_file(), "成功应移走 tmp 并生成 target");
        // 最终失败分支：target 为已存在目录 → 任何平台 rename 都必败（重试耗尽后上抛）
        let tmp_fail = dir.join("src2.pdf");
        std::fs::write(&tmp_fail, b"hello").expect("写 tmp 应成功");
        let tgt_dir = dir.join("existing_dir");
        std::fs::create_dir_all(&tgt_dir).expect("建目录应成功");
        let r = rename_with_retry(&tmp_fail, &tgt_dir);
        assert!(r.is_err(), "覆盖已存在目录应失败");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R0a/R4：rename 失败时临时文件必须被清理（不留 `.pid.seq.tmp.pdf` 点文件），
    /// 且错误上抛为带语义的 anyhow（非裸 io Display「拒绝访问」）。
    #[test]
    fn commit_rename_failure_cleans_tmp() {
        let dir = std::env::temp_dir().join(format!(
            "pdf-commit-clean-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录应成功");
        let tmp = dir.join(".pid.seq.tmp.pdf");
        std::fs::write(&tmp, b"hello").expect("写 tmp 应成功");
        let tgt_dir = dir.join("existing_dir");
        std::fs::create_dir_all(&tgt_dir).expect("建目录应成功");
        let err = commit_rename(&tmp, &tgt_dir).expect_err("覆盖已存在目录应失败");
        assert!(!tmp.exists(), "rename 失败后临时文件应被清理: {}", tmp.display());
        let msg = format!("{err:#}");
        assert!(!msg.is_empty(), "错误文案不应为空");
        assert!(
            msg.contains("写回 PDF 失败：") || is_writeback_busy_error(&msg),
            "错误文案应带写回语义前缀或占用指引: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 校验内嵌 byte 确实是有效 PE 可执行（`MZ` + `PE\0\0`）。
    /// 若磁盘上的 dll 仍在，再确认内嵌与磁盘一致（embed 来源正确）。
    /// 不触碰 pdfium（无需串行锁）。
    #[test]
    fn embedded_pdfium_is_valid_pe_and_matches_disk() {
        #[cfg(embed_pdfium)]
        {
            let bytes = EMBEDDED_PDFIUM;
            // PE 校验：MZ 头 + e_lfanew(0x3C) → 指向 "PE\0\0"
            assert_eq!(&bytes[..2], b"MZ", "内嵌 pdfium 应以 MZ 开头");
            let pe_off = u32::from_le_bytes(bytes[0x3C..0x40].try_into().unwrap()) as usize;
            assert_eq!(
                &bytes[pe_off..pe_off + 4],
                b"PE\x00\x00",
                "MZ 头应指向合法 PE 签名"
            );
            // 若磁盘 dll 尚在（build.rs 注入的是编译期路径），比对一致性
            if let Ok(disk) = std::fs::read(env!("PDFIUM_DLL_PATH")) {
                assert_eq!(bytes.len(), disk.len(), "内嵌与磁盘长度应一致");
                assert_eq!(bytes[..16], disk[..16], "内嵌与磁盘头部应一致");
            }
        }
        // 非 embed 配置下该测试无意义——直接通过（无断言）
    }

    /// 内嵌字节能正确解出到临时目录并返回存在的文件路径。
    ///
    /// 注意：**不清理**解出的文件——pdf 测试并行跑，别的测试（worker_renders_* /
    /// parses_all）可能正并发复用同一个临时 dll；此处清理会让它们读盘时文件已消失。
    /// 该临时文件由系统 temp 清理机制回收，无泄漏问题。
    #[cfg(embed_pdfium)]
    #[test]
    fn embedded_pdfium_extracts_to_temp_dir() {
        let path = extract_embedded_pdfium();
        assert!(path.is_file(), "解出的 dll 文件应存在: {}", path.display());
        let written = std::fs::read(&path).expect("应能读回解出的 dll");
        assert_eq!(written.len(), EMBEDDED_PDFIUM.len(), "解出内容长度应一致");
        assert_eq!(&written[..2], b"MZ", "解出文件应以 MZ 开头");
    }

    fn target_pdf_paths() -> Vec<std::path::PathBuf> {
        // P4：本 crate 位于 crates/pdf，夹具在工作区根 target/（上溯两级）。
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
        // 本机 Rust 进程内 pdfium 无法解析 fixtures/ 下的 PDF（FormatError），只用 target/ 下
        // 生产路径可解析的文件：0=基础文本型、1=长文（≥50 页）。
        vec![
            manifest.join("pdf测试.pdf"),
            manifest.join("pdf测试2.pdf"),
        ]
    }

    /// 文本提取冒烟：真实 PDF 首页能取出非空文本层，且每个字符盒有限、落在页内。
    #[test]
    fn extracts_page_text_from_real_pdf() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let rx = worker.request_text(doc_id, 0);
        let text = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到文本结果")
            .expect("文本提取应成功");
        assert!(
            text.chars.len() >= 10,
            "内容页应有足量文本字符，实际 {}",
            text.chars.len()
        );
        assert!(text.width > 0.0 && text.height > 0.0, "页面尺寸应为正");
        assert!(
            text.synthetic_count <= text.chars.len(),
            "degenerate 字符数不应超过总数"
        );
        for c in &text.chars {
            let (l, r, b, t) = (c.left, c.right, c.bottom, c.top);
            for v in [l, r, b, t] {
                assert!(v.is_finite(), "盒字段应有限: {c:?}");
            }
            assert!(
                l >= 0.0 - 1e-3 && r <= text.width + 1e-3,
                "X 应在页内 [0,{}]: {c:?}",
                text.width
            );
            assert!(
                b >= 0.0 - 1e-3 && t <= text.height + 1e-3,
                "Y 应在页内 [0,{}]: {c:?}",
                text.height
            );
            if !c.synthetic {
                assert!(r >= l && t >= b, "有效字符盒应非负尺寸: {c:?}");
            }
        }
        // 抽取应稳定：同一页重复抽取，字符序列一致
        let rx2 = worker.request_text(doc_id, 0);
        let text2 = rx2
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到第二次结果")
            .expect("第二次抽取应成功");
        assert_eq!(text.chars, text2.chars, "同页重复抽取字符应一致");
    }

    /// 未打开的文档请求文本 → None（区别于“页面无文本”的 Some(空)）。
    #[test]
    fn text_request_for_unknown_doc_returns_none() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, _doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let rx = worker.request_text(u64::MAX, 0); // 从没打开过的 doc
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("应收到应答")
            ;
        assert!(res.is_none(), "未知文档的文本请求应返回 None");
    }

    /// 检索子串匹配（纯逻辑，不触 pdfium）：大小写折叠、跳过 NUL、非重叠。
    #[test]
    fn search_page_text_matches_casefold_skips_nul_non_overlap() {
        use super::{search_page_text, PageText, PdfTextChar};
        // 可见序列 = "ab" + NUL 跳过 + "ABc" → "abABc"
        let mut chars = Vec::new();
        for (i, ch) in ['a', 'b', '\0', 'A', 'B', 'c'].into_iter().enumerate() {
            chars.push(PdfTextChar {
                ch,
                left: i as f32,
                bottom: 0.0,
                right: i as f32 + 1.0,
                top: 1.0,
                synthetic: ch == '\0',
                origin_x: i as f32,
                origin_y: 0.0,
            });
        }
        let text = PageText {
            page_index: 0,
            width: 100.0,
            height: 100.0,
            chars,
            synthetic_count: 1,
        };
        let ndl = |s: &str| s.chars().collect::<Vec<char>>();
        // 可见序列 "abABc"：大小写折叠下 ab/AB/aB 都在 0..2 与 3..5 两处命中
        assert_eq!(
            search_page_text(&text, &ndl("ab")),
            vec![(0, 2), (3, 5)],
            "ab 折叠后命中可见序列两处"
        );
        assert_eq!(
            search_page_text(&text, &ndl("AB")),
            vec![(0, 2), (3, 5)],
            "AB 折叠后与 ab 同命中"
        );
        assert_eq!(
            search_page_text(&text, &ndl("aB")),
            vec![(0, 2), (3, 5)],
            "aB 混合大小写折叠命中"
        );
        assert_eq!(search_page_text(&text, &ndl("c")), vec![(5, 6)], "c 命中可见序列末");
        // NUL 被跳过 → 可见序列连续：'abc'(折叠) 命中可见 'ABc'(3..6)；'bA' 跨过 NUL 连续命中
        assert_eq!(
            search_page_text(&text, &ndl("abc")),
            vec![(3, 6)],
            "跳过 NUL 后可见序列连续，abc(折叠) 命中 ABc"
        );
        assert_eq!(
            search_page_text(&text, &ndl("bA")),
            vec![(1, 4)],
            "bA 跨过 NUL 仍为可见序列连续"
        );
        // 非重叠：连续重复只取不重叠段
        let rep = PageText {
            page_index: 0,
            width: 100.0,
            height: 100.0,
            chars: ['a', 'a', 'a', 'a']
                .into_iter()
                .enumerate()
                .map(|(i, ch)| PdfTextChar {
                    ch,
                    left: i as f32,
                    bottom: 0.0,
                    right: i as f32 + 1.0,
                    top: 1.0,
                    synthetic: false,
                    origin_x: i as f32,
                    origin_y: 0.0,
                })
                .collect(),
            synthetic_count: 0,
        };
        assert_eq!(search_page_text(&rep, &ndl("aa")), vec![(0, 2), (2, 4)], "非重叠命中");
        // 空串 / 长于文本 → 空
        assert_eq!(search_page_text(&text, &ndl("")), Vec::<(usize, usize)>::new());
        assert_eq!(search_page_text(&text, &ndl("abABcx")), Vec::<(usize, usize)>::new());
    }

    /// 纯逻辑（不触 pdfium）：字符盒回退策略 tight > loose > synthetic。
    #[test]
    fn resolve_char_box_prefers_tight_then_loose_then_synthetic() {
        use super::{CharBox, resolve_char_box};
        let tight = Some(CharBox {
            left: 10.0,
            bottom: 20.0,
            right: 12.0,
            top: 22.0,
        });
        let (b, syn) = resolve_char_box(tight, Some(CharBox::ZERO));
        assert!(!syn && b.left == 10.0 && b.top == 22.0, "tight 正常应直接采用");

        // tight 退化（0 宽 0 高）→ 回退到可用的 loose
        let (b2, syn2) = resolve_char_box(Some(CharBox::ZERO), Some(CharBox {
            left: 1.0,
            bottom: 2.0,
            right: 4.0,
            top: 5.0,
        }));
        assert!(!syn2 && b2.right - b2.left > 0.0, "退化的 tight 应回退 loose");

        // 全部退化 → 标记 synthetic（保留盒内容便于复制）
        let (b3, syn3) = resolve_char_box(Some(CharBox::ZERO), Some(CharBox::ZERO));
        assert!(syn3 && b3 == CharBox::ZERO, "全退化应标记 synthetic");

        // 全部缺失 → synthetic + ZERO
        let (b4, syn4) = resolve_char_box(None, None);
        assert!(syn4 && b4 == CharBox::ZERO, "双 None 应 synthetic + ZERO");

        // 负尺寸（引擎异常返回）→ 视为退化，不回退已坏的 tight
        let neg = Some(CharBox {
            left: 10.0,
            bottom: 20.0,
            right: 8.0,
            top: 22.0,
        });
        let (b5, syn5) = resolve_char_box(neg, None);
        assert!(syn5, "负宽 tight 应标记 synthetic");
        assert!(b5 == CharBox::ZERO, "无可用回退时应为 ZERO");
    }

    /// 文本提取单页端到端延迟摸底（Release 目标 ~8ms/页）。打印，不构成断言。
    #[test]
    #[ignore]
    fn bench_page_text_extraction() {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len().min(8);
        let mut ms: Vec<u64> = Vec::new();
        for idx in 0..n {
            let t0 = std::time::Instant::now();
            let rx = worker.request_text(doc_id, idx);
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到文本结果");
            let m = t0.elapsed().as_millis() as u64;
            ms.push(m);
            if let Some(t) = res {
                eprintln!(
                    "page {idx}: {m}ms, {} chars (synthetic {})",
                    t.chars.len(),
                    t.synthetic_count
                );
            }
        }
        let avg = ms.iter().sum::<u64>() / n as u64;
        eprintln!("BENCH-TEXT: 单页文本提取均值 {avg}ms; 明细 {ms:?}");
    }
}
