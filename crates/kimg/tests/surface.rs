//! P3/P4 调用面编译探针：逐条复刻仓库消费方的 `use image::…` 调用形态（仅用 kimg，
//! 不碰 image dev-dep），保证「把 `image` alias/rename 到 kimg 后可一次编译」。
//! 运行时只做最轻量断言（编译通过即主要收益）。随 kimg 变化同步维护。

use std::io::Cursor;

use kimg::codecs::gif::{GifDecoder, GifEncoder};
use kimg::codecs::png::PngDecoder;
use kimg::codecs::webp::WebPDecoder;
use kimg::{
    AnimationDecoder, DynamicImage, Frame, GenericImageView, ImageBuffer, ImageDecoder, ImageFormat,
    ImageReader, Limits, Rgba, RgbaImage,
};

/// 复刻 gpui `platform.rs` / `img.rs` / `image_viewer` 的 PngDecoder + ImageDecoder 消费路径。
#[test]
fn png_decoder_consumer_patterns() {
    // 与 image_viewer::decode_png_display 同款。
    let bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let dec_res = PngDecoder::new(Cursor::new(bytes));
    if let Ok(mut dec) = dec_res {
        let _ = dec.set_limits(Limits::default());
        let _ = dec.dimensions();
        let color = dec.color_type();
        let _n = usize::try_from(dec.total_bytes()).unwrap();
        // ColorType 穷举 match（image_viewer::expand_png_to_rgba8 全套分支）。
        match color {
            kimg::ColorType::Rgba8
            | kimg::ColorType::Rgb8
            | kimg::ColorType::L8
            | kimg::ColorType::La8
            | kimg::ColorType::Rgba16
            | kimg::ColorType::Rgb16
            | kimg::ColorType::L16
            | kimg::ColorType::La16 => {}
            _ => {}
        }
    }
}

/// 复刻 gpui `img.rs` 的 WebP/GIF 动画消费路径（含 set_background_color）。
#[test]
fn animation_consumer_patterns() {
    // gif 路径：GifDecoder::new + into_frames() 逐帧 Ok(mut frame)。
    let gif_bytes = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
    if let Ok(decoder) = GifDecoder::new(Cursor::new(&gif_bytes[..])) {
        let mut n = 0usize;
        for frame in decoder.into_frames() {
            match frame {
                Ok(mut frame) => {
                    let _ = frame.buffer_mut();
                    let _ = frame.delay();
                    let _ = frame.buffer().dimensions();
                    n += 1;
                }
                Err(_) => {}
            }
        }
        assert_eq!(n, 0); // 1x1 GIF 无逻辑屏画布数据，帧流为空是可接受结果
    }

    // webp 路径：new + set_background_color + into_frames（gpui img.rs 精确形态）。
    let webp_bytes = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
    if let Ok(mut decoder) = WebPDecoder::new(Cursor::new(&webp_bytes[..])) {
        let _ = decoder.set_background_color(Rgba([0, 0, 0, 0]));
        let _frames: Vec<kimg::ImageResult<Frame>> = decoder.into_frames().collect();
    }

    // collect_frames（imgbench 消费）。
    let _ = GifDecoder::new(Cursor::new(b"")).map(|d| d.into_frames().collect_frames());
}

/// 复刻 gpui `platform.rs` ImageReader → impl ImageDecoder → DynamicImage → into_rgba8。
#[test]
fn reader_to_dynamic_consumer_patterns() {
    let bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    fn consume(mut decoder: impl ImageDecoder) -> Option<RgbaImage> {
        let orientation = decoder.orientation().ok()?;
        let mut image = DynamicImage::from_decoder(decoder).ok()?;
        image.apply_orientation(orientation);
        Some(image.into_rgba8())
    }

    // 输入为残缺 PNG 头：编译面是重点，解码失败可接受。
    match ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png).into_decoder() {
        Ok(decoder) => {
            let _ = consume(decoder);
        }
        Err(_) => {}
    }
}

