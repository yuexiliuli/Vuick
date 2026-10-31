//! 语料级分账探针 v3：① parse / decode / alloc 分账（PARSE_PROBE）；② corpus filter 类型直方图
//! （FILTER_HIST），把「unfilter 阶段为何在真图语料花 ~186ms」拆到 5 类 filter 的行数与字节量。
//! 运行：`cargo bench -p imgbench --bench stage_parse_probe`

use std::io::Cursor;
use std::time::Instant;

use kimg::codecs::png::PngDecoder;
use kimg::ImageDecoder as _;

const ROUNDS: usize = 7;

fn collect_corpus() -> Vec<(String, Vec<u8>)> {
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
    files
        .iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(p).unwrap(),
            )
        })
        .collect()
}

fn median_ms<F: FnMut()>(f: &mut F, rounds: usize) -> f64 {
    f();
    let mut acc: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        acc.push(t.elapsed().as_secs_f64() * 1e3);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    acc[acc.len() / 2]
}

/// 单文件 parse（PngDecoder::new 仅构造）中位。
fn measure_parse(datas: &[(String, Vec<u8>)]) -> f64 {
    median_ms(
        &mut || {
            for (_, b) in datas {
                let _ = PngDecoder::new(Cursor::new(b.as_slice())).unwrap();
            }
        },
        ROUNDS,
    )
}

// ---------------------------------------------------------------------------
// 同进程配对：fdeflate 仅解压 vs png crate 全解码 vs kimg 全解码
// ---------------------------------------------------------------------------

/// fdeflate 仅解压整 corpora（与 kimg 同款 32KB 分块姿势）。
fn inflate_corpus_fdeflate(datas: &[(String, Vec<u8>)]) -> f64 {
    let mut results = Vec::with_capacity(datas.len());
    for (_, b) in datas {
        let Some((w, h, depth, ct, interlace, idat)) = extract(b) else {
            continue;
        };
        let samples = match ct {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            6 => 4,
            _ => 0,
        };
        if samples == 0 {
            continue;
        }
        let row_for = |ww: usize| (ww * samples as usize * depth as usize + 7) / 8;
        let expected: usize = if interlace == 0 {
            (row_for(w) + 1) * h
        } else {
            let ext = |size: usize, start: usize, step: usize| {
                if size > start {
                    (size - start + step - 1) / step
                } else {
                    0
                }
            };
            let mut total = 0usize;
            for (x0, y0, dx, dy) in ADAM7_PASSES {
                let pw = ext(w, x0, dx);
                let ph = ext(h, y0, dy);
                if pw > 0 && ph > 0 {
                    total += (row_for(pw) + 1) * ph;
                }
            }
            total
        };
        results.push((idat, expected));
    }
    median_ms(
        &mut || {
            for (idat, expected) in &results {
                let _ = inflate_fdeflate(idat, *expected);
            }
        },
        ROUNDS,
    )
}

/// png crate 全解码（EXPAND 语义，同为 release）。
fn png_crate_full_corpus(datas: &[(String, Vec<u8>)]) -> f64 {
    median_ms(
        &mut || {
            for (_, b) in datas {
                let mut dec = png::Decoder::new(Cursor::new(b.as_slice()));
                dec.set_transformations(png::Transformations::EXPAND);
                let mut reader = dec.read_info().unwrap();
                let mut out = vec![0u8; reader.output_buffer_size()];
                reader.next_frame(&mut out).unwrap();
            }
        },
        ROUNDS,
    )
}

/// kimg 全解码。
fn kimg_full_corpus(datas: &[(String, Vec<u8>)]) -> f64 {
    median_ms(
        &mut || {
            for (_, b) in datas {
                let dec = PngDecoder::new(Cursor::new(b.as_slice())).unwrap();
                let mut out = vec![0u8; dec.total_bytes() as usize];
                dec.read_image(&mut out).unwrap();
            }
        },
        ROUNDS,
    )
}

// ---------------------------------------------------------------------------
// filter 直方图：IDAT 抽出 + fdeflate 解压 + 逐行取 filter 字节归类
// ---------------------------------------------------------------------------

const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const ADAM7_PASSES: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

fn endian32(p: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]])
}

/// 返回 (宽,高,位深,色彩类型,隔行, IDAT载荷)。
fn extract(png: &[u8]) -> Option<(usize, usize, u32, u32, u8, Vec<u8>)> {
    if png.len() < 33 || png[..8] != SIGNATURE {
        return None;
    }
    let w = endian32(png, 16) as usize;
    let h = endian32(png, 20) as usize;
    let depth = png[24] as u32;
    let ct = png[25] as u32;
    let interlace = png[28];
    let mut idat = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= png.len() {
        let len = endian32(png, pos) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            idat.extend_from_slice(&png[pos + 8..pos + 8 + len]);
        } else if &png[pos + 4..pos + 8] == b"IEND" {
            break;
        }
        pos += 12 + len;
    }
    Some((w, h, depth, ct, interlace, idat))
}

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

