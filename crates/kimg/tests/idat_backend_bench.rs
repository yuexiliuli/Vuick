//! 一次性微基准：同批真图 IDAT，比 fdeflate（现状）vs zlib-rs vs zune-inflate 的纯解压耗时。
//!
//! 运行（release 中位）：
//! ```text
//! cargo test -p kimg --release --test idat_backend_bench -- --ignored --nocapture
//! ```
//! 语料用 `target/imgbench-corpus/png`（34 张真图，gitignore），缺失则打印 SKIP。
//! 只测 inflate 后端本身（解压到反滤波层输入缓冲），与解码结果无关，故 parity 天然保持。
//! 一次性诊断，不参与常规测试管线（`#[ignore]`）。

use std::time::Instant;

/// PNG 签名。
const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Adam7 七遍（与 chunk.rs 一致，仅本测试自算 expected 尺寸用）。
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
        let len = u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
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
        let chunk = &idat[in_off..(in_off + CHUNK).min(idat.len())];
        if chunk.is_empty() {
            break;
        }
        in_off += chunk.len();
        let mut input = chunk;
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

/// zlib-rs：`decompress_slice` 一次性解到预分配定长缓冲（zlib 头，正向 wbits）。
fn inflate_zlib_rs(idat: &[u8], expected: usize) -> Vec<u8> {
    let mut out = vec![0u8; expected];
    let (written, code) =
        zlib_rs::decompress_slice(&mut out, idat, zlib_rs::InflateConfig::default());
    assert_eq!(code, zlib_rs::ReturnCode::Ok, "zlib-rs 解压失败 {code:?}");
    assert_eq!(written.len(), expected, "zlib-rs 输出长度不符");
    written.to_vec()
}

/// zune-inflate：`DeflateDecoder` + size hint + 跳过 checksum，自管输出 Vec。
fn inflate_zune(idat: &[u8], expected: usize) -> Vec<u8> {
    let options = zune_inflate::DeflateOptions::default()
        .set_confirm_checksum(false)
        .set_size_hint(expected)
        .set_limit(expected.saturating_add(1024)); // 输出上限：期望尺寸 + 松弛
    let mut decoder = zune_inflate::DeflateDecoder::new_with_options(idat, options);
    let out = decoder.decode_zlib().expect("zune-inflate 解压失败");
    assert_eq!(out.len(), expected, "zune-inflate 输出长度不符");
    out
}

fn collect_corpus() -> Vec<(String, Vec<u8>, usize)> {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus")
        .join("png");
    let mut files: Vec<(String, Vec<u8>, usize)> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>, usize)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map(|x| x == "png").unwrap_or(false) {
                let bytes = std::fs::read(&p).unwrap();
                if let Some((idat, expected)) = extract_idat(&bytes) {
                    out.push((
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                        idat,
                        expected,
                    ));
                }
            }
        }
    }
    walk(&corpus, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// 生成合成基准图（同 png_release_bench 的 photo_bytes）：返回 (name, idat, expected)。
fn synthetic(
    name: &str,
    w: u32,
    h: u32,
    ct: png::ColorType,
    bits: png::BitDepth,
) -> (String, Vec<u8>, usize) {
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
                let val: u16 = if bps == 2 {
                    u16::from(v) * 257
                } else {
                    u16::from(v)
                };
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
    (name.to_string(), idat, expected)
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

#[test]
#[ignore = "一次性 inflate 后端微基准：cargo test -p kimg --release --test idat_backend_bench -- --ignored --nocapture"]
fn inflate_backend_compare() {
    let mut set: Vec<(String, Vec<u8>, usize)> = collect_corpus();
    let corpus_count = set.len();
    if corpus_count == 0 {
        eprintln!("SKIP: target/imgbench-corpus/png 缺失");
        return;
    }
    set.push(synthetic(
        "gray8_1920x1080",
        1920,
        1080,
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
    ));
    set.push(synthetic(
        "rgba8_1920x1080",
        1920,
        1080,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
    ));
    set.push(synthetic(
        "rgb8_1920x1080",
        1920,
        1080,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
    ));
    set.push(synthetic(
        "graya8_1280x720",
        1280,
        720,
        png::ColorType::GrayscaleAlpha,
        png::BitDepth::Eight,
    ));

    // 正确性：三个后端对首项输出逐字节一致。
    {
        let (_, idat, expected) = &set[0];
        let a = inflate_fdeflate(idat, *expected);
        let b = inflate_zlib_rs(idat, *expected);
        let c = inflate_zune(idat, *expected);
        assert_eq!(a, b, "fdeflate != zlib-rs");
        assert_eq!(a, c, "fdeflate != zune");
    }

    let rounds = std::env::var("IDAT_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9usize);

    let total_pixels: usize = set.iter().map(|(_, _, e)| *e).sum();
    eprintln!(
        "[inflate-bench] files={} (corpus={corpus_count}) inflated_total={total_pixels} rounds={rounds}",
        set.len()
    );

    // 全批耗时（每次整体跑一遍）。
    let fd = median_ms(
        &mut || {
            for (_, idat, expected) in &set {
                let _ = inflate_fdeflate(idat, *expected);
            }
        },
        rounds,
    );
    let zr = median_ms(
        &mut || {
            for (_, idat, expected) in &set {
                let _ = inflate_zlib_rs(idat, *expected);
            }
        },
        rounds,
    );
    let zn = median_ms(
        &mut || {
            for (_, idat, expected) in &set {
                let _ = inflate_zune(idat, *expected);
            }
        },
        rounds,
    );

    eprintln!(
        "[inflate-bench] ALL fdeflate={fd:.3}ms zlib-rs={zr:.3}ms zune={zn:.3}ms  (zr/fd={:.3} zn/fd={:.3})",
        zr / fd,
        zn / fd
    );

    // 全量逐项中位（真图语料 + 合成），按解压体积降序。
    let mut by_size: Vec<&(String, Vec<u8>, usize)> = set.iter().collect();
    by_size.sort_by_key(|(_, _, e)| std::cmp::Reverse(*e));
    for (name, idat, expected) in by_size.into_iter() {
        let fd = median_ms(
            &mut || {
                let _ = inflate_fdeflate(idat, *expected);
            },
            rounds,
        );
        let zr = median_ms(
            &mut || {
                let _ = inflate_zlib_rs(idat, *expected);
            },
            rounds,
        );
        let zn = median_ms(
            &mut || {
                let _ = inflate_zune(idat, *expected);
            },
            rounds,
        );
        eprintln!(
            "[inflate-bench] {name} inflated={expected} fdeflate={fd:.3}ms zlib-rs={zr:.3}ms zune={zn:.3}ms (zr/fd={:.3} zn/fd={:.3})",
            zr / fd,
            zn / fd
        );
    }
    eprintln!("[inflate-bench] done");
}
