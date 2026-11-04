//! WebP VP8 有损静态 OURS ✕ image-webp 逐字节对拍（V5 收口）+ dwebp fancy golden 抽查。
//!
//! 口径：全部静态有损（无 ALPH）fixture，RGBA8 逐字节一致（主判）；另抽样 fixture 与
//! `webp-oracle-golden.tsv` 的 fancy 列 sha256 对照（dwebp 1.6.0 fancy golden 双 oracle 等价）。
//! 自研侧经 `kimg_webp::WebPDecoder`（自研内核），参照侧直连 `image_webp::WebPDecoder`
//! （dev oracle；kimg 已自包含，运行时零 image-webp 依赖）。

use std::io::Cursor;

use sha2::{Digest, Sha256};

use kimg_webp::{BACKEND_OURS, WebPDecoder, set_webp_backend};
use kimg_core::ImageDecoder;

mod fixtures;

// golden TSV 保留在仓内；图片一律取外部语料（tests/fixtures/webp/mod.rs 定位）。
const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/webp");

fn golden_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(GOLDEN_DIR)
}

/// 存在性驱动取夹具：缺失打印 SKIP 后返回 None（AGENTS §5：缺失属常态，不崩整测试）。
fn fixture_bytes(name: &str) -> Option<Vec<u8>> {
    let p = fixtures::webp::webp_fixture_path(name)?;
    Some(std::fs::read(&p).expect("read fixture"))
}

/// 按当前后端解码静态 fixture 为 RGBA8（无 alpha → 补 255）。
fn decode_rgba_byte_exact(data: &[u8], ours: bool) -> (u32, u32, Vec<u8>) {
    if ours {
        set_webp_backend(BACKEND_OURS);
        let dec = WebPDecoder::new(Cursor::new(data)).expect("WebPDecoder::new 应成功");
        let (w, h) = dec.dimensions();
        let has_alpha = dec.color_type() == kimg_core::ColorType::Rgba8;
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).expect("read_image 应成功");
        if has_alpha {
            (w, h, buf)
        } else {
            let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            (w, h, rgba)
        }
    } else {
        // dev oracle：直连 image_webp::WebPDecoder（自研已自包含，运行时零依赖）。
        let mut dec = image_webp::WebPDecoder::new(Cursor::new(data)).expect("image_webp::new 应成功");
        let (w, h) = dec.dimensions();
        let has_alpha = dec.has_alpha();
        let n = dec.output_buffer_size().expect("image_webp 缓冲大小");
        let mut buf = vec![0u8; n];
        dec.read_image(&mut buf).expect("image_webp read_image 应成功");
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
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// 全部静态（非动画）fixture：OURS vs IMAGEWEBP 逐字节（RGBA8）差异必须 0。
/// 覆盖 VP8 静态有损（34）+ VP8L 静态无损/无损-alpha + ALPH 有损-alpha（V6）。
#[test]
fn static_ours_matches_imagewebp_byte_exact() {
    let mut checked = 0usize;
    let mut diffs: Vec<(String, usize)> = Vec::new();
    let mut entries: Vec<String> = fixtures::webp::webp_fixture_names()
        .into_iter()
        .filter(|n| !n.starts_with("anim-"))
        .collect();
    entries.sort();
    for name in entries {
        let Some(path) = fixtures::webp::webp_fixture_path(&name) else {
            eprintln!("SKIP {name} 外部语料缺失");
            continue;
        };
        let data = std::fs::read(&path).expect("read fixture");
        let (w1, h1, ours) = decode_rgba_byte_exact(&data, true);
        let (w2, h2, img) = decode_rgba_byte_exact(&data, false);
        assert_eq!((w1, h1), (w2, h2), "{name} 尺寸 OURS vs image-webp");
        assert_eq!(ours.len(), img.len(), "{name} 缓冲长");
        let d = ours.iter().zip(img.iter()).filter(|(a, b)| a != b).count();
        checked += 1;
        if d != 0 {
            // 金标准（SimdCodecs 52 图名）不匹配=失败；越界语料（用户/imgbench-corpus）
            // 仅记录不判失败（AGENTS §5：找到几个测几个）。
            if fixtures::webp::is_golden_static_name(&name) {
                diffs.push((name.clone(), d));
            } else {
                eprintln!("VP8_PARITY_EXTRA_DIFF\t{name}\t{d} bytes（非金标准外部语料，仅记录）");
            }
        } else {
            eprintln!("VP8_PARITY_OK\t{name}\t{}x{}", w1, h1);
        }
    }
    for (name, d) in &diffs {
        eprintln!("VP8_PARITY_DIFF\t{name}\t{d} bytes");
    }
    eprintln!("VP8_PARITY_CHECKED\t{checked}, mismatches={}", diffs.len());
    set_webp_backend(BACKEND_OURS);
    assert!(checked >= 6, "应至少对拍 6 个静态 fixture，实 {checked}");
    assert!(diffs.is_empty(), "{} 个静态 fixture 与 image-webp 逐字节不符", diffs.len());
}

/// 判静态有损（VP8 载荷）：裸 `VP8 ` 或 `VP8X` 静态（非动画、无 VP8L）位流。
fn is_static_lossy(data: &[u8]) -> bool {
    if data.len() < 20 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return false;
    }
    match &data[12..16] {
        b"VP8 " => true,
        b"VP8L" => false,
        b"VP8X" => {
            if data[20] & 0x02 != 0 {
                return false;
            }
            !data.windows(4).any(|w| w == b"VP8L")
        }
        _ => false,
    }
}

