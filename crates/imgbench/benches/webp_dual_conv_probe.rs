//! WebP 生产路径双转换分解探针（K53 测量先行 / K59 统一基座）。
//!
//! 目标：把 gen28 有损大图的生产路径（`ImageReader→to_rgba8`，RGBA8 口径）分解到
//! 「内核解码 / read_image 4→3 抽条 / to_rgba8 3→4 扩写 / 分配」各成分，确认大头，
//! 并同进程内 A/B 候选 SIMD 核（优化前后同口径）。
//!
//! 成分（全部对同一 gen28 有损文件，release，`KIMG_WEBP_BACKEND=ours`）：
//! - `full_prod`：ImageReader::decode().to_rgba8()（基准测的完整生产姿态）
//! - `read_rgb`：WebPDecoder::new + read_image(w*h*3)（生产读取侧：内核+4→3+RGB 分配）
//! - `decode_only`：WebPDecoder::new（内核全部：容器+VP8+alpha+yuv→RGBA；read_image 之前）
//! - `conv43_old`/`conv43_new`：4→3 抽条旧循环 vs SSSE3 核（纯转换，预分配，不含分配）
//! - `conv34_old`/`conv34_new`：`to_rgba8` 的 RGB8→RGBA8 旧像素循环 vs SSSE3 核（同上）
//! - `alloc_rgb`/`alloc_rgba`：`vec![0; w*h*3]/[w*h*4]` 分配
//!
//! 归因：4→3 生产成本 ≈ read_rgb - decode_only - alloc_rgb；
//!       3→4 生产成本 ≈ full_prod - read_rgb - alloc_rgba - wrap（wrap≈from_decoder 包装）。
//!
//! 运行（release，即默认 opt-level=z；无需 libwebp-ref，不编 C）：
//! ```text
//! $env:KIMG_WEBP_BACKEND="ours"
//! cargo bench -p imgbench --bench webp_dual_conv_probe
//! ```
//! 输出 `DUAL_CONV <tag> <value>` 行；gen28 缺失则 SKIP（不影响其它探针）。

use std::io::Cursor;
use std::time::Instant;

use kimg::ImageDecoder;
use kimg::Pixel;
use kimg::{ImageFormat, ImageReader, RgbImage, RgbaImage};

const ROUNDS: usize = 9;

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

fn rounds_env() -> usize {
    std::env::var("DUAL_CONV_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|r| *r >= 3)
        .unwrap_or(ROUNDS)
}

// ---------------------------------------------------------------------------
// 4→3 转换核（纯转换，输出缓冲调用方预分配）
// ---------------------------------------------------------------------------

/// 旧实现（webp/mod.rs read_image 现状逐条镜像）：逐 4 字节 copy 到 3 字节。
fn conv43_old(rgba: &[u8], rgb: &mut [u8]) {
    for (px, out) in rgba.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
        out.copy_from_slice(&px[..3]);
    }
}

/// 候选 SSSE3：整缓冲按 4 像素一组（16B 入 → 12B 出 = 8B 存 + 4B 存），尾标量。
/// 数组索引用 `idx[3..]`-内联常数展开，避免 shuf 掩码运行时构造。
fn conv43_simd(rgba: &[u8], rgb: &mut [u8]) {
    debug_assert_eq!(rgb.len(), rgba.len() / 4 * 3);
    let n = rgba.len() / 4; // 像素数
    let ssse3 = detect_ssse3();
    let mut i = 0usize;
    if ssse3 {
        // rgba 指针按 4px 步进；每次读 16B、写 12B。
        let src = rgba.as_ptr();
        let dst = rgb.as_mut_ptr();
        while i + 4 <= n {
            unsafe {
                let v = std::arch::x86_64::_mm_loadu_si128(src.add(i * 4) as *const _);
                let shuf = std::arch::x86_64::_mm_setr_epi8(
                    0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 0, 0, 0, 0,
                );
                let out = std::arch::x86_64::_mm_shuffle_epi8(v, shuf);
                let o = dst.add(i * 3);
                std::arch::x86_64::_mm_storel_epi64(o as *mut _, out);
                let hi = std::arch::x86_64::_mm_cvtsi128_si32(
                    std::arch::x86_64::_mm_srli_si128(out, 8),
                ) as u32;
                std::ptr::write_unaligned(o.add(8) as *mut u32, hi);
            }
            i += 4;
        }
    }
    for (px, out) in rgba[i * 4..].chunks_exact(4).zip(rgb[i * 3..].chunks_exact_mut(3)) {
        out.copy_from_slice(&px[..3]);
    }
}

