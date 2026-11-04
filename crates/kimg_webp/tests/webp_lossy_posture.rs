//! 【解剖探针 · 去留交主 agent 定】WebP 有损 opaque 生产姿态分摊：
//! opaque 静态有损自 T1 起 `decode_static_lossy` 单遍 RGBA4 直出（α=0xFF 熔入发射内层，
//! `OursStatic.rgba` w*h*4，color_type=Rgba8）——`decode().into_rgba8()` 零拷贝移交
//! （DynamicImage::ImageRgba8），生产 `to_rgba8`/`into_rgba8` 不再有 3→4 扩写+零值分配。
//! 本探针同窗对拍 `decode().into_rgba8()`（生产姿态，现零拷贝）vs `decode().into_rgb8()`
//! （ImageRgba8 → 4→3 像素转换；T1 前为 ImageRgb8 零拷贝 unwrap——基线语义随 T1 翻转，
//! 差值不再代表「展开税」，而是「RGBA 直出 vs 4→3 转换」的相对成本；生产绝对耗时下降量
//! 才是 T1 证据）。
//! 另附 `WebPDecoder::new + read_image(预分配 w*h*4 buf)` 作纯内核+RGBA 直拷的交叉参照。
//!
//! 口径镜像既有 bench：1 暖机 + 9 轮中位，整批一轮，按像素数分档输出。逐字节等价先验证
//! （`into_rgba8` 前 3 字节 == `into_rgb8` 逐字节、α==0xFF），不一致即 panic（禁止带病计时）。
//!
//! 运行（release，`#[ignore]`，前台阻塞到结束）：
//! ```text
//! cargo test -p kimg_webp --release webp_lossy_posture -- --ignored --nocapture
//! ```
//! 语料：`target/imgbench-corpus/WEBP` 的 opaque 静态有损（顶层 `VP8 `，或 `VP8X` 无 ALPH/
//! 无 VP8L/非动画）。输出 `KIMG_LOSSY_POSTURE ...` 行。

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kimg_core::ImageDecoder as _;
use kimg_webp::{WebPDecoder, decode_static_lossy_rgba8_into};

/// opaque 静态有损判定：RIFF/WEBP 家族。
/// - 顶层 chunk `VP8 ` → 有损（恒 opaque，无 ALPH 概念）。
/// - 顶层 `VP8L` → 无损，排除。
/// - 顶层 `VP8X`：动画(0x02)/含 `ALPH`/含顶层 `VP8L` 一律排除；否则含顶层 `VP8 ` → opaque 有损。
fn is_opaque_lossy(data: &[u8]) -> bool {
    if data.len() < 30 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return false;
    }
    match &data[12..16] {
        b"VP8 " => true,
        b"VP8L" => false,
        b"VP8X" => {
            let flags = data[20];
            if flags & 0x02 != 0 {
                return false; // 动画
            }
            let mut has_alph = false;
            let mut has_vp8 = false;
            let mut off = 16usize;
            while off + 8 <= data.len() {
                let tag = &data[off..off + 4];
                let size = u32::from_le_bytes([
                    data[off + 4],
                    data[off + 5],
                    data[off + 6],
                    data[off + 7],
                ]) as usize;
                match tag {
                    b"ALPH" => has_alph = true,
                    b"VP8L" => return false,
                    b"VP8 " => has_vp8 = true,
                    _ => {}
                }
                if off + 8 + size > data.len() {
                    break;
                }
                off += 8 + size + (size & 1);
            }
            has_vp8 && !has_alph
        }
        _ => false,
    }
}

/// 逐轮整批计时并取中位（先暖机一轮）。口径镜像既有 bench。
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

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus")
        .join("WEBP")
}

fn collect_opaque_lossy() -> Vec<(String, Vec<u8>, u32, u32)> {
    let dir = corpus_dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "webp").unwrap_or(false) {
            let Ok(b) = std::fs::read(&p) else { continue };
            if is_opaque_lossy(&b) {
                // 门面 ImageReader 拆分前以 WebPDecoder 取尺寸（集合仅一次、不计时，接受整帧解）。
                if let Ok(dec) = WebPDecoder::new(Cursor::new(&b)) {
                    let (w, h) = dec.dimensions();
                    out.push((
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                        b,
                        w,
                        h,
                    ));
                }
            }
        }
    }
    out.sort_by(|a, b| (b.2 as u64 * b.3 as u64).cmp(&(a.2 as u64 * a.3 as u64)).then(a.0.cmp(&b.0)));
    out
}

