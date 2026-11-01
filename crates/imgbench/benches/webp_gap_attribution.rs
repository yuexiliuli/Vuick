//! WebP 聚合缺口归因探针（K59 统一基座 / K53 测量先行 / K68 同窗 A/B 控噪）。
//!
//! 目标：把 `webp_release_bench` 的聚合缺口（有损 1.229× / 无损 1.054×，vs libwebp 1.6.0）
//! 归因到可操作杠杆。本探针对 **每个** 有损文件（全部语料：imgbench-corpus/WEBP +
//! `E:\Rust\SimdCodecs\测试图片\WEBP` + SimdCodecs Fixtures\webp）做**同窗交错**计时（每文件
//! 内交替测 ours（生产姿态 ImageReader→to_rgba8）与 libwebp（WebPDecodeRGBA），多轮取中位，
//! 消漂移），按尺寸类【<64² / 64–256² / 256–1K² / 1K²+】（以面积为准）分桶聚合；无损同口径一桶。
//!
//! 运行（release，o3 与 webp_release_bench 同口径；`KIMG_WEBP_BACKEND=ours` 对齐聚合实测）：
//! ```text
//! $env:KIMG_WEBP_BACKEND="ours"
//! cargo bench -p imgbench --bench webp_gap_attribution --features "stage-profile libwebp-ref" `
//!   --config 'profile.release.opt-level="3"'
//! ```
//! 输出：
//! ```text
//! GAP_ATTR lossy <class> files=n ours=..ms libwebp=..ms ratio=..
//! GAP_ATTR LOSSY_WORST <rank> <name> <w>x<h> ours=..ms libwebp=..ms ratio=..
//! GAP_ATTR lossless <class> files=n ours=..ms libwebp=..ms ratio=..   # 同口径分桶（附）
//! GAP_ATTR lossless TOTAL files=n ours=..ms libwebp=..ms ratio=..     # 「一桶」
//! GAP_ATTR FIXED <class> tiny_files=n min=..us mean=..us fixed_est=..us pct_of_bucket=..%
//! STAGE_ATTR <name> ...   # 归因最差桶代表文件阶段分账（复用 kimg stage_profile 机制）
//! ```
//! 环境变量：`GAP_ATTR_ROUNDS`（默认 7）、`GAP_ATTR_SKIP_STAGE=1`（跳过阶段分账）、
//! `GAP_ATTR_MAX_FILES`（快速子集校验）。未带 `libwebp-ref` 时 libwebp/ratio 输出 N/A。

use std::io::Cursor;
use std::time::Instant;

const ROUNDS: usize = 7;
const STAGE_ROUNDS: usize = 7;

// ---------------------------------------------------------------------------
// 场景与语料（镜像 webp_release_bench 的三目录 + classify）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scenario {
    StaticLossy,
    StaticLossless,
    AlphaLossless,
    Animated,
}

/// 尺寸桶：0=<64²，1=64–256²，2=256–1K²，3=1K²+（按面积 w*h）。
fn bucket_of(area: u64) -> usize {
    let k = (area as f64).sqrt();
    if k < 64.0 {
        0
    } else if k < 256.0 {
        1
    } else if k < 1024.0 {
        2
    } else {
        3
    }
}

const BUCKET_NAMES: [&str; 4] = ["<64²", "64-256²", "256-1K²", "1K²+"];

fn classify(bytes: &[u8]) -> Option<Scenario> {
    if bytes.len() < 30 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8 " => Some(Scenario::StaticLossy),
        b"VP8L" => {
            if lossless_has_alpha(bytes) {
                Some(Scenario::AlphaLossless)
            } else {
                Some(Scenario::StaticLossless)
            }
        }
        b"VP8X" => {
            let flags = bytes[20];
            if flags & 0x02 != 0 {
                return Some(Scenario::Animated);
            }
            if bytes[12..].windows(4).any(|w| w == b"VP8L") {
                if lossless_has_alpha(bytes) {
                    Some(Scenario::AlphaLossless)
                } else {
                    Some(Scenario::StaticLossless)
                }
            } else {
                Some(Scenario::StaticLossy)
            }
        }
        _ => None,
    }
}

