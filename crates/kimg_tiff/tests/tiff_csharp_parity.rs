//! kimg_tiff vs C# Sdcb.SimdCodecs.Tiff 的字节恒等对账测试（oracle 硬闸的一部分）。
//!
//! 语料 = `Benchmarks/corpus/TIFF`（8bit 矩阵，由 `Benchmarks/tiff/scripts/run_gen.ps1` 的 gen
//! crate 生成：{gray8, rgb8, rgba8} × {none, packbits, lzw, deflate} × {predictor off/on} +
//! 2 条多 strip）。C# RGBA8 参照 dump 在 `<repo>/target/csharp-ref/tiff/<stem>.<w>x<h>.rgba`
//! （由 `TiffRef ref` 产出，紧密 4B/px、w*h*4）。
//!
//! 对比口径：自研 `TiffDecoder` 自然输出补 alpha=FF（L8 → L,L,L,FF；RGB8 → R,G,B,FF；RGBA8 直用）
//! 与 C# dump 逐字节对比。8bit 语料是自研 declared 全覆盖（L8/RGB8/RGBA8 + None/PackBits/LZW/Deflate/
//! ZlibPkzip + Predictor2），任何文件解码失败（含 Unsupported）→ panic 并提示「gen 漏格或自研 bug」：
//! 定位后不改解码实现、回报主 agent。
//!
//! 缺语料/dump → SKIP（仓库惯例：手工装配不入库，SKIP 不是回归）；结束时输出
//! `parity ok=N skip=N`。

use std::io::Cursor;

use kimg_core::color::ColorType;
use kimg_tiff::ImageDecoder as _;
use kimg_tiff::TiffDecoder;

/// 仓库根 = crates/kimg_tiff 的上一级再上一级。
fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// 收集目录下全部 *.tif/*.tiff（文件名字典序）。
fn collect_tiffs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("tif") || x.eq_ignore_ascii_case("tiff"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// kimg 解码 → RGBA8（L8/RGB8 补 alpha=FF；RGBA8 直用），返回 (宽, 高, 颜色类型, RGBA)。
/// 解码失败（8bit 语料应全覆盖）→ Err。
fn decode_rgba(data: &[u8]) -> Result<(u32, u32, ColorType, Vec<u8>), String> {
    let dec = TiffDecoder::new(Cursor::new(data)).map_err(|e| format!("解码头失败: {e:?}"))?;
    let ct = dec.color_type();
    let (w, h) = dec.dimensions();
    let mut raw = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut raw)
        .map_err(|e| format!("解码失败: {e:?}"))?;
    let rgba = match ct {
        ColorType::Rgba8 => raw,
        ColorType::Rgb8 => {
            let mut out = Vec::with_capacity(raw.len() / 3 * 4);
            for px in raw.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        ColorType::L8 => {
            let mut out = Vec::with_capacity(raw.len() * 4);
            for &l in &raw {
                out.extend_from_slice(&[l, l, l, 0xFF]);
            }
            out
        }
        other => return Err(format!("自研输出非 RGB(A)8: {other:?}")),
    };
    debug_assert_eq!(rgba.len(), (w as usize) * (h as usize) * 4);
    Ok((w, h, ct, rgba))
}

#[test]
fn csharp_parity_corpus() {
    let root = repo_root();
    let corpus = root.join("Benchmarks").join("corpus").join("TIFF");
    let ref_out = root.join("target").join("csharp-ref").join("tiff");

    // 语料装配检查：TIFF 语料缺 → SKIP（手工装配不入库，SKIP 是常态）。
    let files = collect_tiffs(&corpus);
    if files.is_empty() {
        eprintln!(
            "[tiff-csharp-parity] SKIP: corpus/TIFF 缺失或为空（先跑 Benchmarks/tiff/scripts/run_gen.ps1）: {}",
            corpus.display()
        );
        return;
    }

    // 无 dump → SKIP（说明先跑 Benchmarks/tiff/scripts/run_csharp_ref.ps1）。
    let have_dump = ref_out.is_dir()
        && ref_out
            .read_dir()
            .map(|rd| rd.flatten().next().is_some())
            .unwrap_or(false);
    if !have_dump {
        eprintln!(
            "[tiff-csharp-parity] SKIP: {} 无 C# dump（先跑 run_csharp_ref.ps1）",
            ref_out.display()
        );
        return;
    }

    let mut ok = 0usize;
    let mut skip = 0usize;
    for p in &files {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tiff-csharp-parity] SKIP {name}: 读文件失败 {e}");
                skip += 1;
                continue;
            }
        };
        let (w, h, _ct, rgba) = match decode_rgba(&bytes) {
            Ok(v) => v,
            Err(e) => {
                // 8bit 语料是自研 declared 全覆盖：解不开 = gen 漏格 / 自研未覆盖该格 / 真实 bug。
                panic!("[tiff-csharp-parity] {name} 自研解码失败（8bit 语料应全覆盖，gen 漏格或自研 bug）: {e}");
            }
        };
        let dump = ref_out.join(format!("{}.{}x{}.rgba", stem(&name), w, h));
        if !dump.exists() {
            eprintln!("[tiff-csharp-parity] SKIP {name}: 缺 dump {}", dump.display());
            skip += 1;
            continue;
        }
        let ref_bytes = match std::fs::read(&dump) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tiff-csharp-parity] SKIP {name}: 读 dump 失败 {e}");
                skip += 1;
                continue;
            }
        };
        if ref_bytes.len() != rgba.len() {
            panic!(
                "[tiff-csharp-parity] {name} 尺寸不符: kimg={}B vs C# dump={}B（{w}x{h}）",
                rgba.len(),
                ref_bytes.len()
            );
        }
        if rgba != ref_bytes {
            panic!(
                "[tiff-csharp-parity] {name} kimg(RGBA8) 与 C# dump 全字节不等（真实未知 bug/回归）"
            );
        }
        eprintln!("[tiff-csharp-parity] ok   {name} (RGBA8 全等)");
        ok += 1;
    }
    eprintln!("[tiff-csharp-parity] parity ok={ok} skip={skip}");
    assert!(ok + skip == files.len(), "计数与文件数不符");
}

fn stem(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| name.to_string())
}
