//! HEIF 解码性能探针（Phase-5 参照优先分账前置，只测量不优化）：
//! - `perf_single_thread_decode`：全链 RGBA（容器 → 逐 tile 解码 → grid 合成 → 色彩转换 → transform）
//!   `decode_rgba` 单线程耗时，逐代表夹具多轮中位。对应 C# `docs/bench/ref_000_heif_quick.log` 的 RGBA_ms。
//! - `perf_stage_no_color`：`decode_yuv`（变换前画布、grid 合成、不含 color）耗时，与 C# stage_ms（per-tile
//!   HEVC，无容器/无合成）对照——Rust 含 tgile 逐 tile + grid 合成，故略高于 C# stage 属预期。
//!
//! 语料：`Benchmarks/corpus/heif/<file>`；缺失 → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）。
//! 均 `#[ignore]`，仅 release 测量用；debug 跑无意义。
//! 运行：`cargo test -p kimg_heif --release --test oracle_perf perf_ -- --ignored --nocapture --test-threads=1`

use std::path::PathBuf;
use std::time::Instant;

use kimg_heif::codecs::heif::decode_yuv_par;
use kimg_heif::{decode_rgba, decode_yuv, read_info};

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif");

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

/// grid 多 tile 并行加速比探针（release only）：对 IMG_0083（48 tile）与
/// hevc-main-8192x6144-grid（192 tile），同窗交错量「串行 decode_yuv vs 并行
/// decode_yuv_par」各 ≥5-7 轮取中位，报加速比；`decode_rgba`（全链，已并行）作产品真机口
/// 参考（3 轮中位）。运行：
/// `cargo build --release -p kimg_heif --tests` 后
/// `cargo test -p kimg_heif --release --test oracle_perf perf_tile_parallel_speedup -- --ignored --nocapture --test-threads=1`
#[test]
#[ignore]
fn perf_tile_parallel_speedup() {
    const GRID_FIXTURES: &[&str] = &["IMG_0083.HEIC", "hevc-main-8192x6144-grid.heic"];
    let mut checked = 0usize;
    let mut skipped = 0usize;
    for file in GRID_FIXTURES {
        let corpus = PathBuf::from(CORPUS_DIR).join(file);
        if !corpus.exists() {
            println!("SKIP: missing {corpus:?}");
            skipped += 1;
            continue;
        }
        let data = std::fs::read(&corpus).expect("read corpus");
        let tiles = kimg_heif::codecs::heif::container::HeifContainer::parse(&data)
            .map(|c| c.tiles.len())
            .unwrap_or(0);

        // warmup 1 轮（串行）测温定轮次（>30s 降 5 轮，否则 7 轮）。
        let tw = Instant::now();
        decode_yuv(&data).expect("warmup decode_yuv");
        let warmup_ms = tw.elapsed().as_secs_f64() * 1e3;
        let reps = if warmup_ms > 30_000.0 { 5 } else { 7 };

        // 串/并交错（同窗交替序抵消漂移取中位，只比相对值）。
        let mut serial_ms = Vec::with_capacity(reps);
        let mut par_ms = Vec::with_capacity(reps);
        for round in 0..reps {
            if round % 2 == 0 {
                let t = Instant::now();
                decode_yuv(&data).expect("decode_yuv");
                serial_ms.push(t.elapsed().as_secs_f64() * 1e3);
                let t = Instant::now();
                decode_yuv_par(&data).expect("decode_yuv_par");
                par_ms.push(t.elapsed().as_secs_f64() * 1e3);
            } else {
                let t = Instant::now();
                decode_yuv_par(&data).expect("decode_yuv_par");
                par_ms.push(t.elapsed().as_secs_f64() * 1e3);
                let t = Instant::now();
                decode_yuv(&data).expect("decode_yuv");
                serial_ms.push(t.elapsed().as_secs_f64() * 1e3);
            }
        }
        let serial_med = median_ms(&serial_ms);
        let par_med = median_ms(&par_ms);
        let speedup = serial_med / par_med;

        // 产品真机口（已并行）参考：3 轮中位。
        let info = read_info(&data).expect("read_info");
        let (w, h) = (info.width as usize, info.height as usize);
        let mut out = vec![0u8; w * h * 4];
        let mut rgba_ms = Vec::with_capacity(3);
        for _ in 0..3 {
            let t = Instant::now();
            decode_rgba(&data, &mut out).expect("decode_rgba");
            rgba_ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let rgba_med = median_ms(&rgba_ms);

        println!(
            "perf TILE-PAR {file}: tiles={tiles} reps={reps} serial={serial_med:.1} ms par={par_med:.1} ms speedup={speedup:.2}x decode_rgba(全链,并)={rgba_med:.1} ms (warmup={warmup_ms:.1})"
        );
        checked += 1;
    }
    println!("oracle_perf TILE-PAR: checked={checked} skipped={skipped}");
}

/// 终测表（K96 参照优先分账收口）：遍历 corpus 全部 .heic（名字序），每文件 warmup 1 +
/// `decode_rgba` reps=9 取中位，打印 tiles 与（对 17+3 参照表文件的）Rust/C# 比值；profile-4
/// 解码 Err → 打印 `ERR <file>` 后跳过（不 fail）。最后输出四组 geomean：17 文件(14 手机+3 单
/// tile)、20 文件(17+3 grid 参考)、14 手机、3 单 tile（单 tile=串行等效）。
/// 运行：`cargo build --release -p kimg_heif --tests` 后
/// `target\release\deps\oracle_perf-*.exe --exact perf_acceptance_geo --ignored --nocapture --test-threads=1`
#[test]
#[ignore]
fn perf_acceptance_geo() {
    const REPS: usize = 9;
    /// 参照：stem（大小写不敏感匹配）+ C# 单核 RGBA_ms（RGBA_ms 全链，quick 同窗 reps9 中位）。
    /// 前 14 = 手机 48-tile grid；中 3 = 单 tile main8；后 3 grid = 非过滤 quick 同窗参考。
    const REF_CS: &[(&str, f64, &str)] = &[
        ("IMG_0083", 256.6, "phone"),
        ("IMG_0084", 320.7, "phone"),
        ("IMG_0085", 377.1, "phone"),
        ("IMG_0086", 464.6, "phone"),
        ("IMG_0087", 393.7, "phone"),
        ("IMG_2645", 323.1, "phone"),
        ("IMG_4512", 633.1, "phone"),
        ("IMG_4513", 198.1, "phone"),
        ("IMG_4608", 374.8, "phone"),
        ("IMG_4609", 364.2, "phone"),
        ("IMG_4610", 372.4, "phone"),
        ("IMG_4611", 355.1, "phone"),
        ("IMG_4612", 301.0, "phone"),
        ("IMG_4613", 301.1, "phone"),
        ("synth-photo-fuji-1600x900-main8", 32.1, "single"),
        ("synth-photo-mangrove-1599x1199-main8", 53.3, "single"),
        ("synth-screen-testsrc2-1280x720-main8", 9.4, "single"),
        ("hevc-main-3072x4096-grid", 386.3, "grid"),
        ("hevc-main-8192x6144-grid", 1613.4, "grid"),
        ("hevc-main10-3072x4096-grid", 538.9, "grid"),
    ];

    let mut ratios: Vec<(&str, f64, f64, f64, &str)> = Vec::new(); // (name, rust, cs, ratio, cat)
    let mut checked = 0usize;
    let skipped = 0usize;
    let mut errs = 0usize;

    let mut paths = std::fs::read_dir(CORPUS_DIR)
        .expect("read corpus dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("heic"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let stem = path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                println!("ERR {name} (read: {e})");
                errs += 1;
                continue;
            }
        };
        let info = match read_info(&data) {
            Ok(i) => i,
            Err(e) => {
                println!("ERR {name} (read_info: {e})");
                errs += 1;
                continue;
            }
        };
        let (w, h) = (info.width as usize, info.height as usize);
        let mut out = vec![0u8; w * h * 4];
        let tiles = kimg_heif::codecs::heif::container::HeifContainer::parse(&data)
            .map(|c| c.tiles.len())
            .unwrap_or(0);

        if let Err(e) = decode_rgba(&data, &mut out) {
            println!("ERR {name} (decode: {e})");
            errs += 1;
            continue;
        }
        let mut rounds = Vec::with_capacity(REPS);
        let mut failed = false;
        for _ in 0..REPS {
            let t = Instant::now();
            if let Err(e) = decode_rgba(&data, &mut out) {
                println!("ERR {name} (decode round: {e})");
                errs += 1;
                failed = true;
                break;
            }
            rounds.push(t.elapsed().as_secs_f64() * 1e3);
        }
        if failed {
            continue;
        }
        let med = median_ms(&rounds);

        let cs = REF_CS.iter().find(|(k, _, _)| k.eq_ignore_ascii_case(&stem));
        match cs {
            Some((key, cs_ms, cat)) => {
                let ratio = med / cs_ms;
                ratios.push((key, med, *cs_ms, ratio, cat));
                println!(
                    "GEO {name}: tiles={tiles} rust={med:.1} ms cs={cs_ms:.1} ms ratio={ratio:.3} [{cat}]"
                );
            }
            None => {
                println!("GEO {name}: tiles={tiles} rust={med:.1} ms (无 C# 参照)");
            }
        }
        checked += 1;
    }

    let geomean = |xs: &[f64]| -> f64 {
        let n = xs.len();
        if n == 0 {
            return f64::NAN;
        }
        (xs.iter().map(|x| x.ln()).sum::<f64>() / n as f64).exp()
    };
    let emit = |label: &str, group: &[&(&str, f64, f64, f64, &str)]| {
        let xs: Vec<f64> = group.iter().map(|(_, _, _, r, _)| *r).collect();
        println!(
            "GEO-SUM {label}: n={} geomean(ratio)={:.3}",
            group.len(),
            geomean(&xs)
        );
    };

    let phone: Vec<_> = ratios.iter().filter(|r| r.4 == "phone").collect();
    let single: Vec<_> = ratios.iter().filter(|r| r.4 == "single").collect();
    let grid: Vec<_> = ratios.iter().filter(|r| r.4 == "grid").collect();
    emit("17files(phone+single)", &phone.iter().chain(single.iter()).copied().collect::<Vec<_>>());
    let all20: Vec<_> = ratios.iter().collect();
    emit("20files(+grid ref)", &all20);
    emit("14 phone", &phone);
    emit("3 single", &single);
    let _ = grid;
    println!(
        "oracle_perf perf_acceptance_geo: checked={checked} skipped={skipped} errs={errs}"
    );
}

/// 防呆：release 仅用于测量，勿在 CI debug 下误跑（`#[ignore]` 本就不跑，此处声明意图占位）。
#[test]
fn note_release_only_measurement() {
    println!("oracle_perf: #[ignore] probes are release-only measurement; only run via --ignored --nocapture");
}
