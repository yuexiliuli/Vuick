//! WebP 解码 vs C# Sdcb.SimdCodecs.Webp 对比探针（imgbench，中文注释）。
//!
//! 目的：与 C# `quick`（进程内交错 A/B：current vs libwebp，单线程）同语料、同方法、
//! 同窗口对比 kimg 自研 WebP 解码（纯解码到 RGBA8）。**本探针按「姿态 × 线程」全维度输出**，
//! 防止只报单一组合产出误导性数字（2026-11-04 曾用「生产姿态 × 强制单线程 × 7 张偏置语料」
//! 误报 kimg 慢 1.3–2.7×；对账后真实单线程只慢 C# ~11–13%，见 `WEBP_VS_CSHARP_RECONCILED_SPLIT.md`）。
//!
//! 口径定义（对账铁律，详见 `crates/imgbench/WEBP_PERF_PROTOCOL.md`）：
//! - **posture=direct**（直写姿态，镜像 C# `DecodeTo`）：VP8L 静态走 `decode_static_lossless_rgba8_into`
//!   直写调用方复用缓冲（暖态零分配）；其余静态走 `WebPDecoder` 复用缓冲 `read_image`。**对标 C# 用这个**。
//! - **posture=production**（生产姿态）：`ImageReader→to_rgba8` / `into_frames`（多 20–25% 拷贝清零）。
//! - **threads=serial**（`KIMG_WEBP_THREADS=0`，**默认**）：强制串行——与 C# `quick` 单线程同口径。
//! - **threads=default**（不设 `KIMG_WEBP_THREADS`）：大图（≥2304 宏块）自动开 MT 流水线（产品默认形态）。
//!   本档仅供对照 MT 收益；用 `KIMG_WEBP_PROBE_THREADS=default|all` 才测。
//! - **语料**：缺省 = 交接主语料 `target/imgbench-corpus/WEBP` + `E:\Rust\SimdCodecs\测试图片\WEBP`；
//!   env `KIMG_WEBP_PROBE_CORPUS` 可指定单目录覆盖。打印文件数与宏块档位分布，偏置小集一眼可见。
//!
//! 运行（仓库根）：
//! ```text
//! cargo bench -p imgbench --bench webp_vs_csharp_probe --config 'profile.release.opt-level="3"'
//! ```
//! 环境变量：
//! - `KIMG_WEBP_PROBE_CORPUS`（语料目录）
//! - `KIMG_WEBP_PROBE_ROUNDS`（每文件中位轮数，缺省 15）
//! - `KIMG_WEBP_PROBE_THREADS`：`serial`（默认，单线程）/ `default`（仅 MT 自动）/ `all`（两个都测）
//!
//! 输出：`WEBP_C_PROBE FILE/AGG/...`（见正文）；对标 C# 时取 `posture=direct` 的 AGG 行与
//! C# `quick` 同批结果相除（比值 = kimg ÷ 对方，kimg 快则 <1）。

use std::io::Cursor;
use std::time::Instant;

use kimg::AnimationDecoder as _;
use kimg::ImageDecoder as _;

const ROUNDS_DEFAULT: usize = 15;
/// 大图 MT 门控下界（宏块总数，与 vp8/filter.rs `MT_MIN_MB_TOTAL` 对齐）。
const MT_MIN_MB_TOTAL: usize = 2304;

// ---------------------------------------------------------------------------
// 场景 / 姿势 / 线程 枚举
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scenario {
    Lossy,
    Lossless,
    Alpha,
    Anim,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::Lossy => "lossy",
            Scenario::Lossless => "lossless",
            Scenario::Alpha => "alpha",
            Scenario::Anim => "anim",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Posture {
    Direct,
    Production,
}

impl Posture {
    fn name(self) -> &'static str {
        match self {
            Posture::Direct => "direct",
            Posture::Production => "production",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Threads {
    Default,
    Serial,
}

impl Threads {
    fn name(self) -> &'static str {
        match self {
            Threads::Default => "default",
            Threads::Serial => "serial",
        }
    }
}

/// 按 RIFF 块扫描分类：动画（VP8X anim 位或 ANIM 块）→ anim；
/// 静态含 ALPH 块或 VP8X alpha 位 → alpha；VP8L → lossless；VP8 → lossy。
fn classify(bytes: &[u8]) -> Option<Scenario> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let mut has_vp8l = false;
    let mut has_alph = false;
    let mut anim = false;
    let mut alpha_flag = false;
    let mut i = 12usize;
    while i + 8 <= bytes.len() {
        let fourcc = &bytes[i..i + 4];
        let size =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        match fourcc {
            b"VP8X" => {
                if i + 12 <= bytes.len() {
                    let flags = bytes[i + 8];
                    anim |= flags & 0x02 != 0;
                    alpha_flag |= flags & 0x10 != 0;
                }
            }
            b"VP8L" => has_vp8l = true,
            b"ALPH" => has_alph = true,
            b"ANIM" | b"ANMF" => anim = true,
            _ => {}
        }
        let step = 8 + size + (size & 1);
        if step == 0 {
            return None;
        }
        i = i.saturating_add(step);
    }
    if anim {
        Some(Scenario::Anim)
    } else if has_alph || alpha_flag {
        Some(Scenario::Alpha)
    } else if has_vp8l {
        Some(Scenario::Lossless)
    } else {
        Some(Scenario::Lossy)
    }
}

// ---------------------------------------------------------------------------
// 解码：直写姿态 vs 生产姿态（动画两姿态同路径 into_frames，仅标注不同）
// ---------------------------------------------------------------------------

/// 直写姿态，静态复用调用方缓冲 `out`（须 ≥ w*h*4）。返回 (w, h, 输出字节数)。
fn kimg_direct_static(bytes: &[u8], out: &mut [u8]) -> Option<(u32, u32, usize)> {
    if let Ok(true) = kimg::codecs::webp::decode_static_lossless_rgba8_into(bytes, out) {
        return kimg::codecs::webp::lossless_static_size(bytes)
            .map(|(w, h, _)| (w, h, w as usize * h as usize * 4));
    }
    let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
    let (w, h) = dec.dimensions();
    let n = dec.total_bytes() as usize;
    if out.len() < n {
        return None;
    }
    dec.read_image(&mut out[..n]).ok()?;
    Some((w, h, n))
}

/// 生产姿态静态：`ImageReader→decode→to_rgba8`。返回 (w, h, rgba 长度)。
fn kimg_prod_static(bytes: &[u8]) -> Option<(u32, u32, usize)> {
    let img = kimg::ImageReader::with_format(Cursor::new(bytes), kimg::ImageFormat::WebP)
        .decode()
        .ok()?;
    let rgba = img.to_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw().len()))
}

