//! 打开 / 文本 / 图片抽取簇：`page_dims`（测试入口）+ 单页文本层（[`extract_page_text`]）+
//! 光栅图片盒与像素（[`extract_page_images`] / [`extract_image_pixels`]）。
//!
//! 跨簇可见性（模块重组）：`extract_page_text` 被根 `run`（Text）与 `search`（分片检索）调用、
//! `extract_page_images` / `extract_image_pixels` 被根 `run` 调用 → 三者升 `pub(crate)`。

use anyhow::Result;
use pdfium_render::prelude::*;

use super::{CharBox, PdfImageBox, PdfImagePixels, PdfTextChar, PageText, PdfWorker, resolve_char_box};

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
pub(crate) fn extract_page_text(doc: &PdfDocument<'_>, page_index: usize) -> Option<PageText> {
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

/// 在工作线程内：抽取单页全部 **raster Image 对象**的包围盒与自然分辨率（懒加载，
/// 仅在点击/右键命中图片时调用）。只收集类型为 [`PdfPageObjectType::Image`] 的对象
/// （路径/文本/表单/未支持对象跳过）；页或文档不存在 → `None`，页无图片 → `Some([])`。
/// 盒按 pdfium 对象顺序排列，`obj_index` 即 `objects().iter()` 的下标（`objects().get`
/// 取回后再 `as_image_object()` 即原图），供命中测试与「复制图片」取像素回指。
pub(crate) fn extract_page_images(
    doc: &PdfDocument<'_>,
    page_index: usize,
) -> Option<Vec<PdfImageBox>> {
    let page = doc.pages().get(page_index as i32).ok()?;
    let (page_w, page_h) = (page.width().value, page.height().value);
    let mut images = Vec::new();
    for (idx, obj) in page.objects().iter().enumerate() {
        if obj.object_type() != PdfPageObjectType::Image {
            continue;
        }
        let Some(img) = obj.as_image_object() else {
            continue;
        };
        let Ok(bounds) = obj.bounds() else {
            continue;
        };
        let rect = bounds.to_rect();
        // 自然分辨率取对象内嵌尺寸（失败按 0 → 命中/取数层跳过该盒）。
        let natural_w = img.width().map(|v| v.max(0) as u32).unwrap_or(0);
        let natural_h = img.height().map(|v| v.max(0) as u32).unwrap_or(0);
        let mut b = PdfImageBox {
            page_index,
            obj_index: idx,
            x0: rect.left().value,
            y0: rect.bottom().value,
            x1: rect.right().value,
            y1: rect.top().value,
            natural_w,
            natural_h,
        };
        // 夹到页内并丢弃退化盒（空/越界不可命中，取数也不可靠）。
        b.x0 = b.x0.clamp(0.0, page_w);
        b.x1 = b.x1.clamp(0.0, page_w);
        b.y0 = b.y0.clamp(0.0, page_h);
        b.y1 = b.y1.clamp(0.0, page_h);
        if b.valid() && b.natural_w > 0 && b.natural_h > 0 {
            images.push(b);
        }
    }
    Some(images)
}

/// 在工作线程内：按给定 `req_w × req_h`（= 图片自然分辨率）取回某图片对象的
/// **processed** 位图（应用滤镜 / 图像掩码 / 对象变换，`get_processed_bitmap_with_size`），
/// 归一化为紧凑 **BGRA**（`w*h*4`，无 stride；RGBA→BGRA 交换一次）。灰度位图自动
/// 扩三通道 + 不透明。对象下标失效 / 文档页不存在 / 位图尺寸异常 → `None`。
pub(crate) fn extract_image_pixels(
    doc: &PdfDocument<'_>,
    page_index: usize,
    obj_index: usize,
    req_w: u32,
    req_h: u32,
) -> Option<PdfImagePixels> {
    let page = doc.pages().get(page_index as i32).ok()?;
    let obj = page.objects().get(obj_index).ok()?;
    let img = obj.as_image_object()?;
    // 目标尺寸夹到 i32 正整数域（Pixels=i32；真正图片嵌入尺寸都在合理范围内，仅防病态）。
    let req_w = req_w.clamp(1, i32::MAX as u32);
    let req_h = req_h.clamp(1, i32::MAX as u32);
    // 面积闸（安全护栏，双点之一）：复制大图默认按自然分辨率语义**原样**（节奏稳定，
    // 见下方 1:1 稳定性注释），仅病理性天文尺寸按比例缩到预算内（防 req_w×req_h×4 字节
    // 分配撑爆内存）；正常 A4/大图复制路径尺寸稳定不受影响。口径与 layout::pixel_area_ok 一致。
    let (req_w, req_h) = match crate::pdf::layout::budget_clamped_dims(req_w, req_h) {
        Some(dims) => dims,
        None => return None,
    };
    let w = req_w as i32;
    let h = req_h as i32;
    // —— 尺寸稳定性（2026-11-10，修复「复制图片」剪贴板 DIB 尺寸漂移）——
    // pdfium-render `get_processed_bitmap_with_size` 的缩放收敛 loop 对**首次**渲染会给出
    // 与请求差 ±1 行/列的位图（实证：请求 953×213 → 首次 953×212、第二次及以后 953×213，
    // 且短者为长者的行前缀、字节恒等）。「复制图片」按自然分辨率语义：首渲尺寸 ≠ 请求时
    // **重渲一次取稳定尺寸**，保证同参数多次 request_image_pixels 结果完全一致（尺寸与内容）。
    let mut bmp = img
        .get_processed_bitmap_with_size(doc, w, h)
        .ok()?;
    if bmp.width() != w || bmp.height() != h {
        if let Ok(b2) = img.get_processed_bitmap_with_size(doc, w, h) {
            bmp = b2;
        }
    }
    let bw = bmp.width();
    let bh = bmp.height();
    if bw <= 0 || bh <= 0 {
        return None;
    }
    let (bw, bh) = (bw as u32, bh as u32);
    // 面积闸兜底（双点之二）：实际渲染尺寸（±1 漂移后）仍须在预算内，防 `with_capacity`
    // 天文分配；用「+1 行松弛」变体——恰打满预算的合法大图（4000×3000=12M）双渲 +1 行
    // 不被误伤（精确闸 <见上一改动> 会拒 4000×3001，属评审 NIT#2 修正）；saturating 防环绕。
    if !crate::pdf::layout::pixel_area_ok_with_row_slack(bw, bh) {
        return None;
    }
    // `as_rgba_bytes()` 归一化为紧凑 RGBA；Gray 格式返回原始单通道字节（`w*h`）。
    let rgba = bmp.as_rgba_bytes();
    let px_count = (bw as usize).saturating_mul(bh as usize);
    let mut bgra = Vec::with_capacity(px_count.saturating_mul(4));
    if rgba.len() == px_count * 4 {
        // RGBA → BGRA：交换 R/B 一次（退化为逐 4 字节 flat_map，零分配）。
        bgra.extend(rgba.chunks_exact(4).flat_map(|px| [px[2], px[1], px[0], px[3]]));
    } else if rgba.len() == px_count {
        // 灰度：单字节 → 三通道同值 + 不透明（DIB 32bpp 必需）。
        bgra.reserve(rgba.len() * 4);
        for &g in &rgba {
            bgra.extend_from_slice(&[g, g, g, 255]);
        }
    } else {
        return None;
    }
    // 定稿尺寸：重渲后应 = 请求；防御性兜底——仍差 ≤1 则裁/补到请求尺寸（短 = 复制末行、
    // 长 = 截尾），绝不把 pdfium 漂移尺寸外泄；差 ≥2 视为真实内容差，用实际尺寸并保持自洽。
    let (rw, rh) = (w as u32, h as u32);
    let within = (bw as i64 - rw as i64).abs() <= 1 && (bh as i64 - rh as i64).abs() <= 1;
    let (out_w, out_h) = if within { (rw, rh) } else { (bw, bh) };
    let want = (out_w as usize) * (out_h as usize) * 4;
    if bgra.len() > want {
        bgra.truncate(want);
    } else if bgra.len() < want {
        // 短 ≤1 行：复制最后一条真实行补到尾（确定性填充，保持 bgra 与 out 尺寸自洽）。
        let row = (out_w as usize) * 4;
        let last_row = bgra
            .get(bgra.len().saturating_sub(row)..)
            .unwrap_or_default()
            .to_vec();
        let fill = if last_row.is_empty() { vec![0xff; row] } else { last_row };
        while bgra.len() + fill.len() <= want {
            bgra.extend_from_slice(&fill);
        }
        bgra.resize(want, 0xff);
    }
    Some(PdfImagePixels {
        bgra,
        width: out_w,
        height: out_h,
    })
}