/// 无损位流恒为 ARGB：彻底解一次，判定是否存在非不透明像素。
fn lossless_has_alpha(bytes: &[u8]) -> bool {
    match imagewebp_decode_rgba(bytes) {
        Some((_, _, rgba)) => rgba.chunks_exact(4).any(|px| px[3] != 0xFF),
        None => false,
    }
}

fn corpus_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(val) = std::env::var("KIMG_WEBP_FIXTURES") {
        for p in val.split(';') {
            let p = p.trim();
            if !p.is_empty() {
                dirs.push(std::path::PathBuf::from(p));
            }
        }
    }
    dirs.push(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP"),
    );
    dirs.push(std::path::PathBuf::from(r"E:\Rust\SimdCodecs\测试图片\WEBP"));
    dirs.push(std::path::PathBuf::from(
        r"E:\Rust\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures\webp",
    ));
    dirs
}

/// 收集全部静态 webp（有损/无损/alpha 无损），附尺寸；动画单独计数不入归因桶。
/// 每个文件要求 ours 与 libwebp 都能解（解不了 drop，防止基准中途 panic —— 同
/// webp_release_bench 口径，探 测时 ours 解出即得到真实尺寸）。
fn collect_corpus() -> (Vec<CorpusEntry>, usize) {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for dir in corpus_dirs() {
        if dir.is_dir() {
            collect_dir(&dir, &mut files);
        }
    }
    let mut out = Vec::new();
    let mut animated = 0usize;
    for (name, bytes) in files {
        let Some(scen) = classify(&bytes) else {
            println!("GAP_ATTR note drop {name} (未知 WebP 头部)");
            continue;
        };
        if scen == Scenario::Animated {
            animated += 1;
            continue;
        }
        let Some((w, h, _)) = kimg_decode(&bytes, false) else {
            println!("GAP_ATTR note drop {name} (ours 探测失败)");
            continue;
        };
        let lp_ok = libwebp_decode(&bytes).is_some();
        if !lp_ok {
            println!("GAP_ATTR note drop {name} (libwebp 探测失败)");
            continue;
        }
        out.push(CorpusEntry {
            scen,
            name,
            bytes,
            w,
            h,
            area: w as u64 * h as u64,
        });
    }
    (out, animated)
}

struct CorpusEntry {
    scen: Scenario,
    name: String,
    bytes: Vec<u8>,
    w: u32,
    h: u32,
    area: u64,
}

fn collect_dir(dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "webp").unwrap_or(false))
        .collect();
    paths.sort();
    for p in paths {
        if let Ok(b) = std::fs::read(&p) {
            let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            out.push((name, b));
        }
    }
}

// ---------------------------------------------------------------------------
// 解码通道（两条计时通道都要真实产出、black_box 抑制消除）
// ---------------------------------------------------------------------------

/// kimg 生产姿态：ImageReader→to_rgba8。返回 (宽, 高, RGBA8 字节数)。
fn kimg_decode(bytes: &[u8], _anim: bool) -> Option<(u32, u32, usize)> {
    let img = kimg::ImageReader::with_format(Cursor::new(bytes), kimg::ImageFormat::WebP)
        .decode()
        .ok()?;
    let rgba = img.to_rgba8();
    let n = rgba.as_raw().len();
    std::hint::black_box(&n);
    Some((rgba.width(), rgba.height(), n))
}