// ---------------------------------------------------------------------------
// 3→4 转换核（RGB8→RGBA8，alpha 恒 255）
// ---------------------------------------------------------------------------

/// 旧实现（dynamic_image.rs to_rgba8 的 Rgb8 分支生产形状逐条镜像）：
/// 内层 = `RgbaImage::new`（零值分配）+ `pixels().zip(pixels_mut())` 像素循环。
fn conv34_old(img: &RgbImage, rounds: usize) -> f64 {
    let (w, h) = img.dimensions();
    median_ms(
        || {
            let mut out = RgbaImage::new(w, h);
            for (px, out_px) in img.pixels().zip(out.pixels_mut()) {
                *out_px = px.to_rgba();
            }
            std::hint::black_box(&out);
        },
        rounds,
    )
}

/// 候选实现（生产形状）：`vec![0; w*h*4]` + SSSE3 扩写 + from_raw 包装。
fn conv34_simd_raw(rgb: &[u8], w: u32, h: u32, rounds: usize) -> f64 {
    let n = rgb.len() / 3;
    median_ms(
        || {
            let mut buf = vec![0u8; n * 4];
            conv34_simd(rgb, &mut buf);
            let out = RgbaImage::from_raw(w, h, buf).expect("RGBA 尺寸");
            std::hint::black_box(&out);
        },
        rounds,
    )
}

/// 候选 SSSE3：整缓冲按 4 像素一组（12B 入 → 16B 出），尾标量。12B 读取不越界
/// （offset=(n-4)*3 读 12B ≤ n*3）。
fn conv34_simd(rgb: &[u8], out: &mut [u8]) {
    debug_assert_eq!(out.len(), rgb.len() / 3 * 4);
    let n = rgb.len() / 3;
    let ssse3 = detect_ssse3();
    let mut i = 0usize;
    if ssse3 {
        let src = rgb.as_ptr();
        let dst = out.as_mut_ptr();
        while i + 4 <= n {
            unsafe {
                let lo = std::arch::x86_64::_mm_loadl_epi64(src.add(i * 3) as *const _);
                let hi32 = std::ptr::read_unaligned(src.add(i * 3 + 8) as *const u32) as i32;
                let hi = std::arch::x86_64::_mm_cvtsi32_si128(hi32);
                let data = std::arch::x86_64::_mm_unpacklo_epi64(lo, hi);
                let shuf = std::arch::x86_64::_mm_setr_epi8(
                    0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1,
                );
                let amask = std::arch::x86_64::_mm_setr_epi8(
                    0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1,
                );
                let v = std::arch::x86_64::_mm_shuffle_epi8(data, shuf);
                let out_v = std::arch::x86_64::_mm_or_si128(v, amask);
                std::arch::x86_64::_mm_storeu_si128(dst.add(i * 4) as *mut _, out_v);
            }
            i += 4;
        }
    }
    for (px, out) in rgb[i * 3..].chunks_exact(3).zip(out[i * 4..].chunks_exact_mut(4)) {
        out[0] = px[0];
        out[1] = px[1];
        out[2] = px[2];
        out[3] = 255;
    }
}

