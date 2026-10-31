//! 自包含 PNG 编码器（运行时零 `png` crate 依赖）。
//!
//! 覆盖与 image/png 0.17 编码输出一致的非隔行 8/16 位布局：灰度/灰度+α/RGB/RGBA × 8/16 位，
//! 对应 IHDR 色型 0/4/2/6 与位深 8/16——与 png crate `ColorType`/`BitDepth` 一一映射。
//! 三项要点：
//! - 逐行固定 filter 0（起步保证简单正确，不做自适应滤波；若要压输出体积再接 Sub/Average/Paeth 选择）；
//! - IDAT 用 `fdeflate::Compressor` 直接产**完整 zlib 流**（头 0x78 0x01 + raw deflate + 大端 adler32，
//!   由该 crate `finish()` 收尾，与 png crate 0.17 `Compression::Fast` 同款后端，见其 `compress.rs`）；
//! - 块 CRC 用 `crc32fast`（tag + payload，大端），16 位输入按本机字节序给出、编码前重排为大端。

use std::io::Write;

use crate::color::ExtendedColorType;
use crate::error::{ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind};
use crate::format::ImageFormat;

/// IHDR 中与「色型 + 位深」一一对应的布局信息。
struct Layout {
    /// 位深（8 或 16）。
    bit_depth: u8,
    /// 色型（0 = 灰度，2 = RGB，4 = 灰度+α，6 = RGBA）。
    color_type: u8,
    /// 每像素通道数。
    channels: u8,
}

/// 扩展色型 → IHDR 布局。仅支持 PNG 可直接表达的 8/16 位布局，其余返回 `Unsupported`。
fn layout_for(color: ExtendedColorType) -> ImageResult<Layout> {
    let (bit_depth, color_type, channels) = match color {
        ExtendedColorType::L8 => (8, 0, 1),
        ExtendedColorType::L16 => (16, 0, 1),
        ExtendedColorType::La8 => (8, 4, 2),
        ExtendedColorType::La16 => (16, 4, 2),
        ExtendedColorType::Rgb8 => (8, 2, 3),
        ExtendedColorType::Rgb16 => (16, 2, 3),
        ExtendedColorType::Rgba8 => (8, 6, 4),
        ExtendedColorType::Rgba16 => (16, 6, 4),
        _ => {
            return Err(ImageError::Unsupported(
                UnsupportedError::from_format_and_kind(
                    ImageFormat::Png.into(),
                    UnsupportedErrorKind::Color(color),
                ),
            ));
        }
    };
    Ok(Layout {
        bit_depth,
        color_type,
        channels,
    })
}

/// 写入一个 PNG 块：len + tag + payload + CRC32（CRC 覆盖 tag+payload，均大端）。
fn write_chunk(w: &mut impl Write, tag: &[u8; 4], payload: &[u8]) -> ImageResult<()> {
    let mut crc = crc32fast::Hasher::new();
    crc.update(tag);
    crc.update(payload);
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(tag)?;
    w.write_all(payload)?;
    w.write_all(&crc.finalize().to_be_bytes())?;
    Ok(())
}

/// 自包含 PNG 编码：把 `width×height` 的像素缓冲（16 位按本机字节序）编码写入 `w`。
/// 返回语义与旧 png crate 路径一致：缓冲长度不符时 `assert`（调用方按 `buffer_size` 计算）；
/// 不支持的色型返回 `ImageError::Unsupported`。
pub(crate) fn encode_png(
    w: &mut impl Write,
    buf: &[u8],
    width: u32,
    height: u32,
    color: ExtendedColorType,
) -> ImageResult<()> {
    let layout = layout_for(color)?;
    let expected = color.buffer_size(width, height);
    assert_eq!(
        expected,
        buf.len() as u64,
        "Invalid buffer length: expected {expected} got {} for {width}x{height} image",
        buf.len(),
    );

    // 文件签名。
    w.write_all(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])?;

    // IHDR。
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(layout.bit_depth);
    ihdr.push(layout.color_type);
    ihdr.push(0); // 压缩方法
    ihdr.push(0); // 滤波方法
    ihdr.push(0); // 非隔行
    write_chunk(w, b"IHDR", &ihdr)?;

    // 逐行前置 filter 字节（filter=0）后整段交给 fdeflate 压缩——IDAT 即完整 zlib 流。
    // 16 位在重排为 8/16 位大端字节序后按行写入；`buf` 为空（0×0）则跳过扫描线（避免
    // `chunks_exact(0)` panic，合法防御）。
    let bpp = usize::from(layout.channels * (layout.bit_depth / 8));
    let row_bytes = (width as usize) * bpp;
    let mut scanlines = Vec::with_capacity((row_bytes + 1) * (height as usize));
    if !buf.is_empty() {
        if layout.bit_depth == 16 && cfg!(target_endian = "little") {
            for row in buf.chunks_exact(row_bytes) {
                scanlines.push(0); // filter=0
                for pair in row.chunks_exact(2) {
                    scanlines.extend_from_slice(&[pair[1], pair[0]]);
                }
            }
        } else {
            for row in buf.chunks_exact(row_bytes) {
                scanlines.push(0);
                scanlines.extend_from_slice(row);
            }
        }
    }

    let mut compressor = fdeflate::Compressor::new(Vec::new())?;
    compressor.write_data(&scanlines)?;
    let idat = compressor.finish()?;
    write_chunk(w, b"IDAT", &idat)?;

    write_chunk(w, b"IEND", &[])?;
    w.flush()?;
    Ok(())
}