/// 动画：`WebPDecoder→into_frames→collect_frames`（含逐帧画布合帧）。返回
/// (画布宽, 画布高, 各帧 RGBA 长度总和)。动画无独立直写路径，两种姿态共用本入口。
fn kimg_anim(bytes: &[u8]) -> Option<(u32, u32, usize)> {
    let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
    let frames = dec.into_frames().collect_frames().ok()?;
    let first = frames.first()?;
    let (w, h) = (first.buffer().width(), first.buffer().height());
    let total = frames.iter().map(|f| f.buffer().as_raw().len()).sum::<usize>();
    Some((w, h, total))
}

// ---------------------------------------------------------------------------
// 计时与主流程
// ---------------------------------------------------------------------------

/// 逐轮计时取中位（先暖机一轮）。
fn median_ms<F: FnMut()>(mut f: F, rounds: usize) -> f64 {
    f();
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

fn rounds() -> usize {
    std::env::var("KIMG_WEBP_PROBE_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS_DEFAULT)
}

fn geomean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    (values.iter().map(|v| v.ln()).sum::<f64>() / values.len() as f64).exp()
}

fn set_threads(mode: Threads) {
    // Rust 2024 下 std::env::set_var 为 unsafe；kimg 于每次解码延迟读 env（filter_mt_enabled）。
    match mode {
        Threads::Serial => unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "0");
        },
        Threads::Default => unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        },
    }
}

/// 线程模式选择：默认单线程（serial）——与 C# `quick` 同口径；`default`/`all` 才纳入 MT 对照。
fn selected_threads() -> Vec<Threads> {
    match std::env::var("KIMG_WEBP_PROBE_THREADS")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "default" => vec![Threads::Default],
        "all" => vec![Threads::Default, Threads::Serial],
        _ => vec![Threads::Serial],
    }
}

fn name_to_path(dirs: &[std::path::PathBuf], name: &str) -> Option<std::path::PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| p.exists())
}