/// libwebp 1.6.0（WebPDecodeRGBA，SIMD 运行时检测）。返回 RGBA8 字节数。
fn libwebp_decode(bytes: &[u8]) -> Option<usize> {
    #[cfg(feature = "libwebp-ref")]
    {
        use std::ffi::c_void;
        let mut w: i32 = 0;
        let mut h: i32 = 0;
        let ptr = unsafe {
            libwebp_sys::WebPDecodeRGBA(bytes.as_ptr(), bytes.len(), &mut w, &mut h)
        };
        if ptr.is_null() {
            return None;
        }
        let size = (w as usize) * (h as usize) * 4;
        let out = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
        std::hint::black_box(out.as_ptr());
        unsafe { libwebp_sys::WebPFree(ptr as *mut c_void) };
        Some(size)
    }
    #[cfg(not(feature = "libwebp-ref"))]
    {
        let _ = bytes;
        None
    }
}

fn imagewebp_decode_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = image_webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
    if dec.is_animated() {
        return None;
    }
    let (w, h) = dec.dimensions();
    let n = dec.output_buffer_size()?;
    let mut buf = vec![0u8; n];
    dec.read_image(&mut buf).ok()?;
    Some((w, h, if dec.has_alpha() { buf } else { rgb_to_rgba(&buf) }))
}

fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() + rgb.len() / 3);
    for px in rgb.chunks_exact(3) {
        out.extend_from_slice(px);
        out.push(0xFF);
    }
    out
}

// ---------------------------------------------------------------------------
// 同窗交错计时
// ---------------------------------------------------------------------------

