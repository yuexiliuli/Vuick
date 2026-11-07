//! 集成测试共享夹具：语料定位 + zune-jpeg 参照（oracle）解码。
//!
//! zune-jpeg 0.5.15 是 image 0.25 自用 JPEG 后端（当前 kimg_jpeg 被替换前的后端），
//! 我们把它的输出作为「历史行为 oracle」：自研解码器必须与其逐字节一致。
//! 本模块的 `zune_decode` 复刻旧 `codecs/jpeg.rs` wrapper 的选项语义：
//! 输入 YCbCr→RGB / RGB→RGB / RGBA→RGBA / Luma→L8 / LumaA→La8，其余（CMYK/YCCK）→RGB。

use std::path::{Path, PathBuf};

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace as ZuneColorSpace;
use zune_core::options::DecoderOptions;

use kimg_core::color::ColorType;

/// 语料目录：`<workspace>/target/测试图片/jpeg`（env `KIMG_JPEG_CORPUS` 覆盖）。
pub fn corpus_dir() -> PathBuf {
    if let Ok(p) = std::env::var("KIMG_JPEG_CORPUS") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("测试图片")
        .join("jpeg")
}

pub fn collect_jpegs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("jpg"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// zune 参照解码：同口径（目标输出颜色空间 = 旧 wrapper 映射）解出
/// (width, height, color_type, bytes)。
#[allow(dead_code)] // 仅部分测试（bench/parity）使用，probe_ablate 等不含
pub fn zune_decode(bytes: &[u8]) -> Result<(u32, u32, ColorType, Vec<u8>), String> {
    let options = DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(usize::MAX)
        .set_max_height(usize::MAX);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions().ok_or("解码头后无尺寸")?;
    let input_cs = decoder
        .input_colorspace()
        .ok_or("解码头后无输入颜色空间")?;
    let target = match input_cs {
        ZuneColorSpace::RGB
        | ZuneColorSpace::RGBA
        | ZuneColorSpace::Luma
        | ZuneColorSpace::LumaA => input_cs,
        _ => ZuneColorSpace::RGB,
    };
    decoder.set_options(decoder.options().jpeg_set_out_colorspace(target));
    let ct = match target {
        ZuneColorSpace::RGB => ColorType::Rgb8,
        ZuneColorSpace::RGBA => ColorType::Rgba8,
        ZuneColorSpace::Luma => ColorType::L8,
        ZuneColorSpace::LumaA => ColorType::La8,
        _ => ColorType::Rgb8,
    };
    let mut buf = vec![0u8; w as usize * h as usize * usize::from(ct.bytes_per_pixel())];
    decoder.decode_into(&mut buf).map_err(|e| e.to_string())?;
    Ok((w as u32, h as u32, ct, buf))
}
