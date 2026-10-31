//! PNG 解码 release 基准（cargo bench 手动运行）：自研 pngsimd 引擎（走 kimg 公开 API）
//! vs png crate 参照 vs 真机 libpng 1.6（可选 feature 门控）。
//!
//! 运行（release 默认，harness=false 直接执行）：
//! ```text
//! # 仅 pngsimd vs png crate（不去构建 libpng，libpng 列输出 N/A）
//! cargo bench -p imgbench --bench png_release_bench
//!
//! # 含 libpng 1.6 真机对照通道（需 cmake 在 PATH：VS 自带 CMake 已实测可用；
//! # 首次会以 CMake+MSVC 源码构建静态 libpng，可能耗时几分钟）
//! cargo bench -p imgbench --bench png_release_bench --features libpng-ref
//! ```
//!
//! 输出 `PNG_RELEASE_BENCH <scenario> pngsimd=Xms pngcrate=Yms libpng=Zms
//! ratio_simd_vs_pngcrate=.. ratio_simd_vs_libpng=..`（各场景 N≥9 轮取中位，release）。
//! note workspace `[profile.release]` 为 `opt-level="z"`（体积向）；如需速度向（`opt-level="3"`）可比口径，
//! 加 `--config 'profile.release.opt-level="3"'` 重新运行（会重建依赖）。
//!
//! 证据口径：pngsimd / png crate 均为同一 Rust release profile，逐字节一致（两者互相断言）；
//! libpng 走简化 API 一律解到 RGBA8（PNG_FORMAT_RGBA），与 SimdCodecs 官方基准「decode到RGBA」同口径。

use std::io::Cursor;
use std::time::Instant;

use flate2::{write::ZlibEncoder, Compression};
use kimg::codecs::png::PngDecoder;
use kimg::{ColorType, ImageDecoder as _};

const ROUNDS: usize = 9;

// ---------------------------------------------------------------------------
// 三条解码通道
// ---------------------------------------------------------------------------

/// png crate 参照（EXPAND 语义，与 png_port_bench 同路径）。
fn old_decode(png_bytes: &[u8]) -> (ColorType, Vec<u8>) {
    let mut dec = png::Decoder::new(Cursor::new(png_bytes));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info().unwrap();
    let mut out = vec![0u8; reader.output_buffer_size()];
    reader.next_frame(&mut out).unwrap();
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
        _ => unreachable!(),
    };
    if bits == png::BitDepth::Sixteen {
        for c in out.as_chunks_mut::<2>().0.iter_mut() {
            *c = u16::from_be_bytes(*c).to_ne_bytes();
        }
    }
    (color, out)
}

/// 自研 pngsimd 引擎（走 kimg 公开 API：kimg::codecs::png::PngDecoder + ImageDecoder trait，
/// 与生产路径同一姿态）。
fn new_decode(png_bytes: &[u8]) -> (ColorType, Vec<u8>) {
    let dec = PngDecoder::new(Cursor::new(png_bytes)).unwrap();
    let ct = dec.color_type();
    let mut out = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut out).unwrap();
    (ct, out)
}

/// 真机 libpng 1.6（libpng-vendored-sys 静态链，简化 API 解到 RGBA8）。
#[cfg(feature = "libpng-ref")]
mod libpng_ref {
    use libpng_vendored_sys::{
        png_image, png_image_begin_read_from_memory, png_image_finish_read, png_image_free,
        PNG_FORMAT_RGBA, PNG_IMAGE_VERSION,
    };
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    const MESSAGE_LEN: usize = 64;

    fn empty_image() -> Box<png_image> {
        Box::new(png_image {
            opaque: null_mut(),
            version: PNG_IMAGE_VERSION,
            width: 0,
            height: 0,
            format: 0,
            flags: 0,
            colormap_entries: 0,
            warning_or_error: 0,
            message: [0; MESSAGE_LEN],
        })
    }

    /// 解到 RGBA8，返回 (宽, 高, 像素)。失败返回 None。
    pub fn decode_rgba(png_bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut image = empty_image();
        let status = unsafe {
            png_image_begin_read_from_memory(
                &mut *image,
                png_bytes.as_ptr() as *const c_void,
                png_bytes.len(),
            )
        };
        if status != 1 {
            unsafe { png_image_free(&mut *image) };
            return None;
        }
        image.format = PNG_FORMAT_RGBA;
        let (w, h) = (image.width, image.height);
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let status = unsafe {
            png_image_finish_read(
                &mut *image,
                null(),
                buf.as_mut_ptr() as *mut c_void,
                0,
                null_mut(),
            )
        };
        if status != 1 {
            unsafe { png_image_free(&mut *image) };
            return None;
        }
        unsafe { png_image_free(&mut *image) };
        Some((w, h, buf))
    }
}

// ---------------------------------------------------------------------------
// 计时与夹具
// ---------------------------------------------------------------------------

/// 逐轮计时并取中位（先暖机一轮）。
fn median_ms<F: Fn()>(f: F, rounds: usize) -> f64 {
    f(); // 暖机
    let mut acc: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        acc.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = acc.len() / 2;
    if acc.len() % 2 == 0 {
        (acc[mid - 1] + acc[mid]) / 2.0
    } else {
        acc[mid]
    }
}

