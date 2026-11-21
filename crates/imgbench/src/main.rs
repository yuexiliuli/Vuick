//! 图像解码性能基准（imgbench）
//!
//! 用途：为「用 kimg 替换 image 依赖」采集前后可对比的解码墙钟基线。
//! 方法学（遵循 AGENTS.md K53）：debug 构建、真实图语料、对比只看相对变化不看绝对值。
//!
//! 用法：
//!   cargo run -p imgbench [corpus_root...]
//! 默认语料：仓库内快照 `target/imgbench-corpus`（从 E:\Rust\SimdCodecs\测试图片 固化的
//! 真实图集：JPEG/PNG/BMP/ICO/GIF/WebP，外部目录可能被同步进程改动，容器内不依赖它）。
//! 按扩展名自动归类，逐格式输出：文件数 / 总计 / 中位 / 均值 / 最小 / 最大（ms）。
//! 输出行带 `BENCH <fmt> ...` 便于 grep 归档。
#![allow(dead_code)]

use kimg::codecs::gif::GifDecoder;
use kimg::AnimationDecoder;
use kimg::ImageReader;
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

// 旧默认语料：外部易变目录（同步进程可能删除/重排，不可依赖）。
// const DEFAULT_CORPUS: &[&str] = &[
//     r"E:\Rust\SimdCodecs\测试图片\imagefolder",
//     r"E:\Rust\SimdCodecs\测试图片\GIF",
//     r"E:\Rust\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures",
// ];

// 仓库内快照：CARGO_MANIFEST_DIR=crates/imgbench，上溯两级到 workspace 根，进 target/。
const DEFAULT_CORPUS: &[&str] =
    &[concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/imgbench-corpus")];

fn ext_of(p: &Path) -> Option<String> {
    p.extension()
        .and_then(OsStr::to_str)
        .map(|s| s.to_ascii_lowercase())
}

fn stat_line(ext: &str, ms: &[f64]) {
    if ms.is_empty() {
        return;
    }
    let mut sorted = ms.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    let total: f64 = sorted.iter().sum();
    let mean = total / n as f64;
    let min = sorted[0];
    let max = sorted[n - 1];
    println!(
        "BENCH {ext:>4} files={n:>3} total_ms={total:>9.1} median_ms={median:>8.2} mean_ms={mean:>8.2} min_ms={min:>7.2} max_ms={max:>8.2}"
    );
}

/// 走 gpui 泛用路径的等价解码：ImageReader 猜测格式 → decode → to_rgba8。
fn decode_generic(bytes: &[u8]) -> Result<f64, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes));
    reader = reader.with_guessed_format()?;
    let img = reader.decode()?;
    let _ = img.to_rgba8();
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// GIF 动画全帧解码（镜像 gpui/img.rs 的 into_frames 消费路径）。
fn decode_gif_all(bytes: &[u8]) -> Result<f64, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let frames = GifDecoder::new(std::io::Cursor::new(bytes))?.into_frames();
    for frame in frames.collect_frames()? {
        let _ = frame.buffer().clone();
    }
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

fn collect(root: &Path, map: &mut Vec<(String, PathBuf)>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Some(ext) = ext_of(&p)
                    && matches!(
                        ext.as_str(),
                        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico"
                    ) {
                        map.push((ext, p));
                    }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let roots: Vec<PathBuf> = if args.is_empty() {
        DEFAULT_CORPUS.iter().map(PathBuf::from).collect()
    } else {
        args.iter().map(PathBuf::from).collect()
    };

    let mut items: Vec<(String, PathBuf)> = Vec::new();
    for r in &roots {
        if !r.is_dir() {
            eprintln!("[imgbench] skip missing corpus dir: {}", r.display());
            continue;
        }
        collect(r, &mut items);
    }
    items.sort_by(|a, b| a.1.cmp(&b.1));

    let mut buckets: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
    let mut failures: Vec<String> = Vec::new();

    for (ext, path) in &items {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                failures.push(format!("{}: read err {e}", path.display()));
                continue;
            }
        };
        let ms = if ext == "gif" {
            decode_gif_all(&bytes)
        } else {
            decode_generic(&bytes)
        };
        match ms {
            Ok(ms) => buckets.entry(ext.clone()).or_default().push(ms),
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }

    println!("=== imgbench decode (debug) corpus items={} ===", items.len());
    for (ext, ms) in &buckets {
        stat_line(ext, ms);
    }
    println!("=== failures: {} ===", failures.len());
    for f in failures.iter().take(10) {
        eprintln!("  FAIL {f}");
    }
}
