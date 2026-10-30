//! kimg 基础回归测试：合成图编码→解码回环、PngDecoder 全 API、GIF 时序、EXIF 方向、Limits 防炸弹。
//!
//! 仅用 kimg 自身（不依赖 image crate 参考），保证基础机制成立。

use std::io::Cursor;

use kimg::{
    codecs::gif::{GifDecoder, GifEncoder},
    codecs::png::PngDecoder,
    imageops::{self, FilterType},
    AnimationDecoder, Delay, DynamicImage, Frame, ImageBuffer,
    ImageDecoder as _, ImageFormat, ImageReader, Limits, Luma, Rgba, RgbaImage,
};

/// 1x1 红像素 PNG（RGBA8）。
fn red_png_bytes(width: u32, height: u32) -> Vec<u8> {
    let img = RgbaImage::from_pixel(width, height, Rgba([255, 0, 0, 255]));
    let mut out = Vec::new();
    DynamicImage::from(img)
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

#[test]
fn png_decoder_full_api() {
    let bytes = red_png_bytes(3, 2);
    let mut decoder = PngDecoder::new(Cursor::new(&bytes)).unwrap();
    decoder.set_limits(Limits::default()).unwrap();
    assert_eq!(decoder.dimensions(), (3, 2));
    assert_eq!(decoder.color_type(), kimg::ColorType::Rgba8);
    assert_eq!(decoder.total_bytes(), 3 * 2 * 4);

    let mut buf = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buf).unwrap();
    assert_eq!(&buf[..4], &[255, 0, 0, 255]);
    assert_eq!(buf[12], 255); // (0,2)=(2,1) 行末像素
}

#[test]
fn png_gray16_load_and_convert() {
    // 16 位灰度像素 → to_rgba8 用 (v+128)/257。
    let img = ImageBuffer::<Luma<u16>, Vec<u16>>::from_pixel(2, 1, Luma([0xABCDu16]));
    let mut out = Vec::new();
    DynamicImage::from(img)
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    let decoded = kimg::load_from_memory(&out).unwrap();
    let rgba = decoded.to_rgba8();
    let expected = ((0xABCDu32 + 128) / 257) as u8;
    assert_eq!(rgba.get_pixel(0, 0), &Rgba([expected, expected, expected, 255]));
}

#[test]
fn gif_encoder_decoder_roundtrip() {
    // 造 2 帧 GIF（与 image_viewer 测试同款姿势）并读回逐帧比对。
    let mut bytes = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut bytes);
        let frame = Frame::from_parts(
            RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255])),
            0,
            0,
            Delay::from_numer_denom_ms(200, 1),
        );
        enc.encode_frame(frame).unwrap();
        let frame2 = Frame::new(RgbaImage::from_pixel(4, 4, Rgba([0, 255, 0, 255])));
        enc.encode_frame(frame2).unwrap();
    }

    let decoder = GifDecoder::new(Cursor::new(&bytes)).unwrap();
    assert_eq!(decoder.dimensions(), (4, 4));
    let frames: Vec<_> = decoder.into_frames().collect_frames().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].buffer().as_raw()[0], 255);
    assert_eq!(frames[0].buffer().as_raw()[1], 0);
    assert_eq!(frames[1].buffer().as_raw()[1], 255);
    assert_eq!(Duration::from(frames[0].delay()).as_millis(), 200);
    let _ = std::time::Duration::from_millis(0);
}

use std::time::Duration;

