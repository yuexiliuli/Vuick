//! VP8L 无损熵解码阶段分账探针（K59 统一基座 / K53 测量先行 / K60 决策闸）。
//!
//! 在 imgbench-corpus/WEBP 的大 lossless 语料（gen27/28/32 照片类，MB 级位流）上量
//! 【变换头 / 主熵流(+色缓存段) / 逆变换 / 收尾拷贝】各自墙钟占比 + 全链，release 多轮中位。
//! 前置数字（熵占 lossless 全链 %、全链 ms）用于 K60 决策闸：熵占比 <15% 或优化落地全链
//! 收益 <5% → 维持现状不实现。
//!
//! 运行：
//! ```text
//! cargo bench -p imgbench --bench vp8l_stage_probe --features stage-profile
//! ```
//! 未带 `stage-profile` feature 时输出 `N/A` 提示（kimg 探针入口以该 feature 门控）。
//! 输出 `VP8L_STAGE_PROBE <file> wxh entropy=..ms(..%) cache=..ms(..%) transforms=..ms(..%)
//! head=..ms(..%) torgba=..ms(..%) fullchain_sum=..ms raw_wall=..ms cache_sym=.. fill_px=..
//! symbol_reads=.. subtable_rate=..`
//!
//! 口径说明：`entropy` 桶含逐符号探针计数（thread_local ×2/符号）的开销 → 熵占比为**偏高的
//! 保守上界**（缓存段计时已用同进程 QPC 校准扣除）；决策不依赖精确值（熵是否主导、子表命中
//! 率 0.8–7.2% 结构性结论不受影响）。

use std::time::Instant;

const ROUNDS: usize = 9;

/// 收集 imgbench-corpus/WEBP 下有 VP8L 位流（static lossless）的大文件（size 阈值防微秒级小图）。
fn collect_big_lossless() -> Vec<(String, Vec<u8>)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus")
        .join("WEBP");
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "webp").unwrap_or(false) {
                let Ok(b) = std::fs::read(&p) else { continue };
                // 仅静态 VP8L（动画/有损的 bitstream 为 None 自然被滤掉）。
                if let Some(bs) = extract(&b) {
                    files.push((p.file_name().unwrap().to_string_lossy().into_owned(), bs));
                }
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .filter(|(_, bs)| bs.len() > 1 << 20) // >1MB 位流才算"大"（2400×3600 照片类）
        .collect()
}

#[cfg(feature = "stage-profile")]
fn extract(b: &[u8]) -> Option<Vec<u8>> {
    kimg::codecs::webp::vp8l_extract_bitstream(b)
}

#[cfg(not(feature = "stage-profile"))]
fn extract(_b: &[u8]) -> Option<Vec<u8>> {
    None
}

/// 简单 VP8L 头解析（宽 14 位 / 高 14 位 / alpha 1 位）。
fn header_wh(bs: &[u8]) -> (u32, u32) {
    if bs.len() < 5 {
        return (0, 0);
    }
    let h = u32::from_le_bytes([bs[1], bs[2], bs[3], bs[4]]);
    (1 + (h & 0x3fff), 1 + ((h >> 14) & 0x3fff))
}

/// 逐字段取多轮中位（每轮一次完整解码）。
#[cfg(feature = "stage-profile")]
fn median_profile(bs: &[u8], rounds: usize) -> kimg::codecs::webp::Vp8lStageProfile {
    // 暖机一轮。
    let _ = kimg::codecs::webp::vp8l_stage_decode(bs);
    let mut acc: Vec<_> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        let p = kimg::codecs::webp::vp8l_stage_decode(bs)
            .unwrap_or_else(|e| panic!("vp8l_stage_decode 失败: {e}"));
        let wall = t.elapsed().as_nanos() as u64;
        let mut p = p;
        // 以本轮外部墙钟为准（内部段之和 ≈ 外部墙钟；外部墙钟不含探针段翻录开销）。
        p.total_ns = wall;
        acc.push(p);
    }
    let mut out = kimg::codecs::webp::Vp8lStageProfile::default();
    for f in [
        "transforms_head_ns",
        "entropy_ns",
        "cache_ns",
        "transforms_ns",
        "to_rgba_ns",
        "total_ns",
        "cache_symbols",
        "cache_fill_pixels",
        "symbol_reads",
        "subtable_hits",
    ] {
        let mut v: Vec<u64> = acc.iter().map(|p| field(p, f)).collect();
        v.sort_unstable();
        set_field(&mut out, f, v[v.len() / 2]);
    }
    out.width = header_wh(bs).0;
    out.height = header_wh(bs).1;
    out
}

