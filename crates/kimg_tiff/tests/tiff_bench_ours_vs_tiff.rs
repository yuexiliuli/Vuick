//! 性能对账：自研 `TiffDecoder` vs tiff 0.11.3 crate（dev oracle），同一语料同进程交错计时。
//!
//! 语料 = `Benchmarks/corpus/TIFF`（74 个 8bit chunky II 单页：
//! {gray8, rgb8, rgba8} × {none, packbits, lzw, deflate} × {无/horizontal predictor} + 2 多 strip），
//! 可用 env `KIMG_TIFF_CORPUS` 覆盖。全程只读语料、不改任何解码实现、单线程。
//!
//! 计时口径：两侧各自从 `&[u8]` 构造解码器（含 IFD 解析）+ `read_image` 到**自然色缓冲**
//! （L8/RGB8/RGBA8，即 tiff crate `DecodingResult::U8` 与自研 `total_bytes()` 缓冲）——
//! 两侧都**不**做「补 alpha 成 RGBA」的一次性 pad（计时外等价成本，排除）。
//! 每文件「同进程交错」：ours/tiff 各 {warmup=1 + rounds=9}，取各自中位墙钟。
//! 必须 release 跑（`cargo test --release ... -- --ignored --nocapture`）；计时用 `std::time::Instant`。
//!
//! 输出约定（UTF-8，供主 agent 聚合抓取 `Select-String "KIMG_TIFF_VS_TIFF|AGG"`）：
//! - `KIMG_TIFF_VS_TIFF perfile <stem> ours=<ms> tiff=<ms> ratio=<ours/tiff>`
//! - `KIMG_TIFF_VS_TIFF cat <组> n=<N> geomean_ours_tiff=<ratio>`
//! - `KIMG_TIFF_VS_TIFF AGG files=<N> geomean_ours_tiff=<ratio> total_ours=<ms> total_tiff=<ms>`
//!
//! 缺语料 → SKIP（手工装配不入库，SKIP 是常态）。`#[ignore]`：显式跑（release + 静置后）。

use std::io::Cursor;
use std::time::Instant;

use kimg_tiff::ImageDecoder as _;
use kimg_tiff::TiffDecoder;
use tiff::decoder::DecodingResult;

/// 仓库根 = crates/kimg_tiff 的上一级再上一级。
fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// 收集目录下全部 *.tif/*.tiff（文件名字典序）。
fn collect_tiffs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("tif") || x.eq_ignore_ascii_case("tiff"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("NaN"));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn geomean(v: &[f64]) -> f64 {
    debug_assert!(!v.is_empty());
    (v.iter().map(|x| x.ln()).sum::<f64>() / v.len() as f64).exp()
}

const ROUNDS: usize = 9;

/// 自研：new（IFD 解析）+ read_image 到自然色缓冲，返回中位毫秒。
fn bench_ours(data: &[u8]) -> f64 {
    {
        let dec = TiffDecoder::new(Cursor::new(data)).unwrap();
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).unwrap();
        std::hint::black_box(&buf);
    }
    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        let dec = TiffDecoder::new(Cursor::new(data)).unwrap();
        let total = dec.total_bytes();
        let mut buf = vec![0u8; total as usize];
        dec.read_image(&mut buf).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(&buf);
        samples.push(ms);
    }
    median(samples)
}

/// tiff crate：new + read_image（自然色 U8 缓冲，API 自带输出分配），返回中位毫秒。
fn bench_tiff(data: &[u8]) -> f64 {
    {
        let mut d = tiff::decoder::Decoder::new(Cursor::new(data)).unwrap();
        expect_u8(d.read_image().unwrap());
    }
    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        let mut d = tiff::decoder::Decoder::new(Cursor::new(data)).unwrap();
        let img = d.read_image().unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        expect_u8(img);
        samples.push(ms);
    }
    median(samples)
}

fn expect_u8(img: DecodingResult) {
    match img {
        DecodingResult::U8(v) => std::hint::black_box(&v),
        other => panic!("tiff crate 返回非 8bit 结果: {other:?}"),
    };
}

/// 从文件名 stem 解析分类分组键（语料命名约定 `<src>__<layout>_<comp>[_pred][_rpsN].tif`）。
fn classify(stem: &str) -> (String, String, bool) {
    let layout = if stem.contains("rgba8") {
        "rgba"
    } else if stem.contains("gray8") {
        "gray"
    } else if stem.contains("rgb8") {
        "rgb"
    } else {
        "?"
    };
    let comp = if stem.contains("deflate") {
        "deflate"
    } else if stem.contains("packbits") {
        "packbits"
    } else if stem.contains("lzw") {
        "lzw"
    } else if stem.contains("none") {
        "none"
    } else {
        "?"
    };
    (layout.to_string(), comp.to_string(), stem.contains("_pred"))
}

#[test]
#[ignore]
fn bench_ours_vs_tiff_corpus() {
    let root = repo_root();
    let corpus = std::env::var("KIMG_TIFF_CORPUS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| root.join("Benchmarks").join("corpus").join("TIFF"));
    let files = collect_tiffs(&corpus);
    if files.is_empty() {
        eprintln!(
            "[tiff-vs-tiff] SKIP: corpus/TIFF 缺失或为空（先跑 Benchmarks/tiff/scripts/run_gen.ps1）: {}",
            corpus.display()
        );
        return;
    }
    eprintln!(
        "[tiff-vs-tiff] corpus={} files={} rounds={} threads=1 host_cpus={}",
        corpus.display(),
        files.len(),
        ROUNDS,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
    );

    let mut rows: Vec<(String, String, String, bool, f64, f64)> = Vec::with_capacity(files.len());
    for path in &files {
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(path).unwrap();
        let ours = bench_ours(&data);
        let tiff = bench_tiff(&data);
        let ratio = ours / tiff;
        let (layout, comp, pred) = classify(&stem);
        rows.push((stem.clone(), layout, comp, pred, ours, tiff));
        println!(
            "KIMG_TIFF_VS_TIFF perfile {stem} ours={ours:.4} tiff={tiff:.4} ratio={ratio:.4}"
        );
    }

    let n = rows.len();
    let total_ours: f64 = rows.iter().map(|r| r.4).sum();
    let total_tiff: f64 = rows.iter().map(|r| r.5).sum();
    let all_ratios: Vec<f64> = rows.iter().map(|r| r.4 / r.5).collect();
    println!(
        "KIMG_TIFF_VS_TIFF AGG files={n} geomean_ours_tiff={:.4} total_ours={:.2} total_tiff={:.2}",
        geomean(&all_ratios),
        total_ours,
        total_tiff
    );

    // 分类 geomean：layout / comp / pred 三方向。
    let mut group_map: std::collections::BTreeMap<String, Vec<f64>> = std::collections::BTreeMap::new();
    for r in &rows {
        let (_, layout, comp, pred, ours, tiff) = r;
        group_map
            .entry(format!("layout/{layout}"))
            .or_default()
            .push(ours / tiff);
        group_map
            .entry(format!("comp/{comp}"))
            .or_default()
            .push(ours / tiff);
        group_map
            .entry(format!("pred/{}", if *pred { "pred" } else { "nopred" }))
            .or_default()
            .push(ours / tiff);
        group_map
            .entry(format!("mat/{layout}/{comp}/{}", if *pred { "pred" } else { "nopred" }))
            .or_default()
            .push(ours / tiff);
    }
    for (k, v) in &group_map {
        println!(
            "KIMG_TIFF_VS_TIFF cat {k} n={} geomean_ours_tiff={:.4}",
            v.len(),
            geomean(v)
        );
    }
}