#[test]
fn pnm_ascii_and_binary() {
    // P2 灰度文本：3x2，maxval=255。
    let p2 = b"P2\n3 2\n255\n0 128 255\n10 20 30\n";
    let img = kimg::load_from_memory(p2).unwrap();
    let gray = img.to_luma8();
    assert_eq!(gray.get_pixel(0, 0).0[0], 0);
    assert_eq!(gray.get_pixel(1, 0).0[0], 128);
    assert_eq!(gray.get_pixel(2, 1).0[0], 30);

    // P6 二进制 RGB 2x1。
    let p6 = b"P6\n2 1\n255\n\xff\x00\x00\x00\xff\x00\n";
    let img = kimg::load_from_memory(p6).unwrap();
    let rgb = img.to_rgb8();
    assert_eq!(rgb.get_pixel(0, 0).0, [255, 0, 0]);
    assert_eq!(rgb.get_pixel(1, 0).0, [0, 255, 0]);

    // P4 位图（1 bit；0 位 = 白 → 255；1 位 = 黑 → 0）。
    let p4 = b"P4\n8 1\n\x00";
    let img = kimg::load_from_memory(p4).unwrap();
    let gray = img.to_luma8();
    assert_eq!(gray.dimensions(), (8, 1));
    assert_eq!(gray.get_pixel(0, 0).0[0], 255);
    assert_eq!(gray.get_pixel(7, 0).0[0], 255);

    // 0b10000000 → 首像素黑（0），其余白（255）。
    let p4b = b"P4\n8 1\n\x80";
    let gray = kimg::load_from_memory(p4b).unwrap().to_luma8();
    assert_eq!(gray.get_pixel(0, 0).0[0], 0);
    assert_eq!(gray.get_pixel(1, 0).0[0], 255);
}

#[test]
fn bmp_palette_and_24bit() {
    // 手写 24bit 2x1 BMP（标准 Info 头；行宽 6 字节 + 2 字节填充 = 每行 8 字节）。
    let mut bmp = Vec::new();
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&62u32.to_le_bytes()); // 文件大小
    bmp.extend_from_slice(&0u32.to_le_bytes()); // 保留
    bmp.extend_from_slice(&54u32.to_le_bytes()); // 像素偏移
    bmp.extend_from_slice(&40u32.to_le_bytes()); // 头大小
    bmp.extend_from_slice(&2i32.to_le_bytes()); // 宽
    bmp.extend_from_slice(&1i32.to_le_bytes()); // 高
    bmp.extend_from_slice(&1u16.to_le_bytes()); // 平面
    bmp.extend_from_slice(&24u16.to_le_bytes()); // 位深
    bmp.extend_from_slice(&0u32.to_le_bytes()); // 压缩
    bmp.extend_from_slice(&8u32.to_le_bytes()); // 图像数据大小
    bmp.extend_from_slice(&2835u32.to_le_bytes()); // 分辨率
    bmp.extend_from_slice(&2835u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes()); // 颜色数
    bmp.extend_from_slice(&0u32.to_le_bytes()); // 重要颜色
    // 底行: BGR(红) BGR(绿) + 2 填充；底部起存。
    bmp.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);

    let img = kimg::load_from_memory(&bmp).unwrap();
    let rgb = img.to_rgb8();
    assert_eq!(rgb.dimensions(), (2, 1));
    assert_eq!(rgb.get_pixel(0, 0).0, [255, 0, 0]); // 底部行反转后 (0,0) 为文件首像素
    assert_eq!(rgb.get_pixel(1, 0).0, [0, 255, 0]);
}

#[test]
fn imageops_resize_sizes() {
    let img = RgbaImage::from_fn(8, 8, |x, y| Rgba([x as u8, y as u8, 0, 255]));
    let small = imageops::resize(&img, 4, 4, FilterType::Lanczos3);
    let nearest = imageops::resize(&img, 4, 4, FilterType::Nearest);
    assert_eq!(small.dimensions(), (4, 4));
    assert_eq!(nearest.dimensions(), (4, 4));
    assert_eq!(small.get_pixel(0, 0).0[0], 0);

    // 空源语义（image 0.25 一致）：返回请求尺寸的零图，不 panic、不超大分配。
    let empty = RgbaImage::from_raw(0, 100, vec![]).unwrap();
    let r = imageops::resize(&empty, 256, 256, FilterType::Lanczos3);
    assert_eq!(r.dimensions(), (256, 256));
    assert!(r.as_raw().iter().all(|&b| b == 0));
    // 标题栏路径：288→64 精确落点。
    let icon = RgbaImage::from_pixel(288, 288, Rgba([1, 2, 3, 255]));
    let ic = imageops::resize(&icon, 64, 64, FilterType::Lanczos3);
    assert_eq!(ic.dimensions(), (64, 64));
    assert!(ic.as_raw().chunks_exact(4).any(|p| p[3] == 255));
}

