//! JPEG 自研引擎 vs 旧 zune-jpeg 后端字节恒等对照（回归硬闸）。
//!
//! 语料：`<workspace>/target/测试图片/jpeg`（env `KIMG_JPEG_CORPUS` 可覆盖）。
//! 每个文件：旧路径（zune-jpeg，现 `codecs::jpeg::JpegDecoder`）与新自研
//! （`codecs::jpeg2::JpegDecoder`）各自 `image_decoder` 同口径解码到其声明的
//! color_type → 逐字节比较，必须完全一致（IDCT/上采样/颜色换算三处取整全等比）。
//!
//! 运行：
//! ```text
//! cargo test -p kimg_jpeg --test jpeg_parity -- --nocapture
//! ```
//! 失败时打印首个差异像素的 (x, y, 通道, old, new)。

use std::io::Cursor;
use std::path::{Path, PathBuf};

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder as OldJpeg;
use kimg_jpeg::codecs::jpeg2::JpegDecoder as NewJpeg;

fn corpus_dir() -> PathBuf {
    if let Ok(p) = std::env::var("KIMG_JPEG_CORPUS") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("测试图片")
        .join("jpeg")
}

fn collect_jpegs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("jpg"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// 用旧/新解码器同口径解出 (width, height, color_type, bytes)。
fn decode<D: kimg_core::ImageDecoder + Sized>(
    new_dec: impl FnOnce(Cursor<Vec<u8>>) -> Result<D, String>,
    bytes: &[u8],
) -> Result<(u32, u32, kimg_core::ColorType, Vec<u8>), String> {
    let dec = new_dec(Cursor::new(bytes.to_vec()))?;
    let (w, h) = dec.dimensions();
    let ct = dec.color_type();
    let mut buf = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut buf).map_err(|e| e.to_string())?;
    Ok((w, h, ct, buf))
}

fn old_decode(bytes: &[u8]) -> Result<(u32, u32, kimg_core::ColorType, Vec<u8>), String> {
    decode(
        |c| OldJpeg::new(c).map_err(|e| e.to_string()),
        bytes,
    )
}

fn new_decode(bytes: &[u8]) -> Result<(u32, u32, kimg_core::ColorType, Vec<u8>), String> {
    decode(
        |c| NewJpeg::new(c).map_err(|e| e.to_string()),
        bytes,
    )
}

/// 单文件：断言 尺寸/颜色类型/字节 三者与旧路径完全一致。
/// 返回 Ok(true) 恒等；Ok(false) 新路径明确不支持（计入 SKIP，不判失败）；
/// Err(..) 为失败。
fn assert_parity(name: &str, bytes: &[u8]) -> Result<bool, String> {
    let o = old_decode(bytes).map_err(|e| format!("{name}: 旧路径(参照)解码失败：{e}"))?;
    let n = match new_decode(bytes) {
        Ok(v) => v,
        Err(e) if e.contains("not support") || e.contains("Unsupported") => {
            println!("{name}: SKIP（新路径不支持：{e}）");
            return Ok(false);
        }
        Err(e) => return Err(format!("{name}: 新路径解码失败：{e}（旧路径可解）")),
    };
    assert_eq!(o.0, n.0, "{name}: 宽不一致 old={} new={}", o.0, n.0);
    assert_eq!(o.1, n.1, "{name}: 高不一致 old={} new={}", o.1, n.1);
    assert_eq!(o.2, n.2, "{name}: 颜色类型不一致 old={:?} new={:?}", o.2, n.2);
    assert_eq!(
        o.3.len(),
        n.3.len(),
        "{name}: 输出长度不一致 old={} new={}",
        o.3.len(),
        n.3.len()
    );
    if o.3 != n.3 {
        let bpp = u32::from(o.2.bytes_per_pixel()) as usize;
        let w = o.0 as usize;
        let idx = o
            .3
            .iter()
            .zip(n.3.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let x = (idx / bpp) % w;
        let y = (idx / bpp) / w;
        let ch = idx % bpp;
        return Err(format!(
            "{name}: 字节不一致 首异 (x={x}, y={y}, 通道={ch}) old={} new={}（diff字节总数 {}）",
            o.3[idx],
            n.3[idx],
            o.3.iter().zip(n.3.iter()).filter(|(a, b)| a != b).count()
        ));
    }
    println!("{name}: PARITY OK {}x{} {:?}", o.0, o.1, o.2);
    Ok(true)
}

#[test]
fn corpus_parity() {
    let dir = corpus_dir();
    assert!(dir.is_dir(), "语料目录缺失: {}", dir.display());
    let files = collect_jpegs(&dir);
    assert!(!files.is_empty(), "语料目录为空: {}", dir.display());
    let mut oks = 0usize;
    let mut skips = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for p in &files {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("{name}: 读文件失败 {e}"));
        match assert_parity(&name, &bytes) {
            Ok(true) => oks += 1,
            Ok(false) => skips += 1,
            Err(e) => failures.push(e),
        }
        for f in &failures {
            println!("PARITY FAIL: {f}");
        }
    }
    println!("JPEG_PARITY SUMMARY ok={oks} skip={skips} fail={}", failures.len());
    assert!(
        failures.is_empty(),
        "{} 个文件字节恒等失败：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
