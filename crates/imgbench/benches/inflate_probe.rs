//! IDAT zlib 解压后端对比探针（cargo bench 手动运行）：fdeflate（纯 Rust，kimg 现状）
//! vs flate2 `zlib-ng` backend（C zlib-ng）——量化「纯 Rust 解压天花板」与 C 的差距。
//!
//! 背景：SimdCodecs 博客宣称「PNG 解码 0.61× of libpng 1.6」，主要来源就是 inflate 用 C
//! 库（zlib/zlib-ng）而非纯 Rust。本探针在 **imgbench（bench-only 非 default-member）** 里
//! 直接对比同一 IDAT zlib 流：fdeflate → raw 缓冲 vs flate2(zlib-ng) → 等长缓冲。
//! 结论是量化证据，**绝不**通过 kimg 主线换后端（保持纯 Rust 依赖面）。
//!
//! 运行（release 中位）：
//! ```text
//! # feature 未开：打印 N/A（默认不构建 C zlib-ng）
//! cargo bench -p imgbench --bench inflate_probe
//! # 开探针（首次构建 flate2 的 zlib-ng C 源码，cmake+MSVC，可能几分钟）
//! cargo bench -p imgbench --bench inflate_probe --features inflate-probe
//! ```
//!
//! 口径：
//! - 语料 = `target/imgbench-corpus` 下全部 PNG（34 张真图，递归）+ 4 条合成 photo 流
//!   （photo_bytes 平滑内容经 png crate 编码出真实 IDAT deflate 流）。
//! - 每个样本：fdeflate 用 kimg 生产同款姿势（32KB 分块喂入 + ignore_adler32 + 末尾 flush，
//!   输出预分配定长缓冲）；flate2 走 `ZlibDecoder::read_to_end` 到预分配 expected 容量
//!   Vec（C zlib-ng 走完整 zlib 流含 adler 校验）。两者输出逐字节对拍后各自逐样本中位。
//! - 聚合 = 各样本中位求和 + 整批一次跑完的中位；另附同批 kimg 全解码耗时，用于按实测
//!   inflate 占比折算「kimg 若换 zlib-ng 级解压的理论总耗时」。
//!
//! 注意：本探针不参与常规 build（`--features inflate-probe` 才有 C 构建）。
//! feature 未开时 helpers/主逻辑整体不编（#![cfg]），仅 N/A main 生效；开 feature 后全量编。

#![allow(dead_code)]

use std::io::Read;
use std::time::Instant;

use kimg::codecs::png::PngDecoder;
use kimg::ImageDecoder as _;

const ROUNDS: usize = 15;

// ---------------------------------------------------------------------------
// IDAT 提取与解压
// ---------------------------------------------------------------------------

/// PNG 签名。
const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Adam7 七遍（仅算 expected 尺寸用）。
const ADAM7_PASSES: [(u64, u64, u64, u64); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// 从整份 PNG 抽出拼接的 IDAT zlib 流 + 期望解压字节数（= 反滤波后线性尺寸）。
fn extract_idat(png: &[u8]) -> Option<(Vec<u8>, usize)> {
    if png.len() < 33 || png[..8] != SIGNATURE {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]) as u64;
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]) as u64;
    let depth = png[24] as u64;
    let ct = png[25] as u64;
    let interlace = png[28];

    let samples = match ct {
        0 | 3 => 1,
        4 => 2,
        2 => 3,
        6 => 4,
        _ => 0,
    };
    if samples == 0 {
        return None;
    }
    let bits_per_pixel = samples * depth;
    let row_for = |ww: u64| (ww * bits_per_pixel + 7) / 8;
    let expected: u64 = if interlace == 0 {
        (row_for(w) + 1) * h
    } else {
        let ext = |size: u64, start: u64, step: u64| {
            if size > start {
                (size - start + step - 1) / step
            } else {
                0
            }
        };
        let mut total = 0u64;
        for (x0, y0, dx, dy) in ADAM7_PASSES {
            let pw = ext(w, x0, dx);
            let ph = ext(h, y0, dy);
            if pw > 0 && ph > 0 {
                total += (row_for(pw) + 1) * ph;
            }
        }
        total
    };

    // 收集 IDAT 载荷。
    let mut idat = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= png.len() {
        let len =
            u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            idat.extend_from_slice(&png[pos + 8..pos + 8 + len]);
        } else if &png[pos + 4..pos + 8] == b"IEND" {
            break;
        }
        pos += 12 + len;
    }
    if idat.is_empty() {
        return None;
    }
    Some((idat, expected as usize))
}

