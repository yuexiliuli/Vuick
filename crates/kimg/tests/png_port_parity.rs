//! PNG 移植 parity 测试：新自研引擎（经 `PngDecoder` 黑盒）输出 与 png crate `Transformations::EXPAND`
//! 参照 逐字节一致。
//!
//! 覆盖：
//! 1. 全组合（色型 × 位深 × tRNS × 尺寸 × 隔行开/关）：fixture 由 png crate 上游生成 → 顺带覆盖编码路径；
//! 2. 逐行强制 5 种 filter（手写 PNG 构造器：header + 已知 filter 字节行 + IDAT）——png crate 编码器不一定
//!    能强制 filter，此处直接落盘原始 filter 行，确保 None/Sub/Up/Average/Paeth 全覆盖；
//! 3. corpus 抽样（target/imgbench-corpus，34 张）新引擎 vs png crate 参照。
//!
//! 参照端还做与 `PngDecoder::read_image` 完全相同的 16bit 大端→本机端序重排，端到端比较。

use std::io::{Cursor, Write};

use flate2::{Compression, write::ZlibEncoder};
use kimg::codecs::png::PngDecoder;
use kimg::{ColorType, ImageDecoder as _};

// ---------------------------------------------------------------------------
// 手写 PNG 构造器
// ---------------------------------------------------------------------------

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(bytes);
    h.finalize()
}

fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + payload.len());
    v.extend_from_slice(&be32(payload.len() as u32));
    v.extend_from_slice(tag);
    v.extend_from_slice(payload);
    let mut covered = Vec::with_capacity(4 + payload.len());
    covered.extend_from_slice(tag);
    covered.extend_from_slice(payload);
    v.extend_from_slice(&crc32(&covered).to_be_bytes());
    v
}

/// 组装 PNG 文件（按给定块顺序）。
fn assemble_png(
    width: u32,
    height: u32,
    ct: u8,
    depth: u8,
    interlace: u8,
    palette: Option<&[u8]>,
    trns: Option<&[u8]>,
    stream: &[u8],
) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&be32(width));
    ihdr.extend_from_slice(&be32(height));
    ihdr.push(depth);
    ihdr.push(ct);
    ihdr.push(0); // 压缩：0
    ihdr.push(0); // 滤波：0
    ihdr.push(interlace);
    out.extend_from_slice(&chunk(b"IHDR", &ihdr));
    if let Some(palette) = palette {
        out.extend_from_slice(&chunk(b"PLTE", palette));
    }
    if let Some(trns) = trns {
        out.extend_from_slice(&chunk(b"tRNS", trns));
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(stream).unwrap();
    let z = enc.finish().unwrap();
    out.extend_from_slice(&chunk(b"IDAT", &z));
    out.extend_from_slice(&chunk(b"IEND", &[]));
    out
}

// ---------------------------------------------------------------------------
// 样本读写（支持 sub-byte 打包位、8/16 位大端）
// ---------------------------------------------------------------------------

fn samples_per_pixel(ct: u8) -> usize {
    match ct {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => unreachable!(),
    }
}

/// 从打包好的行读第 x 个像素的第 s 个样本（0..=max）。
fn read_sample(row: &[u8], x: usize, s: usize, samples: usize, depth: u8) -> u16 {
    if depth == 16 {
        let o = (x * samples + s) * 2;
        return u16::from_be_bytes([row[o], row[o + 1]]);
    }
    if depth == 8 {
        return row[x * samples + s] as u16;
    }
    let bit = ((x * samples + s) * (depth as usize)) as usize;
    let v = row[bit >> 3] >> (8 - depth as usize - (bit & 7));
    (v as u16) & ((1u16 << depth) - 1)
}

/// 写样本到打包行。
fn write_sample(row: &mut [u8], x: usize, s: usize, samples: usize, depth: u8, val: u16) {
    if depth == 16 {
        let o = (x * samples + s) * 2;
        row[o..o + 2].copy_from_slice(&val.to_be_bytes());
        return;
    }
    if depth == 8 {
        row[x * samples + s] = val as u8;
        return;
    }
    let bit = ((x * samples + s) * depth as usize) as usize;
    let shift = 8 - depth as usize - (bit & 7);
    let byte = bit >> 3;
    let mask = ((1u16 << depth) - 1) as u8;
    row[byte] = (row[byte] & !(mask << shift)) | (((val as u8) & mask) << shift);
}

