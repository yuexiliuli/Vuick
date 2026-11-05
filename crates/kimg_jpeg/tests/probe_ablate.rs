//! 阶段消融 A/B：单变量跳过 entropy/idct，以墙钟差值得各阶段真实占比（KIMG_JPEG_STAGE_PROFILE=1 必需，
//! 消融开关也要求 profile 模式才生效）。持久化探针（K27：证据入仓可复跑）。
//!
//! 用法（每次独立进程 = 独立 env，避免 OnceLock 缓存跨组合污染）：
//! ```text
//! $env:KIMG_JPEG_STAGE_PROFILE='1'
//! cargo test -p kimg_jpeg --release --test probe_ablate -- --ignored --nocapture                    # baseline
//! $env:KIMG_JPEG_SKIP_ENTROPY='1';  <同上>                                                            # 消融 entropy
//! $env:KIMG_JPEG_SKIP_IDCT='1';     <同上>                                                            # 消融 idct
//! $env:KIMG_JPEG_SKIP_ENTROPY='1'; $env:KIMG_JPEG_SKIP_IDCT='1'; <同上>                              # 双消融
//! ```
//! 占比推导：entropy = base - skip_e；idct = base - skip_i；inner+post = skip_both。
//! 消融耦合会导致被测大阶段被净差法略微高估（另侧缓存压力移除），用于定靶足够。

use std::io::Cursor;
use std::time::Instant;

use kimg_core::ImageDecoder as _;

const ROUNDS: usize = 5;

fn median_ms<F: FnMut()>(mut f: F, rounds: usize) -> f64 {
    f(); // 暖机
    let mut acc: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        acc.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    acc[acc.len() / 2]
}

/// 选定代表文件：2048 4:4:4 / 2000 4:4:4 / 4096 灰度 / 4096 progressive。
fn file_list() -> Vec<(&'static str, String)> {
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = format!("{root}/../../target/测试图片/jpeg");
    vec![
        ("Aggregate", format!("{dir}/Aggregate_Granite_N_1m.jpg")),
        ("oak_02", format!("{dir}/oak_02.jpg")),
        ("Carpet_D", format!("{dir}/Carpet_Simple_D_500mm.jpg")),
        ("CarPaint", format!("{dir}/CarPaint_Flake_M_1m.jpg")),
    ]
}

#[test]
#[ignore]
fn ablate() {
    let skip_e = std::env::var("KIMG_JPEG_SKIP_ENTROPY").as_deref() == Ok("1");
    let skip_i = std::env::var("KIMG_JPEG_SKIP_IDCT").as_deref() == Ok("1");
    println!(
        "ABLATE skip_entropy={skip_e} skip_idct={skip_i} rounds={ROUNDS} (KIMG_JPEG_STAGE_PROFILE=1 必需)"
    );
    for (name, path) in file_list() {
        let bytes = std::fs::read(path).unwrap();
        let n = {
            let d =
                kimg_jpeg::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes.as_slice())).unwrap();
            d.total_bytes() as usize
        };
        let t = median_ms(
            || {
                let dec = kimg_jpeg::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes.as_slice()))
                    .unwrap();
                let mut buf = vec![0u8; n];
                dec.read_image(&mut buf).unwrap();
            },
            ROUNDS,
        );
        println!("ABLATE FILE {name:<12} wall={t:.3}ms");
    }
}
