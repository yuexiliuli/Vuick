//! HEIF 解码性能探针（Phase-5 参照优先分账前置，只测量不优化）：
//! - `perf_single_thread_decode`：全链 RGBA（容器 → 逐 tile 解码 → grid 合成 → 色彩转换 → transform）
//!   `decode_rgba` 单线程耗时，逐代表夹具多轮中位。对应 C# `docs/bench/ref_000_heif_quick.log` 的 RGBA_ms。
//! - `perf_stage_no_color`：`decode_yuv`（变换前画布、grid 合成、不含 color）耗时，与 C# stage_ms（per-tile
//!   HEVC，无容器/无合成）对照——Rust 含 tgile 逐 tile + grid 合成，故略高于 C# stage 属预期。
//!
//! 语料：`target/heif/corpus/<file>`；缺失 → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）。
//! 均 `#[ignore]`，仅 release 测量用；debug 跑无意义。
//! 运行：`cargo test -p kimg_heif --release --test oracle_perf perf_ -- --ignored --nocapture --test-threads=1`

use std::path::PathBuf;
use std::time::Instant;

use kimg_heif::{decode_rgba, decode_yuv, read_info};

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/corpus");

/// 代表夹具（从易到难）：语料文件名 + 单轮 >30s 是否可降轮。
/// C# 基线仅含前三（IMG_0083 / fuji / testsrc2 见 ref_000_heif_quick.log）；三个 *_grid 无 C# 基线。
const FIXTURES: &[&str] = &[
    "synth-screen-testsrc2-1280x720-main8.heic",
    "synth-photo-fuji-1600x900-main8.heic",
    "IMG_0083.HEIC",
    "hevc-main-3072x4096-grid.heic",
    "hevc-main10-3072x4096-grid.heic",
    "hevc-main-8192x6144-grid.heic",
];

/// 多轮毫秒取中位（自定序，不动轮序）。
fn median_ms(rounds: &[f64]) -> f64 {
    let mut v = rounds.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// 对单个夹具：warmup 1 轮（不计时，仅测温决定轮次与预热）→ 正常 ≥9 轮（单轮 >30s 降 3 轮）取中位。
/// 返回 (轮次, 各轮 ms, warmup ms)。
fn decode_reps_rgba(data: &[u8], reps: usize) -> Vec<f64> {
    let info = read_info(data).expect("read_info");
    let (w, h) = (info.width as usize, info.height as usize);
    let mut out = vec![0u8; w * h * 4];
    let mut ms: Vec<f64> = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        decode_rgba(data, &mut out).expect("decode_rgba");
        ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    ms
}

fn decode_reps_yuv(data: &[u8], reps: usize) -> Vec<f64> {
    let mut ms: Vec<f64> = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        decode_yuv(data).expect("decode_yuv");
        ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    ms
}

/// 跑一组夹具：warmup 实测时长决定降轮；打印 per-file diagnostics。
fn run_bench(entry: &str, yuv: bool) {
    let mut checked = 0usize;
    let mut skipped = 0usize;
    for file in FIXTURES {
        let corpus = PathBuf::from(CORPUS_DIR).join(file);
        if !corpus.exists() {
            println!("SKIP: missing {corpus:?}");
            skipped += 1;
            continue;
        }
        let data = std::fs::read(&corpus).expect("read corpus");

        // warmup 1 轮：只测温，决定轮次（>30s 降 3 轮）。
        let tw = Instant::now();
        if yuv {
            decode_yuv(&data).expect("warmup decode_yuv");
        } else {
            let info = read_info(&data).expect("warmup read_info");
            let (w, h) = (info.width as usize, info.height as usize);
            let mut out = vec![0u8; w * h * 4];
            decode_rgba(&data, &mut out).expect("warmup decode_rgba");
        }
        let warmup_ms = tw.elapsed().as_secs_f64() * 1e3;
        let reps = if warmup_ms > 30_000.0 { 3 } else { 9 };

        let rounds = if yuv {
            decode_reps_yuv(&data, reps)
        } else {
            decode_reps_rgba(&data, reps)
        };
        let med = median_ms(&rounds);
        let min = rounds.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = rounds.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let tiles = kimg_heif::codecs::heif::container::HeifContainer::parse(&data)
            .map(|c| c.tiles.len())
            .unwrap_or(0);
        println!(
            "perf {entry} {file}: tiles={tiles} reps={reps} median={med:.1} ms (min={min:.1} max={max:.1}, warmup={warmup_ms:.1})",
        );
        checked += 1;
    }
    println!("oracle_perf {entry}: checked={checked} skipped={skipped}");
}

/// 全链 RGBA 单线程耗时（对应 C# RGBA_ms 全链）。
#[test]
#[ignore]
fn perf_single_thread_decode() {
    run_bench("RGBA", false);
}

/// stage 对照：`decode_yuv`（变换前、grid 合成、不含 color；对应 C# stage_ms 但多合成）。
#[test]
#[ignore]
fn perf_stage_no_color() {
    run_bench("STAGE", true);
}

/// 防呆：release 仅用于测量，勿在 CI debug 下误跑（`#[ignore]` 本就不跑，此处声明意图占位）。
#[test]
fn note_release_only_measurement() {
    println!("oracle_perf: #[ignore] probes are release-only measurement; only run via --ignored --nocapture");
}