/// 以指定 `KIMG_WEBP_THREADS` 走 OURS 后端解码为 RGBA8（无 alpha 补 255）。
fn decode_ours_with_threads(data: &[u8], threads: &str) -> (u32, u32, Vec<u8>) {
    unsafe {
        std::env::set_var("KIMG_WEBP_THREADS", threads);
    }
    set_webp_backend(BACKEND_OURS);
    let dec = WebPDecoder::new(Cursor::new(data)).expect("WebPDecoder::new 应成功");
    let (w, h) = dec.dimensions();
    let has_alpha = dec.color_type() == kimg_core::ColorType::Rgba8;
    let mut buf = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut buf).expect("read_image 应成功");
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

/// 硬闸（集成层）：双线程分带滤波（threads=2）与串行（threads=0）全部静态有损 fixture
/// RGBA8 逐字节一致——并行路径仅调度，字节语义与现串行完全相等（数字孪生校验）。
#[test]
fn static_lossy_threads2_matches_serial_byte_exact() {
    let mut checked = 0usize;
    for name in fixtures::webp::webp_fixture_names() {
        let Some(p) = fixtures::webp::webp_fixture_path(&name) else {
            continue;
        };
        let data = std::fs::read(&p).expect("read fixture");
        if !is_static_lossy(&data) {
            continue;
        }
        let (w1, h1, serial) = decode_ours_with_threads(&data, "0");
        let (w2, h2, para) = decode_ours_with_threads(&data, "2");
        assert_eq!((w1, h1), (w2, h2), "{name} 尺寸 threads=2 vs threads=0");
        assert_eq!(
            para, serial,
            "{name}: OURS threads=2 与 threads=0 RGBA 逐字节不一致"
        );
        checked += 1;
    }
    unsafe {
        std::env::remove_var("KIMG_WEBP_THREADS");
    }
    set_webp_backend(BACKEND_OURS);
    assert!(
        checked >= 3,
        "应至少对拍 3 个静态有损 fixture（当前 {checked}；缺外部语料）"
    );
    eprintln!("WEBP_MT_INTEGRATION_PARITY_OK\t{checked}");
}

