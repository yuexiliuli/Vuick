//! WebP 解码 release 基准（cargo bench 手动运行）：kimg WebP 通道（封装 image-webp 0.2.4）
//! vs image-webp crate 0.2.4 直连 vs 真机 libwebp 1.6.0 SIMD（可选 feature 门控）。
//!
//! 运行（release 默认，harness=false 直接执行）：
//! ```text
//! # 仅 kimg vs image-webp 直连（不去构建 libwebp，libwebp 列输出 N/A）
//! cargo bench -p imgbench --bench webp_release_bench
//!
//! # 含 libwebp 1.6.0 真机对照通道（cc 源码构建静态库；首次需 C 编译器，可能耗时几分钟）
//! cargo bench -p imgbench --bench webp_release_bench --features libwebp-ref
//! ```
//!
//! 输出 `WEBP_RELEASE_BENCH <scenario> files=N kimg=..ms imagewebp=..ms libwebp=..ms
//! ratio_kimg_vs_imagewebp=.. ratio_kimg_vs_libwebp=..`（各场景 N≥9 轮取中位，release）。
//! note workspace `[profile.release]` 为 `opt-level="z"`（体积向）；如需速度向（`opt-level="3"`）可比口径，
//! 加 `--config 'profile.release.opt-level="3"'` 重新运行（会重建依赖）。
//!
//! 语料：`target/imgbench-corpus/WEBP`（16 张静态有损）+ 可选
//! `E:\Rust\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures\webp\`（大量小图：静态有损/无损/动画/
//! alpha；缺则打印 SKIP）。按位流头部四字码与 VP8X flags 归入四场景：static-lossy（VP8 或 VP8X 无
//! 动画）、static-lossless / alpha-lossless（VP8L；无损位流恒为 ARGB，彻底解一次判定是否存在非
//! 不透明像素，区分两者）、animated（VP8X animation 位 0x02）。分类/装载时对所有通道做一次解码探测，
//! 任一启用通道解不了的文件 drop 并打 note（防止基准中途 panic 与部分通道静默缺数）。
//!
//! 证据口径（一律 RGBA8）：kimg 静态走 `ImageReader→to_rgba8`、动画走 `WebPDecoder.into_frames()`
//! （生产姿态）；image-webp 直连静态 `read_image`、动画 `read_frame` 循环；libwebp 走
//! `WebPDecodeRGBA`。libwebp 列仅覆盖静态场景（动画需 demux 的 WebPDemux* + WebPDecodeRGBAInto，
//! 本版未接，动画场景输出 N/A）。正确性交叉断言仅汇总日志提示：libwebp 与 image-webp 的
//! YUV→RGB 上采样/alpha 路径实现不同，像素允许不一致，不 panic（非阻塞）。

use std::io::Cursor;
use std::time::Instant;

use kimg::AnimationDecoder as _;

const ROUNDS: usize = 9;

// ---------------------------------------------------------------------------
// 场景与语料
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scenario {
    StaticLossy,
    StaticLossless,
    AlphaLossless,
    Animated,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::StaticLossy => "static-lossy",
            Scenario::StaticLossless => "static-lossless",
            Scenario::AlphaLossless => "alpha-lossless",
            Scenario::Animated => "animated",
        }
    }
    fn animated(self) -> bool {
        matches!(self, Scenario::Animated)
    }
}

static SCENARIOS: [Scenario; 4] = [
    Scenario::StaticLossy,
    Scenario::StaticLossless,
    Scenario::AlphaLossless,
    Scenario::Animated,
];

/// 按位流头部归类：`VP8 `=有损、`VP8L`=无损、`VP8X`=扩展（flags 0x02=动画、0x10=alpha）。
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