#[test]
fn exif_orientation_chunk_parse() {
    // 构造小 EXIF 块：II + 0x2a + 0 + 1 条目(tag0x0112 form3 count1 value6)。
    let mut chunk = Vec::new();
    chunk.extend_from_slice(&[0x49, 0x49, 42, 0]);
    chunk.extend_from_slice(&(8u32).to_le_bytes()); // ifd 偏移 = 8
    chunk.extend_from_slice(&(1u16).to_le_bytes()); // 条目数
    // tag=0x0112, format=3, count=1, value=6
    chunk.extend_from_slice(&0x0112u16.to_le_bytes());
    chunk.extend_from_slice(&3u16.to_le_bytes());
    chunk.extend_from_slice(&1u32.to_le_bytes());
    let mut value = [0u8; 4];
    value[0] = 6;
    chunk.extend_from_slice(&value);

    assert_eq!(
        kimg::Orientation::from_exif_chunk(&chunk),
        Some(kimg::Orientation::Rotate90)
    );

    // Rotate90 应用后宽高互换。
    let img = RgbaImage::from_fn(3, 2, |x, y| Rgba([x as u8, y as u8, 0, 255]));
    let mut dyn_img = DynamicImage::ImageRgba8(img);
    dyn_img.apply_orientation(kimg::Orientation::Rotate90);
    assert_eq!(dyn_img.dimensions(), (2, 3));
}

#[test]
fn limits_bomb_rejected() {
    // 伪造声明巨大尺寸的 PNG（真实解码路径会因 png crate limits 报错而非 OOM）。
    // 这里用 Limits 尺寸限制拦截超大声明：构造一个 1x1 PNG 但声明 70000 宽 → 抛 Limits 错误。
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    bytes.extend_from_slice(&(13u32).to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&70000u32.to_be_bytes()); // 宽
    bytes.extend_from_slice(&70000u32.to_be_bytes()); // 高
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]); // 8bit RGBA
    bytes.extend_from_slice(&0u32.to_be_bytes()); // CRC（不校验何时触发取决于实现）
    bytes.extend_from_slice(&(0u32).to_be_bytes()); // IEND
    bytes.extend_from_slice(b"IEND");

    let decoder = PngDecoder::new(Cursor::new(&bytes));
    if let Ok(mut d) = decoder {
        let r = d.set_limits(Limits {
            max_image_width: Some(65535),
            max_image_height: Some(65535),
            max_alloc: Some(1 << 20),
        });
        assert!(r.is_err(), "超限应返回错误而非分配");
    }
}

#[test]
fn image_reader_with_format_and_guessed() {
    let bytes = red_png_bytes(1, 1);
    let reader = ImageReader::with_format(Cursor::new(&bytes), ImageFormat::Png);
    assert_eq!(reader.format(), Some(ImageFormat::Png));
    let img = reader.decode().unwrap();
    assert_eq!(img.dimensions(), (1, 1));

    // 猜测路径。
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .unwrap();
    assert_eq!(reader.format(), Some(ImageFormat::Png));

    // 自由函数。
    assert_eq!(kimg::guess_format(&bytes).unwrap(), ImageFormat::Png);
    assert_eq!(
        kimg::load_from_memory_with_format(&bytes, ImageFormat::Png)
            .unwrap()
            .to_rgba8()
            .get_pixel(0, 0),
        &Rgba([255, 0, 0, 255])
    );
}