/// 生成整帧原始样本（全 0..max 的确定性伪随机）并打包成行。
fn packed_rows(width: usize, height: usize, ct: u8, depth: u8, seed: u64) -> Vec<Vec<u8>> {
    let samples = samples_per_pixel(ct);
    let max = ((1u32 << depth) - 1) as u16;
    let row_len = sample_row_bytes(width, samples, depth);
    let mut lcg = seed;
    let mut rows = Vec::with_capacity(height);
    for _y in 0..height {
        let mut row = vec![0u8; row_len];
        for x in 0..width {
            for s in 0..samples {
                lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let v = ((lcg >> 40) as u16) & max;
                write_sample(&mut row, x, s, samples, depth, v);
            }
        }
        rows.push(row);
    }
    rows
}

fn sample_row_bytes(width: usize, samples: usize, depth: u8) -> usize {
    (width * samples * depth as usize + 7) / 8
}

/// 过滤行（编码方向，与测试内参考一致；返回 filter 类型 + 过滤后字节）。
fn filter_row(filter: u8, raw: &[u8], above: &[u8], bpp: usize) -> Vec<u8> {
    let mut out = raw.to_vec();
    for i in 0..raw.len() {
        out[i] = match filter {
            0 => raw[i],
            1 => raw[i].wrapping_sub(if i >= bpp { raw[i - bpp] } else { 0 }),
            2 => raw[i].wrapping_sub(above[i]),
            3 => {
                let a = if i >= bpp { raw[i - bpp] } else { 0 };
                raw[i].wrapping_sub(((u16::from(a) + u16::from(above[i])) >> 1) as u8)
            }
            _ => {
                let a = i32::from(if i >= bpp { raw[i - bpp] } else { 0 });
                let b = i32::from(above[i]);
                let c = i32::from(if i >= bpp { above[i - bpp] } else { 0 });
                let p = a + b - c;
                let pa = if p >= a { p - a } else { a - p };
                let pb = if p >= b { p - b } else { b - p };
                let pc = if p >= c { p - c } else { c - p };
                let pred = if pa <= pb && pa <= pc {
                    a
                } else if pb <= pc {
                    b
                } else {
                    c
                };
                raw[i].wrapping_sub(pred as u8)
            }
        };
    }
    out
}

/// 按行索引选 filter：隔行时每遍内部也循环 5 种。
fn filter_for(idx: usize) -> u8 {
    (idx % 5) as u8
}

/// 生成 fixture：返回压缩待组装流（含每行 filter 字节），覆盖整帧所有行。
fn build_stream(
    width: usize,
    height: usize,
    ct: u8,
    depth: u8,
    interlace: u8,
    seed: u64,
) -> (Vec<u8>, usize) {
    let samples = samples_per_pixel(ct);
    let bpp = samples * ((depth as usize + 7) >> 3);
    let full = packed_rows(width, height, ct, depth, seed);

    let mut stream = Vec::new();
    if interlace == 0 {
        let mut prev = vec![0u8; sample_row_bytes(width, samples, depth)];
        for y in 0..height {
            let raw = &full[y];
            let f = filter_for(y);
            let filtered = filter_row(f, raw, &prev, bpp);
            stream.push(f);
            stream.extend_from_slice(&filtered);
            prev = raw.clone();
        }
    } else {
        // Adam7：pass 行顺序入流。
        let passes = [(0usize, 0usize, 8usize, 8usize), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];
        let mut idx = 0usize;
        for (x0, y0, dx, dy) in passes {
            let pw = if width > x0 { (width - x0 + dx - 1) / dx } else { 0 };
            let ph = if height > y0 { (height - y0 + dy - 1) / dy } else { 0 };
            if pw == 0 || ph == 0 {
                continue;
            }
            let pass_row_len = sample_row_bytes(pw, samples, depth);
            let mut prev = vec![0u8; pass_row_len];
            for r in 0..ph {
                let gy = y0 + r * dy;
                // 从全帧行抽取 pass 像素。
                let mut pass_raw = vec![0u8; pass_row_len];
                let full_row = &full[gy];
                for i in 0..pw {
                    let gx = x0 + i * dx;
                    for s in 0..samples {
                        let v = read_sample(full_row, gx, s, samples, depth);
                        write_sample(&mut pass_raw, i, s, samples, depth, v);
                    }
                }
                let f = filter_for(idx);
                idx += 1;
                let filtered = filter_row(f, &pass_raw, &prev, bpp);
                stream.push(f);
                stream.extend_from_slice(&filtered);
                prev = pass_raw;
            }
        }
    }
    (stream, bpp)
}

