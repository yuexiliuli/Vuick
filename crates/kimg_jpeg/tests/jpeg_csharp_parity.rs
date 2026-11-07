//! JPEG 自研引擎 vs C# Sdcb.SimdCodecs.Jpeg `DecodeTo` 参照字节恒等对照（正确性硬闸）。
//!
//! 语料：`<workspace>/Benchmarks/corpus/JPEG`（446 = Sdcb 官方矩阵；env `KIMG_JPEG_CORPUS` 可覆盖）。
//! 参照：`<workspace>/target/csharp-ref`（外部 C# runner 产出，每语料一张）：
//! `<stem>.<w>x<h>.{gray|rgb}`。gray = 灰度 1B/像素（= 我们 `ColorType::L8`），
//! rgb = 交错 RGB 3B/像素（= 我们 `ColorType::Rgb8`）。
//! 自研 `codecs::jpeg::JpegDecoder` 以 `ImageDecoder` 同口径解码到其声明的 color_type，
//! 再与 C# 参照逐字节比较；尺寸/颜色类型也一并核对。
//!
//! 运行：
//! ```text
//! cargo test -p kimg_jpeg --test jpeg_csharp_parity -- --nocapture
//! ```
//! 失败时打印首个差异像素的 (x, y, 通道, old(ref), new(ours))。
//!
//! P-A2 双模式验证：`KIMG_JPEG_DUAL_SYM=1` 与缺省（0）各跑一遍本套件，均 ok=19 才算过
//! （K27 可复跑姿势；同进程内 OnceLock 一次判定，须分进程跑，如两次 `--test-threads=1`）。

mod fixtures;

use std::io::Cursor;
use std::path::{Path, PathBuf};

use kimg_core::ColorType;
use kimg_jpeg::codecs::jpeg::JpegDecoder as NewJpeg;

use fixtures::*;

/// 参照目录：`<workspace>/target/csharp-ref`。
fn csharp_ref_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("csharp-ref")
}

/// 用新解码器同口径解出 (width, height, color_type, bytes)。
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

fn new_decode(bytes: &[u8]) -> Result<(u32, u32, kimg_core::ColorType, Vec<u8>), String> {
    decode(
        |c| NewJpeg::new(c).map_err(|e| e.to_string()),
        bytes,
    )
}

/// 单文件：尺寸/颜色类型/字节 三者与 C# 参照完全一致。
/// 返回 Ok(true) 恒等；Ok(false) 新路径明确不支持（计入 SKIP，不判失败）；
/// Err(..) 为失败。
fn assert_csharp_parity(
    ref_dir: &Path,
    name: &str,
    stem: &str,
    bytes: &[u8],
) -> Result<bool, String> {
    let (w, h, ct, our) = match new_decode(bytes) {
        Ok(v) => v,
        Err(e) if e.contains("not support") || e.contains("Unsupported") => {
            println!("{name}: SKIP（新路径不支持：{e}）");
            return Ok(false);
        }
        Err(e) => return Err(format!("{name}: 新路径解码失败：{e}（C# 参照可解）")),
    };

    let rgb_cand = format!("{stem}.{w}x{h}.rgb");
    let gray_cand = format!("{stem}.{w}x{h}.gray");
    let (cand, ext) = if ref_dir.join(&rgb_cand).is_file() {
        (rgb_cand, "rgb")
    } else if ref_dir.join(&gray_cand).is_file() {
        (gray_cand, "gray")
    } else {
        return Err(format!(
            "no C# ref for {name}: expected target/csharp-ref/{stem}.{w}x{h}.[rgb|gray]"
        ));
    };

    // 防御性：从匹配到的 ref 文件名解析 `.<w>x<h>.`，与其声明尺寸核对。
    let body = cand.trim_end_matches(&format!(".{ext}"));
    let dims = body.rsplit('.').next().unwrap_or_default();
    let mut dims_it = dims.split('x');
    let (rw, rh) = match (dims_it.next(), dims_it.next()) {
        (Some(a), Some(b)) if dims_it.next().is_none() => match (a.parse::<u32>(), b.parse::<u32>())
        {
            (Ok(a), Ok(b)) => (a, b),
            _ => return Err(format!("{name}: 无法解析 ref 文件名尺寸: {cand}")),
        },
        _ => return Err(format!("{name}: 无法解析 ref 文件名尺寸: {cand}")),
    };
    if rw != w || rh != h {
        return Err(format!(
            "{name}: 尺寸不一致 ref={rw}x{rh}（文件名 {cand}）ours={w}x{h}"
        ));
    }

    let expected_ct = match ext {
        "gray" => ColorType::L8,
        "rgb" => ColorType::Rgb8,
        _ => unreachable!(),
    };
    if ct != expected_ct {
        return Err(format!(
            "{name}: 颜色类型不一致 ref={ext}→{:?} ours={:?}（{cand}）",
            expected_ct, ct
        ));
    }

    let ref_bytes = std::fs::read(ref_dir.join(&cand))
        .map_err(|e| format!("{name}: 读 ref 文件失败 {cand}: {e}"))?;
    let expected_len = w as usize * h as usize * usize::from(expected_ct.bytes_per_pixel());
    if ref_bytes.len() != expected_len || our.len() != expected_len {
        return Err(format!(
            "{name}: 输出长度不一致 ref={} ours={} expected={}（{cand}）",
            ref_bytes.len(),
            our.len(),
            expected_len
        ));
    }

    if ref_bytes != our {
        let bpp = usize::from(expected_ct.bytes_per_pixel());
        let wu = w as usize;
        let idx = ref_bytes
            .iter()
            .zip(our.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let x = (idx / bpp) % wu;
        let y = (idx / bpp) / wu;
        let ch = idx % bpp;
        return Err(format!(
            "{name}: 字节不一致 首异 (x={x}, y={y}, 通道={ch}) old(ref)={} new(ours)={}（diff字节总数 {}）",
            ref_bytes[idx],
            our[idx],
            ref_bytes
                .iter()
                .zip(our.iter())
                .filter(|(a, b)| a != b)
                .count()
        ));
    }
    println!("PARITY CS OK {name} {w}x{h} {:?}", ct);
    Ok(true)
}

#[test]
fn corpus_csharp_parity() {
    println!("cargo test -p kimg_jpeg --test jpeg_csharp_parity -- --nocapture");
    let dir = corpus_dir();
    assert!(dir.is_dir(), "语料目录缺失: {}", dir.display());
    let files = collect_jpegs(&dir);
    assert!(!files.is_empty(), "语料目录为空: {}", dir.display());
    let ref_dir = csharp_ref_dir();
    assert!(
        ref_dir.is_dir(),
        "missing target/csharp-ref — run the C# reference emitter first: {}",
        ref_dir.display()
    );
    let mut oks = 0usize;
    let mut skips = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for p in &files {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("{name}: 读文件失败 {e}"));
        match assert_csharp_parity(&ref_dir, &name, &stem, &bytes) {
            Ok(true) => oks += 1,
            Ok(false) => skips += 1,
            Err(e) => failures.push(e),
        }
        for f in &failures {
            println!("PARITY FAIL: {f}");
        }
    }
    println!(
        "JPEG_CS_PARITY SUMMARY ok={oks} skip={skips} fail={}",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} 个文件与 C# 参照不一致：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
