//! PDF 底层原语：解析、页尺寸、单页栅格化（参照 gpui-pdf / zorite `gpui-pdf`）。
//!
//! 均不涉及 UI；解析与栅格化都是纯 CPU 的 [`hayro`]，无原生 DLL。

use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::RenderImage;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::{DecryptionError, LoadPdfError, Pdf};
use image::{Frame, RgbaImage};

use super::Document;

/// 解析失败：加密且缺少/错误密码，或其它解析错误。
#[derive(Debug)]
pub enum LoadError {
    /// 加密 PDF，且给定密码缺/错（调用方可提示密码后重试）。
    Locked,
    /// 其它失败（文件损坏、加密方式不支持等）。
    Other(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Locked => write!(f, "PDF 已加密，需要密码"),
            LoadError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// 解析 PDF 字节（不落盘），返回可复用、可跨线程共享的 [`Document`]。
/// 只做结构解析 + 页缓存初始化，**不栅格化**——页尺寸用 [`page_dims`] 便宜读取。
pub fn parse_bytes(bytes: Vec<u8>) -> Result<Arc<Document>> {
    parse_with_password(bytes, "").map_err(anyhow::Error::from)
}

/// 带密码解析（加密 PDF 通过密码解锁）。
pub fn parse_with_password(bytes: Vec<u8>, password: &str) -> Result<Arc<Document>, LoadError> {
    match Pdf::new_with_password(Arc::new(bytes), password) {
        Ok(pdf) => Ok(Arc::new(pdf)),
        Err(LoadPdfError::Decryption(DecryptionError::PasswordProtected)) => Err(LoadError::Locked),
        Err(e) => Err(LoadError::Other(format!("parse PDF: {e:?}"))),
    }
}

/// 每页 `(宽, 高)`（PDF 点单位）。只读结构，不栅格化，用于先铺设正确的页槽。
pub fn page_dims(doc: &Document) -> Vec<(f32, f32)> {
    doc.pages().iter().map(|p| p.render_dimensions()).collect()
}

/// 栅格化单页为 [`gpui::RenderImage`]（BGRA）。`scale` = PDF 点尺寸 × 此系数；
/// 页面以白色为底（透明处合成白底），超出视口按比例缩放。
pub fn render_page(doc: &Document, idx: usize, scale: f32) -> Result<Arc<RenderImage>> {
    let pixmaps = hayro::render_pdf(doc, scale, InterpreterSettings::default(), Some(idx..=idx))
        .context("render_pdf 失败")?;
    let pixmap = pixmaps
        .into_iter()
        .next()
        .context("render_pdf 未产出该页")?;

    let (w, h) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
    let src = pixmap.data_as_u8_slice(); // premultiplied RGBA8, row-major
    let mut bgra = vec![0u8; src.len()];
    for (out, p) in bgra
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<4>().0)
    {
        // 预乘 RGBA → BGRA（gpui RenderImage 以 BGRA 存储）
        out[0] = p[2]; // B
        out[1] = p[1]; // G
        out[2] = p[0]; // R
        out[3] = p[3]; // A
    }
    let buf = RgbaImage::from_raw(w, h, bgra).context("坏像素缓冲")?;
    Ok(Arc::new(RenderImage::new(vec![Frame::new(buf)])))
}