/// 生产姿态：`decode().into_rgba8()`（T1 起 `ImageRgba8` 零拷贝移交，无扩写/无 RGBA 分配）的
/// kimg_webp 等价——`decode_static_lossy_rgba8_into` 把 opaque 有损直出 RGBA4 到调用方缓冲
/// （kimg_core 无 DynamicImage 的 into_rgba8；门面拆分后可换回原口径）。`(w,h)` 来自集合预读。
fn prod_rgba(data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    let ok = decode_static_lossy_rgba8_into(data, &mut out).expect("直写解码失败");
    assert!(ok, "opaque 有损应被直写接管");
    out
}

/// 对照出口：`decode().into_rgb8()`（`ImageRgba8` → 4→3 像素转换）的 kimg_webp 等价——直写
/// RGBA4 后本地 4→3 抽条（口径待门面；仓内无 RGB3 消费方，与旧 into_rgb8 同为对照口径）。
fn prod_rgb(data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let rgba = prod_rgba(data, w, h);
    let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    rgb
}

/// 纯内核 + 预分配拷贝交叉参照：`WebPDecoder::new`（全部内核）+ `read_image` 到重用 w*h*4 buf
/// （opaque T1 起 color_type=Rgba8，`read_image` RGBA 目标直拷）。
fn kernel_read_rgba_reuse(data: &[u8], buf: &mut Vec<u8>) -> usize {
    let dec = WebPDecoder::new(Cursor::new(data)).expect("dec");
    let (w, h) = dec.dimensions();
    let expect = (w as usize) * (h as usize) * 4;
    if buf.len() != expect {
        *buf = vec![0u8; expect];
    }
    dec.read_image(buf).expect("read_image rgba");
    buf.len()
}

