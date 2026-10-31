//! 自包含 PNG 编码器（`codecs/png_encoder.rs`）回归：PngEncoder 及 save/write_to 全链路的编码产物
//! 可被自研 pngsimd 解码（tests/encode_self 的自 oracle）与 png crate（dev 对照）读回**逐字节一致**。
//!
//! 覆盖：
//! - (a) PngEncoder 编码 → kimg PngDecoder 解码 == 源缓冲（8/16 位全色型）；
//! - (b) PngEncoder 编码 → png crate EXPAND 解码 == 源缓冲（cross oracle）；
//! - (c) save_buffer_with_format / ImageBuffer::save / write_to(DynamicImage 8 变体) 用遍；
//! - (d) 16bit 变体回环（L16/La16/Rgb16/Rgba16，含字节序敏感取值）。
//! 另校验输出 PNG 结构与块 CRC（直接解包 IHDR/IDAT/IEND 逐块校验）。

use std::io::Cursor;

use kimg::codecs::png::PngDecoder;
use kimg::{
    ColorType, DynamicImage, ExtendedColorType, ImageBuffer, ImageDecoder as _, ImageFormat, Luma,
    LumaA, Rgb, Rgba, Gray16Image, RgbImage,
};

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 生成 `w×h`、给定扩展色型的像素缓冲（8 位按字节；16 位按本机字节序写入内存）。
fn make_buf(w: u32, h: u32, color: ExtendedColorType) -> Vec<u8> {
    let len = color.buffer_size(w, h) as usize;
    let mut buf = Vec::with_capacity(len);
    match color {
        // 16 位：每个样本 `idx*29+1`（小端写，值域横跨高低字节，端序错即失真）。
        ExtendedColorType::L16
        | ExtendedColorType::La16
        | ExtendedColorType::Rgb16
        | ExtendedColorType::Rgba16 => {
            let mut idx = 0usize;
            while buf.len() < len {
                let v = ((idx as u16).wrapping_mul(29)).wrapping_add(1);
                buf.push((v & 0xff) as u8);
                buf.push((v >> 8) as u8);
                idx += 1;
            }
        }
        // 8 位：字节级确定性排列。
        _ => {
            for i in 0..len {
                buf.push((i as u8).wrapping_mul(37).wrapping_add((i / 13) as u8));
            }
        }
    }
    buf
}

/// 把字节缓冲重解释为 `Vec<u16>`（本机字节序；与 `make_buf` 的 16 位写入对应）。
fn bytes_to_u16(b: &[u8]) -> Vec<u16> {
    b.chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect()
}

/// png crate 参照解码（EXPAND + 16bit 大端→本机，与 `png_port_parity::reference_expand` 同款）。
fn reference_expand(png_bytes: &[u8]) -> (ColorType, Vec<u8>) {
    let mut dec = png::Decoder::new(Cursor::new(png_bytes));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info().expect("png 参照 read_info");
    let mut out = vec![0u8; reader.output_buffer_size()];
    reader.next_frame(&mut out).expect("png 参照 next_frame");
    let (ct, bits) = reader.output_color_type();
    let color = match (ct, bits) {
        (png::ColorType::Grayscale, png::BitDepth::Eight) => ColorType::L8,
        (png::ColorType::Grayscale, png::BitDepth::Sixteen) => ColorType::L16,
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => ColorType::La8,
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Sixteen) => ColorType::La16,
        (png::ColorType::Rgb, png::BitDepth::Eight) => ColorType::Rgb8,
        (png::ColorType::Rgb, png::BitDepth::Sixteen) => ColorType::Rgb16,
        (png::ColorType::Rgba, png::BitDepth::Eight) => ColorType::Rgba8,
        (png::ColorType::Rgba, png::BitDepth::Sixteen) => ColorType::Rgba16,
        _ => panic!("参照输出色彩类型未知"),
    };
    if bits == png::BitDepth::Sixteen {
        for c in out.as_chunks_mut::<2>().0.iter_mut() {
            *c = u16::from_be_bytes(*c).to_ne_bytes();
        }
    }
    (color, out)
}

/// 自研 pngsimd 解码为（ColorType, 本机字节序缓冲）。
fn self_expand(png_bytes: &[u8]) -> (ColorType, Vec<u8>) {
    let dec = PngDecoder::new(Cursor::new(png_bytes)).expect("自研解码 read_info");
    let ct = dec.color_type();
    let mut out = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut out).expect("自研解码 next_frame");
    (ct, out)
}

