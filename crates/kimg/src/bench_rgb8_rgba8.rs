//! 临时根因探针：Rgb8 → Rgba8 的 `to_rgba8` 分量墙钟（K53/K107 三件套，事后可选保留）。
//!
//! 目标：复现「BMP 全解 `.to_rgba8()` ≈ 785ms vs `BmpDecoder::read_image` ≈ 121ms」，
//! 回答 660ms 落在哪（逐像素标量核？分配？二次整缓冲拷贝？），并量化现有
//! `kimg_webp::rgb8_to_rgba8` SSSE3 快道在 debug 下的真实成本，及
//! 常数提升 / AVX2 变体是否有 headroom。
//!
//! 目录 env `IMG_BMP_CORPUS_DIR`（与 kimg_bmp 探针同款）；`--ignored --nocapture` 运行。

use std::path::PathBuf;
use std::time::Instant;

use crate::buffer::{RgbImage, RgbaImage};
use crate::dynamic_image::DynamicImage;
use crate::free_functions::load_from_memory;
use crate::ImageDecoder as _;

fn corpus_dir() -> PathBuf {
    // 统一走 kimg_core::testutil::corpus_dir：env `IMG_BMP_CORPUS_DIR` 覆盖；
    // 缺省 `<workspace>/Benchmarks/corpus/BMP`（原硬编码绝对路径 = 同路径，改相对更通用，K66）。
    // 缺失/无法读 → panic（与原 read_dir panic 语义一致）。
    kimg_core::testutil::corpus_dir("IMG_BMP_CORPUS_DIR", "BMP")
        .expect("语料目录缺失：设置 IMG_BMP_CORPUS_DIR 或装配 Benchmarks/corpus/BMP")
}

/// 语料最大 BMP（与 kimg_bmp probe 同口径：按像素数）。
fn largest_bmp_bytes() -> (String, Vec<u8>) {
    let dir = corpus_dir();
    let mut best: Option<(String, Vec<u8>, u64)> = None;
    for entry in std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读取语料目录失败 {}: {e}", dir.display()))
        .flatten()
    {
        let p = entry.path();
        if p.extension()
            .and_then(|e| e.to_str())
            .is_none_or(|e| !e.eq_ignore_ascii_case("bmp"))
        {
            continue;
        }
        let bytes = std::fs::read(&p).unwrap_or_default();
        if bytes.len() < 54 || &bytes[0..2] != b"BM" {
            continue;
        }
        let w = u32::from_le_bytes(bytes[18..22].try_into().unwrap()) as u64;
        let h = u32::from_le_bytes(bytes[22..26].try_into().unwrap()) as u64;
        let px = w * h;
        if best.as_ref().is_none_or(|(_, _, b)| px > *b) {
            best = Some((p.file_name().unwrap().to_string_lossy().to_string(), bytes, px));
        }
    }
    let (name, bytes, _) = best.expect("语料目录无 BMP");
    (name, bytes)
}

/// 中位墙钟（5 轮取中间值，抗抖动）。
fn median_5<F: FnMut()>(mut f: F) -> f64 {
    let mut v = Vec::with_capacity(5);
    for _ in 0..5 {
        let t = Instant::now();
        f();
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[2]
}

/// 本地变体 A：常数提出循环外的现有 SSSE3 4px 算法（对照现有内核是否被
/// 每轮 `_mm_setr_epi8` 常数重建拖慢）。与现有内核逐字节一致。
/// 与 raw/predictor.rs::rgb8_to_rgba8 同步，改核须同步此探针。
#[cfg(target_arch = "x86_64")]
fn variant_hoisted_ssse3(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    let n = src.len() / 3;
    let mut i = 0usize;
    unsafe {
        let shuf = _mm_setr_epi8(0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1);
        let alpha = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
        let s = src.as_ptr();
        let d = dst.as_mut_ptr();
        while i + 4 <= n {
            let lo = _mm_loadl_epi64(s.add(i * 3) as *const __m128i);
            let hi32 = std::ptr::read_unaligned(s.add(i * 3 + 8) as *const u32) as i32;
            let hi = _mm_cvtsi32_si128(hi32);
            let data = _mm_unpacklo_epi64(lo, hi);
            let out = _mm_shuffle_epi8(data, shuf);
            _mm_storeu_si128(
                d.add(i * 4) as *mut __m128i,
                _mm_or_si128(out, alpha),
            );
            i += 4;
        }
    }
    for (px, out) in src[i * 3..].chunks_exact(3).zip(dst[i * 4..].chunks_exact_mut(4)) {
        out[0] = px[0];
        out[1] = px[1];
        out[2] = px[2];
        out[3] = 255;
    }
}

/// 本地变体 B：RGB8→RGBA8 AVX2（bmp.rs bgr24_to_rgba_avx2 同款结构）：
/// 两次 16 字节载入各出 4 RGBA（v0=px[0..3]、v1=px[4..7]、v1 偏移 12 重叠载入），拼 256 位一次写。
/// 与现有内核逐字节一致。前进条件 `3i+28<=3n` 保证两次载入整段在输入内（尾像素标量）。
/// 与 raw/predictor.rs::rgb8_to_rgba8 同步，改核须同步此探针。
#[cfg(target_arch = "x86_64")]
fn variant_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    let n = src.len() / 3;
    let mut i = 0usize;
    unsafe {
        let shuf = _mm_setr_epi8(0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1);
        let alpha = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
        let s = src.as_ptr();
        let d = dst.as_mut_ptr();
        while 3 * i + 28 <= 3 * n {
            let v0 = _mm_loadu_si128(s.add(i * 3) as *const __m128i);
            let v1 = _mm_loadu_si128(s.add(i * 3 + 12) as *const __m128i);
            let o0 = _mm_or_si128(_mm_shuffle_epi8(v0, shuf), alpha);
            let o1 = _mm_or_si128(_mm_shuffle_epi8(v1, shuf), alpha);
            _mm256_storeu_si256(d.add(i * 4) as *mut __m256i, _mm256_set_m128i(o1, o0));
            i += 8;
        }
    }
    for (px, out) in src[i * 3..].chunks_exact(3).zip(dst[i * 4..].chunks_exact_mut(4)) {
        out[0] = px[0];
        out[1] = px[1];
        out[2] = px[2];
        out[3] = 255;
    }
}

