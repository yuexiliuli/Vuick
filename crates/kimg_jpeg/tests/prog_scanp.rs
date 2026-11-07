//! progressive 逐 scan 类别墙钟占比测量（`KIMG_JPEG_SCAN_PROFILE=1`，release，`#[ignore]`）。
//!
//! 探针本体在 `mcu.rs decode_mcu_ycbcr_progressive`（env 门控，缺省关=零注入）：每文件解码结束
//! 打印一行 `SCANP total=..ms dc_first=..(..%) ac_first=..(..%) ac_refine=..(..%) dc_refine=..(..%)
//! n_scans=.. other_scan=..(..%)`（纯墙钟，只量扫描熵段 + scan 间公共开销，不含 idct/post/finish）。
//!
//! 本测试是测量驱动：只在每文件解码**前**打印一行 `SCANP FILE <name> dims=..`（文件名关联口径，
//! 与 STAGE FILE / STAGE-DECODE 同款，探针本体不持文件名、零生产语义影响），随后跑 1 轮暖机 +
//! `KIMG_JPEG_SCAN_ROUNDS`（缺省 3）轮解码。离线聚合（python）把每个 `SCANP FILE` 之后紧跟的
//! 若干 `SCANP` 行归到该文件，逐类别取中位出占比表。
//!
//! 用法（长命令前台阻塞到结束，K86）：
//! ```text
//! $env:KIMG_JPEG_CORPUS='C:\Users\yuexi\AppData\Local\Temp\kilo\jpeg_broad'
//! $env:KIMG_JPEG_SCAN_PROFILE='1'
//! cargo test -p kimg_jpeg --release --test prog_scanp -- --ignored --nocapture | Tee-Object -FilePath Benchmarks\jpeg\logs\prog_scanp_<ts>.log
//! ```

mod fixtures;

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder as NewJpeg;

use fixtures::*;

#[test]
#[ignore]
fn scanp() {
    let rounds: usize = std::env::var("KIMG_JPEG_SCAN_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    if std::env::var("KIMG_JPEG_SCAN_PROFILE").as_deref() != Ok("1") {
        println!("SCANP note: KIMG_JPEG_SCAN_PROFILE=1 未设置，探针不打印 SCANP 行（本测试仅驱动文件标记+轮次）");
    }
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SCANP SKIP: 语料目录不存在/不可读: {}", dir.display());
        return;
    }
    let files = collect_jpegs(&dir);
    // 只统计名含 `progressive-` 的（任务口径）。
    let prog: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().contains("progressive-"))
                .unwrap_or(false)
        })
        .collect();
    if prog.is_empty() {
        println!("SCANP SKIP: 语料无 progressive 文件: {}", dir.display());
        return;
    }
    println!("SCANP header profile=release rounds={rounds} corpus={} prog_files={}", dir.display(), prog.len());

    for p in &prog {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                println!("SCANP WARN {name}: 读文件失败 {e}");
                continue;
            }
        };
        let Some(dec) = NewJpeg::new(Cursor::new(&bytes)).ok() else {
            println!("SCANP WARN {name}: 解码头失败，跳过");
            continue;
        };
        let (w, h) = dec.dimensions();
        let n = dec.total_bytes() as usize;
        println!("SCANP FILE {name} dims={w}x{h}");
        // 暖机 1 轮（丢弃其 SCANP 行）后解 rounds 轮；解码器每轮内部打印一行 SCANP。
        let mut buf = vec![0u8; n];
        let _ = NewJpeg::new(Cursor::new(&bytes))
            .expect("解码头已保证")
            .read_image(&mut buf);
        for _ in 0..rounds {
            let mut buf = vec![0u8; n];
            let t0 = Instant::now();
            let _ = NewJpeg::new(Cursor::new(&bytes))
                .expect("解码头已保证")
                .read_image(&mut buf);
            let t1 = Instant::now();
            println!(
                "SCANP ROUND wall={:.3}ms",
                (t1 - t0).as_secs_f64() * 1000.0
            );
        }
    }
    println!("SCANP done.");
}