/// 复刻 tray `create_hicon_from_ico`：imageops::resize + Lanczos3 + as_raw。
#[test]
fn imageops_resize_consumer_pattern() {
    let rgba = RgbaImage::from_fn(288, 288, |x, y| Rgba([x as u8, y as u8, 0, 255]));
    let small = kimg::imageops::resize(
        &rgba,
        64,
        64,
        kimg::imageops::FilterType::Lanczos3,
    );
    let (w, h) = small.dimensions();
    assert_eq!((w, h), (64, 64));
    let mut bgra = Vec::new();
    for px in small.as_raw().chunks_exact(4) {
        bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    assert_eq!(bgra.len(), 64 * 64 * 4);
}

/// 复刻 ui/assets + pdf/math_typeset + music/cover + video/display 的自由函数形态。
#[test]
fn free_function_consumer_patterns() {
    // ui/assets：ImageBuffer::from_fn + GenericImageView::get_pixel（显式经 trait 调用）。
    let src = RgbaImage::from_fn(2, 2, |x, y| Rgba([x as u8, y as u8, 0, 255]));
    let p0 = GenericImageView::get_pixel(&src, 0, 0);
    assert_eq!(p0.0, [0, 0, 0, 255]);
    let p11 = GenericImageView::get_pixel(&src, 1, 1);
    assert_eq!(p11.0, [1, 1, 0, 255]);
    let bgra: ImageBuffer<kimg::Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(2, 2, |x, y| {
        let p = src.get_pixel(x, y);
        Rgba([p[2], p[1], p[0], p[3]])
    });
    let _frame = Frame::new(bgra);

    // image_viewer 测试 / gpui img.rs：GifEncoder::new + encode_frame + set_repeat。
    let mut gif_bytes = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut gif_bytes);
        let _ = enc.set_repeat(kimg::codecs::gif::Repeat::Finite(2));
        enc.encode_frame(Frame::new(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 4]))))
            .unwrap();
    }
    assert!(gif_bytes.len() > 10);

    // math_typeset：load_from_memory → into_rgba8 → dimensions → into_raw。
    let bytes = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let _ = kimg::load_from_memory(&bytes)
        .ok()
        .map(|img| img.into_rgba8())
        .map(|rgba| {
            let _ = rgba.dimensions();
            rgba.into_raw()
        });

    // pdf/lib：RgbaImage::from_raw + Frame::new。
    let buf = RgbaImage::from_raw(2, 1, vec![0u8; 8]).unwrap();
    let _frame = Frame::new(buf);

    // music/cover + view/tests：DynamicImage::ImageRgba8(...).write_to(...Png)。
    let img = RgbaImage::from_pixel(2, 1, Rgba([255u8, 0, 0, 255]));
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("png encode ok");
    assert!(out.len() > 8);

    // video/display + viewer/tests：save_buffer_with_format + ExtendedColorType。
    let pixels = vec![0u8; 4 * 4];
    kimg::save_buffer_with_format(
        std::env::temp_dir().join("kimg_surface_probe.png"),
        &pixels,
        2,
        2,
        kimg::ExtendedColorType::Rgba8,
        ImageFormat::Png,
    )
    .expect("png save ok");

    // gpui_windows clipboard：load_from_memory_with_format + write_to。
    let bytes2 = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let _ = kimg::load_from_memory_with_format(&bytes2, ImageFormat::Png)
        .ok()
        .map(|img| img.write_to(&mut Cursor::new(Vec::new()), ImageFormat::Png));

    // ImageFormat 变体（gpui platform.rs 的 decode_static_image 列举）。
    let fmts = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::WebP,
        ImageFormat::Bmp,
        ImageFormat::Tiff,
        ImageFormat::Ico,
        ImageFormat::Pnm,
    ];
    assert_eq!(fmts.len(), 7);
    let _ = kimg::guess_format(&bytes);
}

/// 复刻 image_viewer 测试 `write_png`/灰度 PNG 路径：ImageBuffer 的 inherent
/// `enumerate_pixels_mut`（无 trait 入作用域）+ `save`（PNG，含 16bit 灰度）须可编译可跑。
#[test]
fn imagebuffer_save_and_inherent_iterators() {
    use kimg::RgbImage;

    let dir = std::env::temp_dir().join("kimg_surface_save");
    std::fs::create_dir_all(&dir).unwrap();

    // Rgb8 渐变（用 inherent enumerate_pixels_mut，不 import GenericImage）。
    let mut rgb = RgbImage::new(40, 20);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = kimg::Rgb([(x * 6) as u8, (y * 12) as u8, 200]);
    }
    let p = dir.join("save_rgb.png");
    rgb.save(&p).expect("RGB PNG save ok");
    assert!(std::fs::metadata(&p).unwrap().len() > 0);

    // 读回断言尺寸与像素（kimg 自产自销 roundtrip）。
    let back = kimg::load_from_memory(&std::fs::read(&p).unwrap()).unwrap();
    assert_eq!(back.dimensions(), (40, 20));
    let rgba = back.into_rgba8();
    assert_eq!(rgba.get_pixel(1, 1).0[0], 6);

    // Luma<u16> 16bit 灰度 PNG。
    let mut g16 = kimg::Gray16Image::new(4, 4);
    g16.put_pixel(1, 1, kimg::Luma([257u16]));
    let p16 = dir.join("save_gray16.png");
    g16.save(&p16).expect("gray16 PNG save ok");
    let back16 = kimg::load_from_memory(&std::fs::read(&p16).unwrap()).unwrap();
    assert_eq!(back16.color(), kimg::ColorType::L16);

    // 自由函数 save_buffer（按扩展名推断格式）。
    kimg::save_buffer(
        dir.join("free.png"),
        &[0u8; 4 * 4],
        2,
        2,
        kimg::ExtendedColorType::Rgba8,
    )
    .expect("save_buffer png ok");

    let _ = std::fs::remove_dir_all(&dir);
}