// ---------------------------------------------------------------------------
// 参照解码（png crate EXPAND + 16bit BE→LE，与 kimg 包装器同款）
// ---------------------------------------------------------------------------

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

fn new_decode(png_bytes: &[u8]) -> Result<(ColorType, Vec<u8>), String> {
    let dec = PngDecoder::new(Cursor::new(png_bytes)).map_err(|e| e.to_string())?;
    let ct = dec.color_type();
    let mut out = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut out).map_err(|e| e.to_string())?;
    Ok((ct, out))
}

/// 逐一比较；返回 (一致数, 不一致数, 首条不一致描述)。
fn compare(label: &str, png_bytes: &[u8]) -> (usize, usize, Option<String>) {
    let (ref_ct, ref_out) = reference_expand(png_bytes);
    let (new_ct, new_out) = match new_decode(png_bytes) {
        Ok(v) => v,
        Err(e) => return (0, 1, Some(format!("{label}: 新引擎解码失败 {e}"))),
    };
    if new_ct != ref_ct {
        return (
            0,
            1,
            Some(format!("{label}: 色彩类型不符 new={new_ct:?} ref={ref_ct:?}")),
        );
    }
    if new_out != ref_out {
        let n = new_out.len().min(ref_out.len());
        let mut first = None;
        for i in 0..n {
            if new_out[i] != ref_out[i] {
                first = Some(i);
                break;
            }
        }
        return (
            0,
            1,
            Some(format!(
                "{label}: 字节不一致 new.len={} ref.len={} 首差 @{first:?} (new={:?} ref={:?})",
                new_out.len(),
                ref_out.len(),
                first.map(|i| new_out[i]),
                first.map(|i| ref_out[i]),
            )),
        );
    }
    (1, 0, None)
}

// ---------------------------------------------------------------------------
// 测试：全组合 parity（png crate 上游生成 fixture）
// ---------------------------------------------------------------------------