/// fdeflate：kimg 现状的快速姿势（32KB 分块喂 + ignore_adler32 + 末尾 flush），解到预分配定长缓冲。
fn inflate_fdeflate(idat: &[u8], expected: usize) -> Vec<u8> {
    const CHUNK: usize = 32 * 1024;
    let mut dc = fdeflate::Decompressor::new();
    dc.ignore_adler32();
    let mut inflated = vec![0u8; expected];
    let mut out_pos = 0usize;
    let mut in_off = 0usize;
    'main: loop {
        if dc.is_done() {
            break;
        }
        let end = (in_off + CHUNK).min(idat.len());
        if end == in_off {
            break;
        }
        let mut input = &idat[in_off..end];
        in_off = end;
        loop {
            if dc.is_done() {
                break 'main;
            }
            let (consumed, produced) = dc.read(input, &mut inflated, out_pos, false).unwrap();
            input = &input[consumed..];
            out_pos += produced;
            if input.is_empty() {
                break;
            }
            if produced == 0 {
                break 'main;
            }
        }
    }
    if !dc.is_done() {
        let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
        out_pos += produced;
    }
    assert_eq!(out_pos, expected, "fdeflate 输出长度不符");
    inflated
}

/// flate2 zlib-ng：C 解码器 `ZlibDecoder::read_to_end` 到预分配 expected 容量 Vec。
#[cfg(feature = "inflate-probe")]
fn inflate_zlib_ng(idat: &[u8], expected: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(expected);
    let mut dec = flate2::read::ZlibDecoder::new(idat);
    dec.read_to_end(&mut out).unwrap();
    assert_eq!(out.len(), expected, "flate2(zlib-ng) 输出长度不符");
    out
}

/// kimg 全解码（生产路径，同 PNG_RELEASE_BENCH 姿态）。
fn full_decode_kimg(png: &[u8]) -> Option<Vec<u8>> {
    let dec = PngDecoder::new(std::io::Cursor::new(png)).ok()?;
    let mut out = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut out).ok()?;
    Some(out)
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

struct Sample {
    name: String,
    idat: Vec<u8>,
    expected: usize,
    png: Vec<u8>,
    /// 该样本是否纳入 corpus 聚合（真图 vs 合成）。
    corpus: bool,
}

fn collect_corpus() -> Vec<Sample> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus");
    let mut files: Vec<Sample> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<Sample>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map(|x| x == "png").unwrap_or(false) {
                let bytes = std::fs::read(&p).unwrap();
                if let Some((idat, expected)) = extract_idat(&bytes) {
                    out.push(Sample {
                        name: p
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        idat,
                        expected,
                        png: bytes,
                        corpus: true,
                    });
                }
            }
        }
    }
    walk(&root, &mut files);
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

/// 合成 photo 流（同 png_release_bench 的 photo_bytes 内容，png crate 编码出真实 IDAT）。
fn synthetic<'a>(
    name: &'a str,
    w: u32,
    h: u32,
    ct: png::ColorType,
    bits: png::BitDepth,
) -> Sample {
    let spp = match ct {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => unreachable!(),
    };
    let bps = if bits == png::BitDepth::Sixteen { 2 } else { 1 };
    let row = w as usize * spp * bps;
    let mut img = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            for c in 0..spp {
                let base = ((x as f64 * 1.3 + y as f64 * 0.7) as usize & 0x3F) as u8;
                let s = ((x as f64 / 37.0 + c as f64) * std::f64::consts::PI * 2.0).sin();
                let s2 = ((y as f64 / 41.0 + x as f64 / 97.0) * std::f64::consts::PI * 2.0).sin();
                let v = (base as f64 + 96.0 * s + 64.0 * s2 + 48.0).clamp(0.0, 255.0) as u8;
                let val: u16 = if bps == 2 { u16::from(v) * 257 } else { u16::from(v) };
                if bps == 2 {
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
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(ct);
        enc.set_depth(bits);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&img).unwrap();
    }
    let (idat, expected) = extract_idat(&out).unwrap();
    let png = out;
    Sample {
        name: name.to_string(),
        idat,
        expected,
        png,
        corpus: false,
    }
}

// ---------------------------------------------------------------------------
// 计时
// ---------------------------------------------------------------------------

