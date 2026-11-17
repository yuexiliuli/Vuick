//! GIF/JPEG/BMP 解码 release 基准（cargo bench 手动运行）：kimg 三通道生产姿态基线，
//! 供后续自研解码优化前后对比。JPEG 的 turbo 参照通道（libjpeg-turbo-rs）已移除，
//! 该列固定输出 N/A。
//!
//! 运行（release 默认，harness=false 直接执行）：
//! ```text
//! # 仅 kimg 三通道基线（JPEG turbo 列输出 N/A）
//! cargo bench -p imgbench --bench gif_jpeg_bmp_release_bench
//! ```
//!
//! 输出 `GIF_JPEG_BMP_RELEASE_BENCH <fmt> files=N ...`（各场景 N≥9 轮取中位，release；
//! 轮数可用 `GIF_JPEG_BMP_RELEASE_BENCH_ROUNDS` 覆盖，第一轮为暖机不计入）。
//! note workspace `[profile.release]` 为 `opt-level="z"`（体积向）；如需速度向可比口径，
//! 加 `--config 'profile.release.opt-level="3"'` 重新运行（会重建依赖）。
//!
//! 语料：`target/imgbench-corpus/{BMP,JPEG,GIF}`（gitignored；由
//! `E:\Rust\SimdCodecs\测试图片\{BMP,JPEG,GIF}` 快照，BMP 6×10-26MB / JPEG 19×300-570KB /
//! GIF 7×7-9MB 动画）。扩展名过滤：BMP→.bmp、JPEG→.jpg/.jpeg、GIF→.gif。
//!
//! 口径（一律输出 RGBA8 字节数）：
//! - BMP/JPEG 静态：`kimg::ImageReader::with_format(...).decode()?.to_rgba8()`（生产姿态）；
//!   正确性哨兵校验输出字节数 == 宽*高*4，不符视为探测失败。
//! - GIF 双列：GIF_STATIC = `ImageReader…Gif…to_rgba8` 首帧（生产 static 姿态）；
//!   GIF_ANIM = `kimg::codecs::gif::GifDecoder…into_frames().collect_frames()` 帧缓冲字节总和。
//!   装载时逐文件对启用通道各解一次，解不了则 drop + note（防基准中途 panic）。

use std::io::Cursor;
use std::time::Instant;

use kimg::AnimationDecoder as _;

const ROUNDS: usize = 9;

// ---------------------------------------------------------------------------
// 场景与语料
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scenario {
    Bmp,
    Jpeg,
    Gif,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::Bmp => "Bmp",
            Scenario::Jpeg => "Jpeg",
            Scenario::Gif => "Gif",
        }
    }
    /// 语料目录名（target/imgbench-corpus/<dir>）。
    fn corpus_dir(self) -> &'static str {
        match self {
            Scenario::Bmp => "BMP",
            Scenario::Jpeg => "JPEG",
            Scenario::Gif => "GIF",
        }
    }
    /// 允许的扩展名（小写；JPEG 同时收 .jpg 与 .jpeg）。
    fn extensions(self) -> &'static [&'static str] {
        match self {
            Scenario::Bmp => &["bmp"],
            Scenario::Jpeg => &["jpg", "jpeg"],
            Scenario::Gif => &["gif"],
        }
    }
}

static SCENARIOS: [Scenario; 3] = [Scenario::Bmp, Scenario::Jpeg, Scenario::Gif];

fn collect_dir(dir: &std::path::Path, exts: &[&str], out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|x| exts.contains(&x.to_string_lossy().as_ref()))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    for p in paths {
        if let Ok(b) = std::fs::read(&p) {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push((name, b));
        }
    }
}

