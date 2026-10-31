//! PNG 移植基准（debug、可复跑）：新自研引擎 vs png crate EXPAND 参照，各解码 N 轮取中位。
//!
//! 运行（`#[ignore]`，不影响常规套件）：
//! ```text
//! cargo test -p kimg --test png_port_bench -- --ignored --nocapture
//! ```
//! 输出 `PNG_PORT_BENCH <case> new=..ms old=..ms ratio=..`（debug 相对法，规避 debug≈release 10× 之扰）。
//! ratio < 1 即新引擎更快；brief 目标：RGBA8/RGB8 8bit 常见路径 ≥10% 更快，全组合不慢于 5%。

use std::io::Cursor;
use std::time::Instant;

use flate2::Compression;
use kimg::codecs::png::PngDecoder;
use kimg::{ColorType, ImageDecoder as _};

// ---------------------------------------------------------------------------
// 参照实现：旧 kimg 路径（png crate + EXPAND + 16bit BE→LE）
// ---------------------------------------------------------------------------

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

fn new_decode(png_bytes: &[u8]) -> (ColorType, Vec<u8>) {
    let dec = PngDecoder::new(Cursor::new(png_bytes)).unwrap();
    let ct = dec.color_type();
    let mut out = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut out).unwrap();
    (ct, out)
}

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

/// 生成一张指定格式的确定性大图 PNG。
/// 数据用「照片式」内容：平滑渐变 + 正弦调制，压缩率接近真实照片（占位随机字节的近乎不可压
/// 数据会让整份文件读取/CRC 主导计时、偏离「常见 RGBA8/RGB8 路径」；真实内容见 corpus 行）。
fn make_png(width: u32, height: u32, ct: png::ColorType, bits: png::BitDepth) -> Vec<u8> {
    let spp = match ct {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => unreachable!(),
    };
    let bytes_per_sample = if bits == png::BitDepth::Sixteen { 2 } else { 1 };
    let row = (width as usize) * spp * bytes_per_sample;
    let mut img = Vec::with_capacity(row * height as usize);
    let (w, h) = (width as usize, height as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..spp {
                // 平滑确定性内容：基波 8 像素 + 两路正弦调制 + 细节。
                let base = ((x as f64 * 1.3 + y as f64 * 0.7) as usize & 0x3F) as u8;
                let s = ((x as f64 / 37.0 + c as f64) * std::f64::consts::PI * 2.0).sin();
                let s2 = ((y as f64 / 41.0 + x as f64 / 97.0) * std::f64::consts::PI * 2.0).sin();
                let v = base as f64 + 96.0 * s + 64.0 * s2 + 48.0;
                let v = v.clamp(0.0, 255.0) as u8;
                let val: u16 = if bytes_per_sample == 2 {
                    u16::from(v) * 257
                } else {
                    u16::from(v)
                };
                if bytes_per_sample == 2 {
                    img.push((val >> 8) as u8);
                    img.push((val & 0xFF) as u8);
                } else {
                    img.push(val as u8);
                }
            }
        }
    }
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

fn bench_case(name: &str, png_bytes: &[u8], rounds: usize, new_out: &mut Option<Vec<u8>>) {
    // 先各自解一次，防初始分配抖动进计时（并顺带校验两路径一致）。
    let (ref_ct, ref_out) = old_decode(png_bytes);
    let (new_ct, new_out_bytes) = new_decode(png_bytes);
    assert_eq!(new_ct, ref_ct, "{name} 色彩类型不符");
    assert_eq!(
        new_out_bytes, ref_out,
        "{name} 输出与 png crate 不一致——基准前提不成立"
    );
    *new_out = Some(new_out_bytes);

    let new_ms = median_ms(|| {
        let _ = new_decode(png_bytes);
    }, rounds);
    let old_ms = median_ms(|| {
        let _ = old_decode(png_bytes);
    }, rounds);
    let ratio = new_ms / old_ms;
    println!("PNG_PORT_BENCH {name} new={new_ms:.3}ms old={old_ms:.3}ms ratio={ratio:.3}");
}

#[test]
#[ignore = "手动运行：cargo test -p kimg --test png_port_bench -- --ignored --nocapture"]
fn png_port_bench() {
    let rounds = 9;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("rgba8_1920x1080", make_png(1920, 1080, png::ColorType::Rgba, png::BitDepth::Eight)),
        ("rgb8_1920x1080", make_png(1920, 1080, png::ColorType::Rgb, png::BitDepth::Eight)),
        ("rgba16_1280x720", make_png(1280, 720, png::ColorType::Rgba, png::BitDepth::Sixteen)),
        ("rgb16_1280x720", make_png(1280, 720, png::ColorType::Rgb, png::BitDepth::Sixteen)),
        ("gray8_1920x1080", make_png(1920, 1080, png::ColorType::Grayscale, png::BitDepth::Eight)),
        ("graya8_1280x720", make_png(1280, 720, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight)),
    ];
    let mut out_holder = None;
    for (name, bytes) in &cases {
        bench_case(name, bytes, rounds, &mut out_holder);
    }

    // 语料（若存在）：整批汇总计时。
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus");
    fn collect_pngs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    collect_pngs(&p, out);
                } else if p.extension().map(|x| x == "png").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    }
    let mut files = Vec::new();
    collect_pngs(&corpus, &mut files);
    files.sort();
    if files.is_empty() {
        println!("PNG_PORT_BENCH corpus SKIP (target/imgbench-corpus 缺失)");
        return;
    }
    let datas: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let new_ms = median_ms(|| {
        for d in &datas {
            let _ = new_decode(d);
        }
    }, 5);
    let old_ms = median_ms(|| {
        for d in &datas {
            let _ = old_decode(d);
        }
    }, 5);
    let ratio = new_ms / old_ms;
    println!(
        "PNG_PORT_BENCH corpus({} files) new={new_ms:.3}ms old={old_ms:.3}ms ratio={ratio:.3}",
        datas.len()
    );
    let _ = Compression::default();
    let _ = out_holder;
    eprintln!("[bench] rendering done");
}