/// 分量表：8.9MP Rgb8 → Rgba8 的 to_rgba8 各阶段 debug 墙钟。
#[test]
#[ignore]
fn probe_rgb8_to_rgba8_breakdown() {
    let (name, bytes) = largest_bmp_bytes();

    let dyn_a = load_from_memory(&bytes).expect("BMP 解码失败");
    let (w, h) = dyn_a.dimensions();
    let n = (w as usize) * (h as usize);
    eprintln!("\n[R2R-PROBE] {} {}×{} n={} px", name, w, h, n);

    // 基准 A：整链路 load_from_memory().to_rgba8()
    let tot_full = median_5(|| {
        let img = load_from_memory(&bytes).expect("BMP 解码失败");
        let rgba = img.to_rgba8();
        std::hint::black_box(rgba.into_raw());
    });

    // 基准 B：仅 load（DynamicImage Rgb8）
    let tot_load = median_5(|| {
        let img = load_from_memory(&bytes).expect("BMP 解码失败");
        std::hint::black_box(img);
    });

    // 基准 C：BmpDecoder::read_image 单独
    let dec_read = median_5(|| {
        let dec = kimg_bmp::BmpDecoder::new(std::io::Cursor::new(&bytes)).expect("BMP 解码失败");
        let mut raw = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut raw).expect("BMP 解码失败");
        std::hint::black_box(raw);
    });

    // 准备 ImageRgb8 与固定 RGB 源
    let dec = kimg_bmp::BmpDecoder::new(std::io::Cursor::new(&bytes)).expect("BMP 解码失败");
    let color = dec.color_type();
    let total = dec.total_bytes() as usize;
    let mut raw = vec![0u8; total];
    dec.read_image(&mut raw).expect("BMP 解码失败");
    let rgb_img = RgbImage::from_raw(w, h, raw).expect("buffer 尺寸已校验");
    let dyn_rgb = DynamicImage::ImageRgb8(rgb_img.clone());
    let src_slice: &[u8] = rgb_img.as_raw();

    // to_rgba8 整段（给定 ImageRgb8）
    let t2r_total = median_5(|| {
        let rgba = dyn_rgb.to_rgba8();
        std::hint::black_box(rgba.into_raw());
    });

    // 分量 1：分配（vec![0u8; 4n]，含清零）
    let alloc_ms = median_5(|| {
        let out = vec![0u8; n * 4];
        std::hint::black_box(out);
    });

    // 分量 2：kimg_webp::rgb8_to_rgba8 内核（现有 SSSE3 快道）
    let kernel_ms = median_5(|| {
        let mut out = vec![0u8; n * 4];
        kimg_webp::rgb8_to_rgba8(src_slice, &mut out);
        std::hint::black_box(out);
    });

    // 分量 3：RgbaImage::from_raw 包装
    let from_raw_ms = median_5(|| {
        let out = vec![0u8; n * 4];
        let img = RgbaImage::from_raw(w, h, out).expect("buffer 尺寸已校验");
        std::hint::black_box(img);
    });

    // 纯标量 3→4 循环下限（读 3n + 写 4n）
    let cp_ms = median_5(|| {
        let src = vec![0x5Au8; n * 3];
        let mut dst = vec![0u8; n * 4];
        for (px, o) in src.chunks_exact(3).zip(dst.chunks_exact_mut(4)) {
            o[..3].copy_from_slice(px);
            o[3] = 255;
        }
        std::hint::black_box(dst);
    });

    eprintln!(
        "[R2R-PROBE] full(to_rgba8) {:8.2}ms | loadのみ {:8.2}ms | dec_readのみ {:8.2}ms | to_rgba8(给定Rgb8) {:8.2}ms",
        tot_full, tot_load, dec_read, t2r_total
    );
    eprintln!(
        "[R2R-PROBE]   to_rgba8 分量: alloc(清零) {:7.3}ms + 内核(rgb8_to_rgba8) {:7.3}ms + from_raw {:7.3}ms | 标量3→4下限 {:7.3}ms | color_type={color:?}",
        alloc_ms, kernel_ms, from_raw_ms, cp_ms
    );

    // 变体对照（同输入、同 3→4 语义）
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected;
        eprintln!(
            "[R2R-PROBE]   特性: ssse3={} sse4.1={} avx2={}",
            is_x86_feature_detected!("ssse3"),
            is_x86_feature_detected!("sse4.1"),
            is_x86_feature_detected!("avx2")
        );
        if is_x86_feature_detected!("ssse3") {
            let hoist_ms = median_5(|| {
                let mut out = vec![0u8; n * 4];
                variant_hoisted_ssse3(src_slice, &mut out);
                std::hint::black_box(out);
            });
            eprintln!("[R2R-PROBE]   现有核 {:7.3}ms | 常数提升SSSE3-4px {:7.3}ms", kernel_ms, hoist_ms);
        }
        if is_x86_feature_detected!("avx2") {
            let avx2_ms = median_5(|| {
                let mut out = vec![0u8; n * 4];
                variant_avx2(src_slice, &mut out);
                std::hint::black_box(out);
            });
            eprintln!("[R2R-PROBE]   AVX2-8px {:7.3}ms", avx2_ms);
        }
    }
}