fn collect_corpus_webps() -> Vec<(Scenario, String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let main = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus")
        .join("WEBP");
    // 大图无损语料（K53）：lossless 现仅微秒级小图不可测，用 libwebp 编码器生成照片类
    // 大图 lossless（+1 lossy q75），供 ours/imagewebp/libwebp 三通道真实分辨率对拍。
    #[cfg(feature = "libwebp-ref")]
    ensure_generated_webp_corpus(&main);
    collect_dir(&main, &mut files);
    if files.is_empty() {
        println!("WEBP_RELEASE_BENCH corpus SKIP (target/imgbench-corpus/WEBP 缺失)");
    }

    let extra = std::path::Path::new(r"D:\WorkSpace\1\csharp\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures\webp");
    let mut extra_files = Vec::new();
    collect_dir(extra, &mut extra_files);
    if extra_files.is_empty() {
        println!("WEBP_RELEASE_BENCH note extra-corpus SKIP (SimdCodecs webp fixtures 缺失)");
    }
    files.extend(extra_files);

    // 用户主语料（E:\Rust\SimdCodecs\测试图片\WEBP，001-036.webp 等）；缺则无影响。
    let user = std::path::Path::new(r"E:\Rust\SimdCodecs\测试图片\WEBP");
    let mut user_files = Vec::new();
    collect_dir(user, &mut user_files);
    if user_files.is_empty() {
        println!("WEBP_RELEASE_BENCH note user-corpus SKIP (SimdCodecs\\测试图片\\WEBP 缺失)");
    }
    files.extend(user_files);

    // 分类 + 探测：任一启用通道解不了的文件直接 drop（防止基准中途 panic）。
    let mut out = Vec::new();
    for (name, bytes) in files {
        let Some(scen) = classify(&bytes) else {
            println!("WEBP_RELEASE_BENCH note drop {name} (未知 WebP 头部)");
            continue;
        };
        let anim = scen.animated();
        let kimg_ok = kimg_decode(&bytes, anim).is_some();
        let iw_ok = imagewebp_decode(&bytes, anim).is_some();
        #[cfg(feature = "libwebp-ref")]
        let lp_ok = anim || libwebp_ref::decode_rgba(&bytes).is_some();
        #[cfg(not(feature = "libwebp-ref"))]
        let lp_ok = true;
        if !(kimg_ok && iw_ok && lp_ok) {
            println!(
                "WEBP_RELEASE_BENCH note drop {name} (解码探测失败: kimg={kimg_ok} imagewebp={iw_ok} libwebp={lp_ok})"
            );
            continue;
        }
        out.push((scen, name, bytes));
    }
    out
}

/// 大图无损语料生成（K53）：把 `target/imgbench-corpus/PNG` 的 2-3 张照片类大图（2400×3600/
/// 3000×3000）用 libwebp 编码器编码成 lossless webp（+1 张 lossy q75），写入
/// `target/imgbench-corpus/WEBP/`（gitignored）。文件已存在则跳过（bench 启动零负担；
/// 可删文件重生成）。三通道解码探测在 `collect_corpus_webps` 统一做，解不掉的自动 drop。
#[cfg(feature = "libwebp-ref")]
fn ensure_generated_webp_corpus(main: &std::path::Path) {
    let png_dir = main
        .parent()
        .map(|p| p.join("PNG"))
        .unwrap_or_default();
    // 照片类大图：28(RGB 2400x3600)/32(RGB 3600x2400) 出 lossless，27(RGBA 3000x3000)
    // 出 lossless-alpha + 28 出 lossy q75。文件已存在即跳过。
    for (png, webp, kind) in [
        ("28.png", "gen28-photo-lossless.webp", GenKind::Lossless),
        ("32.png", "gen32-photo-large-lossless.webp", GenKind::Lossless),
        ("27.png", "gen27-photo-rgba-lossless.webp", GenKind::Lossless),
        ("28.png", "gen28-photo-lossy-q75.webp", GenKind::LossyQ75),
    ] {
        let src = png_dir.join(png);
        let dst = main.join(webp);
        if !src.is_file() || dst.is_file() {
            continue;
        }
        match gen_webp(&src, kind) {
            Some(bytes) => {
                let _ = std::fs::write(&dst, &bytes);
                println!(
                    "WEBP_RELEASE_BENCH note corpus-gen {webp} {}B (src {png})",
                    bytes.len()
                );
            }
            None => println!("WEBP_RELEASE_BENCH note corpus-gen {webp} FAILED (src {png})"),
        }
    }
}

#[cfg(feature = "libwebp-ref")]
#[derive(Clone, Copy)]
enum GenKind {
    Lossless,
    LossyQ75,
}