fn detect_ssse3() -> bool {
    std::is_x86_feature_detected!("ssse3")
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let rounds = rounds_env();
    let backend = std::env::var("KIMG_WEBP_BACKEND").unwrap_or_default();
    println!(
        "DUAL_CONV profile={profile} rounds={rounds} KIMG_WEBP_BACKEND={backend} ssse3={}",
        detect_ssse3()
    );

    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus")
        .join("WEBP");
    let names = [
        "gen28-photo-lossy-q75.webp",
        "04_05_fixed.webp",
        "00.webp",
    ];
    let mut any = false;
    for name in names {
        let path = corpus.join(name);
        if !path.is_file() {
            println!("DUAL_CONV SKIP {name} (target/imgbench-corpus/WEBP 缺失)");
            continue;
        }
        any = true;
        let bytes = std::fs::read(&path).expect("读语料");
        decompose(&name, &bytes, rounds);
    }
    if !any {
        println!("DUAL_CONV corpus SKIP (target/imgbench-corpus/WEBP 缺失)");
    }
    eprintln!("[webp_dual_conv_probe] done");
}

fn decompose(name: &str, bytes: &[u8], rounds: usize) {
    // 预热 + 取尺寸/解码产物。
    let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).expect("ours 解 gen28");
    let (w, h) = dec.dimensions();
    let total = dec.total_bytes() as usize;
    let color = dec.color_type();
    let n_px = (w as usize) * (h as usize);
    println!(
        "DUAL_CONV {name} dims={w}x{h} px={n_px} color_type={color:?} total_bytes={total}"
    );

    // full_prod：生产姿态。
    let full_prod = median_ms(
        || {
            let img = ImageReader::with_format(Cursor::new(bytes), ImageFormat::WebP)
                .decode()
                .expect("prod decode");
            let rgba = img.to_rgba8();
            std::hint::black_box(&rgba);
        },
        rounds,
    );

    // read_rgb：decoder + read_image(w*h*3)（生产读取侧：内核+4→3+RGB 分配+零页）。
    let read_rgb = median_ms(
        || {
            let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).expect("dec");
            let mut buf = vec![0u8; (w as usize) * (h as usize) * 3];
            dec.read_image(&mut buf).expect("read_image rgb");
            std::hint::black_box(&buf);
        },
        rounds,
    );

    // read_rgb_reuse_buf：decoder + read_image 到闭包外复用的缓冲（隔离「分配+零页」；
    // 若 ≈ decode_only + conv43 直接量，则 read_rgb 的差额是分配/页错噪音）。
    let mut reused_rgb = vec![0u8; (w as usize) * (h as usize) * 3];
    let read_rgb_reuse = median_ms(
        || {
            let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).expect("dec");
            dec.read_image(&mut reused_rgb).expect("read_image rgb");
            std::hint::black_box(&reused_rgb);
        },
        rounds,
    );

    // decode_only：内核。
    let decode_only = median_ms(
        || {
            let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).expect("dec");
            std::hint::black_box(dec);
        },
        rounds,
    );

    // 本探针聚焦无 alpha 有损（gen28）：color_type 非 Rgb8 的语料跳过分成分，避免口径混淆。
    if color != kimg::ColorType::Rgb8 {
        println!("DUAL_CONV {name} note: color_type={color:?} != Rgb8（本探针仅归因无 alpha 有损），SKIP 后续成分");
        return;
    }
    // 夹具：真实解码 RGB（read_image）+ 由 RGB 扩回 RGBA（内核 RGBA 即 [r,g,b,255] 同字节闭环）。
    let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).expect("dec");
    let mut rgb_storage = vec![0u8; n_px * 3];
    dec.read_image(&mut rgb_storage).expect("read_image rgb");
    let mut rgba_fixture = vec![0u8; n_px * 4];
    for (i, px) in rgb_storage.chunks_exact(3).enumerate() {
        let o = i * 4;
        rgba_fixture[o] = px[0];
        rgba_fixture[o + 1] = px[1];
        rgba_fixture[o + 2] = px[2];
        rgba_fixture[o + 3] = 255;
    }

    // 4→3 纯转换核（预分配输出，不含分配）。
    let mut rgb_out = vec![0u8; n_px * 3];
    let t_conv43_old = median_ms(
        || {
            conv43_old(&rgba_fixture, &mut rgb_out);
        },
        rounds,
    );
    let t_conv43_new = median_ms(
        || {
            conv43_simd(&rgba_fixture, &mut rgb_out);
        },
        rounds,
    );
    std::hint::black_box(&rgb_out);

    // 3→4 生产形状（旧 = RgbaImage::new + 像素循环；新 = vec + SSSE3 扩写 + from_raw）。
    let img_rgb = RgbImage::from_raw(w, h, rgb_storage.clone()).expect("RGB 尺寸");
    let t_conv34_old = conv34_old(&img_rgb, rounds);
    let t_conv34_new = conv34_simd_raw(&rgb_storage, w, h, rounds);

    // 分配。
    let alloc_rgb = median_ms(
        || {
            std::hint::black_box(vec![0u8; n_px * 3]);
        },
        rounds,
    );
    let alloc_rgba = median_ms(
        || {
            std::hint::black_box(vec![0u8; n_px * 4]);
        },
        rounds,
    );

    // 字节恒等校验（同一夹具上新旧核逐字节一致）。
    let mut ref_rgb = vec![0u8; n_px * 3];
    conv43_old(&rgba_fixture, &mut ref_rgb);
    let mut simd_rgb = vec![0u8; n_px * 3];
    conv43_simd(&rgba_fixture, &mut simd_rgb);
    let conv43_eq = ref_rgb == simd_rgb;

    let mut ref_img = RgbaImage::new(w, h);
    for (px, out_px) in img_rgb.pixels().zip(ref_img.pixels_mut()) {
        *out_px = px.to_rgba();
    }
    let ref_rgba = ref_img.as_raw().clone();
    let mut simd_rgba = vec![0u8; n_px * 4];
    conv34_simd(&rgb_storage, &mut simd_rgba);
    let conv34_eq = *ref_rgba == simd_rgba;

    println!("DUAL_CONV {name} full_prod={full_prod:.3}ms");
    println!("DUAL_CONV {name} read_rgb={read_rgb:.3}ms read_rgb_reuse_buf={read_rgb_reuse:.3}ms");
    println!("DUAL_CONV {name} decode_only={decode_only:.3}ms");
    println!("DUAL_CONV {name} conv43_old={t_conv43_old:.3}ms conv43_new={t_conv43_new:.3}ms (eq={conv43_eq})");
    println!("DUAL_CONV {name} conv34_old={t_conv34_old:.3}ms conv34_new={t_conv34_new:.3}ms (eq={conv34_eq})");
    println!("DUAL_CONV {name} alloc_rgb={alloc_rgb:.3}ms alloc_rgba={alloc_rgba:.3}ms");
    let conv43_prod = (read_rgb_reuse - decode_only).max(0.0);
    let conv34_prod = (full_prod - read_rgb - alloc_rgba).max(0.0);
    println!(
        "DUAL_CONV {name} ATTRIB 4-3生产侧~{conv43_prod:.3}ms(直接量 {t_conv43_old:.3}ms) \
         3-4生产侧~{conv34_prod:.3}ms(直接量 {t_conv34_old:.3}ms) kernel~{decode_only:.3}ms 双转换直接量~{:.3}ms/全生产~{full_prod:.3}ms",
        t_conv43_old + t_conv34_old
    );
    // 削减假设：去掉双转换直接量后，full_prod 的预估。
    let est = (full_prod - t_conv43_old - t_conv34_old).max(0.0);
    let est_new = (full_prod - t_conv43_old - t_conv34_old + t_conv43_new + t_conv34_new).max(0.0);
    println!(
        "DUAL_CONV {name} EST no-conv~{est:.3}ms with-new-kernels~{est_new:.3}ms \
         (full_prod - 双转换旧直接量 + 双转换新直接量)"
    );
}
