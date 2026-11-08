//! kimg_bmp 自研解码器（OURS） vs C# Sdcb.SimdCodecs.Bmp `BmpImageDecoder.TryDecodeIntoRgba8` 参照探针。
//!
//! 目标口径：RGBA8 —— kimg 对每张 BMP 解码到「自然输出」（Rgb8 3B/px 或 Rgba8 4B/px，取决于
//! 是否带 alpha：32bpp BI_RGB 丢第 4 字节 → Rgb8；BITFIELDS32 带 alpha 掩码 → Rgba8），
//! 统一补尾巴/直用为 RGBA8（紧密 4B/px、w*h*4），与 C# `TryDecodeIntoRgba8` 输出布局一致。
//! BMP 全部文件都是 RGBA8 可比（无 png 那种 layout 排除），布局类别由 compare_refs.py 按
//! 文件名 `__<layout>` 后缀划分（无后缀 = native）。
//!
//! ⚠️ 时序口径（2026-11-08 记录）：kimg 的 RGB8→RGBA8 补 alpha 在**计时循环外**一次完成
//! （计时只测自然输出解到 `buf`），C# 侧每调用都是 4B/px 满写 —— 故 Rgb8 自然输出文件（24bit、
//! 16bit、调色板、RLE 等）kimg 实际逐帧写 3B/px、C# 写 4B/px，**轻微偏向 kimg**（约 <1.3× 内存
//! 带宽差）。该偏差不翻转任何结论（产物字节与 C# 全等，2–4× 级差距与 <1.3× 带宽差可明确区分）。
//!
//! 运行（release，长命令前台阻塞到结束，禁后台化；见 K86）：
//! ```text
//! cargo run --release --manifest-path Benchmarks/bmp/probe/Cargo.toml -- --corpus Benchmarks/corpus
//! ```
//! env / 参数：
//! - `--corpus <dir>` 优先；否则 env `KIMG_BMP_CORPUS`；缺省 `Benchmarks/corpus`（相对本 crate）。
//!   探针自动读 <corpus>/BMP 与 <corpus>/BMP-MATRIX 两个子目录。
//! - env `KIMG_BMP_ROUNDS`：计时轮数（缺省 9，另带 1 轮暖机）。
//! - env `KIMG_BMP_NO_SIMD`：kimg_bmp 已识别（关 SSSE3/AVX2 行内核，量化 SIMD 收益）。
//!
//! 输出契约（compare_refs.py / run_acceptance.ps1 解析，勿改前缀）：
//! ```text
//! KIMG_BMP_VS_CSHARP profile=release rounds=9 corpus=<绝对路径> files=<n>
//! KIMG_BMP_VS_CSHARP perfile <文件名> <w>x<h> color=<RGB8|RGBA8> layout=rgba8 ours=<ms>
//! KIMG_BMP_VS_CSHARP AGG files=<n> layout_rgba8=<m> geomean_ours=<ms> total_ours_ms=<t>
//! KIMG_BMP_VS_CSHARP done.
//! ```

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use kimg_bmp::BmpDecoder;
use kimg_core::color::ColorType;
use kimg_core::ImageDecoder as _;

const ROUNDS_DEFAULT: usize = 9;

fn median_ms<F: FnMut()>(mut f: F, rounds: usize) -> f64 {
    f(); // 暖机 1 轮
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

fn geomean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    (values.iter().map(|v| v.ln()).sum::<f64>() / values.len() as f64).exp()
}

