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

/// 进程级单例：pdfium.dll 路径（只定位一次）。
static PDFIUM_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// 定位 pdfium 动态库：可执行文件同目录 → `resources/pdfium/pdfium.dll`。
fn locate_dll() -> Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let mut candidates = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("pdfium.dll"));
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("resources/pdfium/pdfium.dll"));
    }
    candidates
        .into_iter()
        .find(|c| c.is_file())
        .ok_or_else(|| anyhow::anyhow!("无法找到 pdfium.dll：请在可执行文件旁或 resources/pdfium/ 放置"))
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

enum WorkerMsg {
    Open(DocId, Vec<u8>, mpsc::Sender<Result<Vec<(f32, f32)>>>),
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
    /// 便于调用方同时得到文档句柄与页面布局数据。
    pub fn open_and_dims(bytes: Vec<u8>) -> Result<(Self, DocId, Vec<(f32, f32)>)> {
        let worker = shared_worker()?;
        let (tx, rx) = mpsc::channel();
        let doc_id = next_doc_id();
        let _ = worker.tx.send(WorkerMsg::Open(doc_id, bytes, tx));
        let dims = match rx.recv() {
            Ok(Ok(dims)) => dims,
            Ok(Err(e)) => return Err(anyhow::anyhow!("{e}")),
            Err(_) => return Err(anyhow::anyhow!("pdfium 工作线程已退出")),
        };
        Ok((worker.clone(), doc_id, dims))
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
                    let dims = match pdfium.load_pdf_from_byte_vec(bytes, None) {
                        Ok(doc) => {
                            let dims: Vec<(f32, f32)> = doc
                                .pages()
                                .iter()
                                .map(|p| (p.width().value, p.height().value))
                                .collect();
                            docs.insert(doc_id, doc);
                            Ok(dims)
                        }
                        Err(e) => Err(anyhow::anyhow!("PDF 解析失败：{e}")),
                    };
                    let _ = tx.send(dims);
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
