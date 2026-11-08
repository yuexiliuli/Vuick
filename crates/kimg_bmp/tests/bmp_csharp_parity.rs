//! kimg_bmp vs C# Sdcb.SimdCodecs.Bmp 的字节恒等对账测试（oracle 硬闸的一部分）。
//!
//! 语料 = `Benchmarks/corpus/BMP` + `Benchmarks/corpus/BMP-MATRIX`（matrix 由
//! `Benchmarks/bmp/scripts/run_csharp_ref.ps1` 的 `BmpRef gen` 生成）。C# RGBA8 参照 dump 在
//! `<repo>/target/csharp-ref/bmp/<stem>.<w>x<h>.rgba`（由 `BmpRef ref` 产出，紧密 4B/px、w*h*4）。
//!
//! 对比口径：kimg 自然输出（RGB8 → 补 alpha=FF → RGBA8；RGBA8 直用）与 C# dump 逐字节对比。
//! **2026-11-08 解码器修复后**：32bpp BI_RGB 通道旋转、16bit 展宽（改位重复）、RLE4/RLE8 解析
//! 均已修复，唯一保留的已知分歧是 **32bpp BI_RGB 的 alpha 语义**：kimg 按 image-0.25/WIC 口径把
//! 第 4 字节当 alpha 丢弃恒 FF（不透明），C# 照浏览器口径暴露真实 alpha（源含非 0 alpha 时）——
//! 即 RGB 全等、C# 带真实 alpha、kimg 恒 FF → 记 div（非 bug，语义差异）。
//! 其余文件必须 **RGBA8 逐字节全等**：任何 kimg RGB 与 C# 不等 → panic（真实未知 bug / 回归）。
//!
//! 缺语料/dump → SKIP（仓库惯例：手工装配不入库，SKIP 不是回归）；结束时输出
//! `parity ok=N div=N skip=N`。
//!
//! 缺语料/dump → SKIP（仓库惯例：手工装配不入库，SKIP 不是回归）；结束时输出
//! `parity ok=N div=N skip=N`。若出现**非预期**字节不等 → panic 列文件名：
//! 定位是 kimg bug 还是 C# 语义差异，不改解码实现、回报主 agent。

use kimg_core::color::ColorType;
use kimg_core::ImageDecoder as _;

