//! PDFium 绑定与 PDF 解析：定位 DLL、逐页栅格化、PNG 编码。
//!
//! 本文件只负责“把 PDF 逐页变成内存 PNG 位图”，产出 [`super::PdfDocument`]。
//! 这是纯“原样”栅格化——不做任何文本抽取或布局改写，交由 [`super::PdfViewer`]
//! 直接逐页显示。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result};
use gpui::{Image, ImageFormat};
use image::ImageEncoder;
use pdfium_render::prelude::*;

use super::{PdfDocument, PdfPage};

/// 栅格化最大边长（px），超出按比例缩小（避免内存爆炸）。
const PAGE_MAX_EDGE: f32 = 1600.0;
/// 栅格化最小边长（px）。
const PAGE_MIN_EDGE: f32 = 720.0;

/// 定位 pdfium 动态库路径（首次成功后缓存）。
fn pdfium_dll_path() -> Result<PathBuf> {
    // 手动实现 get_or_try_init（当前稳定工具链无 once_cell_try）：缓存成
    // Result 之后用稳定 API `get_or_init` 只做一次定位。
    static CACHE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let cached = CACHE.get_or_init(|| locate_pdfium_dll().map_err(|e| e.to_string()));
    match cached {
        Ok(pb) => Ok(pb.clone()),
        Err(msg) => Err(anyhow::anyhow!("{msg}")),
    }
}

/// 实际定位：可执行文件同目录 → 资源目录 `resources/pdfium/pdfium.dll`。
fn locate_pdfium_dll() -> Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let mut candidates = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("pdfium.dll"));
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(Path::new(&manifest).join("resources/pdfium/pdfium.dll"));
    }
    candidates.into_iter().find(|c| c.is_file()).ok_or_else(|| {
        anyhow::anyhow!("无法找到 pdfium.dll：请在可执行文件旁或 resources/pdfium/ 放置 pdfium.dll")
    })
}

/// 进程级单例：绑定一次 pdfium（`Pdfium: Send + Sync`），DLL 常驻进程生命周期。
/// 反复“加载→卸载→再加载”pdfium.dll 在 Windows 上偶发
/// `加载 pdfium.dll 失败`（尤其测试并行执行 / DLL 已卸载后重载时），
/// 绑定一次并永不释放可彻底规避。
static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

/// 取进程级 pdfium 单例（须已持有 [`PDFIUM_PARSE_LOCK`]）。
fn pdfium() -> Result<&'static Pdfium> {
    let cached = PDFIUM.get_or_init(|| {
        pdfium_dll_path()
            .and_then(|p| {
                Pdfium::bind_to_library(&p)
                    .map(Pdfium::new)
                    .with_context(|| format!("加载 pdfium.dll 失败：{}", p.display()))
            })
            .map_err(|e| e.to_string())
    });
    match cached {
        Ok(p) => Ok(p),
        Err(msg) => Err(anyhow::anyhow!("{msg}")),
    }
}

/// 进程级串行化：共享同一个 pdfium 实例时，文档解析与逐页栅格化全程
/// 只允许一个线程进入（pdfium-render 对并发文档操作是线程兼容的，
/// 但保持串行更稳，尤其在测试并行执行时）。
static PDFIUM_PARSE_LOCK: Mutex<()> = Mutex::new(());

/// 从 PDF 文件路径解析为可分页文档（栅格化分页位图）。
pub fn parse_pdf_file(path: &Path) -> Result<PdfDocument> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("无法读取 PDF 文件：{}", path.display()))?;
    parse_pdf_bytes(&bytes, path)
}

/// 从内存字节解析 PDF（不落盘）。
pub fn parse_pdf_bytes(bytes: &[u8], path: &Path) -> Result<PdfDocument> {
    // 原生 pdfium 全程串行（见 PDFIUM_PARSE_LOCK 文档）
    let _guard = PDFIUM_PARSE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let pdfium = pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(&bytes[..], None)
        .context("PDF 解析失败（可能已加密或损坏）")?;
    let pages = doc.pages();

    let title = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "PDF".into());

    let mut out: Vec<PdfPage> = Vec::with_capacity(pages.len() as usize);
    for i in pages.as_range() {
        let page = pages
            .get(i)
            .with_context(|| format!("读取 PDF 第 {} 页失败", i + 1))?;
        // 页面尺寸（pt）：按比例推算渲染像素尺寸
        let rect = page.page_size();
        let pw = rect.width().to_inches() * 72.0;
        let ph = rect.height().to_inches() * 72.0;
        let ratio = ph / pw.max(0.001);
        let target_w = if pw >= ph {
            PAGE_MAX_EDGE
        } else {
            (PAGE_MAX_EDGE / ratio).clamp(PAGE_MIN_EDGE, PAGE_MAX_EDGE)
        };
        let target_h = target_w * ratio;
        let (tw, th) = (target_w.ceil() as u32, target_h.ceil() as u32);

        let bitmap = page
            .render(tw as Pixels, th as Pixels, None)
            .with_context(|| format!("渲染 PDF 第 {} 页失败", i + 1))?;
        let rgba = bitmap.as_rgba_bytes();
        let png = encode_png(rgba, tw, th)
            .with_context(|| format!("编码 PDF 第 {} 页位图失败", i + 1))?;

        out.push(PdfPage {
            image: Arc::new(Image::from_bytes(ImageFormat::Png, png)),
            width: tw,
            height: th,
        });
    }

    Ok(PdfDocument {
        title,
        page_count: out.len(),
        pages: out,
    })
}

/// 把 RGBA 像素编码为 PNG 字节。
fn encode_png(rgba: Vec<u8>, w: u32, h: u32) -> Result<Vec<u8>> {
    let img = image::RgbaImage::from_raw(w, h, rgba).context("PDF 页面位图尺寸与像素不符")?;
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .context("PNG 编码失败")?;
    Ok(out)
}