fn collect_bmps(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for sub in ["BMP", "BMP-MATRIX"] {
        let d = dir.join(sub);
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension()
                    .map(|x| x.eq_ignore_ascii_case("bmp"))
                    .unwrap_or(false)
                {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

fn main() {
    let rounds: usize = std::env::var("KIMG_BMP_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS_DEFAULT);

    // 语料：--corpus 参数 > env KIMG_BMP_CORPUS > 缺省 Benchmarks/corpus。
    let mut cli_corpus: Option<String> = None;
    {
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i + 1 < args.len() {
            if args[i] == "--corpus" {
                cli_corpus = Some(args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    let corpus_default = format!("{}/../../corpus", env!("CARGO_MANIFEST_DIR"));
    let dir = PathBuf::from(
        cli_corpus
            .or_else(|| std::env::var("KIMG_BMP_CORPUS").ok())
            .unwrap_or(corpus_default),
    );
    if !dir.is_dir() {
        println!("KIMG_BMP_VS_CSHARP note corpus dir missing: {}", dir.display());
        return;
    }
    let canonical = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
    let canonical_str = canonical.to_string_lossy();
    let corpus_display = canonical_str.strip_prefix(r"\\?\").unwrap_or(&canonical_str);

    let files = collect_bmps(&dir);
    if files.is_empty() {
        println!("KIMG_BMP_VS_CSHARP corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "KIMG_BMP_VS_CSHARP profile={profile} rounds={rounds} corpus={corpus_display} files={}",
        files.len()
    );

    struct Case {
        name: String,
        bytes: Vec<u8>,
        w: usize,
        h: usize,
        color: ColorType,
        buf: Vec<u8>,
    }
    let mut cases: Vec<Case> = Vec::new();
    let mut skips: Vec<String> = Vec::new();
    for p in &files {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                skips.push(format!("{name}: 读文件失败 {e}"));
                continue;
            }
        };
        // 冒烟：零拷贝借用切片构造 + 解码一遍（复用缓冲）。
        let dec = match BmpDecoder::new(Cursor::new(&bytes)) {
            Ok(d) => d,
            Err(e) => {
                skips.push(format!("{name}: 解码头失败 {e:?}"));
                continue;
            }
        };
        let (w, h) = dec.dimensions();
        let color = dec.color_type();
        let bpp = usize::from(color.bytes_per_pixel());
        let mut buf = vec![0u8; (w as usize) * (h as usize) * bpp];
        if dec.read_image(&mut buf).is_err() {
            skips.push(format!("{name}: 冒烟解码失败"));
            continue;
        }
        let _ = (bpp, &buf);
        // RGBA8 归一（读侧尺寸与 C# dump w*h*4 一致可用）：Rgb8 补 alpha=FF → RGBA8。
        let rgba = match color {
            ColorType::Rgba8 => buf.clone(),
            ColorType::Rgb8 => buf
                .chunks_exact(3)
                .flat_map(|px| [px[0], px[1], px[2], 0xFF])
                .collect::<Vec<u8>>(),
            other => {
                skips.push(format!("{name}: 非 RGB(A)8 输出 {other:?}"));
                continue;
            }
        };
        debug_assert_eq!(rgba.len(), (w as usize) * (h as usize) * 4);
        cases.push(Case {
            name,
            bytes,
            w: w as usize,
            h: h as usize,
            color,
            buf,
        });
    }
    for s in &skips {
        println!("KIMG_BMP_VS_CSHARP SKIP {s}");
    }
    if cases.is_empty() {
        println!("KIMG_BMP_VS_CSHARP no benchable files");
        return;
    }

    let mut agg: Vec<f64> = Vec::new();
    let mut layout_rgba8 = 0usize;
    let mut total_ms = 0.0;
    for c in cases.iter_mut() {
        let (w, h) = (c.w, c.h);
        // 每轮全套解码（含头解析，对齐 C# TryDecodeIntoRgba8 每调用自解析）；零拷贝借用输入。
        let ms = median_ms(
            || {
                let d = BmpDecoder::new(Cursor::new(&c.bytes)).expect("冒烟已保证");
                d.read_image(&mut c.buf).expect("冒烟已保证");
            },
            rounds,
        );
        layout_rgba8 += 1;
        agg.push(ms);
        total_ms += ms;
        println!(
            "KIMG_BMP_VS_CSHARP perfile {} {}x{} color={:?} layout=rgba8 ours={ms:.3}",
            c.name, w, h, c.color
        );
    }
    let g_ours = geomean(&agg);
    println!(
        "KIMG_BMP_VS_CSHARP AGG files={} layout_rgba8={} geomean_ours={g_ours:.3} total_ours_ms={total_ms:.3}",
        cases.len(),
        layout_rgba8
    );
    println!("KIMG_BMP_VS_CSHARP done.");
}