/// 逐字节恒等自证：现有核 / 常数提升 SSSE3 / AVX2 三路输出与 `[r,g,b,255]` 标量一致。
#[test]
#[ignore]
fn probe_rgb8_to_rgba8_byte_identical() {
    // 构造含奇数宽度特征的尺寸：非 4/8 倍像素尾、宽 1…17 的混合图
    for n in [0usize, 1, 2, 3, 4, 5, 15, 16, 17, 1000_001] {
        let mut src = Vec::with_capacity(n * 3);
        for i in 0..n * 3 {
            src.push((i.wrapping_mul(131) ^ 7) as u8);
        }
        let mut ref_out = vec![0u8; n * 4];
        for (px, o) in src.chunks_exact(3).zip(ref_out.chunks_exact_mut(4)) {
            o[0] = px[0];
            o[1] = px[1];
            o[2] = px[2];
            o[3] = 255;
        }
        let mut out = vec![0u8; n * 4];
        kimg_webp::rgb8_to_rgba8(&src, &mut out);
        assert_eq!(
            out, ref_out,
            "现有核逐字节恒等失败 n={n}"
        );
        #[cfg(target_arch = "x86_64")]
        if n >= 4 && std::arch::is_x86_feature_detected!("ssse3") {
            let mut o2 = vec![0u8; n * 4];
            variant_hoisted_ssse3(&src, &mut o2);
            assert_eq!(o2, ref_out, "常数提升SSSE3 恒等失败 n={n}");
        }
        #[cfg(target_arch = "x86_64")]
        if n >= 8 && std::arch::is_x86_feature_detected!("avx2") {
            let mut o3 = vec![0u8; n * 4];
            variant_avx2(&src, &mut o3);
            assert_eq!(o3, ref_out, "AVX2 恒等失败 n={n}");
        }
        // 变体间互相恒等
        #[cfg(target_arch = "x86_64")]
        if n >= 8
            && std::arch::is_x86_feature_detected!("ssse3")
            && std::arch::is_x86_feature_detected!("avx2")
        {
            let mut a = vec![0u8; n * 4];
            let mut b = vec![0u8; n * 4];
            variant_hoisted_ssse3(&src, &mut a);
            variant_avx2(&src, &mut b);
            assert_eq!(a, b, "SSSE3 vs AVX2 不一致 n={n}");
        }
    }
    eprintln!("[R2R-PROBE] 逐字节恒等自证通过（现有核/SSSE3/AVX2 ≡ 标量 [r,g,b,255]）");
}