fn rounds() -> usize {
    std::env::var("INFLATE_PROBE_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS)
}

fn median_ms<F: FnMut()>(f: &mut F, rounds: usize) -> f64 {
    f(); // 暖机
    let mut acc: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        acc.push(t.elapsed().as_secs_f64() * 1e3);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    acc[acc.len() / 2]
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

#[cfg(feature = "inflate-probe")]
fn main() {
    let rounds = rounds();
    let mut set = collect_corpus();
    let corpus_count = set.len();
    set.push(synthetic("photo_rgba8_1920x1080", 1920, 1080, png::ColorType::Rgba, png::BitDepth::Eight));
    set.push(synthetic("photo_rgb8_1920x1080", 1920, 1080, png::ColorType::Rgb, png::BitDepth::Eight));
    set.push(synthetic("photo_rgba16_1280x720", 1280, 720, png::ColorType::Rgba, png::BitDepth::Sixteen));
    set.push(synthetic("photo_gray8_1920x1080", 1920, 1080, png::ColorType::Grayscale, png::BitDepth::Eight));

    if corpus_count == 0 {
        println!("INFLATE_PROBE SKIP (target/imgbench-corpus 缺失)");
        return;
    }

    // 正确性：全部样本 fdeflate == flate2(zlib-ng) == kimg 全解码渲染层面先行对拍
    // （此处只对拍两个 inflate 后端的输出；kimg 全解码另有 png_port_parity 保证）。
    for s in &set {
        let a = inflate_fdeflate(&s.idat, s.expected);
        let b = inflate_zlib_ng(&s.idat, s.expected);
        assert_eq!(a, b, "{}: fdeflate != flate2(zlib-ng)", s.name);
        assert_eq!(a.len(), s.expected);
    }

    let total_inflated: usize = set.iter().map(|s| s.expected).sum();
    println!(
        "INFLATE_PROBE profile=release rounds={rounds} files={} corpus={corpus_count} inflated_total={total_inflated}",
        set.len()
    );

    // 每个样本：fdeflate / flate2(zlib-ng) 中位 + kimg 全解码中位。
    let mut per: Vec<(String, usize, f64, f64, f64)> = Vec::with_capacity(set.len());
    for s in set.iter() {
        let fd = median_ms(&mut || { let _ = inflate_fdeflate(&s.idat, s.expected); }, rounds);
        let zng = median_ms(&mut || { let _ = inflate_zlib_ng(&s.idat, s.expected); }, rounds);
        let full = match full_decode_kimg(&s.png) {
            Some(_) => median_ms(&mut || { let _ = full_decode_kimg(&s.png); }, rounds),
            None => f64::NAN,
        };
        per.push((s.name.clone(), s.expected, fd, zng, full));
    }

    // 聚合（各样本中位求和）：corpus 34（真图）与 corpus+synth 38 各一份。
    let (corpus_fd, corpus_zng, corpus_full): (f64, f64, f64) = set
        .iter()
        .zip(per.iter())
        .filter(|(s, _)| s.corpus)
        .fold((0.0, 0.0, 0.0), |(a, b, c), (_, p)| (a + p.2, b + p.3, c + p.4));
    let all_fd: f64 = per.iter().map(|p| p.2).sum();
    let all_zng: f64 = per.iter().map(|p| p.3).sum();

    // 整批一次跑完的中位（corpus+synth）。
    let batch_fd = median_ms(
        &mut || {
            for s in &set {
                let _ = inflate_fdeflate(&s.idat, s.expected);
            }
        },
        rounds,
    );
    let batch_zng = median_ms(
        &mut || {
            for s in &set {
                let _ = inflate_zlib_ng(&s.idat, s.expected);
            }
        },
        rounds,
    );

    // 理论：kimg 若把 inflate 换成 zlib-ng 级（同批 corpus 全解码 - fdeflate inflate + zng inflate）。
    let lead_ratio = corpus_zng / corpus_fd;
    let inflate_share = corpus_fd / corpus_full;
    let theoretical_total = corpus_full - corpus_fd + corpus_zng;

    println!(
        "INFLATE_PROBE corpus_agg_files_{corpus_count} fdeflate={corpus_fd:.3}ms zlibng={corpus_zng:.3}ms ratio={lead_ratio:.3} \
         kimg_full={corpus_full:.3}ms inflate_share={inflate_share:.3} theoretical_kimg_total={theoretical_total:.3}ms \
         implied_speedup={:.3}x",
        corpus_full / theoretical_total
    );
    println!(
        "INFLATE_PROBE all_agg_files_{} fdeflate={all_fd:.3}ms zlibng={all_zng:.3}ms ratio={:.3}",
        set.len(),
        all_zng / all_fd
    );
    println!(
        "INFLATE_PROBE all_batch_median_files_{} fdeflate={batch_fd:.3}ms zlibng={batch_zng:.3}ms ratio={:.3}",
        set.len(),
        batch_zng / batch_fd
    );

    // 逐样本中位（按解压体积降序，便于读大图样例）。
    let mut by_size: Vec<&(String, usize, f64, f64, f64)> = per.iter().collect();
    by_size.sort_by_key(|(_, e, _, _, _)| std::cmp::Reverse(*e));
    for (name, expected, fd, zng, full) in by_size {
        let full_s = if full.is_finite() {
            format!("{full:.3}")
        } else {
            "n/a".to_string()
        };
        println!(
            "INFLATE_PROBE {name} inflated={expected} fdeflate={fd:.3}ms zlibng={zng:.3}ms ratio={:.3} kimg_full={full_s}ms",
            zng / fd
        );
    }
    eprintln!("[inflate_probe] done");
}

#[cfg(not(feature = "inflate-probe"))]
fn main() {
    println!("INFLATE_PROBE N/A (feature inflate-probe 未开；cargo bench -p imgbench --bench inflate_probe --features inflate-probe)");
}