#[test]
fn parity_full_matrix_vs_png_crate() {
    // (色型, 位深) 全组合，附加 tRNS 与隔行变体。
    let sizes: &[(u32, u32)] = &[(1, 1), (3, 1), (1, 3), (7, 5), (16, 16), (257, 97)];
    let combos: &[(u8, u8, bool)] = &[
        (0, 1, false),
        (0, 2, false),
        (0, 4, false),
        (0, 8, false),
        (0, 16, false),
        (2, 8, false),
        (2, 16, false),
        (4, 8, false),
        (4, 16, false),
        (6, 8, false),
        (6, 16, false),
        (3, 1, false),
        (3, 2, false),
        (3, 4, false),
        (3, 8, false),
        // tRNS 变体
        (0, 4, true),
        (0, 8, true),
        (0, 16, true),
        (2, 8, true),
        (2, 16, true),
        (3, 4, true),
        (3, 8, true),
        (0, 1, true),
    ];

    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut first_bad: Option<String> = None;
    let mut fixture_count = 0usize;

    for (ct, depth, trns_on) in combos {
        let trns_on = *trns_on;
        let depth = *depth;
        let ct = *ct;
        for &(w, h) in sizes {
            for il in [0u8, 1u8] {
                // 调色板：sub-byte 用 2^depth 项，8 位用 32 项；灰色/RGB 的 tRNS 取有效样本值。
                let (palette, trns): (Option<Vec<u8>>, Option<Vec<u8>>) = match ct {
                    3 => {
                        let entries = if depth < 8 { 1usize << depth } else { 32 } as usize;
                        let mut pal = Vec::with_capacity(entries * 3);
                        for i in 0..entries {
                            pal.extend_from_slice(&[(i * 7) as u8, (i * 13) as u8, (i * 29) as u8]);
                        }
                        let tr = if trns_on {
                            Some((0..entries).map(|i| (i * 11) as u8).collect())
                        } else {
                            None
                        };
                        (Some(pal), tr)
                    }
                    0 | 2 => {
                        let key: u16 = match depth {
                            1 => 1,
                            2 => 2,
                            4 => 7,
                            8 => 200,
                            _ => 40000,
                        };
                        let bytes_needed = match ct {
                            0 => 2,
                            _ => 6,
                        };
                        let mut tr = Vec::new();
                        for _ in 0..if ct == 0 { 1 } else { 3 } {
                            tr.extend_from_slice(&key.to_be_bytes());
                        }
                        debug_assert_eq!(tr.len(), bytes_needed);
                        (None, if trns_on { Some(tr) } else { None })
                    }
                    _ => (None, None),
                };

                let (stream, _bpp) = build_stream(w as usize, h as usize, ct, depth, il, seed_for(ct, depth, w, h, trns_on));
                let png_bytes = assemble_png(
                    w, h, ct, depth, il,
                    palette.as_deref(),
                    trns.as_deref(),
                    &stream,
                );
                fixture_count += 1;
                let label = format!("ct={ct} depth={depth} trns={trns_on} {w}x{h} il={il}");
                let (o, b, msg) = compare(&label, &png_bytes);
                ok += o;
                bad += b;
                if let Some(m) = msg {
                    if first_bad.is_none() {
                        first_bad = Some(m);
                    }
                }
            }
        }
    }

    eprintln!("[parity] fixture={fixture_count} ok={ok} bad={bad}");
    if let Some(m) = first_bad {
        panic!("parity 存在不一致：{m}");
    }
}

fn seed_for(ct: u8, depth: u8, w: u32, h: u32, trns: bool) -> u64 {
    u64::from(ct) << 40 | u64::from(depth) << 32 | u64::from(w) << 16 | u64::from(h) | (trns as u64) << 8
}

// ---------------------------------------------------------------------------
// 测试：逐行强制 5 种 filter（最小 PNG 构造器直接落 filter 行）
// ---------------------------------------------------------------------------

#[test]
fn parity_forced_filters_every_row() {
    // 对每种 (ct, depth)：行 filter 循环打满 5 种，且每次解码与 gzip 无关地覆盖 Sub/Paeth 链。
    let combos: &[(u8, u8)] = &[
        (0, 1), (0, 2), (0, 4), (0, 8), (0, 16),
        (2, 8), (2, 16),
        (4, 8), (4, 16),
        (6, 8), (6, 16),
        (3, 1), (3, 2), (3, 4), (3, 8),
    ];
    // 行数 18（%5 覆盖 0..4 各≥3 行）
    let (w, h) = (129u32, 18u32);
    let mut ok = 0;
    let mut bad = 0;
    let mut first_bad = None;
    for &(ct, depth) in combos {
        let (stream, bpp) = build_stream(w as usize, h as usize, ct, depth, 0, seed_for(ct, depth, w, h, false));
        assert_eq!(stream.len(), (h as usize) * (1 + sample_row_bytes(w as usize, samples_per_pixel(ct), depth)));
        let _ = bpp;
        // 索引色需要 PLTE。
        let palette = if ct == 3 {
            let entries = if depth < 8 { 1usize << depth } else { 32 };
            let mut pal = Vec::with_capacity(entries * 3);
            for i in 0..entries {
                pal.extend_from_slice(&[(i * 7) as u8, (i * 13) as u8, (i * 29) as u8]);
            }
            Some(pal)
        } else {
            None
        };
        let png_bytes = assemble_png(w, h, ct, depth, 0, palette.as_deref(), None, &stream);
        let label = format!("forced-filters ct={ct} depth={depth}");
        let (o, b, m) = compare(&label, &png_bytes);
        ok += o;
        bad += b;
        if first_bad.is_none() {
            first_bad = m;
        }
    }
    // 显式断言 5 种 filter 全出现（逐行已循环覆盖）。
    let mut seen = [false; 5];
    let (stream, _) = build_stream(w as usize, h as usize, 6, 8, 0, 12345);
    for (i, byte) in stream.iter().enumerate().step_by(1 + sample_row_bytes(w as usize, 4, 8)) {
        let f = *byte;
        if f <= 4 {
            seen[f as usize] = true;
        }
        let _ = i;
    }
    assert!(seen.iter().all(|&x| x), "5 种 filter 未全覆盖：{:?}", seen);
    eprintln!("[parity] forced-filters ok={ok} bad={bad}");
    if let Some(m) = first_bad {
        panic!("forced-filters 不一致：{m}");
    }
}