/// 解包 PNG 逐块校验 CRC（tag+payload 的 crc32）；同时返回 IHDR 字段，供结构断言。
fn check_chunks_crc_and_ihdr(png_bytes: &[u8]) -> (u32, u32, u8, u8) {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut h = crc32fast::Hasher::new();
        h.update(bytes);
        h.finalize()
    }
    let mut pos = 8usize; // 跳过 8 字节签名
    let mut ihdr = None;
    while pos + 8 <= png_bytes.len() {
        let len = u32::from_be_bytes(png_bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let tag = &png_bytes[pos + 4..pos + 8];
        let payload = &png_bytes[pos + 8..pos + 8 + len];
        let expected_crc =
            u32::from_be_bytes(png_bytes[pos + 8 + len..pos + 12 + len].try_into().unwrap());
        let mut covered = Vec::with_capacity(4 + len);
        covered.extend_from_slice(tag);
        covered.extend_from_slice(payload);
        assert_eq!(crc32(&covered), expected_crc, "块 {} CRC 不符", String::from_utf8_lossy(tag));
        if tag == b"IHDR" {
            let w = u32::from_be_bytes(payload[0..4].try_into().unwrap());
            let h = u32::from_be_bytes(payload[4..8].try_into().unwrap());
            ihdr = Some((w, h, payload[8], payload[9])); // 宽 / 高 / 位深 / 色型
        }
        if tag == b"IEND" {
            break;
        }
        pos += 12 + len;
    }
    ihdr.expect("缺 IEND/IHDR")
}

// ---------------------------------------------------------------------------
// (a)(b)(d)：PngEncoder 全色型 × 双 oracle
// ---------------------------------------------------------------------------

/// 覆盖的 8 个可编码色型（与 png crate `ColorType`/`BitDepth` 一一对应）。
const COLOR_CASES: &[(ExtendedColorType, (u32, u32))] = &[
    (ExtendedColorType::L8, (1, 1)),
    (ExtendedColorType::L8, (7, 5)),
    (ExtendedColorType::La8, (3, 4)),
    (ExtendedColorType::Rgb8, (5, 3)),
    (ExtendedColorType::Rgba8, (16, 9)),
    (ExtendedColorType::L16, (4, 4)),
    (ExtendedColorType::La16, (6, 2)),
    (ExtendedColorType::Rgb16, (3, 7)),
    (ExtendedColorType::Rgba16, (11, 8)),
];