/// 仓库根 = crates/kimg_bmp 的上一级再上一级。
fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// 收集目录下全部 *.bmp（文件名字典序）。
fn collect_bmps(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("bmp"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// kimg 解码 → RGBA8（RGB8 补 alpha=FF；RGBA8 直用），返回 (宽, 高, 颜色类型, RGBA)。
/// 解码失败（如 RLE 已知 bug）经 Ok::<_, String> 的 Err 返回，由调用方分类。
fn decode_rgba(data: &[u8]) -> Result<(u32, u32, ColorType, Vec<u8>), String> {
    let dec = kimg_bmp::BmpDecoder::new(std::io::Cursor::new(data))
        .map_err(|e| format!("解码头失败: {e:?}"))?;
    let ct = dec.color_type();
    let (w, h) = dec.dimensions();
    let mut raw = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut raw).map_err(|e| format!("解码失败: {e:?}"))?;
    let rgba = match ct {
        ColorType::Rgba8 => raw,
        ColorType::Rgb8 => {
            let mut out = Vec::with_capacity(raw.len() / 3 * 4);
            for px in raw.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        other => return Err(format!("kimg 输出非 RGB(A)8: {other:?}")),
    };
    debug_assert_eq!(rgba.len(), (w as usize) * (h as usize) * 4);
    Ok((w, h, ct, rgba))
}

#[test]
fn csharp_parity_corpus() {
    let root = repo_root();
    let corpus = root.join("Benchmarks").join("corpus");
    let ref_out = root.join("target").join("csharp-ref").join("bmp");

    // 语料装配检查：BMP 与 BMP-MATRIX 任一缺 → SKIP（手工装配不入库，SKIP 是常态）。
    let bmp_dir = corpus.join("BMP");
    let matrix_dir = corpus.join("BMP-MATRIX");
    let have_bmp = bmp_dir.is_dir() && !collect_bmps(&bmp_dir).is_empty();
    let have_matrix = matrix_dir.is_dir() && !collect_bmps(&matrix_dir).is_empty();
    if !have_bmp && !have_matrix {
        eprintln!("[bmp-csharp-parity] SKIP: corpus/BMP 与 corpus/BMP-MATRIX 均缺失");
        return;
    }

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if have_bmp {
        files.extend(collect_bmps(&bmp_dir));
    }
    if have_matrix {
        files.extend(collect_bmps(&matrix_dir));
    }
    files.sort();

    // 无 dump → SKIP（说明先跑 Benchmarks/bmp/scripts/run_csharp_ref.ps1）。
    let have_dump = ref_out.is_dir()
        && ref_out
            .read_dir()
            .map(|rd| rd.flatten().next().is_some())
            .unwrap_or(false);
    if !have_dump {
        eprintln!(
            "[bmp-csharp-parity] SKIP: {} 无 C# dump（先跑 run_csharp_ref.ps1）",
            ref_out.display()
        );
        return;
    }

    let mut ok = 0usize;
    let mut div = 0usize;
    let mut skip = 0usize;
    for p in &files {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[bmp-csharp-parity] SKIP {name}: 读文件失败 {e}");
                skip += 1;
                continue;
            }
        };
        // RLE4/RLE8 已于 2026-11-08 修复（absolute 补偶跳过 + 4bit run 高 nibble/C# 交替），
        // 与 C# dump 逐字节对读，不再提前记 div。
        let (w, h, ct, rgba) = match decode_rgba(&bytes) {
            Ok(v) => v,
            Err(e) => {
                panic!("[bmp-csharp-parity] {name} kimg 解码失败: {e}");
            }
        };
        let dump = ref_out.join(format!("{}.{}x{}.rgba", stem(&name), w, h));
        if !dump.exists() {
            eprintln!("[bmp-csharp-parity] SKIP {name}: 缺 dump {}", dump.display());
            skip += 1;
            continue;
        }
        let ref_bytes = match std::fs::read(&dump) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[bmp-csharp-parity] SKIP {name}: 读 dump 失败 {e}");
                skip += 1;
                continue;
            }
        };
        if ref_bytes.len() != rgba.len() {
            panic!(
                "[bmp-csharp-parity] {name} 尺寸不符: kimg={}B vs C# dump={}B（{w}x{h}）",
                rgba.len(),
                ref_bytes.len()
            );
        }
        match ct {
            ColorType::Rgba8 => {
                if rgba != ref_bytes {
                    panic!(
                        "[bmp-csharp-parity] {name} kimg(RGBA8) 与 C# dump 全字节不等（非预期差异）"
                    );
                }
                eprintln!("[bmp-csharp-parity] ok   {name} (RGBA8 全等)");
                ok += 1;
            }
            ColorType::Rgb8 => {
                // kimg RGB 必须与 C# RGB 全等（3 项解码 bug 已修）。唯一允许分歧 = 32bpp BI_RGB
                // 的 alpha 语义（kimg 恒 FF vs C# 真实 alpha），此时 C# 侧含非 0 alpha → 记 div。
                let px_ours: Vec<[u8; 4]> = rgba.chunks_exact(4).map(|a| [a[0], a[1], a[2], a[3]]).collect();
                let px_ref: Vec<[u8; 4]> = ref_bytes.chunks_exact(4).map(|a| [a[0], a[1], a[2], a[3]]).collect();
                let rgb_equal = px_ours.iter().zip(&px_ref).all(|(a, b)| a[0] == b[0] && a[1] == b[1] && a[2] == b[2]);
                if !rgb_equal {
                    panic!(
                        "[bmp-csharp-parity] {name} kimg(RGB) 与 C# 不等（均已修应全等；真实未知 bug/回归）"
                    );
                }
                // RGB 相等：alpha 归因（C# 带真实 alpha、kimg 恒 FF 的已知分歧）。
                let has_real_alpha = px_ref.iter().any(|px| px[3] != 0xFF);
                if has_real_alpha {
                    eprintln!(
                        "[bmp-csharp-parity] div  {name} (RGB 相等；C# 带真实 alpha → \
                         kimg 恒 FF 的已知语义分歧：WIC/image 同向，非 bug)"
                    );
                    div += 1;
                } else {
                    eprintln!("[bmp-csharp-parity] ok   {name} (全等)");
                    ok += 1;
                }
            }
            other => panic!("[bmp-csharp-parity] {name} 输出非 RGB(A)8: {other:?}"),
        }
    }
    eprintln!("[bmp-csharp-parity] parity ok={ok} div={div} skip={skip}");
    assert!(ok + div + skip == files.len(), "计数与文件数不符");
}

fn stem(name: &str) -> String {
    name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or_else(|| name.to_string())
}