/// 逐行 filter 直方图（None/Sub/Up/Avg/Paeth × 行数与字节量）。
fn filter_hist(png: &[u8]) -> Option<([u64; 5], [u64; 5])> {
    let (w, h, depth, ct, interlace, idat) = extract(png)?;
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
    let row_for = |ww: usize| (ww * samples as usize * depth as usize + 7) / 8;
    let expected: usize = if interlace == 0 {
        (row_for(w) + 1) * h
    } else {
        let ext = |size: usize, start: usize, step: usize| {
            if size > start {
                (size - start + step - 1) / step
            } else {
                0
            }
        };
        let mut total = 0usize;
        for (x0, y0, dx, dy) in ADAM7_PASSES {
            let pw = ext(w, x0, dx);
            let ph = ext(h, y0, dy);
            if pw > 0 && ph > 0 {
                total += (row_for(pw) + 1) * ph;
            }
        }
        total
    };
    let inf = inflate_fdeflate(&idat, expected);
    let mut rows = [0u64; 5];
    let mut bytes = [0u64; 5];
    let mut pos = 0usize;
    if interlace == 0 {
        let rlen = row_for(w);
        for _ in 0..h {
            let f = inf[pos] as usize;
            if f < 5 {
                rows[f] += 1;
                bytes[f] += rlen as u64;
            }
            pos += 1 + rlen;
        }
    } else {
        for (x0, y0, dx, dy) in ADAM7_PASSES {
            let ext = |size: usize, start: usize, step: usize| {
                if size > start {
                    (size - start + step - 1) / step
                } else {
                    0
                }
            };
            let pw = ext(w, x0, dx);
            let ph = ext(h, y0, dy);
            if pw == 0 || ph == 0 {
                continue;
            }
            let rlen = row_for(pw);
            for _ in 0..ph {
                let f = inf[pos] as usize;
                if f < 5 {
                    rows[f] += 1;
                    bytes[f] += rlen as u64;
                }
                pos += 1 + rlen;
            }
        }
    }
    debug_assert_eq!(pos, expected);
    Some((rows, bytes))
}

// ---------------------------------------------------------------------------

fn main() {
    let datas = collect_corpus();
    if datas.is_empty() {
        println!("PARSE_PROBE SKIP (target/imgbench-corpus 缺失)");
        return;
    }
    println!("PARSE_PROBE files={} rounds={ROUNDS}", datas.len());

    let parse = measure_parse(&datas);
    let mut per_total: Vec<(String, f64)> = Vec::with_capacity(datas.len());
    for (name, b) in &datas {
        let t = median_ms(
            &mut || {
                let dec = PngDecoder::new(Cursor::new(b.as_slice())).unwrap();
                let mut out = vec![0u8; dec.total_bytes() as usize];
                dec.read_image(&mut out).unwrap();
            },
            ROUNDS,
        );
        per_total.push((name.clone(), t));
    }
    per_total.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let batch_total: f64 = per_total.iter().map(|(_, t)| t).sum();
    println!("PARSE_PROBE parse_all_files_median={parse:.3}ms per_file_full_median_sum={batch_total:.3}ms");
    let mut n_interlaced = 0usize;
    for (name, b) in &datas {
        if extract(b).map(|x| x.4 == 1).unwrap_or(false) {
            n_interlaced += 1;
        }
    }
    println!("PARSE_PROBE interlaced_files={n_interlaced}");

    let mut rows = [0u64; 5];
    let mut bytes = [0u64; 5];
    let mut total_inflated = 0u64;
    for (name, b) in &datas {
        if let Some((r, by)) = filter_hist(b) {
            for i in 0..5 {
                rows[i] += r[i];
                bytes[i] += by[i];
            }
            total_inflated += by.iter().sum::<u64>() + r.iter().sum::<u64>();
        } else {
            eprintln!("[filter_hist] skip {name}");
        }
    }
    let names = ["None", "Sub", "Up", "Avg", "Paeth"];
    println!("FILTER_HIST total_inflated={total_inflated}B");
    for i in 0..5 {
        println!(
            "FILTER_HIST {}: rows={} ({:.1}%) bytes={}B ({:.1}%)",
            names[i],
            rows[i],
            rows[i] as f64 / rows.iter().sum::<u64>() as f64 * 100.0,
            bytes[i],
            bytes[i] as f64 / bytes.iter().sum::<u64>() as f64 * 100.0,
        );
    }

    // 同进程配对三测（同一机器态）：
    let fd = inflate_corpus_fdeflate(&datas);
    let png_full = png_crate_full_corpus(&datas);
    let kimg_full = kimg_full_corpus(&datas);
    let png_non_inflate = png_full - fd;
    let kimg_non_inflate = kimg_full - fd;
    println!(
        "PAIRED fdeflate_only={fd:.3}ms png_full={png_full:.3}ms kimg_full={kimg_full:.3}ms"
    );
    println!(
        "PAIRED png_non_inflate={png_non_inflate:.3}ms kimg_non_inflate={kimg_non_inflate:.3}ms"
    );
    eprintln!("[stage_parse_probe] done");
}