#[test]
fn png_encoder_self_and_cross_oracle() {
    /// 8 位可编码扩展色型 → 颜色类型（与解码输出一一对应）。
    fn to_color(color: ExtendedColorType) -> ColorType {
        match color {
            ExtendedColorType::L8 => ColorType::L8,
            ExtendedColorType::L16 => ColorType::L16,
            ExtendedColorType::La8 => ColorType::La8,
            ExtendedColorType::La16 => ColorType::La16,
            ExtendedColorType::Rgb8 => ColorType::Rgb8,
            ExtendedColorType::Rgb16 => ColorType::Rgb16,
            ExtendedColorType::Rgba8 => ColorType::Rgba8,
            ExtendedColorType::Rgba16 => ColorType::Rgba16,
            other => panic!("未覆盖色型 {other:?}"),
        }
    }

    for &(color, (w, h)) in COLOR_CASES {
        let src = make_buf(w, h, color);
        let mut out = Vec::new();
        kimg::codecs::png::PngEncoder::new(&mut out)
            .write_image(&src, w, h, color)
            .unwrap();

        // 结构：签名 + IHDR 位深/色型 反射 + 全部块 CRC。
        assert!(out.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        let (iw, ih, depth, ct) = check_chunks_crc_and_ihdr(&out);
        assert_eq!((iw, ih), (w, h), "{color:?} 尺寸");
        let (exp_depth, exp_ct) = match color {
            ExtendedColorType::L8 => (8, 0),
            ExtendedColorType::L16 => (16, 0),
            ExtendedColorType::La8 => (8, 4),
            ExtendedColorType::La16 => (16, 4),
            ExtendedColorType::Rgb8 => (8, 2),
            ExtendedColorType::Rgb16 => (16, 2),
            ExtendedColorType::Rgba8 => (8, 6),
            ExtendedColorType::Rgba16 => (16, 6),
            _ => unreachable!(),
        };
        assert_eq!((depth, ct), (exp_depth, exp_ct), "{color:?} IHDR");

        // (a) 自研 pngsimd 解码回环。
        let (rct, rbuf) = self_expand(&out);
        assert_eq!(rct, to_color(color), "{color:?} 自解码色型");
        assert_eq!(rbuf, src, "自 oracle {color:?} {w}x{h}");

        // (b) png crate 对照解码。
        let (cct, cbuf) = reference_expand(&out);
        assert_eq!(cct, to_color(color), "{color:?} 对照色型");
        assert_eq!(cbuf, src, "cross oracle {color:?} {w}x{h}");

        // 双 oracle 之间也逐字节一致。
        assert_eq!(rbuf, cbuf, "双 oracle 一致 {color:?} {w}x{h}");
    }
}

// ---------------------------------------------------------------------------
// (c)：save_buffer_with_format / ImageBuffer::save / DynamicImage::write_to 全链路
// ---------------------------------------------------------------------------

/// 字节缓冲 → 对应 `DynamicImage` 变体（16 位按本机字节序）。
fn dyn_from_bytes(w: u32, h: u32, color: ColorType, bytes: &[u8]) -> DynamicImage {
    macro_rules! img8 {
        ($ctor:ident, $t:ty) => {
            DynamicImage::$ctor(ImageBuffer::<$t, Vec<u8>>::from_raw(w, h, bytes.to_vec()).unwrap())
        };
    }
    macro_rules! img16 {
        ($ctor:ident, $t:ty) => {
            DynamicImage::$ctor(
                ImageBuffer::<$t, Vec<u16>>::from_raw(w, h, bytes_to_u16(bytes)).unwrap(),
            )
        };
    }
    match color {
        ColorType::L8 => img8!(ImageLuma8, Luma<u8>),
        ColorType::La8 => img8!(ImageLumaA8, LumaA<u8>),
        ColorType::Rgb8 => img8!(ImageRgb8, Rgb<u8>),
        ColorType::Rgba8 => img8!(ImageRgba8, Rgba<u8>),
        ColorType::L16 => img16!(ImageLuma16, Luma<u16>),
        ColorType::La16 => img16!(ImageLumaA16, LumaA<u16>),
        ColorType::Rgb16 => img16!(ImageRgb16, Rgb<u16>),
        ColorType::Rgba16 => img16!(ImageRgba16, Rgba<u16>),
        other => panic!("未覆盖变体 {other:?}"),
    }
}

/// DynamicImage::write_to(Png) 八变体 → 双 oracle 读回 == 源缓冲。
#[test]
fn dynamic_image_write_to_all_variants() {
    let (w, h) = (5u32, 4u32);
    let variants = [
        (ColorType::L8, ExtendedColorType::L8),
        (ColorType::La8, ExtendedColorType::La8),
        (ColorType::Rgb8, ExtendedColorType::Rgb8),
        (ColorType::Rgba8, ExtendedColorType::Rgba8),
        (ColorType::L16, ExtendedColorType::L16),
        (ColorType::La16, ExtendedColorType::La16),
        (ColorType::Rgb16, ExtendedColorType::Rgb16),
        (ColorType::Rgba16, ExtendedColorType::Rgba16),
    ];
    for (color, ecolor) in variants {
        let bytes = make_buf(w, h, ecolor);
        let img = dyn_from_bytes(w, h, color, &bytes);
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap_or_else(|e| panic!("write_to {color:?} 编码失败 {e}"));

        let (_, rbuf) = self_expand(&out);
        assert_eq!(rbuf, bytes, "write_to 自 oracle {color:?}");
        let (_, cbuf) = reference_expand(&out);
        assert_eq!(cbuf, bytes, "write_to cross oracle {color:?}");
    }
}

/// ImageBuffer::save（Rgb8 与 Luma16）+ save_buffer_with_format（Rgba8）落盘路径。
#[test]
fn save_buffer_and_imagebuffer_save_paths() {
    let dir = std::env::temp_dir().join("kimg_encode_self");
    std::fs::create_dir_all(&dir).unwrap();

    // ImageBuffer::save：8 位 RGB。
    let w = 9;
    let h = 7;
    let rgb_bytes = make_buf(w, h, ExtendedColorType::Rgb8);
    let rgb = RgbImage::from_raw(w, h, rgb_bytes.clone()).unwrap();
    let p1 = dir.join("save_rgb.png");
    rgb.save(&p1).expect("RGB save");
    let (_, rref) = reference_expand(&std::fs::read(&p1).unwrap());
    assert_eq!(rref, rgb_bytes, "ImageBuffer::save Rgb8");

    // ImageBuffer::save：16 位灰度（端序敏感）。
    let g16_bytes = make_buf(6, 4, ExtendedColorType::L16);
    let g16 = Gray16Image::from_raw(6, 4, bytes_to_u16(&g16_bytes)).unwrap();
    let p2 = dir.join("save_gray16.png");
    g16.save(&p2).expect("gray16 save");
    let (_, rref16) = reference_expand(&std::fs::read(&p2).unwrap());
    assert_eq!(rref16, g16_bytes, "ImageBuffer::save Luma16");
    // 自研解码色型保持 L16。
    let back16 = kimg::load_from_memory(&std::fs::read(&p2).unwrap()).unwrap();
    assert_eq!(back16.color(), ColorType::L16);

    // save_buffer_with_format：Rgba8。
    let px = make_buf(4, 3, ExtendedColorType::Rgba8);
    kimg::save_buffer_with_format(
        dir.join("free.png"),
        &px,
        4,
        3,
        ExtendedColorType::Rgba8,
        ImageFormat::Png,
    )
    .expect("save_buffer_with_format");
    let (_, rref3) = reference_expand(&std::fs::read(dir.join("free.png")).unwrap());
    assert_eq!(rref3, px, "save_buffer_with_format Rgba8");

    // 不支持色型（32F）返回 Unsupported，不 panic。
    let err = kimg::codecs::png::PngEncoder::new(&mut Vec::new())
        .write_image(&[0u8; 16], 1, 1, ExtendedColorType::Rgba32F);
    assert!(matches!(err, Err(kimg::ImageError::Unsupported(_))));

    let _ = std::fs::remove_dir_all(&dir);
}
