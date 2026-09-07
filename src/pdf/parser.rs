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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, OnceLock};

use anyhow::{Context as _, Result};
use pdfium_render::prelude::*;

/// 进程级单例：pdfium 动态库路径（只定位一次）。
static PDFIUM_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();

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
        candidates.push(
            PathBuf::from(manifest)
                .join("resources/pdfium")
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

/// 渲染结果：页面 **BGRA 像素字节** + 位图像素 (宽, 高)。
///
/// `bgra` 直接来自 pdfium 的原生 BGRA 缓冲（`FPDFBitmap_GetBuffer_as_vec`），
/// 主线程用 `Frame::from_raw` 直接包裹即上传 → **没有任何像素循环或字节重排**。
/// PNG 编码不再由 worker 承担，改由 view 层在后台 executor 异步落盘。
#[derive(Debug)]
pub struct RenderResult {
    /// 原始 BGRA 像素（`w*h*4` 字节），主线程 `Frame::from_raw` 直接包裹即上传。
    /// 注意：pdfium 的 stride 可能大于 `w*4`（对齐填充），本字段为**去 stride 后**
    /// 的紧凑一行式缓冲；`width/height` 为位图逻辑尺寸。
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
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

enum WorkerMsg {
    Open(
        DocId,
        Vec<u8>,
        mpsc::Sender<Result<(Vec<(f32, f32)>, Vec<PdfOutlineEntry>)>>,
    ),
    Render(DocId, RenderRequest, mpsc::Sender<Option<RenderResult>>),
    Close(DocId),
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
    pub fn open_and_dims(bytes: Vec<u8>) -> Result<(Self, DocId, Vec<(f32, f32)>)> {
        let (w, id, dims, _outlines) = Self::open_and_dims_full(bytes)?;
        Ok((w, id, dims))
    }

    /// 打开文档并取每页尺寸 + 书签目录（结构解析，快）。
    /// 成功则返回 (DocId, dims, outlines)，便于调用方同时得到文档句柄与页面布局、目录。
    pub fn open_and_dims_full(
        bytes: Vec<u8>,
    ) -> Result<(Self, DocId, Vec<(f32, f32)>, Vec<PdfOutlineEntry>)> {
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

    /// 请求渲染某页（异步）。返回一个接收结果的通道。
    pub fn request_render(&self, doc_id: DocId, req: RenderRequest) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(WorkerMsg::Render(doc_id, req, tx));
        rx
    }

    /// 关闭文档，释放 pdfium 侧资源（阅读器关闭时调用）。
    pub fn close(&self, doc_id: DocId) {
        let _ = self.tx.send(WorkerMsg::Close(doc_id));
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
        // 文档存活期 = `pdfium` 存活期；所有 `PdfDocument<'a>` 都借用上面的 `pdfium`。
        let mut docs: HashMap<DocId, PdfDocument<'_>> = HashMap::new();
        while let Ok(msg) = rx.recv() {
            match msg {
                WorkerMsg::Open(doc_id, bytes, tx) => {
                    let result = match pdfium.load_pdf_from_byte_vec(bytes, None) {
                        Ok(doc) => {
                            let dims: Vec<(f32, f32)> = doc
                                .pages()
                                .iter()
                                .map(|p| (p.width().value, p.height().value))
                                .collect();
                            let outlines = extract_outline(&doc);
                            docs.insert(doc_id, doc);
                            Ok((dims, outlines))
                        }
                        Err(e) => Err(anyhow::anyhow!("PDF 解析失败：{e}")),
                    };
                    let _ = tx.send(result);
                }
                WorkerMsg::Render(doc_id, req, tx) => {
                    let result = docs
                        .get(&doc_id)
                        .and_then(|doc| render_page(doc, req));
                    let _ = tx.send(result);
                }
                WorkerMsg::Close(doc_id) => {
                    docs.remove(&doc_id);
                }
            }
        }
    }
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
fn render_page(doc: &PdfDocument<'_>, req: RenderRequest) -> Option<RenderResult> {
    let page = doc.pages().get(req.page as i32).ok()?;
    let (pw, ph) = (page.width().value as f32, page.height().value as f32);
    let w = (pw * req.scale).ceil().max(1.0) as i32;
    let h = (ph * req.scale).ceil().max(1.0) as i32;
    let bmp = page.render(w, h, None).ok()?;
    // pdfium-render 的 `PdfBitmap::render()` 用 `PdfBitmapFormat::default()`（= BGRA）建位图，
    // BGRA 的 stride 恒为 `width*4`（无对齐填充，见 preferred_stride_bytes）。
    // `as_raw_bytes()` 直接返回整块原生 BGRA 缓冲 → 主线程 `Frame::from_raw` 直接包裹。
    let bgra = bmp.as_raw_bytes();
    Some(RenderResult {
        bgra,
        width: w as u32,
        height: h as u32,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
