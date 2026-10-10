//! 内容/图片的尺寸估算与解析：
//! - 表格列宽估算（超宽表格在块内横向滚动，列宽决定其内容宽）；
//! - 解析常见图片格式的文件头得到固有尺寸（不整图解码，快）。
//!
//! 注：页面已恒换行（无「非换行自然宽 + 页面级横向滚动」），单行文本宽估算
//! （`est_row_width` / `est_row_width_meta`）已随该模式移除。

use crate::markdown::parse::TableData;
use crate::markdown::view::style::StyleParams;

/// 读取 4 字节大端无符号整数（辅助解析 PNG/JPEG 头）。
fn be_u32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// 从本地图片文件头读取固有尺寸 (w, h)（不整图解码，快）。
/// 仅解析常见格式；失败返回 None，渲染回退到固定高度。
pub(crate) fn image_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    image_dimensions_bytes(&std::fs::read(path).ok()?)
}

/// 从图片字节头读取固有尺寸（不整图解码）。支持 PNG/JPEG/GIF/BMP。
pub(crate) fn image_dimensions_bytes(d: &[u8]) -> Option<(u32, u32)> {
    if d.len() < 8 {
        return None;
    }
    // PNG: 8 字节签名 + IHDR(4 长度 + 4 类型) + 宽高(各 4 大端)
    if d.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some((be_u32(d, 16)?, be_u32(d, 20)?));
    }
    // JPEG: 扫描段找 SOF0/SOF1/SOF2 (0xFFC0..0xFFC3)，高度在前
    if d.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2usize;
        while i + 9 < d.len() {
            if d[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = d[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            // standalone markers
            if matches!(marker, 0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7) {
                i += 2;
                continue;
            }
            if i + 4 > d.len() {
                break;
            }
            let seg_len = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
            if matches!(marker, 0xC0..=0xC3) {
                return Some((
                    u16::from_be_bytes([d[i + 7], d[i + 8]]) as u32,
                    u16::from_be_bytes([d[i + 5], d[i + 6]]) as u32,
                ));
            }
            i += 2 + seg_len;
        }
        return None;
    }
    // GIF: "GIF87a"/"GIF89a" + 逻辑屏幕宽高（小端）
    if d.starts_with(b"GIF8") {
        return Some((
            u16::from_le_bytes([d[6], d[7]]) as u32,
            u16::from_le_bytes([d[8], d[9]]) as u32,
        ));
    }
    // BMP: "BM" + 14 字节头后有 40 字节 DIB，宽高（有符号小端）
    if d.starts_with(b"BM") && d.len() >= 26 {
        let w = i32::from_le_bytes([d[18], d[19], d[20], d[21]]);
        let h = i32::from_le_bytes([d[22], d[23], d[24], d[25]]);
        if w > 0 {
            return Some((w as u32, h.unsigned_abs()));
        }
        return None;
    }
    None
}

/// CJK 等全角字符：宽度按 1 个 em 估算，英文数字按 0.6 em。
fn is_wide_char(c: char) -> bool {
    let cp = c as u32;
    (0x2E80..=0x9FFF).contains(&cp) // CJK 统一表意 + 部首 + 假名
        || (0xAC00..=0xD7FF).contains(&cp) // 谚文
        || (0xF900..=0xFAFF).contains(&cp) // CJK 兼容表意
        || (0x3000..=0x303F).contains(&cp) // CJK 标点
        || (0xFF00..=0xFF60).contains(&cp) // 全角符号
}

/// 估算一段文本的渲染宽度（px）。表格列宽估算用（`table_column_widths`）。
fn est_text_width(text: &str, font_size: f32) -> f32 {
    text.chars().fold(0.0, |acc, c| {
        acc + if is_wide_char(c) {
            font_size
        } else {
            font_size * 0.6
        }
    })
}

/// 表格每列估算宽度（px）：逐格 `est_text_width` + 左右各 10px 内距，
/// 下限 48px，与 [`crate::markdown::view::table::render_table`] 的实际布局一致
/// （超宽表格撑出横向滚动范围）。
pub(crate) fn table_column_widths(table: &TableData, p: &StyleParams) -> Vec<f32> {
    let ncols = table.header.len().max(1);
    let mut widths = vec![0.0f32; ncols];
    let font = p.para_size.as_f32();
    for (ci, cell) in table.header.iter().enumerate() {
        widths[ci] = widths[ci].max(est_text_width(cell.as_ref(), font) + 20.0);
    }
    for row in &table.rows {
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            widths[ci] = widths[ci].max(est_text_width(cell.as_ref(), font) + 20.0);
        }
    }
    for w in &mut widths {
        *w = (*w).max(48.0);
    }
    widths
}
