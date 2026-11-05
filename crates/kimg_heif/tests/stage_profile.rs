//! HEIF 阶段分账探针（K96 参照优先分账第一阶段，只测量不优化）。
//!
//! `perf_stage_profile`：在 `decode_rgba` 全链（容器 → 逐 tile HEVC 解码 → grid 合成 → 色转 →
//! transform）上，读 `stage_clock`（`feature="stage_profile"` 门控，默认关闭）的各阶段累计粗钟，
//! 逐代表夹具多轮中位，输出各阶段 ms 与占全链 %。阶段对齐 C# `quick --ablate` 内插口径：
//! parse=容器+切片+CABAC+残差语法（等效 parse-only）、intra、residual、deblock、sao、grid、color、transform。
//!
//! 语料：`target/heif/corpus/<file>`；缺失 → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）。
//! 均 `#[ignore]`，仅 release 测量用；debug 跑无意义，也不加 feature 跑无意义（stage_clock 全 0）。
//! 运行：`cargo test -p kimg_heif --release --features stage_profile --test stage_profile perf_ -- --ignored --nocapture --test-threads=1`
//!（长命令前台阻塞到结束，禁止后台化。）

use std::path::PathBuf;
use std::time::Instant;

use kimg_heif::codecs::heif::stage_clock;
use kimg_heif::{decode_rgba, read_info};

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/corpus");

/// 代表夹具（K96 对齐 C# ablate 主行）：main8 单 tile ×2、48-tile 照片、main8/main10 grid。
const FIXTURES: &[&str] = &[
    "synth-screen-testsrc2-1280x720-main8.heic",
    "synth-photo-fuji-1600x900-main8.heic",
    "IMG_0083.HEIC",
    "hevc-main-3072x4096-grid.heic",
    "hevc-main10-3072x4096-grid.heic",
];

/// 多轮毫秒取中位（自定序，不动轮序）。
fn median_ms(rounds: &[f64]) -> f64 {
    let mut v = rounds.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// 对一个夹具：warmup 1 轮（不计时，只测温决定轮次）→ ≥9 轮（单轮 >30s 降 3 轮）取中位。
/// 每轮：`stage_clock::reset()` → 全链 `decode_rgba` 真墙钟 + `stage_clock::snapshot()`（各阶段 ms）。
/// 返回 (reps, full_ms, [stage_ms; 8] 的中位)。
fn profile_fixture(data: &[u8], out: &mut [u8]) -> (usize, f64, [f64; 8]) {
    // warmup 1：只测温，决定轮次（>30s 降 3 轮）。
    stage_clock::reset();
    let tw = Instant::now();
    decode_rgba(data, out).expect("warmup decode_rgba");
    let warmup_ms = tw.elapsed().as_secs_f64() * 1e3;
    let reps = if warmup_ms > 30_000.0 { 3 } else { 9 };

    let mut walls = Vec::with_capacity(reps);
    let mut stages: Vec<[f64; 8]> = Vec::with_capacity(reps);
    for _ in 0..reps {
        stage_clock::reset();
        let t = Instant::now();
        decode_rgba(data, out).expect("decode_rgba");
        walls.push(t.elapsed().as_secs_f64() * 1e3);
        stages.push(stage_clock::snapshot());
    }
    let full = median_ms(&walls);
    let mut med = [0.0f64; 8];
    for i in 0..8 {
        let col: Vec<f64> = stages.iter().map(|s| s[i]).collect();
        med[i] = median_ms(&col);
    }
    (reps, full, med)
}

/// 全链 RGBA 各阶段累计粗钟分账（对应 C# ablate 的内插阶段 share）。
#[test]
#[ignore]
fn perf_stage_profile() {
    let mut checked = 0usize;
    let mut skipped = 0usize;
    println!(
        "perf_stage_profile: stages={:?}",
        stage_clock::STAGE_NAMES
    );
    for file in FIXTURES {
        let corpus = PathBuf::from(CORPUS_DIR).join(file);
        if !corpus.exists() {
            println!("SKIP: missing {corpus:?}");
            skipped += 1;
            continue;
        }
        let data = std::fs::read(&corpus).expect("read corpus");
        let info = read_info(&data).expect("read_info");
        let (w, h) = (info.width as usize, info.height as usize);
        let mut out = vec![0u8; w * h * 4];
        let tiles = info.tile_count;
        let (reps, full, med) = profile_fixture(&data, &mut out);

        let sum: f64 = med.iter().sum();
        println!("perf_stage_profile {file}: tiles={tiles} reps={reps} full={full:.1}ms (stageΣ={sum:.1}ms, 未meas={:.1}ms)",
            (full - sum).max(0.0));
        for (i, name) in stage_clock::STAGE_NAMES.iter().enumerate() {
            let m = med[i];
            let p = m / full * 100.0;
            println!("  {name:<9} {m:>8.2} ms  {p:>5.1}%");
        }
        checked += 1;
    }
    println!("perf_stage_profile: checked={checked} skipped={skipped}");
}
