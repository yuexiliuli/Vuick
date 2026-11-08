//! PNG 自研引擎 vs C# Sdcb.SimdCodecs.Png `TryDecodeIntoRgba8` 参照字节恒等对照（正确性硬闸）。
//!
//! 语料：`<workspace>/Benchmarks/corpus/PNG`（34 真图 + 可选生成比较集例子；env `KIMG_PNG_CORPUS` 覆盖）。
//! 参照：`<workspace>/target/csharp-ref/png`（外部 C# runner 产出，每语料一张）：
//! `<stem>.<w>x<h>.rgba` = RGBA8 紧密 4B/px（w*h*4）。
//! 比较集 = RGBA8 布局文件：kimg 自然输出（`ColorType::Rgba8`，EXPAND 语义）与 C# dump 逐字节比较，
//! 失败打印首异像素 (x, y, 通道) + 差异总数；非 RGBA8-natural 文件（RGB8/gray8 等布局不同，
//! phase-1 比较集排除）计入 SKIP，不判失败。
//!
//! 运行：
//! ```text
//! cargo test -p kimg_png --test png_csharp_parity -- --nocapture
//! ```

use std::path::{Path, PathBuf};

use kimg_core::ColorType;
use kimg_png::codecs::png::PngDecoder;
use kimg_png::ImageDecoder as _;

/// 语料目录：默认 `<workspace>/Benchmarks/corpus/PNG`；env `KIMG_PNG_CORPUS` 优先覆盖。
fn corpus_dir() -> PathBuf {
    if let Ok(p) = std::env::var("KIMG_PNG_CORPUS") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("Benchmarks")
        .join("corpus")
        .join("PNG")
}

/// 参照目录：`<workspace>/target/csharp-ref/png`（png 专用子目录，防与 jpeg 等格式参照互清）。
fn csharp_ref_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("csharp-ref")
        .join("png")
}

fn collect_pngs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("png"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn corpus_csharp_parity() {
    let dir = corpus_dir();
    assert!(dir.is_dir(), "语料目录缺失: {}", dir.display());
    let files = collect_pngs(&dir);
    assert!(!files.is_empty(), "语料目录为空: {}", dir.display());
    let ref_dir = csharp_ref_dir();
    assert!(
        ref_dir.is_dir(),
        "missing target/csharp-ref/png — 先跑 pwsh -File Benchmarks\\png\\scripts\\run_csharp_ref.ps1: {}",
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

        // 零拷贝借用切片构造 + 自然 EXPAND 解码（复用构造期缓存头部状态，走内存路径）。
        let dec = match PngDecoder::from_slice(&bytes) {
            Ok(d) => d,
            Err(e) => {
                failures.push(format!("{name}: 解码失败：{e}"));
                continue;
            }
        };
        let (w, h) = dec.dimensions();
        let ct = dec.color_type();
        let total = dec.total_bytes() as usize;
        let mut our = vec![0u8; total];
        if let Err(e) = dec.read_image(&mut our) {
            failures.push(format!("{name}: read_image 失败：{e}"));
            continue;
        }

        let cand = format!("{stem}.{w}x{h}.rgba");
        let ref_path = ref_dir.join(&cand);
        if !ref_path.is_file() {
            println!("{name}: SKIP（无 C# rgba dump {cand} — 非比较集/未生成）");
            skips += 1;
            continue;
        }
        let ref_bytes =
            std::fs::read(&ref_path).unwrap_or_else(|e| panic!("{name}: 读 ref 失败 {cand}: {e}"));

        if ct != ColorType::Rgba8 {
            println!("{name}: SKIP（非 RGBA8-natural：{ct:?} 布局不同，phase-1 比较集排除）");
            skips += 1;
            continue;
        }

        let expected = w as usize * h as usize * 4;
        if our.len() != expected || ref_bytes.len() != expected {
            failures.push(format!(
                "{name}: 输出长度不一致 ref={} ours={} expected={}（{cand}）",
                ref_bytes.len(),
                our.len(),
                expected
            ));
            continue;
        }

        if ref_bytes != our {
            let idx = ref_bytes
                .iter()
                .zip(our.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            let x = (idx / 4) % (w as usize);
            let y = (idx / 4) / (w as usize);
            let ch = idx % 4;
            failures.push(format!(
                "{name}: 字节不一致 首异 (x={x}, y={y}, 通道={ch}) old(ref)={} new(ours)={}（diff字节总数 {}）",
                ref_bytes[idx],
                our[idx],
                ref_bytes
                    .iter()
                    .zip(our.iter())
                    .filter(|(a, b)| a != b)
                    .count()
            ));
            continue;
        }
        println!("PARITY CS OK {name} {w}x{h} {ct:?}");
        oks += 1;
    }

    println!("PNG_CS_PARITY SUMMARY ok={oks} skip={skips} fail={}", failures.len());
    for f in &failures {
        println!("PARITY FAIL: {f}");
    }
    assert!(
        failures.is_empty(),
        "{} 个文件与 C# 参照不一致：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