/// 轮数：默认 ROUNDS；可用 `PNG_RELEASE_BENCH_ROUNDS` 覆盖（调试/快速校验时收紧）。
fn rounds() -> usize {
    std::env::var("PNG_RELEASE_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS)
}

/// 平滑确定性内容（近照片压缩率）：基波 8px + 两路正弦调制 + 细节。
fn photo_bytes(width: u32, height: u32, spp: usize, bytes_per_sample: usize) -> Vec<u8> {
    let row = (width as usize) * spp * bytes_per_sample;
    let mut img = Vec::with_capacity(row * height as usize);
    let (w, h) = (width as usize, height as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..spp {
                let base = ((x as f64 * 1.3 + y as f64 * 0.7) as usize & 0x3F) as u8;
                let s = ((x as f64 / 37.0 + c as f64) * std::f64::consts::PI * 2.0).sin();
                let s2 = ((y as f64 / 41.0 + x as f64 / 97.0) * std::f64::consts::PI * 2.0).sin();
                let v = base as f64 + 96.0 * s + 64.0 * s2 + 48.0;
                let v = v.clamp(0.0, 255.0) as u8;
                let val: u16 = if bytes_per_sample == 2 { u16::from(v) * 257 } else { u16::from(v) };
                if bytes_per_sample == 2 {
                    img.push((val >> 8) as u8);
                    img.push((val & 0xFF) as u8);
                } else {
                    img.push(val as u8);
                }
            }
        }
    }
    img
}

fn make_png(width: u32, height: u32, ct: png::ColorType, bits: png::BitDepth) -> Vec<u8> {
    let spp = match ct {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => unreachable!(),
    };
    let bytes_per_sample = if bits == png::BitDepth::Sixteen { 2 } else { 1 };
    let img = photo_bytes(width, height, spp, bytes_per_sample);
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(ct);
        enc.set_depth(bits);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&img).unwrap();
    }
    out
}

fn png_chunk(buf: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(data);
    let mut h = crc32fast::Hasher::new();
    h.update(name);
    h.update(data);
    buf.extend_from_slice(&h.finalize().to_be_bytes());
}

/// 手工构造 Adam7 交错 RGBA8 PNG（png crate 不支持写交错，这里按 7 pass 割 + raw deflate）。
fn make_adam7_png(width: u32, height: u32) -> Vec<u8> {
    const PASSES: [(usize, usize, usize, usize); 7] = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];
    let (w, h) = (width as usize, height as usize);
    let rgba = photo_bytes(width, height, 4, 1);
    let mut rows = Vec::new();
    for (x0, y0, dx, dy) in PASSES {
        let mut y = y0;
        while y < h {
            let mut row = vec![0u8]; // filter None
            let mut x = x0;
            while x < w {
                let idx = (y * w + x) * 4;
                row.extend_from_slice(&rgba[idx..idx + 4]);
                x += dx;
            }
            rows.extend_from_slice(&row);
            y += dy;
        }
    }
    let mut compressed = Vec::new();
    {
        use std::io::Write;
        // PNG IDAT 是 zlib 流（RFC1950），须用 ZlibEncoder（同 png_port_parity 的手工 PNG 构造姿势）。
        let mut enc = ZlibEncoder::new(&mut compressed, Compression::fast());
        enc.write_all(&rows).unwrap();
        enc.finish().unwrap();
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 1]); // bit8, RGBA, deflate, none, Adam7
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &compressed);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

/// 8bit 调色板 PNG（带 tRNS → EXPAND 后为 RGBA8）。
fn make_palette_png(width: u32, height: u32) -> Vec<u8> {
    let mut palette = Vec::new();
    let mut trns = Vec::new();
    for i in 0..256u16 {
        palette.extend_from_slice(&[((i * 7) % 256) as u8, ((i * 13) % 256) as u8, ((i * 29) % 256) as u8]);
        trns.push(((i * 5) % 256) as u8);
    }
    let mut idx = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        for x in 0..width as usize {
            idx.push(((x * 3 + y * 5) % 256) as u8);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(palette);
        enc.set_trns(trns);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&idx).unwrap();
    }
    out
}

fn collect_corpus_pngs() -> Vec<Vec<u8>> {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus");
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().map(|x| x == "png").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(&corpus, &mut files);
    files.sort();
    if files.is_empty() {
        return Vec::new();
    }
    files.iter().map(|p| std::fs::read(p).unwrap()).collect()
}

