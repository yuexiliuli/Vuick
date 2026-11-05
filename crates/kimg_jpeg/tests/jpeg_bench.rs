//! JPEG 移植基准（release、可复跑）：旧 zune-jpeg 路径 vs 新自研引擎，各解码 N 轮取中位。
//!
//! 运行（`#[ignore]`，不影响常规套件）：
//! ```text
//! cargo test -p kimg_jpeg --release --test jpeg_bench -- --ignored --nocapture
//! ```
//! 语料：`<workspace>/target/测试图片/jpeg`（env `KIMG_JPEG_CORPUS` 可覆盖）。
//! 输出 `JPEG_BENCH FILE <name> old=..ms new=..ms ratio=..` 与 `JPEG_BENCH AGG ...`
//! （ratio = new ÷ old，new 更快则 <1）。
//!
//! 口径说明（对标 C# Sdcb.SimdCodecs `JpegDecoder.DecodeTo(data, rgba)`）：
//! - old = zune-jpeg oracle（== image 0.25 用后端，即被替换前的旧行为；标量默认路径）。
//! - new = kimg_jpeg 自研引擎 `codecs::jpeg::JpegDecoder`（AVX2/标量双路径）。
//! - 姿态 = 复用调用方已分配缓冲（暖态零分配 direct 姿态），输出到解码器声明的 color_type。

mod fixtures;

use std::io::Cursor;
use std::time::Instant;

use kimg_jpeg::codecs::jpeg::JpegDecoder as NewJpeg;
use kimg_core::ImageDecoder as _;

use fixtures::*;

/// 读文件并取得新路径输出缓冲尺寸（仅解码头，快）。
fn load(path: &std::path::Path) -> Option<(String, Vec<u8>, usize)> {
    let bytes = std::fs::read(path).ok()?;
    let dec = NewJpeg::new(Cursor::new(bytes.as_slice())).ok()?;
    let n = dec.total_bytes() as usize;
    Some((
        path.file_name()?.to_string_lossy().into_owned(),
        bytes,
        n,
    ))
}

// ---------------------------------------------------------------------------
// 旧路径（zune-jpeg oracle）
// ---------------------------------------------------------------------------

fn old_decode(bytes: &[u8], out: &mut [u8]) -> Result<(), String> {
    let (_, _, _, pix) = zune_decode(bytes)?;
    if pix.len() != out.len() {
        return Err(format!("长度不一致 oracle={} out={}", pix.len(), out.len()));
    }
    out.copy_from_slice(&pix);
    Ok(())
}

// ---------------------------------------------------------------------------
// 新路径（自研引擎）
// ---------------------------------------------------------------------------

