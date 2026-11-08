//! kimg_gif 自研解码器（OURS） vs C# Sdcb.SimdCodecs.Gif `GifFrameReader` 参照探针。
//!
//! 目标口径：RGBA8、单核、**全帧解码总墙钟**（静态=1 帧，与 C# 单一 `| `name` | ms |` 表对齐）。
//! - 静态（1 帧）：`GifDecoder::read_image` 首帧合成到预分配缓冲（对齐 C# `TryDecodeIntoRgba8`）。
//! - 动画（>1 帧）：`GifDecoder::into_frames()` 逐帧全画布 RGBA 合成，测循环内全量消费的总墙钟。
//! GIF 恒 RGBA8（无 png/tiff 那种 L8/RGB8 补 alpha 差异）；C# 侧强制 `PixelFormat.Rgba32`，两侧
//! 4B/px 紧密对齐。
//!
//! ⚠️ 时序口径（2026-11-08 记录，同 tiff）：kimg 动画路径每帧新建 w*h*4 缓冲 + `RgbaImage`
//! （`into_frames()` 真实 API 行为），C# 侧把帧写进每文件复用的一次性 dest —— kimg 动画路径
//! 每帧多一次分配的带宽/分配开销，轻微偏向 C#。静态路径两腿都复用缓冲，无差。动画语料
//! ≤320×240×8 帧，分配开销小，不翻转同源实现对比结论。
//!
//! 运行（release，长命令前台阻塞到结束，禁后台化；见 K86）：
//! ```text
//! cargo run --release --manifest-path Benchmarks/gif/probe/Cargo.toml -- --corpus Benchmarks/corpus/GIF
//! ```
//! env / 参数：
//! - `--corpus <dir>` 优先；否则 env `KIMG_GIF_CORPUS`；缺省 `Benchmarks/corpus/GIF`（相对本 crate）。
//!   参数指向 **GIF 语料目录本身**（不是 corpus 根）。
//! - env `KIMG_GIF_ROUNDS`：计时轮数（缺省 9，另带 1 轮暖机）。
//!
//! 输出契约（compare_refs.py / run_acceptance.ps1 解析，勿改前缀）：
//! ```text
//! KIMG_GIF_VS_CSHARP profile=release rounds=9 corpus=<绝对路径> files=<n>
//! KIMG_GIF_VS_CSHARP perfile <文件名> <w>x<h> frames=<N> layout=rgba8 ours=<ms>
//! KIMG_GIF_VS_CSHARP AGG files=<n> layout_rgba8=<m> geomean_ours=<ms> total_ours_ms=<t>
//! KIMG_GIF_VS_CSHARP done.
//! ```

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use kimg_core::AnimationDecoder as _;
use kimg_core::ImageDecoder as _;
use kimg_gif::GifDecoder;

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

fn collect_gifs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x.eq_ignore_ascii_case("gif")).unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn main() {
    let rounds: usize = std::env::var("KIMG_GIF_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS_DEFAULT);

    // 语料：--corpus 参数 > env KIMG_GIF_CORPUS > 缺省 Benchmarks/corpus/GIF。
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
    let corpus_default = format!("{}/../../corpus/GIF", env!("CARGO_MANIFEST_DIR"));
    let dir = PathBuf::from(
        cli_corpus
            .or_else(|| std::env::var("KIMG_GIF_CORPUS").ok())
            .unwrap_or(corpus_default),
    );
    if !dir.is_dir() {
        println!("KIMG_GIF_VS_CSHARP note corpus dir missing: {}", dir.display());
        return;
    }
    let canonical = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
    let canonical_str = canonical.to_string_lossy();
    let corpus_display = canonical_str.strip_prefix(r"\\?\").unwrap_or(&canonical_str);

    let files = collect_gifs(&dir);
    if files.is_empty() {
        println!("KIMG_GIF_VS_CSHARP corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "KIMG_GIF_VS_CSHARP profile={profile} rounds={rounds} corpus={corpus_display} files={}",
        files.len()
    );

    struct Case {
        name: String,
        bytes: Vec<u8>,
        w: usize,
        h: usize,
        frames: usize,
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
        // 冒烟 + 帧数统计 + 暖机：消费一遍 into_frames。
        let dec = match GifDecoder::new(Cursor::new(&bytes)) {
            Ok(d) => d,
            Err(e) => {
                skips.push(format!("{name}: 解码头失败 {e:?}"));
                continue;
            }
        };
        let (w, h) = dec.dimensions();
        let mut frames = 0usize;
        let mut err = false;
        for fr in dec.into_frames() {
            if fr.is_err() {
                err = true;
                break;
            }
            frames += 1;
        }
        if err {
            skips.push(format!("{name}: 冒烟逐帧解码失败"));
            continue;
        }
        cases.push(Case {
            name,
            bytes,
            w: w as usize,
            h: h as usize,
            frames,
        });
    }
    for s in &skips {
        println!("KIMG_GIF_VS_CSHARP SKIP {s}");
    }
    if cases.is_empty() {
        println!("KIMG_GIF_VS_CSHARP no benchable files");
        return;
    }

    let mut agg: Vec<f64> = Vec::new();
    let mut layout_rgba8 = 0usize;
    let mut total_ms = 0.0;
    for c in cases.iter_mut() {
        let (w, h, frames) = (c.w, c.h, c.frames);
        let ms = if frames == 1 {
            // 静态：read_image 首帧合成到复用缓冲（对齐 C# TryDecodeIntoRgba8 复用 dest）。
            let mut buf = vec![0u8; w * h * 4];
            median_ms(
                || {
                    let d = GifDecoder::new(Cursor::new(&c.bytes)).expect("冒烟已保证");
                    d.read_image(&mut buf).expect("冒烟已保证");
                },
                rounds,
            )
        } else {
            // 动画：逐帧全画布合成总墙钟（C# 单表同口径）。
            median_ms(
                || {
                    let d = GifDecoder::new(Cursor::new(&c.bytes)).expect("冒烟已保证");
                    for fr in d.into_frames() {
                        fr.expect("冒烟已保证");
                    }
                },
                rounds,
            )
        };
        layout_rgba8 += 1;
        agg.push(ms);
        total_ms += ms;
        println!(
            "KIMG_GIF_VS_CSHARP perfile {} {}x{} frames={frames} layout=rgba8 ours={ms:.3}",
            c.name, w, h
        );
    }
    let g_ours = geomean(&agg);
    println!(
        "KIMG_GIF_VS_CSHARP AGG files={} layout_rgba8={} geomean_ours={g_ours:.3} total_ours_ms={total_ms:.3}",
        cases.len(),
        layout_rgba8
    );
    println!("KIMG_GIF_VS_CSHARP done.");
}