// ---------------------------------------------------------------------------
// 场景
// ---------------------------------------------------------------------------

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let rounds = rounds();
    println!("PNG_RELEASE_BENCH profile={profile} rounds={rounds} simd={} pngcrate=0.17.16 libpng={}",
        if cfg!(debug_assertions) { "OFF(debug gate)" } else { "ON" },
        min_libpng_version());

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("rgba8_1920x1080", make_png(1920, 1080, png::ColorType::Rgba, png::BitDepth::Eight)),
        ("rgb8_1920x1080", make_png(1920, 1080, png::ColorType::Rgb, png::BitDepth::Eight)),
        ("rgba16_1280x720", make_png(1280, 720, png::ColorType::Rgba, png::BitDepth::Sixteen)),
        ("rgb16_1280x720", make_png(1280, 720, png::ColorType::Rgb, png::BitDepth::Sixteen)),
        ("gray8_1920x1080", make_png(1920, 1080, png::ColorType::Grayscale, png::BitDepth::Eight)),
        ("graya8_1280x720", make_png(1280, 720, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight)),
        ("adam7_rgba8_960x540", make_adam7_png(960, 540)),
        ("palette_rgba8_640x480", make_palette_png(640, 480)),
    ];

    // 语料（真实照片，若存在）。
    let datas = collect_corpus_pngs();
    let corpus_name = if datas.is_empty() {
        None
    } else {
        Some(format!("corpus_{}files", datas.len()))
    };

    for (name, bytes) in &cases {
        bench_one(name, bytes, rounds);
    }
    if let Some(name) = &corpus_name {
        bench_corpus(name, &datas, rounds);
    } else {
        println!("PNG_RELEASE_BENCH corpus SKIP (target/imgbench-corpus 缺失)");
    }
    eprintln!("[png_release_bench] done");
}

fn min_libpng_version() -> &'static str {
    #[cfg(feature = "libpng-ref")]
    { "1.6.43 (vendored)" }
    #[cfg(not(feature = "libpng-ref"))]
    { "n/a" }
}

/// 单图场景：先互验 pngsimd == png crate 逐字节，再各计时；libpng 通道存在时对照并互验（RGBA8 时逐字节）。
fn bench_one(name: &str, bytes: &[u8], rounds: usize) {
    let (ref_ct, ref_out) = old_decode(bytes);
    let (new_ct, new_out) = new_decode(bytes);
    assert_eq!(new_ct, ref_ct, "{name} 色彩类型不符");
    assert_eq!(new_out, ref_out, "{name} pngsimd 与 png crate 输出不一致——基准前提不成立");

    let new_ms = median_ms(|| { let _ = new_decode(bytes); }, rounds);
    let old_ms = median_ms(|| { let _ = old_decode(bytes); }, rounds);

    #[cfg(feature = "libpng-ref")]
    {
        let (w, h, buf) = libpng_ref::decode_rgba(bytes)
            .unwrap_or_else(|| panic!("{name} libpng 解码失败"));
        if ref_ct == ColorType::Rgba8 {
            assert_eq!(w * h as u32 * 4, ref_out.len() as u32, "{name} 尺寸不符");
            assert_eq!(buf, ref_out, "{name} libpng RGBA 与 png crate 输出不一致");
        } else {
            // 非 RGBA8 原生通道：libpng 一律展开到 RGBA8（口径与 SimdCodecs 一致），只比尺寸不解码校验。
            assert_eq!((w, h), (bytes_image_dims(bytes)));
        }
        let lp_ms = median_ms(|| { let _ = libpng_ref::decode_rgba(bytes); }, rounds);
        print_line(name, new_ms, old_ms, Some(lp_ms));
        return;
    }
    #[cfg(not(feature = "libpng-ref"))]
    print_line(name, new_ms, old_ms, None);
}

/// corpus 场景：整批累计计时。
fn bench_corpus(name: &str, datas: &[Vec<u8>], rounds: usize) {
    let new_ms = median_ms(|| { for d in datas { let _ = new_decode(d); } }, rounds);
    let old_ms = median_ms(|| { for d in datas { let _ = old_decode(d); } }, rounds);
    #[cfg(feature = "libpng-ref")]
    {
        let lp_ms = median_ms(|| { for d in datas { let _ = libpng_ref::decode_rgba(d); } }, rounds);
        print_line(name, new_ms, old_ms, Some(lp_ms));
        return;
    }
    #[cfg(not(feature = "libpng-ref"))]
    print_line(name, new_ms, old_ms, None);
}

fn print_line(name: &str, new_ms: f64, old_ms: f64, libpng_ms: Option<f64>) {
    match libpng_ms {
        Some(lp) => {
            println!(
                "PNG_RELEASE_BENCH {name} pngsimd={new_ms:.3}ms pngcrate={old_ms:.3}ms libpng={lp:.3}ms \
                 ratio_simd_vs_pngcrate={:.3} ratio_simd_vs_libpng={:.3}",
                new_ms / old_ms,
                new_ms / lp
            );
        }
        None => {
            println!(
                "PNG_RELEASE_BENCH {name} pngsimd={new_ms:.3}ms pngcrate={old_ms:.3}ms libpng=N/A \
                 ratio_simd_vs_pngcrate={:.3}  (libpng 通道未启用：cargo bench -p imgbench --features libpng-ref 可启用 libpng 真机对照)",
                new_ms / old_ms
            );
        }
    }
}

#[cfg(feature = "libpng-ref")]
fn bytes_image_dims(bytes: &[u8]) -> (u32, u32) {
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w, h)
}