/// 用 libwebp 编码器把 PNG 解码为 RGBA8 再编码成 webp（输出内存需 WebPFree）。
#[cfg(feature = "libwebp-ref")]
fn gen_webp(src: &std::path::Path, kind: GenKind) -> Option<Vec<u8>> {
    use std::ffi::c_void;

    let bytes = std::fs::read(src).ok()?;
    let img = kimg::ImageReader::with_format(std::io::Cursor::new(&bytes), kimg::ImageFormat::Png)
        .decode()
        .ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    let stride = rgba.width() as i32 * 4;
    let rgba_ptr = rgba.as_raw().as_ptr();
    let mut out: *mut u8 = std::ptr::null_mut();
    let size = match kind {
        GenKind::Lossless => unsafe {
            libwebp_sys::WebPEncodeLosslessRGBA(rgba_ptr, w, h, stride, &mut out)
        },
        GenKind::LossyQ75 => unsafe {
            libwebp_sys::WebPEncodeRGBA(rgba_ptr, w, h, stride, 75.0, &mut out)
        },
    };
    if size == 0 || out.is_null() {
        return None;
    }
    let v = unsafe { std::slice::from_raw_parts(out, size).to_vec() };
    unsafe { libwebp_sys::WebPFree(out as *mut c_void) };
    Some(v)
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
// 三条解码通道（均输出 RGBA8 字节数；解不了返回 None）
// ---------------------------------------------------------------------------

/// kimg 通道：静态走 ImageReader→to_rgba8（格式分发+转换，生产姿态）；动画走 into_frames。
fn kimg_decode(bytes: &[u8], anim: bool) -> Option<usize> {
    if anim {
        let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
        let frames = dec.into_frames().collect_frames().ok()?;
        Some(frames.iter().map(|f| f.buffer().len()).sum())
    } else {
        let img = kimg::ImageReader::with_format(Cursor::new(bytes), kimg::ImageFormat::WebP)
            .decode()
            .ok()?;
        Some(img.to_rgba8().into_raw().len())
    }
}

/// image-webp 直连：静态 read_image（按 has_alpha 取 3/4 字节每像素再展开 RGBA8，与 kimg 同口径）；
/// 动画 read_frame 循环。
fn imagewebp_decode(bytes: &[u8], anim: bool) -> Option<usize> {
    if anim {
        imagewebp_decode_animated(bytes)
    } else {
        imagewebp_decode_rgba(bytes).map(|(_, _, rgba)| rgba.len())
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

fn imagewebp_decode_animated(bytes: &[u8]) -> Option<usize> {
    let mut dec = image_webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
    let n = dec.output_buffer_size()?;
    let mut buf = vec![0u8; n];
    let mut total = 0usize;
    loop {
        match dec.read_frame(&mut buf) {
            Ok(_) => total += n,
            Err(image_webp::DecodingError::NoMoreFrames) => break,
            Err(_) => return None,
        }
    }
    Some(total)
}

/// 真机 libwebp 1.6.0（libwebp-sys vendored 静态链，WebPDecodeRGBA 解到 RGBA8）。
#[cfg(feature = "libwebp-ref")]
mod libwebp_ref {
    use std::ffi::c_void;
    use std::os::raw::c_int;

    /// 静态图解到 RGBA8，返回 (宽, 高, 像素)。失败返回 None（输出用 WebPFree 释放）。
    pub fn decode_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut w: c_int = 0;
        let mut h: c_int = 0;
        let ptr = unsafe { libwebp_sys::WebPDecodeRGBA(bytes.as_ptr(), bytes.len(), &mut w, &mut h) };
        if ptr.is_null() {
            return None;
        }
        let size = (w as usize) * (h as usize) * 4;
        let out = unsafe { std::slice::from_raw_parts(ptr, size).to_vec() };
        unsafe { libwebp_sys::WebPFree(ptr as *mut c_void) };
        Some((w as u32, h as u32, out))
    }
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

/// 轮数：默认 ROUNDS；可用 `WEBP_RELEASE_BENCH_ROUNDS` 覆盖（调试/快速校验时收紧）。
fn rounds() -> usize {
    std::env::var("WEBP_RELEASE_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS)
}

fn libwebp_version() -> &'static str {
    #[cfg(feature = "libwebp-ref")]
    { "1.6.0 (vendored, SIMD: MSVC 默认 SSE2/SSE4.1/AVX2 运行时检测)" }
    #[cfg(not(feature = "libwebp-ref"))]
    { "n/a" }
}

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let rounds = rounds();
    println!("WEBP_RELEASE_BENCH profile={profile} rounds={rounds} libwebp={}", libwebp_version());

    let corpus = collect_corpus_webps();
    let mut total = 0usize;
    for scen in SCENARIOS {
        let files: Vec<&(Scenario, String, Vec<u8>)> = corpus
            .iter()
            .filter(|(s, _, _)| *s == scen)
            .collect();
        if files.is_empty() {
            println!("WEBP_RELEASE_BENCH {} SKIP (无该场景语料)", scen.name());
            continue;
        }
        total += files.len();
        bench_scenario(scen, &files, rounds);
    }
    println!("WEBP_RELEASE_BENCH note corpus_size={corpus_len} bench_files={total}", corpus_len = corpus.len());

    #[cfg(feature = "libwebp-ref")]
    cross_check_libwebp(&corpus);
    eprintln!("[webp_release_bench] done");
}

fn bench_scenario(scen: Scenario, files: &[&(Scenario, String, Vec<u8>)], rounds: usize) {
    let anim = scen.animated();
    let kimg_ms = median_ms(|| {
        for (_, _, b) in files {
            let _ = kimg_decode(b, anim).expect("探测已保证可解");
        }
    }, rounds);
    let iw_ms = median_ms(|| {
        for (_, _, b) in files {
            let _ = imagewebp_decode(b, anim).expect("探测已保证可解");
        }
    }, rounds);

    #[cfg(feature = "libwebp-ref")]
    {
        if anim {
            println!(
                "WEBP_RELEASE_BENCH {} files={} kimg={kimg_ms:.3}ms imagewebp={iw_ms:.3}ms libwebp=N/A \
                 (动画需 demux 的 WebPDemux* 接口，本版仅 WebPDecodeRGBA，未接) ratio_kimg_vs_imagewebp={:.3}",
                scen.name(),
                files.len(),
                kimg_ms / iw_ms
            );
        } else {
            let lp_ms = median_ms(|| {
                for (_, _, b) in files {
                    let _ = libwebp_ref::decode_rgba(b).expect("探测已保证可解");
                }
            }, rounds);
            print_line(scen.name(), files.len(), kimg_ms, iw_ms, Some(lp_ms));
        }
        return;
    }
    #[cfg(not(feature = "libwebp-ref"))]
    print_line(scen.name(), files.len(), kimg_ms, iw_ms, None);
}

fn print_line(name: &str, n: usize, kimg_ms: f64, iw_ms: f64, libwebp_ms: Option<f64>) {
    match libwebp_ms {
        Some(lp) => {
            println!(
                "WEBP_RELEASE_BENCH {name} files={n} kimg={kimg_ms:.3}ms imagewebp={iw_ms:.3}ms libwebp={lp:.3}ms \
                 ratio_kimg_vs_imagewebp={:.3} ratio_kimg_vs_libwebp={:.3}",
                kimg_ms / iw_ms,
                kimg_ms / lp
            );
        }
        None => {
            println!(
                "WEBP_RELEASE_BENCH {name} files={n} kimg={kimg_ms:.3}ms imagewebp={iw_ms:.3}ms libwebp=N/A \
                 ratio_kimg_vs_imagewebp={:.3}  (libwebp 通道未启用：cargo bench -p imgbench --bench webp_release_bench --features libwebp-ref)",
                kimg_ms / iw_ms
            );
        }
    }
}

/// 正确性交叉断言（仅日志，不阻塞）：libwebp RGBA8 vs image-webp RGBA8 逐像素比对汇总。
#[cfg(feature = "libwebp-ref")]
fn cross_check_libwebp(corpus: &[(Scenario, String, Vec<u8>)]) {
    let mut dims_bad = 0usize;
    let mut px_diff_files = 0usize;
    let mut checked = 0usize;
    let mut total_px = 0usize;
    let mut total_diff = 0usize;
    let mut max_ratio = 0.0f64;
    let mut worst: Option<&str> = None;
    for (scen, name, bytes) in corpus {
        if scen.animated() {
            continue;
        }
        let Some((wl, hl, lp)) = libwebp_ref::decode_rgba(bytes) else {
            continue;
        };
        let Some((wi, hi, iw)) = imagewebp_decode_rgba(bytes) else {
            continue;
        };
        if wl != wi || hl != hi {
            dims_bad += 1;
            println!("WEBP_RELEASE_BENCH note cross-check {name}: dims libwebp=({wl}x{hl}) imagewebp=({wi}x{hi}) 不一致（不阻塞）");
            continue;
        }
        checked += 1;
        let n = lp.len() / 4;
        total_px += n;
        let diff = lp
            .chunks_exact(4)
            .zip(iw.chunks_exact(4))
            .filter(|(a, b)| a != b)
            .count();
        total_diff += diff;
        if diff > 0 {
            px_diff_files += 1;
            let r = diff as f64 / n as f64;
            if r > max_ratio {
                max_ratio = r;
                worst = Some(name);
            }
        }
    }
    println!(
        "WEBP_RELEASE_BENCH note cross-check dims_mismatch_files={dims_bad} px_diff_files={px_diff_files}/{checked} \
         overall_px={total_px} diff_px={total_diff} worst={:?}(ratio={max_ratio:.4}) （libwebp vs image-webp 像素可不同，不阻塞）",
        worst
    );
}