fn new_decode(bytes: &[u8], out: &mut [u8]) -> Result<(), String> {
    NewJpeg::new(Cursor::new(bytes))
        .map_err(|e| e.to_string())?
        .read_image(out)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 计时与主流程
// ---------------------------------------------------------------------------

fn median_ms<F: FnMut()>(mut f: F, rounds: usize) -> f64 {
    f(); // 暖机
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

/// `#[ignore]` 基准入口：`cargo test -p kimg_jpeg --release --test jpeg_bench -- --ignored --nocapture`。
#[test]
#[ignore]
fn bench_main() {
    let rounds: usize = std::env::var("KIMG_JPEG_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("JPEG_BENCH corpus dir missing: {}", dir.display());
        return;
    }
    let files = collect_jpegs(&dir);
    if files.is_empty() {
        println!("JPEG_BENCH corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!("JPEG_BENCH profile={profile} rounds={rounds} files={}", files.len());

    // 冒烟：每个文件两路径先解一次；失败/输出不一致 → 跳过（不回比不可信数）。
    let mut bench: Vec<(String, Vec<u8>, usize)> = Vec::new();
    let mut old_out: Vec<Vec<u8>> = Vec::new();
    for p in &files {
        let Some(loaded) = load(p) else {
            let nm = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            println!("JPEG_BENCH SMOKE {nm}: 解码头失败，跳过");
            continue;
        };
        let (name, bytes, n) = loaded;
        let mut out = vec![0u8; n];
        let ok_old = old_decode(&bytes, &mut out).is_ok();
        let mut out2 = vec![0u8; n];
        let ok_new = new_decode(&bytes, &mut out2).is_ok();
        if !ok_old || !ok_new {
            println!("JPEG_BENCH SMOKE {name}: old={ok_old} new={ok_new} 未全过，跳过");
            continue;
        }
        if out_to_length_mismatch(&out, &out2) {
            println!("JPEG_BENCH SMOKE {name}: old/new 输出字节数不一致，跳过");
            continue;
        }
        bench.push((name, bytes, n));
        old_out.push(out);
    }
    if bench.is_empty() {
        println!("JPEG_BENCH no benchable files after smoke");
        return;
    }

    // 阶段探针模式：`KIMG_JPEG_STAGE_PROFILE=1` 时跑分布表（warmup + 3 轮），
    // 不做 A/B；decode 内部每轮打印一行 `STAGE-DECODE ...`（env 门控，见 jpeg/mcu.rs）。
    // `KIMG_JPEG_STAGE_FILTER=<子串>` 可只跑名字含该子串的文件。
    if std::env::var("KIMG_JPEG_STAGE_PROFILE").as_deref() == Ok("1") {
        let filter = std::env::var("KIMG_JPEG_STAGE_FILTER").unwrap_or_default();
        for (name, bytes, n) in bench.iter().filter(|(n, _, _)| {
            filter.is_empty() || n.contains(&filter)
        }) {
            let dims = NewJpeg::new(Cursor::new(bytes.as_slice()))
                .map(|d| {
                    let (w, h) = d.dimensions();
                    format!("{w}x{h}")
                })
                .unwrap_or_else(|_| "(dims?)".into());
            println!("STAGE FILE {name} dims={dims}");
            let mut buf = vec![0u8; *n];
            let t0 = Instant::now();
            let dec = NewJpeg::new(Cursor::new(bytes.as_slice())).expect("冒烟已保证可解");
            let t1 = Instant::now();
            let _ = dec.read_image(&mut buf).expect("冒烟已保证可解");
            let t2 = Instant::now();
            println!(
                "STAGE WARMUP wall new={:.3}ms read={:.3}ms",
                (t1 - t0).as_secs_f64() * 1000.0,
                (t2 - t1).as_secs_f64() * 1000.0
            );
            for _ in 0..3 {
                let mut buf = vec![0u8; *n];
                let t0 = Instant::now();
                let dec = NewJpeg::new(Cursor::new(bytes.as_slice())).expect("冒烟已保证可解");
                let t1 = Instant::now();
                let _ = dec.read_image(&mut buf).expect("冒烟已保证可解");
                let t2 = Instant::now();
                println!(
                    "STAGE ROUND wall new={:.3}ms read={:.3}ms",
                    (t1 - t0).as_secs_f64() * 1000.0,
                    (t2 - t1).as_secs_f64() * 1000.0
                );
            }
        }
        println!("STAGE done.");
        return;
    }

    // A/B 计时：old 与 new 各预分配一次缓冲复用（暖态零分配 direct 姿态）。
    let mut ratios: Vec<f64> = Vec::new();
    let mut agg_old: Vec<f64> = Vec::new();
    let mut agg_new: Vec<f64> = Vec::new();
    for (i, (name, bytes, n)) in bench.iter().enumerate() {
        let mut new_buf = vec![0u8; *n];
        let t_old = median_ms(
            || {
                let _ = old_decode(bytes, &mut old_out[i]).expect("冒烟已保证可解");
            },
            rounds,
        );
        let t_new = median_ms(
            || {
                let _ = new_decode(bytes, &mut new_buf).expect("冒烟已保证可解");
            },
            rounds,
        );
        let ratio = t_new / t_old;
        ratios.push(ratio);
        agg_old.push(t_old);
        agg_new.push(t_new);
        println!(
            "JPEG_BENCH FILE {name} old={t_old:.3}ms new={t_new:.3}ms ratio={ratio:.3} rounds={rounds}"
        );
    }
    let g_old = geomean(&agg_old);
    let g_new = geomean(&agg_new);
    println!(
        "JPEG_BENCH AGG files={} geomean_old={g_old:.3}ms geomean_new={g_new:.3}ms geomean_ratio={:.3}",
        bench.len(),
        g_new / g_old
    );
    println!("JPEG_BENCH done.");
}

fn out_to_length_mismatch(a: &[u8], b: &[u8]) -> bool {
    a.len() != b.len()
}