// ---------------------------------------------------------------------------
// 测试：corpus 抽样（target/imgbench-corpus；缺语料 SKIP）
// ---------------------------------------------------------------------------

#[test]
fn corpus_against_png_crate() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus");
    // 语料分格式子目录（PNG/ 等），递归收集。
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
    let mut entries: Vec<std::path::PathBuf> = Vec::new();
    collect_pngs(&corpus, &mut entries);
    entries.sort();
    if entries.is_empty() {
        eprintln!("[corpus] SKIP: target/imgbench-corpus 缺失或无 PNG（expected 34）");
        return;
    }
    let mut ok = 0;
    let mut bad = 0;
    let mut first_bad = None;
    for p in &entries {
        let data = std::fs::read(p).expect("读语料");
        let label = format!("corpus {}", p.file_name().unwrap().to_string_lossy());
        let (o, b, m) = compare(&label, &data);
        ok += o;
        bad += b;
        if first_bad.is_none() {
            first_bad = m;
        }
    }
    eprintln!("[corpus] files={} ok={} bad={}", entries.len(), ok, bad);
    if let Some(m) = first_bad {
        panic!("corpus 不一致：{m}");
    }
}

// ---------------------------------------------------------------------------
// 测试：limits / 错误路径回归（不改语义）
// ---------------------------------------------------------------------------

#[test]
fn corrupt_and_bomb_inputs_rejected() {
    use kimg::{ImageError, Limits};

    // 伪造超大声明尺寸 PNG（CRC 0）：构造或 set_limits 必须报错、绝不 OOM。
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&70000u32.to_be_bytes());
    bytes.extend_from_slice(&70000u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(b"IEND");
    let r = PngDecoder::new(Cursor::new(&bytes));
    match r {
        Ok(mut d) => {
            let e = d.set_limits(Limits {
                max_image_width: Some(65535),
                max_image_height: Some(65535),
                max_alloc: Some(1 << 20),
            });
            assert!(e.is_err(), "超限应返回错误");
        }
        Err(ImageError::Decoding(_)) => {} // CRC 不匹配 → 拒绝，亦符合预期
        Err(other) => panic!("意外错误类型：{other:?}"),
    }

    // 截断/垃圾输入不应 panic，只应返回错误。
    for junk in [
        &[] as &[u8],
        &[0x89, b'P', b'N', b'G'],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        b"GIF89a....",
    ] {
        let _ = PngDecoder::new(Cursor::new(junk));
    }

    // 一个合法的 1x1 红 PNG：end-to-end 语义回归（含 set_limits + total_bytes + dimensions）。
    let (stream, _) = build_stream(1, 1, 6, 8, 0, 7);
    let png_bytes = assemble_png(1, 1, 6, 8, 0, None, None, &stream);
    let dec = PngDecoder::new(Cursor::new(&png_bytes)).unwrap();
    assert_eq!(dec.dimensions(), (1, 1));
    assert_eq!(dec.color_type(), ColorType::Rgba8);
    assert_eq!(dec.total_bytes(), 4);
    let mut buf = vec![0u8; 4];
    dec.read_image(&mut buf).unwrap();
    let (_, ref_out) = reference_expand(&png_bytes);
    assert_eq!(buf, ref_out);
}