#[cfg(feature = "stage-profile")]
fn field(p: &kimg::codecs::webp::Vp8lStageProfile, f: &str) -> u64 {
    match f {
        "transforms_head_ns" => p.transforms_head_ns,
        "entropy_ns" => p.entropy_ns,
        "cache_ns" => p.cache_ns,
        "transforms_ns" => p.transforms_ns,
        "to_rgba_ns" => p.to_rgba_ns,
        "total_ns" => p.total_ns,
        "cache_symbols" => p.cache_symbols,
        "cache_fill_pixels" => p.cache_fill_pixels,
        "symbol_reads" => p.symbol_reads,
        "subtable_hits" => p.subtable_hits,
        _ => unreachable!(),
    }
}

#[cfg(feature = "stage-profile")]
fn set_field(p: &mut kimg::codecs::webp::Vp8lStageProfile, f: &str, v: u64) {
    match f {
        "transforms_head_ns" => p.transforms_head_ns = v,
        "entropy_ns" => p.entropy_ns = v,
        "cache_ns" => p.cache_ns = v,
        "transforms_ns" => p.transforms_ns = v,
        "to_rgba_ns" => p.to_rgba_ns = v,
        "total_ns" => p.total_ns = v,
        "cache_symbols" => p.cache_symbols = v,
        "cache_fill_pixels" => p.cache_fill_pixels = v,
        "symbol_reads" => p.symbol_reads = v,
        "subtable_hits" => p.subtable_hits = v,
        _ => unreachable!(),
    }
}

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let have_probe = cfg!(feature = "stage-profile");
    println!(
        "VP8L_STAGE_PROBE profile={profile} rounds={ROUNDS} stage_profile={have_probe}"
    );
    let files = collect_big_lossless();
    if files.is_empty() {
        println!("VP8L_STAGE_PROBE corpus SKIP (target/imgbench-corpus/WEBP 无 >1MB lossless)");
        return;
    }
    if !have_probe {
        println!("VP8L_STAGE_PROBE N/A (需 --features stage-profile 启用 kimg 阶段计时)");
        return;
    }
    #[cfg(feature = "stage-profile")]
    {
        run_probe(&files);
    }
    eprintln!("[vp8l_stage_probe] done");
}

#[cfg(feature = "stage-profile")]
fn run_probe(files: &[(String, Vec<u8>)]) {
    let mut tot_entropy = 0.0f64;
    let mut tot_total = 0.0f64;
    for (name, bs) in files {
        let (w, h) = header_wh(bs);
        let p = median_profile(bs, ROUNDS);
        let ms = |n: u64| n as f64 / 1e6;
        // 干净全链 = 各段之和（扣除逐符号计时器固有开销后的校正段）；原始墙钟另列。
        let sum = p.entropy_ns
            + p.cache_ns
            + p.transforms_ns
            + p.transforms_head_ns
            + p.to_rgba_ns;
        let entropy_total = p.entropy_ns + p.cache_ns;
        let ent_pct = entropy_total as f64 / sum as f64 * 100.0;
        println!(
            "VP8L_STAGE_PROBE {name} {}x{} entropy={:.3}ms({:.1}%) cache={:.3}ms({:.1}%) \
             transforms={:.3}ms({:.1}%) head={:.3}ms({:.1}%) torgba={:.3}ms({:.1}%) \
             fullchain_sum={:.3}ms raw_wall={:.3}ms \
             cache_sym={} fill_px={} symbol_reads={} subtable_rate={:.2}%",
            w,
            h,
            ms(entropy_total),
            ent_pct,
            ms(p.cache_ns),
            p.cache_ns as f64 / sum as f64 * 100.0,
            ms(p.transforms_ns),
            p.transforms_ns as f64 / sum as f64 * 100.0,
            ms(p.transforms_head_ns),
            p.transforms_head_ns as f64 / sum as f64 * 100.0,
            ms(p.to_rgba_ns),
            p.to_rgba_ns as f64 / sum as f64 * 100.0,
            ms(sum),
            ms(p.total_ns),
            p.cache_symbols,
            p.cache_fill_pixels,
            p.symbol_reads,
            p.subtable_hits as f64 / p.symbol_reads.max(1) as f64 * 100.0,
        );
        tot_entropy += entropy_total as f64 / sum as f64;
        tot_total += ms(sum);
    }
    let n = files.len() as f64;
    println!(
        "VP8L_STAGE_PROBE SUMMARY files={} mean_entropy_share={:.1}% mean_fullchain={:.3}ms",
        files.len(),
        tot_entropy / n * 100.0,
        tot_total / n
    );
    eprintln!("[vp8l_stage_probe] done");
}
