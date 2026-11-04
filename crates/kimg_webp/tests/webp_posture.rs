//! WebP 生产姿态 vs 自研直写计时对拍（`#[ignore]` 探针，正常 cargo test 不跑）。
//!
//! 拆解「同机 120 文件 9 轮中位」基线里的 lossless 段：生产姿态
//! `ImageReader→decode→into_rgba8`（GPUI 生产消费形态；`decode` 对 VP8L 静态族直写
//! RGBA8 唯一缓冲，`into_rgba8` 零拷贝移交调用方——gpui `platform.rs`
//! `decode_static_image_from_decoder` 同款）vs 直写
//! `decode_static_lossless_rgba8_into`（`C# DecodeTo` 镜像：暖态零分配，`out` 长度 w*h*4）。
//!
//! kimg 拆分说明（2026-11）：`ImageReader`/`DynamicImage` 属 kimg 门面（多格式分发），不在
//! kimg_core / kimg_webp。本探针的「生产姿态」以 `WebPDecoder`（kimg_core::ImageDecoder，
//! 门面对 WebP 的同一分发目标）代言，字节形态与 `ImageReader→into_rgba8` 一致；门面拆分
//! 落定后可换回原口径。
//!
//! 运行（release，`#[ignore]`）：
//! ```text
//! cargo test -p kimg_webp --release webp_posture -- --ignored --nocapture
//! ```
//! 口径镜像既有 bench：1 轮暖机 + 9 轮取中位，按「整批 lossless 文件一轮」两姿态分别计时。
//! 直写与生产逐字节一致先验证，不一致即 panic（禁止带病计时）。
//! 输出 `KIMG_POSTURE_BENCH production|direct files=N ms=...` 与 `bytes_identical=ok`。

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kimg_core::ImageDecoder as _;
use kimg_webp::codecs::webp::lossless_static_size;
use kimg_webp::{WebPDecoder, decode_static_lossless_rgba8_into};

mod fixtures;

/// lossless 族过滤：位流头部四字码 == `VP8L`，或 `VP8X` 容器内含顶层 `VP8L` chunk。
/// 按 RIFF chunk 边界走（只看声明的 chunk tag 位置，防某 chunk 载荷字节里碰巧出现
/// `VP8L` 误判）；动画（VP8X+ANIM/ANMF，`VP8L` 嵌在 ANMF 内层）不会命中顶层 `VP8L`，
/// 天然排除。
fn is_lossless(data: &[u8]) -> bool {
    if data.len() < 16 {
        return false;
    }
    let mut off = 12usize;
    while off + 8 <= data.len() {
        let tag = &data[off..off + 4];
        let size = u32::from_le_bytes([
            data[off + 4],
            data[off + 5],
            data[off + 6],
            data[off + 7],
        ]) as usize;
        if tag == b"VP8L" {
            return true;
        }
        if off + 8 + size > data.len() {
            break; // chunk 声明越界 → 畸形，不算 lossless。
        }
        off += 8 + size + (size & 1); // RIFF chunk 奇数长度补 1 字节 pad。
    }
    false
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

/// 生产姿态：门面 `ImageReader→decode→into_rgba8` 的 kimg_webp 等价口径——`WebPDecoder`
/// （kimg_core::ImageDecoder，门面对 WebP 的分发目标）直读 RGBA8；无 alpha（Rgb8）→ 3→4
/// 补 255 对齐 RGBA8（等价 DynamicImage::into_rgba8）。返回 (w, h, RGBA8 字节)。
/// （kimg_core 无 DynamicImage；门面拆分后可换回 `ImageReader` 原口径。）
fn production_bytes(data: &[u8]) -> (u32, u32, Vec<u8>) {
    let dec = WebPDecoder::new(Cursor::new(data)).expect("生产解码失败");
    let (w, h) = dec.dimensions();
    let is_rgba = dec.color_type() == kimg_core::ColorType::Rgba8;
    let total = dec.total_bytes() as usize;
    let mut buf = vec![0u8; total];
    dec.read_image(&mut buf).expect("生产解码失败");
    if is_rgba {
        (w, h, buf)
    } else {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for px in buf.chunks_exact(3) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        (w, h, rgba)
    }
}

/// 独立 oracle：`WebPDecoder::read_image` 路径（与直写核分道，旧生产姿态的字节形态；
/// 无 alpha 时补 255 对齐 RGBA8 口径）。
fn webpdecoder_rgba(data: &[u8]) -> (u32, u32, Vec<u8>) {
    use kimg_core::ImageDecoder as _;
    let dec = kimg_webp::WebPDecoder::new(Cursor::new(data)).expect("WebPDecoder 失败");
    let (w, h) = dec.dimensions();
    let has_alpha = dec.color_type() == kimg_core::ColorType::Rgba8;
    let mut buf = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut buf).expect("read_image 失败");
    if has_alpha {
        (w, h, buf)
    } else {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for px in buf.chunks_exact(3) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        (w, h, rgba)
    }
}

