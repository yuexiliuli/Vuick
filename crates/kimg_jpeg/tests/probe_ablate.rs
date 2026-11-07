//! 阶段消融 A/B：单变量跳过 entropy/idct，以墙钟差值得各阶段真实占比（KIMG_JPEG_STAGE_PROFILE=1 必需，
//! 消融开关也要求 profile 模式才生效）。持久化探针（K27：证据入仓可复跑）。
//!
//! 用法（每次独立进程 = 独立 env，避免 OnceLock 缓存跨组合污染）：
//! ```text
//! $env:KIMG_JPEG_STAGE_PROFILE='1'
//! $env:KIMG_JPEG_NO_BUSY='1'   # 测量卫生：忙钟不插桩，消融 A/B 得净值（可选；缺省兼容旧行为）
//! cargo test -p kimg_jpeg --release --test probe_ablate -- --ignored --nocapture                    # baseline
//! $env:KIMG_JPEG_SKIP_ENTROPY='1';  <同上>                                                            # 消融 entropy
//! $env:KIMG_JPEG_SKIP_IDCT='1';     <同上>                                                            # 消融 idct
//! $env:KIMG_JPEG_SKIP_ENTROPY='1'; $env:KIMG_JPEG_SKIP_IDCT='1'; <同上>                              # 双消融
//! ```
//! 占比推导：entropy = base - skip_e；idct = base - skip_i；inner+post = skip_both。
//! 消融耦合会导致被测大阶段被净差法略微高估（另侧缓存压力移除），用于定靶足够。

mod fixtures;

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder as NewJpeg;

use fixtures::*;

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

/// 候选文件：语料 `KIMG_JPEG_CORPUS` 覆盖，缺省 `<workspace>/Benchmarks/corpus/JPEG`
///（fixtures::corpus_dir，与其余测试同口径，不硬编码不存在的目录）。选样按尺寸
///（2048 宽 / 1024 方 / 1152 竖）优先、不足按语料序补可解文件，不依赖固定文件名——446 下依旧成立。
type Candidate = (String, PathBuf, (u32, u32)); // (文件名, 路径, 尺寸)

/// 选取代表文件：优先 2048 宽 / 1024 方 / 1152 竖 各一（按语料排序首次命中），
/// 代表不足 3 个时按语料顺序补可解文件；语料缺失/为空/无可解 → 打印 SKIP 返回空
///（不 panic，与 §5 口径一致）。
fn file_list() -> Vec<Candidate> {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SKIP: 语料目录不存在/不可读: {}", dir.display());
        return Vec::new();
    }
    let files = collect_jpegs(&dir);
    if files.is_empty() {
        println!("SKIP: 语料目录为空: {}", dir.display());
        return Vec::new();
    }
    let mut decodable: Vec<(PathBuf, (u32, u32))> = Vec::new();
    for p in files {
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let Ok(dec) = NewJpeg::new(Cursor::new(bytes)) else {
            continue;
        };
        let (w, h) = dec.dimensions();
        if w == 0 || h == 0 {
            continue;
        }
        decodable.push((p, (w, h)));
    }
    if decodable.is_empty() {
        println!("SKIP: 语料无可解文件: {}", dir.display());
        return Vec::new();
    }
    let mut idx_wide = None; // 2048 宽
    let mut idx_square = None; // 1024 方
    let mut idx_portrait = None; // 1152 竖
    for (i, (_, (w, h))) in decodable.iter().enumerate() {
        if idx_wide.is_none() && *w >= 2048 {
            idx_wide = Some(i);
        }
        if idx_square.is_none() && *w == 1024 && *h == 1024 {
            idx_square = Some(i);
        }
        if idx_portrait.is_none() && *w == 1152 && *h > *w {
            idx_portrait = Some(i);
        }
    }
    let mut order: Vec<usize> = Vec::new();
    for i in [idx_wide, idx_square, idx_portrait].into_iter().flatten() {
        order.push(i);
    }
    let mut i = 0;
    while order.len() < 3 && i < decodable.len() {
        if !order.contains(&i) {
            order.push(i);
        }
        i += 1;
    }
    order.sort_unstable();
    order
        .into_iter()
        .map(|i| {
            let (p, dims) = &decodable[i];
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            (name, p.clone(), *dims)
        })
        .collect()
}

#[test]
#[ignore]
fn ablate() {
    let skip_e = std::env::var("KIMG_JPEG_SKIP_ENTROPY").as_deref() == Ok("1");
    let skip_i = std::env::var("KIMG_JPEG_SKIP_IDCT").as_deref() == Ok("1");
    println!(
        "ABLATE skip_entropy={skip_e} skip_idct={skip_i} rounds={ROUNDS} (KIMG_JPEG_STAGE_PROFILE=1 必需)"
    );
    let files = file_list();
    if files.is_empty() {
        println!("ABLATE SKIP: 无代表文件，返回");
        return;
    }
    for (name, path, (w, h)) in files {
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
        println!("ABLATE FILE {name:<12} {w}x{h} wall={t:.3}ms");
    }
}