fn main() {
    let rounds = rounds();
    let modes = selected_threads();
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "WEBP_C_PROBE profile={profile} rounds={rounds} posture=all threads={}",
        modes.iter().map(|m| m.name()).collect::<Vec<_>>().join("+")
    );

    // 语料：env 指定单目录覆盖；缺省用交接主语料两来源（各自存在才纳入）。
    let dirs: Vec<std::path::PathBuf> = if let Ok(env_dir) = std::env::var("KIMG_WEBP_PROBE_CORPUS")
    {
        vec![std::path::PathBuf::from(env_dir)]
    } else {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..");
        vec![
            root.join("target").join("imgbench-corpus").join("WEBP"),
            std::path::PathBuf::from(r"E:\Rust\SimdCodecs\测试图片\WEBP"),
        ]
    };

    // 收集并去重所有 *.webp 字节。
    let mut names: Vec<String> = Vec::new();
    for d in &dirs {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                if e.path().extension().map(|x| x == "webp").unwrap_or(false) {
                    names.push(
                        e.path()
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    );
                }
            }
        }
    }
    names.sort();
    names.dedup();
    let mut files: Vec<(String, Scenario, Vec<u8>)> = Vec::new();
    for name in &names {
        let Some(p) = name_to_path(&dirs, name) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&p) else { continue };
        match classify(&bytes) {
            Some(scen) => files.push((name.clone(), scen, bytes)),
            None => println!("WEBP_C_PROBE SMOKE skip {name} (未知 WebP 头部)"),
        }
    }
    if files.is_empty() {
        println!("WEBP_C_PROBE corpus dirs missing/empty: {:?}", dirs);
        return;
    }

    // 宏块档位统计：提示哪些文件默认会走 MT（一眼看出偏置小集问题）。
    let mut mb_ge_mt = 0usize;
    let mut mb_total = 0usize;
    for (_, scen, b) in &files {
        if *scen == Scenario::Anim {
            continue;
        }
        if let Ok(dec) = kimg::codecs::webp::WebPDecoder::new(Cursor::new(b)) {
            let (w, h) = dec.dimensions();
            let mb = ((w as usize + 15) / 16) * ((h as usize + 15) / 16);
            mb_total += 1;
            if mb >= MT_MIN_MB_TOTAL {
                mb_ge_mt += 1;
            }
        }
    }
    println!(
        "WEBP_C_PROBE corpus files={} static_mb<2304={} static_mb>=2304(默认走MT)={}",
        files.len(),
        mb_total - mb_ge_mt,
        mb_ge_mt
    );

    // 冒烟：每个文件双姿态先解一次；失败/尺寸异常 → 记行并跳过（不回比不可信数）。
    let mut bench: Vec<(String, Scenario, Vec<u8>, u32, u32)> = Vec::new();
    for (name, scen, bytes) in &files {
        let mut ok_direct = false;
        if let Some((w, h, n)) = if *scen == Scenario::Anim {
            kimg_anim(bytes)
        } else {
            let mut w = 0u32;
            let mut h = 0u32;
            if let Ok(dec) = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)) {
                (w, h) = dec.dimensions();
            } else if let Some((dw, dh, _)) = kimg::codecs::webp::lossless_static_size(bytes) {
                (w, h) = (dw, dh);
            }
            let mut out = vec![0u8; w as usize * h as usize * 4];
            kimg_direct_static(bytes, &mut out)
        } {
            if w != 0 && h != 0 && n != 0 {
                ok_direct = true;
                bench.push((name.clone(), *scen, bytes.clone(), w, h));
            }
        }
        let ok_prod = if *scen == Scenario::Anim {
            kimg_anim(bytes).is_some()
        } else {
            kimg_prod_static(bytes).is_some()
        };
        if !ok_direct || !ok_prod {
            println!("WEBP_C_PROBE SMOKE {name}: direct={ok_direct} prod={ok_prod} 双姿态未全过，跳过");
            bench.retain(|(n, _, _, _, _)| n != name);
        }
    }
    if bench.is_empty() {
        println!("WEBP_C_PROBE no benchable files after smoke");
        return;
    }

    for mode in &modes {
        set_threads(*mode);
        for posture in [Posture::Direct, Posture::Production] {
            // direct 姿态：每文件预分配一次 w*h*4 复用缓冲（暖态零分配，与 C# DecodeTo 同姿态）。
            let mut bufs: Vec<Vec<u8>> = bench
                .iter()
                .map(|(_, scen, _, w, h)| {
                    if *scen == Scenario::Anim {
                        Vec::new()
                    } else {
                        vec![0u8; (*w as usize) * (*h as usize) * 4]
                    }
                })
                .collect();
            let mut agg: Vec<(Scenario, Vec<f64>)> = Vec::new();
            let mut all: Vec<f64> = Vec::new();
            for (i, (name, scen, bytes, _, _)) in bench.iter().enumerate() {
                let ms = match posture {
                    Posture::Direct if *scen != Scenario::Anim => median_ms(
                        || {
                            let _ = kimg_direct_static(bytes, &mut bufs[i]).expect("冒烟已保证可解");
                        },
                        rounds,
                    ),
                    _ => median_ms(
                        || {
                            let _ = if *scen == Scenario::Anim {
                                kimg_anim(bytes)
                            } else if posture == Posture::Direct {
                                kimg_direct_static(bytes, &mut bufs[i])
                            } else {
                                kimg_prod_static(bytes)
                            }
                            .expect("冒烟已保证可解");
                        },
                        rounds,
                    ),
                };
                println!(
                    "WEBP_C_PROBE FILE {name} scenario={} posture={} threads={} ms={ms:.3} rounds={rounds}",
                    scen.name(),
                    posture.name(),
                    mode.name()
                );
                if let Some(e) = agg.iter_mut().find(|(s, _)| *s == *scen) {
                    e.1.push(ms);
                } else {
                    agg.push((*scen, vec![ms]));
                }
                all.push(ms);
            }
            for (scen, vals) in &agg {
                println!(
                    "WEBP_C_PROBE AGG scenario={} posture={} threads={} files={} geomean_ms={:.3}",
                    scen.name(),
                    posture.name(),
                    mode.name(),
                    vals.len(),
                    geomean(vals)
                );
            }
            println!(
                "WEBP_C_PROBE AGG all posture={} threads={} files={} geomean_ms={:.3}",
                posture.name(),
                mode.name(),
                all.len(),
                geomean(&all)
            );
        }
    }
    eprintln!("[webp_vs_csharp_probe] done. 对标 C# quick 取 posture=direct；默认/单线程用 threads=default/serial");
}
