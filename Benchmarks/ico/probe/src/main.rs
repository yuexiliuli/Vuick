//! kimg_ico 自研解码器（OURS） vs C# Sdcb.SimdCodecs.Ico `IcoDecoder` 参照探针。
//!
//! 目标口径：RGBA8、单核、**Best 条目解码总墙钟**（两侧都「读目录 → 选 Best → 解码该条目」）。
//! - C# 侧：`IcoDecoder.Decode(data)`（读目录 + Best 选择 + 解码 + 分配 Rgba）。icon 与 cursor 同路径。
//! - 本探针：`IcoDecoder::new(Cursor)`（读目录 + Best 选择）→ `read_image`（解码进预分配缓冲）。
//! ICO 恒 RGBA8（C# 恒 8-bit RGBA，kimg_ico 恒 Rgba8），双端 4B/px 紧密对齐。
//!
//! ⚠️ 时序口径（2026-11-08 记录）：两侧每轮都新建输出缓冲（C# `Decode` 新建 Rgba；本侧每轮
//! `vec![0u8; n]`），分配形态对称。语料 ≤256×256，分配开销小，不翻转同源实现对比结论。
//!
//! 运行（release，长命令前台阻塞到结束，禁后台化；见 K86）：
//! ```text
//! cargo run --release --manifest-path Benchmarks/ico/probe/Cargo.toml -- --corpus Benchmarks/corpus/ICO
//! ```
//! env / 参数：
//! - `--corpus <dir>` 优先；否则 env `KIMG_ICO_CORPUS`；缺省 `Benchmarks/corpus/ICO`（相对本 crate）。
//!   参数指向 **ICO 语料目录本身**（不是 corpus 根）。
//! - env `KIMG_ICO_ROUNDS`：计时轮数（缺省 9，另带 1 轮暖机）。
//!
//! 输出契约（compare_refs.py / run_acceptance.ps1 解析，勿改前缀）：
//! ```text
//! KIMG_ICO_VS_CSHARP profile=release rounds=9 corpus=<绝对路径> files=<n>
//! KIMG_ICO_VS_CSHARP SKIP <name>: ...
//! KIMG_ICO_VS_CSHARP perfile <文件名> <w>x<h> layout=rgba8 ours=<ms>
//! KIMG_ICO_VS_CSHARP AGG files=<n> layout_rgba8=<m> geomean_ours=<ms> total_ours_ms=<t>
//! KIMG_ICO_VS_CSHARP done.
//! ```

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use kimg_core::ImageDecoder as _;
use kimg_ico::IcoDecoder;

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

fn collect(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase());
            if matches!(ext.as_deref(), Some("ico") | Some("cur")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn main() {
    let rounds: usize = std::env::var("KIMG_ICO_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS_DEFAULT);

    // 语料：--corpus 参数 > env KIMG_ICO_CORPUS > 缺省 Benchmarks/corpus/ICO。
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
    let corpus_default = format!("{}/../../corpus/ICO", env!("CARGO_MANIFEST_DIR"));
    let dir = PathBuf::from(
        cli_corpus
            .or_else(|| std::env::var("KIMG_ICO_CORPUS").ok())
            .unwrap_or(corpus_default),
    );
    if !dir.is_dir() {
        println!(
            "KIMG_ICO_VS_CSHARP note corpus dir missing: {}",
            dir.display()
        );
        return;
    }
    let canonical = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
    let canonical_str = canonical.to_string_lossy();
    let corpus_display = canonical_str.strip_prefix(r"\\?\").unwrap_or(&canonical_str);

    let files = collect(&dir);
    if files.is_empty() {
        println!("KIMG_ICO_VS_CSHARP corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "KIMG_ICO_VS_CSHARP profile={profile} rounds={rounds} corpus={corpus_display} files={}",
        files.len()
    );

    struct Case {
        name: String,
        bytes: Vec<u8>,
        w: usize,
        h: usize,
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
        // 冒烟 + 尺寸 + 暖机：解码头 + 解码一遍。
        let dec = match IcoDecoder::new(Cursor::new(&bytes)) {
            Ok(d) => d,
            Err(e) => {
                skips.push(format!("{name}: 解码头失败 {e:?}"));
                continue;
            }
        };
        if dec.color_type() != kimg_core::color::ColorType::Rgba8 {
            skips.push(format!("{name}: 非 RGBA8 输出"));
            continue;
        }
        let (w, h) = dec.dimensions();
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        if dec.read_image(&mut buf).is_err() {
            skips.push(format!("{name}: 冒烟解码失败"));
            continue;
        }
        cases.push(Case {
            name,
            bytes,
            w: w as usize,
            h: h as usize,
        });
    }
    for s in &skips {
        println!("KIMG_ICO_VS_CSHARP SKIP {s}");
    }
    if cases.is_empty() {
        println!("KIMG_ICO_VS_CSHARP no benchable files");
        return;
    }

    let mut agg: Vec<f64> = Vec::new();
    let mut layout_rgba8 = 0usize;
    let mut total_ms = 0.0;
    for c in cases.iter_mut() {
        let (w, h) = (c.w, c.h);
        let n = w * h * 4;
        let ms = median_ms(
            || {
                let d = IcoDecoder::new(Cursor::new(&c.bytes)).expect("冒烟已保证");
                let mut buf = vec![0u8; n];
                d.read_image(&mut buf).expect("冒烟已保证");
            },
            rounds,
        );
        layout_rgba8 += 1;
        agg.push(ms);
        total_ms += ms;
        println!(
            "KIMG_ICO_VS_CSHARP perfile {} {}x{} layout=rgba8 ours={ms:.3}",
            c.name, w, h
        );
    }
    let g_ours = geomean(&agg);
    println!(
        "KIMG_ICO_VS_CSHARP AGG files={} layout_rgba8={} geomean_ours={g_ours:.3} total_ours_ms={total_ms:.3}",
        cases.len(),
        layout_rgba8
    );
    println!("KIMG_ICO_VS_CSHARP done.");
}