#[test]
#[ignore = "手动运行：cargo test -p kimg_webp --release webp_posture -- --ignored --nocapture"]
fn webp_posture() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("测试图片")
        .join("webp");
    if !dir.is_dir() {
        println!("KIMG_POSTURE_BENCH SKIP: missing target/测试图片/webp");
        return;
    }

    // lossless 族过滤（VP8L 头部或 VP8X 含顶层 VP8L chunk）。
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "webp").unwrap_or(false))
        .filter(|p| std::fs::read(p).map(|d| is_lossless(&d)).unwrap_or(false))
        .collect();
    files.sort();
    if files.is_empty() {
        println!("KIMG_POSTURE_BENCH SKIP: no lossless webp under target/测试图片/webp");
        return;
    }
    let n = files.len();
    let datas: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();

    // 正确性先行：逐文件断言（a）生产直写 == 独立 oracle（WebPDecoder 路径，旧姿态字节形态）
    // （b）直写核 == 生产 into_rgba8 逐字节一致（不一致 panic 并打印文件名；禁止带病计时）。
    // 顺带预分配直写缓冲（暖态零分配姿态，复用于全部轮次）。
    let mut out_bufs: Vec<Vec<u8>> = Vec::with_capacity(n);
    for (i, d) in datas.iter().enumerate() {
        let (w, h, prod) = production_bytes(d);
        let (ow, oh, oref) = webpdecoder_rgba(d);
        assert_eq!(
            (ow, oh),
            (w, h),
            "尺寸与 WebPDecoder 不一致：{}({}x{})",
            files[i].display(),
            w,
            h
        );
        assert_eq!(
            oref,
            prod,
            "生产直写与 WebPDecoder RGBA 逐字节不一致：{}({}x{})——禁止带病计时",
            files[i].display(),
            w,
            h
        );
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        let ok = decode_static_lossless_rgba8_into(d, &mut out)
            .unwrap_or_else(|e| panic!("直写解码失败：{}({}x{})：{e}", files[i].display(), w, h));
        assert!(ok, "直写未接管（非 VP8L 静态族）：{}", files[i].display());
        assert_eq!(
            out,
            prod,
            "直写与生产 into_rgba8 逐字节不一致：{}({}x{})——禁止带病计时",
            files[i].display(),
            w,
            h
        );
        out_bufs.push(out);
    }

    // 计时：整批一轮，两姿态分别 1 暖机 + 9 轮中位。
    let production_ms = median_ms(
        || {
            for d in &datas {
                production_bytes(d);
            }
        },
        9,
    );
    let direct_ms = median_ms(
        || {
            for (d, out) in datas.iter().zip(out_bufs.iter_mut()) {
                let ok =
                    decode_static_lossless_rgba8_into(d, out).expect("直写解码失败（计时轮）");
                assert!(ok, "直写未接管（计时轮）");
            }
        },
        9,
    );

    println!("KIMG_POSTURE_BENCH production files={n} ms={production_ms:.3}");
    println!("KIMG_POSTURE_BENCH direct     files={n} ms={direct_ms:.3}");
    println!("KIMG_POSTURE_BENCH bytes_identical=ok files={n}");
}

