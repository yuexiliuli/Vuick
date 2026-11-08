//! kimg_png 自研引擎（OURS） vs C# Sdcb.SimdCodecs.Png `PngImageDecoder.TryDecodeIntoRgba8` 参照探针。
//!
//! 目标口径：RGBA8 —— 对语料每张 PNG 解码到「自然输出」（PngDecoder EXPAND 语义），与 C#
//! RGBA8（紧密 4B/px、w*h*4）双端可比的部分记入比较集（layout=rgba8）；RGB8（3B/px）/gray8
//! （1B/px）等布局不同的文件标 layout=other，**不计入 geomean**（phase-1 比较集只收 RGBA8 布局）。
//! 单核口径：进程内强制 `KIMG_PNG_PIPELINE=0`（关双核管道，对齐 C# 单线程；task 蓝图口径）。
//!
//! 运行（release，长命令前台阻塞到结束，禁后台化；见 K86）：
//! ```text
//! cargo run --release --manifest-path Benchmarks/png/probe/Cargo.toml -- --corpus Benchmarks/corpus/PNG
//! ```
//! env / 参数：
//! - `--corpus <dir>` 优先；否则 env `KIMG_PNG_CORPUS`；缺省 `Benchmarks/corpus/PNG`（相对本 crate）
//! - env `KIMG_PNG_ROUNDS`：计时轮数（缺省 9，另带 1 轮暖机）
//! - 分账：`--features stage-profile` 构建 + `KIMG_PNG_STAGE_PROFILE=1` 时每次解码打印
//!   `STAGE ... inflate=..ms unfilter=..ms expand=..ms copy=..ms wall=..ms` 行；
//!   `KIMG_PNG_NO_SIMD=1` 强制反滤波/展开走标量（量化 SIMD 收益）。
//!
//! 输出契约（compare_refs.py / run_acceptance.ps1 解析，勿改前缀）：
//! ```text
//! KIMG_PNG_VS_CSHARP profile=release rounds=9 corpus=<绝对路径> files=<n>
//! KIMG_PNG_VS_CSHARP perfile <文件名> <w>x<h> color=<CT> bpp=<n> layout=<rgba8|other> ours=<ms>
//! KIMG_PNG_VS_CSHARP AGG files=<n> layout_rgba8=<m> geomean_ours=<ms> total_ours_ms=<t>
//! KIMG_PNG_VS_CSHARP done.
//! ```

use std::path::PathBuf;
use std::time::Instant;

use kimg_core::ColorType;
use kimg_core::ImageDecoder as _;
use kimg_png::codecs::png::PngDecoder as OursPng;

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

fn collect_pngs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("png"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn main() {
    // 单核口径：进程内强制关双核管道（KIMG_PNG_PIPELINE=0）。测试可经独立 env 翻转，探针固定单核。
    // SAFETY：main 单线程早期设置，无并发 read/write env 竞争；子线程均在之后 spawn（无）。
    unsafe { std::env::set_var("KIMG_PNG_PIPELINE", "0") };

    let rounds: usize = std::env::var("KIMG_PNG_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS_DEFAULT);

    // 语料：--corpus 参数 > env KIMG_PNG_CORPUS > 缺省 Benchmarks/corpus/PNG。
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
    let corpus_default = format!("{}/../../corpus/PNG", env!("CARGO_MANIFEST_DIR"));
    let dir = PathBuf::from(
        cli_corpus
            .or_else(|| std::env::var("KIMG_PNG_CORPUS").ok())
            .unwrap_or(corpus_default),
    );
    if !dir.is_dir() {
        println!("KIMG_PNG_VS_CSHARP note corpus dir missing: {}", dir.display());
        return;
    }
    let canonical = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
    let canonical_str = canonical.to_string_lossy();
    let corpus_display = canonical_str
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical_str);

    let files = collect_pngs(&dir);
    if files.is_empty() {
        println!("KIMG_PNG_VS_CSHARP corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "KIMG_PNG_VS_CSHARP profile={profile} rounds={rounds} corpus={corpus_display} files={}",
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
        // 冒烟：零拷贝借用切片解析 + 解码一遍（复用缓冲）。
        let dec = match OursPng::from_slice(&bytes) {
            Ok(d) => d,
            Err(e) => {
                skips.push(format!("{name}: 解码头失败 {e}"));
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
        println!("KIMG_PNG_VS_CSHARP SKIP {s}");
    }
    if cases.is_empty() {
        println!("KIMG_PNG_VS_CSHARP no benchable files");
        return;
    }

    let mut agg: Vec<f64> = Vec::new();
    let mut layout_rgba8 = 0usize;
    let mut total_ms = 0.0;
    for c in cases.iter_mut() {
        let bpp = usize::from(c.color.bytes_per_pixel());
        let layout = if c.color == ColorType::Rgba8 && bpp == 4 {
            "rgba8"
        } else {
            "other"
        };
        // 每轮全套解码（含头解析，对齐 C# TryDecodeIntoRgba8 每调用自解析）；零拷贝借用输入。
        let ms = median_ms(
            || {
                let d = OursPng::from_slice(&c.bytes).expect("冒烟已保证");
                let _ = d.read_image(&mut c.buf).expect("冒烟已保证");
            },
            rounds,
        );
        if layout == "rgba8" {
            layout_rgba8 += 1;
            agg.push(ms);
        }
        total_ms += ms;
        println!(
            "KIMG_PNG_VS_CSHARP perfile {} {}x{} color={:?} bpp={} layout={} ours={ms:.3}",
            c.name, c.w, c.h, c.color, bpp, layout
        );
    }
    let g_ours = geomean(&agg);
    println!(
        "KIMG_PNG_VS_CSHARP AGG files={} layout_rgba8={} geomean_ours={g_ours:.3} total_ours_ms={total_ms:.3}",
        cases.len(),
        layout_rgba8
    );
    println!("KIMG_PNG_VS_CSHARP done.");
}