/// 每文件交替测 ours / libwebp 共 ROUNDS 轮，各取中位（K68 同窗 A/B 控噪）。
fn interleaved_median(bytes: &[u8], rounds: usize) -> (f64, f64) {
    // 暖机一轮（温缓存/分派路径）。
    let _ = kimg_decode(bytes, false);
    let _ = libwebp_decode(bytes);
    let mut ours: Vec<f64> = Vec::with_capacity(rounds);
    let mut lw: Vec<f64> = Vec::with_capacity(rounds);
    for i in 0..rounds {
        // 每轮交替先测谁，抵消慢漂移方向。
        if i % 2 == 0 {
            let t = Instant::now();
            let r = kimg_decode(bytes, false);
            std::hint::black_box(r);
            ours.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            let r = libwebp_decode(bytes);
            std::hint::black_box(r);
            lw.push(t.elapsed().as_secs_f64() * 1000.0);
        } else {
            let t = Instant::now();
            let r = libwebp_decode(bytes);
            std::hint::black_box(r);
            lw.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            let r = kimg_decode(bytes, false);
            std::hint::black_box(r);
            ours.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let sort = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    (sort(&mut ours), sort(&mut lw))
}

fn rounds_env() -> usize {
    std::env::var("GAP_ATTR_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|r| *r >= 3)
        .unwrap_or(ROUNDS)
}

// ---------------------------------------------------------------------------
// 阶段分账（复用 kimg stage_profile 机制；K59 统一基座）
// ---------------------------------------------------------------------------

/// 对代表文件跑 vp8 阶段分账（每轮完整解码，取 total 中位那轮的各字段）。
#[cfg(feature = "stage-profile")]
fn stage_attr_lossy(bytes: &[u8], name: &str) {
    let Some((frame, w, h, has_alpha)) = kimg::codecs::webp::vp8_extract_frame(bytes) else {
        println!("STAGE_ATTR {name} extract-fail (非 VP8 有损帧)");
        return;
    };
    let _ = kimg::codecs::webp::vp8_stage_decode(&frame, w, h); // 暖机
    let mut acc = Vec::with_capacity(STAGE_ROUNDS);
    for _ in 0..STAGE_ROUNDS {
        let mut p = kimg::codecs::webp::vp8_stage_decode(&frame, w, h)
            .unwrap_or_else(|e| panic!("vp8_stage_decode 失败: {e}"));
        p.total_ns = p.entropy_ns + p.predict_ns + p.idct_ns + p.filter_ns + p.yuv_ns;
        acc.push(p);
    }
    acc.sort_by_key(|p| p.total_ns);
    let p = &acc[acc.len() / 2];
    let sum = p.entropy_ns + p.predict_ns + p.idct_ns + p.filter_ns + p.yuv_ns;
    let pct = |n: u64| n as f64 / sum as f64 * 100.0;
    println!(
        "STAGE_ATTR {name} {}x{} alpha={} entropy={:.3}ms({:.1}%) predict={:.3}ms({:.1}%) \
         idct={:.3}ms({:.1}%) filter={:.3}ms({:.1}%) yuv={:.3}ms({:.1}%) inner_sum={:.3}ms",
        w,
        h,
        has_alpha,
        p.entropy_ns as f64 / 1e6,
        pct(p.entropy_ns),
        p.predict_ns as f64 / 1e6,
        pct(p.predict_ns),
        p.idct_ns as f64 / 1e6,
        pct(p.idct_ns),
        p.filter_ns as f64 / 1e6,
        pct(p.filter_ns),
        p.yuv_ns as f64 / 1e6,
        pct(p.yuv_ns),
        sum as f64 / 1e6,
    );
}

/// 对代表文件跑 vp8l 无损阶段分账。
#[cfg(feature = "stage-profile")]
fn stage_attr_lossless(bytes: &[u8], name: &str) {
    let Some(bs) = kimg::codecs::webp::vp8l_extract_bitstream(bytes) else {
        println!("STAGE_ATTR {name} extract-fail (非 VP8L 无损)");
        return;
    };
    let _ = kimg::codecs::webp::vp8l_stage_decode(&bs); // 暖机
    let mut acc = Vec::with_capacity(STAGE_ROUNDS);
    for _ in 0..STAGE_ROUNDS {
        let p = kimg::codecs::webp::vp8l_stage_decode(&bs)
            .unwrap_or_else(|e| panic!("vp8l_stage_decode 失败: {e}"));
        acc.push(p);
    }
    acc.sort_by_key(|p| p.total_ns);
    let p = &acc[acc.len() / 2];
    let sum = p.entropy_ns + p.cache_ns + p.transforms_ns + p.transforms_head_ns + p.to_rgba_ns;
    let ent = p.entropy_ns + p.cache_ns;
    let pct = |n: u64| n as f64 / sum as f64 * 100.0;
    println!(
        "STAGE_ATTR {name} {}x{} entropy={:.3}ms({:.1}%) cache={:.3}ms transforms={:.3}ms({:.1}%) \
         head={:.3}ms torgba={:.3}ms({:.1}%) inner_sum={:.3}ms raw_wall={:.3}ms",
        p.width,
        p.height,
        ent as f64 / 1e6,
        pct(ent),
        p.cache_ns as f64 / 1e6,
        p.transforms_ns as f64 / 1e6,
        pct(p.transforms_ns),
        p.transforms_head_ns as f64 / 1e6,
        p.to_rgba_ns as f64 / 1e6,
        pct(p.to_rgba_ns),
        sum as f64 / 1e6,
        p.total_ns as f64 / 1e6,
    );
}

#[cfg(not(feature = "stage-profile"))]
fn stage_attr_lossy(_bytes: &[u8], _name: &str) {}
#[cfg(not(feature = "stage-profile"))]
fn stage_attr_lossless(_bytes: &[u8], _name: &str) {}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let rounds = rounds_env();
    let backend = std::env::var("KIMG_WEBP_BACKEND").unwrap_or_default();
    println!(
        "GAP_ATTR profile={profile} rounds={rounds} libwebp_ref={} stage_profile={} \
         KIMG_WEBP_BACKEND={backend}",
        cfg!(feature = "libwebp-ref"),
        cfg!(feature = "stage-profile")
    );

    let (corpus, animated) = collect_corpus();
    println!(
        "GAP_ATTR note corpus_static={} animated={animated} (动画不入归因桶)",
        corpus.len()
    );
    if corpus.is_empty() {
        println!("GAP_ATTR corpus SKIP (无静态 webp 语料)");
        return;
    }
    let max_files = std::env::var("GAP_ATTR_MAX_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);

    let mut lossy: Vec<&CorpusEntry> = corpus
        .iter()
        .filter(|e| e.scen == Scenario::StaticLossy)
        .collect();
    let mut lossless: Vec<&CorpusEntry> = corpus
        .iter()
        .filter(|e| e.scen == Scenario::StaticLossless)
        .collect();
    // 有损优先（核心归因），保序前 max_files 个子集用于快速校验。
    lossy.truncate(max_files);
    lossless.truncate(max_files);

    let mut lossy_buckets = [BucketAcc::default(); 4];
    let mut lossy_row: Vec<FileRow> = Vec::with_capacity(lossy.len());
    for e in &lossy {
        let (o, l) = interleaved_median(&e.bytes, rounds);
        let b = &mut lossy_buckets[bucket_of(e.area)];
        b.files += 1;
        b.ours_ms += o;
        b.lw_ms += l;
        lossy_row.push(FileRow {
            name: e.name.clone(),
            w: e.w,
            h: e.h,
            ours_ms: o,
            lw_ms: l,
            ratio: if l > 0.0 { o / l } else { f64::INFINITY },
        });
    }
    emit_buckets("lossy", &lossy_buckets);

    // top-8 最差文件（按 ratio）。
    lossy_row.sort_by(|a, b| b.ratio.partial_cmp(&a.ratio).unwrap());
    for (i, r) in lossy_row.iter().take(8).enumerate() {
        println!(
            "GAP_ATTR LOSSY_WORST {} {} {}x{} ours={:.3}ms libwebp={:.3}ms ratio={:.2}",
            i + 1, r.name, r.w, r.h, r.ours_ms, r.lw_ms, r.ratio
        );
    }
    // 每桶 top-4 最差（含 1K²+ 大文件：聚合溢价的真实落点，桶均值会掩盖桶内散点）。
    for (bi, bname) in BUCKET_NAMES.iter().enumerate() {
        let mut rows: Vec<&FileRow> = lossy_row
            .iter()
            .filter(|r| bucket_of(r.w as u64 * r.h as u64) == bi)
            .collect();
        rows.sort_by(|a, b| b.ratio.partial_cmp(&a.ratio).unwrap());
        for r in rows.iter().take(4) {
            println!(
                "GAP_ATTR BUCKET_WORST {bname} {} {}x{} ours={:.3}ms libwebp={:.3}ms ratio={:.2}",
                r.name, r.w, r.h, r.ours_ms, r.lw_ms, r.ratio
            );
        }
    }

    // 无损：同口径分桶 + 一桶 total。
    let mut lossless_buckets = [BucketAcc::default(); 4];
    let mut lossless_row: Vec<FileRow> = Vec::with_capacity(lossless.len());
    for e in &lossless {
        let (o, l) = interleaved_median(&e.bytes, rounds);
        let b = &mut lossless_buckets[bucket_of(e.area)];
        b.files += 1;
        b.ours_ms += o;
        b.lw_ms += l;
        lossless_row.push(FileRow {
            name: e.name.clone(),
            w: e.w,
            h: e.h,
            ours_ms: o,
            lw_ms: l,
            ratio: if l > 0.0 { o / l } else { f64::INFINITY },
        });
    }
    emit_buckets("lossless", &lossless_buckets);
    let total: BucketAcc = lossless_buckets.iter().copied().sum();
    println!(
        "GAP_ATTR lossless TOTAL files={} ours={:.3}ms libwebp={:.3}ms ratio={:.3}",
        total.files,
        total.ours_ms,
        total.lw_ms,
        total.ours_ms / total.lw_ms.max(1e-9)
    );
    lossless_row.sort_by(|a, b| b.ratio.partial_cmp(&a.ratio).unwrap());
    for r in lossless_row.iter().take(5) {
        println!(
            "GAP_ATTR LOSSLESS_WORST {} {}x{} ours={:.3}ms libwebp={:.3}ms ratio={:.2}",
            r.name, r.w, r.h, r.ours_ms, r.lw_ms, r.ratio
        );
    }

    // 最小类固定开销（有损）：fixed_est ≈ 最小每文件耗时（微图像素解码≈0，余量为固定段）。
    let tiny: Vec<&FileRow> = lossy_row.iter().filter(|r| bucket_of(r.w as u64 * r.h as u64) == 0).collect();
    if !tiny.is_empty() {
        let mut sorted: Vec<f64> = tiny.iter().map(|r| r.ours_ms * 1000.0).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let min = sorted[0];
        let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let fixed_est = min;
        let pct = fixed_est * tiny.len() as f64 / (lossy_buckets[0].ours_ms * 1000.0).max(1e-9) * 100.0;
        println!(
            "GAP_ATTR FIXED {} tiny_files={} min={:.2}us mean={:.2}us fixed_est={:.2}us pct_of_bucket={:.1}%",
            BUCKET_NAMES[0], tiny.len(), min, mean, fixed_est, pct
        );
    }

    // 归因最差桶代表文件阶段分账：每个有损桶的最差 ratio 文件 + lossless 最差一文件。
    if std::env::var("GAP_ATTR_SKIP_STAGE").is_err() {
        for (bi, b) in BUCKET_NAMES.iter().enumerate() {
            let worst = lossy_row
                .iter()
                .filter(|r| bucket_of(r.w as u64 * r.h as u64) == bi)
                .max_by(|a, b| a.ratio.partial_cmp(&b.ratio).unwrap());
                if let Some(w) = worst {
                    let e = lossy.iter().find(|e| e.name == w.name && e.w == w.w && e.h == w.h);
                    if let Some(e) = e {
                        let wn = &w.name;
                        println!(
                            "GAP_ATTR note stage-rep lossy {b} {wn} (ratio={:.2})",
                            w.ratio
                        );
                        stage_attr_lossy(&e.bytes, &w.name);
                    }
                }
        }
        // 无损代表 = 最大面积文件（照片类大 lossless 是 1.054× 缺口所在档）。
        let lw_worst = lossless.iter().max_by_key(|e| e.area);
        if let Some(e) = lw_worst {
            let (o, l) = interleaved_median(&e.bytes, rounds);
            println!(
                "GAP_ATTR note stage-rep lossless {} (ratio={:.2})",
                e.name,
                o / l.max(1e-9)
            );
            stage_attr_lossless(&e.bytes, &e.name);
        }
    }

    eprintln!("[webp_gap_attribution] done");
}

#[derive(Clone, Copy, Default)]
struct BucketAcc {
    files: usize,
    ours_ms: f64,
    lw_ms: f64,
}

impl std::iter::Sum for BucketAcc {
    fn sum<I: Iterator<Item = BucketAcc>>(iter: I) -> Self {
        iter.fold(BucketAcc::default(), |a, b| BucketAcc {
            files: a.files + b.files,
            ours_ms: a.ours_ms + b.ours_ms,
            lw_ms: a.lw_ms + b.lw_ms,
        })
    }
}

struct FileRow {
    name: String,
    w: u32,
    h: u32,
    ours_ms: f64,
    lw_ms: f64,
    ratio: f64,
}

fn emit_buckets(tag: &str, buckets: &[BucketAcc; 4]) {
    for (i, b) in buckets.iter().enumerate() {
        let ratio = if b.lw_ms > 1e-9 { b.ours_ms / b.lw_ms } else { f64::INFINITY };
        println!(
            "GAP_ATTR {tag} {} files={} ours={:.3}ms libwebp={:.3}ms ratio={:.3}",
            BUCKET_NAMES[i], b.files, b.ours_ms, b.lw_ms, ratio
        );
    }
}