/// 抽样与 dwebp fancy golden 列对照（双 oracle 等价抽查）。
#[test]
fn lossy_sample_matches_dwebp_fancy_golden() {
    let golden_path = golden_dir().join("webp-oracle-golden.tsv");
    let text = std::fs::read_to_string(&golden_path).expect("golden TSV");
    let mut rows: std::collections::BTreeMap<String, (u32, u32, String, String, String)> =
        std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() == 6 {
            rows.insert(
                p[0].to_string(),
                (
                    p[1].parse().unwrap(),
                    p[2].parse().unwrap(),
                    p[3].to_lowercase(),
                    p[4].to_lowercase(),
                    p[5].to_lowercase(),
                ),
            );
        }
    }
    // 抽查（有损）：flat/gradient/astronaut/testsrc2/noise/odd/tiny。
    let samples = [
        "flat-gray-16x16.webp",
        "gradient-16x16.webp",
        "astronaut-192-q10-normal.webp",
        "astronaut-192-q20-simple.webp",
        "testsrc2-320x240-q75.webp",
        "noise-32x32-q50.webp",
        "odd-33x19-libvpx.webp",
        "tiny.webp",
        "noise-64x48-vp8-partitions8.webp",
    ];
    let mut checked = 0usize;
    for name in samples {
        let Some(path) = fixtures::webp::webp_fixture_path(name) else {
            eprintln!("SKIP {name} 外部语料缺失");
            continue;
        };
        let Some((gw, gh, _gyuv, _gnofancy, gfancy)) = rows.get(name).cloned() else {
            continue;
        };
        let data = std::fs::read(&path).unwrap();
        let (w, h, rgba) = decode_rgba_byte_exact(&data, true);
        assert_eq!((w, h), (gw, gh), "{name} 尺寸 vs golden");
        let got = sha256_hex(&rgba);
        assert_eq!(got, gfancy, "{name} fancy RGBA 应与 dwebp golden 一致");
        checked += 1;
    }
    set_webp_backend(BACKEND_OURS);
    assert!(checked >= 3, "应至少抽查 3 个 fixture，实 {checked}");
    eprintln!("VP8_FANCY_GOLDEN_SPOT\t{checked}");
}

/// 右缘不完整宏块路径回归（V7 修为真 BUG 后的硬闸）：5 张代表图（含 w%16=1/2/3）
/// OURS fancy RGBA 必须 == libwebp 1.6.0 golden sha256。
/// 回归前此路径 OURS 唯一偏出（Y 平面右缘 1-4 宏块列，源自 `prepare_above` 右上像素
/// 夹到可见末列 vs 参照镜像 padded 末列）；golden 文本入仓、图片外部语料缺失 SKIP。
#[test]
fn right_edge_incomplete_mb_matches_libwebp_golden() {
    let golden_path = golden_dir().join("webp-right-edge-golden.tsv");
    let text = std::fs::read_to_string(&golden_path).expect("右缘 golden TSV");
    let mut rows: Vec<(String, u32, u32, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() == 4 {
            rows.push((p[0].to_string(), p[1].parse().unwrap(), p[2].parse().unwrap(), p[3].to_lowercase()));
        }
    }
    let mut checked = 0usize;
    for (name, gw, gh, ghsh) in rows {
        let Some(path) = fixtures::webp::webp_fixture_path(&name) else {
            eprintln!("SKIP {name} 外部语料缺失");
            continue;
        };
        let data = std::fs::read(&path).unwrap();
        let (w, h, rgba) = decode_rgba_byte_exact(&data, true);
        assert_eq!((w, h), (gw, gh), "{name} 尺寸 vs libwebp golden");
        // 修复前（`prepare_above` 夹到 width-1）此处 FAIL：5 图 diff 9..2207 px。
        let got = sha256_hex(&rgba);
        assert_eq!(got, ghsh, "{name} OURS fancy RGBA 应与 libwebp 1.6.0 golden 一致");
        checked += 1;
    }
    set_webp_backend(BACKEND_OURS);
    assert!(
        checked >= 3,
        "应至少核对 3 个右缘 golden 条目（当前 {checked}；缺外部语料）"
    );
    eprintln!("VP8_RIGHT_EDGE_GOLDEN\t{checked}");
}