/// 装载 + 探测：每文件对启用通道各解一次，任一失败则 drop + note（防止基准中途 panic）。
fn collect_corpus() -> Vec<(Scenario, String, Vec<u8>)> {
    let mut out = Vec::new();
    for scen in SCENARIOS {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join(scen.corpus_dir());
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        collect_dir(&dir, scen.extensions(), &mut files);
        if files.is_empty() {
            println!(
                "GIF_JPEG_BMP_RELEASE_BENCH {} SKIP (target/imgbench-corpus/{} 缺失或为空)",
                scen.name(),
                scen.corpus_dir()
            );
            continue;
        }
        for (name, bytes) in files {
            let kimg_ok = kimg_decode(scen, &bytes).is_some();
            // 恒真死变量（dormancy 休眠保留，K24）：turbo 侧探测已移除，`!(kimg_ok && turbo_ok)`
            // 等效 `!kimg_ok`；保留占位避免改动门控语义，仅日志里仍显式输出两个字段。
            let turbo_ok = true;
            if !(kimg_ok && turbo_ok) {
                println!(
                    "GIF_JPEG_BMP_RELEASE_BENCH note drop {name} (解码探测失败: kimg={kimg_ok} turbo={turbo_ok})"
                );
                continue;
            }
            out.push((scen, name, bytes));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 解码通道（均输出 RGBA8 字节数或帧字节和；解不了返回 None）
// ---------------------------------------------------------------------------

/// kimg 通道（生产姿态）：BMP/JPEG 走 ImageReader→to_rgba8；
/// GIF 双姿态——首帧 to_rgba8（GIF_STATIC）与整段帧缓冲和（GIF_ANIM）。
/// 返回 (static_bytes, anim_bytes)：GIF 双数，其余 static 为 RGBA8 字节数、anim 为 None。
fn kimg_decode(scen: Scenario, bytes: &[u8]) -> Option<(usize, Option<usize>)> {
    let format = match scen {
        Scenario::Bmp => kimg::ImageFormat::Bmp,
        Scenario::Jpeg => kimg::ImageFormat::Jpeg,
        Scenario::Gif => kimg::ImageFormat::Gif,
    };
    let img = kimg::ImageReader::with_format(Cursor::new(bytes), format)
        .decode()
        .ok()?;
    let rgba = img.to_rgba8();
    // 正确性哨兵：输出字节数与 宽*高*4 匹配。
    let expected = (rgba.width() as usize).saturating_mul(rgba.height() as usize).saturating_mul(4);
    let static_len = rgba.into_raw().len();
    if static_len != expected {
        return None;
    }
    let anim_bytes = if scen == Scenario::Gif {
        let dec = kimg::codecs::gif::GifDecoder::new(Cursor::new(bytes)).ok()?;
        let frames = dec.into_frames().collect_frames().ok()?;
        Some(frames.iter().map(|f| f.buffer().len()).sum())
    } else {
        None
    };
    Some((static_len, anim_bytes))
}

/// turbo 参照通道已移除：此处保留签名与输出列，恒返回 N/A 占位。
fn turbo_version() -> &'static str {
    "n/a (libjpeg-turbo-rs 参照通道已移除)"
}

// ---------------------------------------------------------------------------
// 计时与基准主流程
// ---------------------------------------------------------------------------

/// 逐轮计时并取中位（先暖机一轮）。
fn median_ms<F: Fn()>(f: F, rounds: usize) -> f64 {
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

/// 轮数：默认 ROUNDS；可用 `GIF_JPEG_BMP_RELEASE_BENCH_ROUNDS` 覆盖。
fn rounds() -> usize {
    std::env::var("GIF_JPEG_BMP_RELEASE_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS)
}

fn bench_scenario(
    scen: Scenario,
    files: &[&(Scenario, String, Vec<u8>)],
    rounds: usize,
    turbo_on: bool,
) {
    match scen {
        Scenario::Gif => {
            let static_ms = median_ms(
                || {
                    for (_, _, b) in files {
                        let _ = kimg_decode(scen, b).expect("探测已保证可解");
                    }
                },
                rounds,
            );
            let anim_ms = median_ms(
                || {
                    for (_, _, b) in files {
                        let (_, anim) = kimg_decode(scen, b).expect("探测已保证可解");
                        let _ = anim.expect("GIF 必返回 anim 数");
                    }
                },
                rounds,
            );
            println!(
                "GIF_JPEG_BMP_RELEASE_BENCH Gif files={} kimg_static={:.3}ms kimg_anim={:.3}ms ratio_anim_vs_static={:.3}",
                files.len(),
                static_ms,
                anim_ms,
                anim_ms / static_ms
            );
        }
        Scenario::Bmp => {
            let kimg_ms = median_ms(
                || {
                    for (_, _, b) in files {
                        let _ = kimg_decode(scen, b).expect("探测已保证可解");
                    }
                },
                rounds,
            );
            println!(
                "GIF_JPEG_BMP_RELEASE_BENCH Bmp files={} kimg={:.3}ms turbo=N/A (BMP 无 turbo 对照)",
                files.len(),
                kimg_ms
            );
        }
        Scenario::Jpeg => {
            let kimg_ms = median_ms(
                || {
                    for (_, _, b) in files {
                        let _ = kimg_decode(scen, b).expect("探测已保证可解");
                    }
                },
                rounds,
            );
            let _ = turbo_on;
            println!(
                "GIF_JPEG_BMP_RELEASE_BENCH Jpeg files={} kimg={:.3}ms turbo=N/A (libjpeg-turbo-rs 参照通道已移除)",
                files.len(),
                kimg_ms
            );
        }
    }
}

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let rounds = rounds();
    println!(
        "GIF_JPEG_BMP_RELEASE_BENCH profile={profile} rounds={rounds} turbo={}",
        turbo_version()
    );

    let corpus = collect_corpus();
    for scen in SCENARIOS {
        let files: Vec<&(Scenario, String, Vec<u8>)> = corpus
            .iter()
            .filter(|(s, _, _)| *s == scen)
            .collect();
        if files.is_empty() {
            continue;
        }
        bench_scenario(scen, &files, rounds, false);
    }
    println!(
        "GIF_JPEG_BMP_RELEASE_BENCH note corpus_size={} bench_files={}",
        corpus.len(),
        corpus.len()
    );
    eprintln!("[gif_jpeg_bmp_release_bench] done");
}
