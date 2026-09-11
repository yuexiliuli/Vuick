//! 内容/图片的尺寸估算与解析：
//! - 估算每行渲染宽度（决定横向滚动容器的内容宽）；
//! - 解析常见图片格式的文件头得到固有尺寸（不整图解码，快）。

use crate::markdown::parser::{BlockMeta, Row, RowKind, TableData};
use crate::markdown::view::style::StyleParams;

/// 内容宽估算的额外缓冲（px），避免估算值微偏导致最后几个字符被截断。
pub(crate) const WIDTH_BUFFER_PX: f32 = 24.0;
/// 未知尺寸图片行的默认显示高度（px）。
pub(crate) const IMAGE_ROW_DEFAULT_H: f32 = 280.0;
/// 图片固有宽度显示上限（px）：更宽的图按 contain 缩放，避免小图被拉大。
pub(crate) const IMAGE_MAX_W: f32 = 600.0;

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

/// 估算一段文本的渲染宽度（px）。行元素的真实宽度由元素布局决定，这里仅用来给
/// 横向滚动容器定一个“足够放下超宽行”的内容宽。
fn est_text_width(text: &str, font_size: f32) -> f32 {
    text.chars().fold(0.0, |acc, c| {
        acc + if is_wide_char(c) {
            font_size
        } else {
            font_size * 0.6
        }
    })
}

/// 估算一行的渲染宽度（px）。
pub(crate) fn est_row_width(row: &Row, p: &StyleParams) -> f32 {
    match row.kind {
        RowKind::Blank | RowKind::Rule => 0.0,
        RowKind::Image => {
            // 图片行 by contain 撑满可用宽度 → 宽度由视口决定，不计入最宽内容
            0.0
        }
        RowKind::Code => {
            // 等宽字体按 0.65em/字符 + 内部 padding（pl_3 + pr_3 = 24px）+ 外层内缩（px_3 = 24px）
            let widest = row
                .code_lines()
                .iter()
                .map(|l| l.chars().count() as f32)
                .fold(0.0f32, f32::max);
            widest * p.code_size.as_f32() * 0.65 + 48.0
        }
        RowKind::Heading(n) => {
            let (fs, _) = p.heading_metrics(n);
            est_text_width(&row.text, fs.as_f32())
        }
        // 引述：外层 px_3 内缩(左右 24) + 内层 pl_3/pr_3(左右 24) + 左边框 2px（同代码块做法）
        RowKind::Quote => est_text_width(&row.text, p.para_size.as_f32()) + 50.0,
        RowKind::Bullet => {
            let prefix_w = row
                .prefix
                .as_ref()
                .map(|s| s.chars().count() as f32 * 8.0)
                .unwrap_or(0.0);
            est_text_width(&row.text, p.para_size.as_f32()) + prefix_w + 12.0
        }
        RowKind::Paragraph => est_text_width(&row.text, p.para_size.as_f32()),
        RowKind::Table => row
            .table()
            .map(|t| table_column_widths(t, p).iter().sum())
            .unwrap_or(0.0)
            // 渲染时外层 px_3 内缩（左右共 24px）
            + 24.0,
    }
}

/// 估算一个块的渲染宽度（px）。窗口模式（大文档）用：块索引不含清理后文本，
/// 按首行原始字节长度粗估（CJK 每字节 ≈ 0.2em，但这里统一按 0.6em/字节 取略宽，
/// 宁可让横向滚动范围略宽也不截断）。
pub(crate) fn est_row_width_meta(m: &BlockMeta, p: &StyleParams) -> f32 {
    let bytes_w = |b: u32, font: f32| b as f32 * font * 0.6;
    match m.kind {
        // 围栏行本身很短，代码块宽由内部最宽行决定（无法无文本精确估算，
        // 取 fence 行宽与 40 字符的较大者，够用的下限）
        RowKind::Code => m.raw_len.max(40) as f32 * p.code_size.as_f32() * 0.65 + 48.0,
        RowKind::Heading(n) => bytes_w(m.raw_len, p.heading_metrics(n).0.as_f32()),
        RowKind::Quote => bytes_w(m.raw_len, p.para_size.as_f32()) + 50.0,
        RowKind::Bullet => bytes_w(m.raw_len, p.para_size.as_f32()) + 20.0,
        RowKind::Paragraph => bytes_w(m.raw_len, p.para_size.as_f32()),
        RowKind::Table => m.raw_len as f32 * p.para_size.as_f32() * 0.6 + 24.0,
        _ => 0.0,
    }
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