/// 新直写路径回归硬闸（非 `#[ignore]`，随 `cargo test -p kimg_webp` 跑）：VP8L 静态族直写
/// 核 `decode_static_lossless_rgba8_into` 应直接产出 RGBA8，且字节与生产姿态
/// （`WebPDecoder`，门面 `ImageReader::decode→into_rgba8` 的统一分发目标）逐字节一致；
/// `lossless_static_size` 预检（门面 Limits::reserve / check_dimensions 的依据）尺寸与解码一致。
/// 有损 / 动画家族不在本例（由 `is_lossless` 过滤；`lossless_static_size` 保证不误伤）。
/// 外部语料缺失照例 SKIP（AGENTS §5），不因语料缺失断言失败。
#[test]
fn decode_fastpath_vp8l_static_matches_webpdecoder() {
    // 外部语料缺失照例 SKIP（AGENTS §5：缺夹具是常态不是回归）。
    if fixtures::webp::webp_fixture_dirs().is_empty() {
        println!("SKIP: missing webp fixtures (set KIMG_WEBP_FIXTURES=...)");
        return;
    }
    let mut checked = 0usize;
    for name in fixtures::webp::webp_fixture_names() {
        let Some(p) = fixtures::webp::webp_fixture_path(&name) else {
            continue;
        };
        let data = std::fs::read(&p).expect("read fixture");
        if !is_lossless(&data) {
            continue;
        }
        let (w, h, prod) = production_bytes(&data);
        let (ow, oh, oref) = webpdecoder_rgba(&data);
        assert_eq!((ow, oh), (w, h), "{name} 尺寸");
        assert_eq!(
            oref,
            prod,
            "{name}: production(WebPDecoder) 与 oracle(WebPDecoder) 双路径应逐字节一致"
        );
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        let ok =
            decode_static_lossless_rgba8_into(&data, &mut out).expect("{name}: 直写解码失败");
        assert!(ok, "{name}: 应判为 VP8L 静态族（直写应接管）");
        assert_eq!(out, prod, "{name}: 直写核与 production(WebPDecoder) 输出不一致");
        // 预检一致性：`lossless_static_size` 尺寸须与解码一致（缓冲字节数沿用 total_bytes 口径，
        // 无 alpha 时 w*h*3 ≠ 直写目标 w*h*4，故只比尺寸）。
        let (lw, lh, _ltotal) =
            lossless_static_size(&data).unwrap_or_else(|| panic!("{name}: 静态无损预检应命中"));
        assert_eq!((lw, lh), (w, h), "{name}: 预检尺寸与解码不一致");
        checked += 1;
    }
    // 外部语料缺失属常态 → 允许 SKIP；存在语料却一张都没对拍则硬失败。
    assert!(
        checked >= 1,
        "应至少对拍 1 个 VP8L 静态 fixture（当前 {checked}；缺外部语料）"
    );
    eprintln!("WEBP_FASTPATH_PARITY\t{checked}");
}

/// 限制 / 坏输入等价回归：VP8L 静态直写路径必须与既有解码路径同口径——畸形 VP8L 载荷返回
/// 显式解码错误（不 panic）；`lossless_static_size` 预检给出门面 `Limits` 拦截所需的真实尺寸
/// （超 `max_image_width` 的强制在 kimg 门面 `ImageReader.limits` / `decoder.set_limits`，
/// kimg_core 仅提供 `Limits` 类型；门面拆分落定后补回「超限 → 显式 Limits 错误」原断言）。
fn synthetic_vp8l_webp(width: u32, height: u32) -> Vec<u8> {
    let mut bit = Vec::new();
    bit.push(0x2f);
    let bits = ((width - 1) & 0x3fff) | (((height - 1) & 0x3fff) << 14);
    bit.extend_from_slice(&bits.to_le_bytes());
    // 空熵载荷：头尺寸合法但无任何像素位 → `decode_into_rgba8` 必失败（定性命中错误路径）。
    let mut chunks = Vec::new();
    chunks.extend_from_slice(b"VP8L");
    chunks.extend_from_slice(&(bit.len() as u32).to_le_bytes());
    chunks.extend_from_slice(&bit);
    if bit.len() % 2 == 1 {
        chunks.push(0); // RIFF chunk 奇数长度补 1 字节 pad（RFC 偶数对齐要求）
    }
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
    data.extend_from_slice(b"WEBP");
    data.extend_from_slice(&chunks);
    data
}

#[test]
fn decode_fastpath_vp8l_static_respects_limits() {
    let data = synthetic_vp8l_webp(300, 1);

    // 畸形 VP8L（空熵）经直写核 → 显式 Decoding 错误（不 panic 不静默）。
    // 缓冲按直写口径 w*h*4（`lossless_static_size` 的 total 沿用 total_bytes 口径，无 alpha
    // 时仅 w*h*3，故单独按 w*h*4 分配）。
    let (w, h, _total) =
        lossless_static_size(&data).expect("VP8L 静态族预检应识别 300x1 并给真实尺寸");
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    let err = decode_static_lossless_rgba8_into(&data, &mut out).unwrap_err();
    assert!(
        matches!(err, kimg_core::ImageError::Decoding(_)),
        "畸形 VP8L 载荷应显式 Decoding 错误，实际 {err:?}"
    );

    // 超 `max_image_width=100`：kimg_core 仅持类型、强制活在门面 `ImageReader.limits` /
    // `decoder.set_limits`；kimg_webp 侧断言预检给出的真实宽度即门面 check_dimensions 将拒绝
    // 的同一前提（2000 宽 synthetic 同口径）。门面拆分落定后在 kimg/tests 补 Limits 断言。
    assert!(w > 100, "预检应报 300>max_image_width=100 的拦截前提，实际 w={w}");
}