#[test]
#[ignore = "手动运行：cargo test -p kimg_webp --release webp_lossy_posture -- --ignored --nocapture（解剖探针）"]
fn webp_lossy_posture() {
    let files = collect_opaque_lossy();
    if files.is_empty() {
        println!("KIMG_LOSSY_POSTURE SKIP: missing opaque-lossy under target/imgbench-corpus/WEBP");
        return;
    }
    let n = files.len();

    // 正确性先行：逐文件断言 `into_rgba8` 前 3 字节 == `into_rgb8`（4→3 转换）逐字节，且 α==0xFF
    // （禁止带病计时）。
    for (name, d, w, h) in &files {
        let rgba = prod_rgba(d, *w, *h);
        let rgb = prod_rgb(d, *w, *h);
        assert_eq!(rgba.len(), rgb.len() / 3 * 4, "{name}: RGBA/RGB 长度不一致");
        for (i, px) in rgb.chunks_exact(3).enumerate() {
            assert_eq!(&rgba[i * 4..i * 4 + 3], px, "{name} px={i}: rgba 前 3 字节应等于 rgb");
            assert_eq!(rgba[i * 4 + 3], 0xFF, "{name} px={i}: opaque alpha 恒 255");
        }
    }

    // 分档（按像素数）：
    let tier = |w: u32, h: u32| -> &'static str {
        let px = w as u64 * h as u64;
        if px >= 8_000_000 {
            "4k"
        } else if px >= 1_000_000 {
            "hd"
        } else {
            "small"
        }
    };

    // 整批一轮 × 1 暖机 + 9 轮中位，同窗对拍三姿态。
    let rgba_ms = median_ms(
        || {
            for (_, d, w, h) in &files {
                std::hint::black_box(prod_rgba(d, *w, *h));
            }
        },
        9,
    );
    let rgb_ms = median_ms(
        || {
            for (_, d, w, h) in &files {
                std::hint::black_box(prod_rgb(d, *w, *h));
            }
        },
        9,
    );
    // 逐文件同窗 A/B（配对，扣除批内顺序差异）→ 分档占比（按各档文件数平均）。
    let (mut acc_f, mut acc_4k, mut acc_hd, mut acc_small) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut n4, mut nh, mut ns) = (0usize, 0usize, 0usize);
    for (name, d, w, h) in &files {
        let t0 = Instant::now();
        for _ in 0..5 {
            std::hint::black_box(prod_rgba(d, *w, *h));
        }
        let ta = t0.elapsed().as_secs_f64() * 1000.0 / 5.0;
        let t1 = Instant::now();
        for _ in 0..5 {
            std::hint::black_box(prod_rgb(d, *w, *h));
        }
        let tb = t1.elapsed().as_secs_f64() * 1000.0 / 5.0;
        let share = (ta - tb) / ta * 100.0;
        acc_f += share;
        match tier(*w, *h) {
            "4k" => {
                acc_4k += share;
                n4 += 1;
            }
            "hd" => {
                acc_hd += share;
                nh += 1;
            }
            _ => {
                acc_small += share;
                ns += 1;
            }
        }
        println!(
            "KIMG_LOSSY_POSTURE per {name} {}x{} rgba={ta:.3}ms rgb={tb:.3}ms expand_share={share:.1}%",
            w, h
        );
    }
    println!(
        "KIMG_LOSSY_POSTURE SUMMARY files={n} whole_batch rgba={rgba_ms:.3}ms rgb={rgb_ms:.3}ms \
         expand_ms={:.3}ms expand_share={:.1}%",
        rgba_ms - rgb_ms,
        (rgba_ms - rgb_ms) / rgba_ms * 100.0
    );
    println!(
        "KIMG_LOSSY_POSTURE per-file-mean expand_share_all={:.1}% 4k({n4})={:.1}% hd({nh})={:.1}% small({ns})={:.1}%",
        acc_f / n as f64,
        acc_4k / n4.max(1) as f64,
        acc_hd / nh.max(1) as f64,
        acc_small / ns.max(1) as f64
    );

    // 纯内核交叉参照（重用缓冲，隔离分配噪声；逐文件）。
    let mut buf = Vec::new();
    let mut kernel_total = 0.0f64;
    for (name, d, w, h) in &files {
        let tk = Instant::now();
        for _ in 0..5 {
            std::hint::black_box(kernel_read_rgba_reuse(d, &mut buf));
        }
        let k = tk.elapsed().as_secs_f64() * 1000.0 / 5.0;
        kernel_total += k;
        println!("KIMG_LOSSY_POSTURE kernel {name} {}x{} read_rgba_reuse={k:.3}ms", w, h);
    }
    // 生产 decode 相对纯内核(读+预分配直拷 RGBA4)的差额 ≈ WebPDecoder 之外的包装帧分配（w*h*4）。
    println!(
        "KIMG_LOSSY_POSTURE kernel TOTAL files={n} read_rgba_reuse_total={kernel_total:.3}ms \
         vs_prod_rgba_mean={:.1}ms/轮批",
        (rgba_ms / 5.0)
    );
    // 敏感性：去掉熵病态 outliers 后整批扩写占比（454 噪声 4K 一帧占大量墙钟、压扁相对占比时，
    // 说明该文件是「熵解码重」而非「姿态重」）。整批中位数档各文件占比已由 per-file 行给出。
    let x = files.iter().find(|(name, _, _, _)| name.contains("syn-noise-3840x2160"));
    if let Some((_, nd, nw, nh)) = x {
        let mut rest: Vec<(&Vec<u8>, u32, u32)> = Vec::new();
        for (_, d, w, h) in &files {
            if d.as_ptr() != nd.as_ptr() {
                rest.push((d, *w, *h));
            }
        }
        let ra = median_ms(
            || {
                for (d, w, h) in &rest {
                    std::hint::black_box(prod_rgba(d, *w, *h));
                }
            },
            9,
        );
        let rb = median_ms(
            || {
                for (d, w, h) in &rest {
                    std::hint::black_box(prod_rgb(d, *w, *h));
                }
            },
            9,
        );
        println!(
            "KIMG_LOSSY_POSTURE SUMMARY-minus-noise files={} {}x{} rgba={ra:.3}ms rgb={rb:.3}ms \
             expand_share={:.1}%",
            rest.len(),
            nw,
            nh,
            (ra - rb) / ra * 100.0
        );
    }
    eprintln!("KIMG_LOSSY_POSTURE done");
}
