//! JPEG/PNG/BMP 三语料全量尺寸清单（header-only，不整图解码）。
//! 新增 10–50MB 大图后重测：输出每目录「文件 | MB | 宽x高 | MP | 全解 RGBA 字节(占 256MB 缓存预算比)」，
//! 并按字节降序标 top-N 大图。`--ignored --nocapture` 运行。

use std::io::Cursor;
use std::path::{Path, PathBuf};

use kimg::ImageDecoder as _;

fn corpus_dir() -> PathBuf {
    std::env::var("IMG_DIMS_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("Benchmarks")
                .join("corpus")
        })
}

static CACHE_BUDGET: u64 = 256 * 1024 * 1024;

fn dims(p: &Path) -> Option<(u32, u32)> {
    let bytes = std::fs::read(p).ok()?;
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    let r = Cursor::new(&bytes);
    match ext.as_str() {
        "jpg" | "jpeg" => {
            let dec = kimg::codecs::jpeg::JpegDecoder::new(r).ok()?;
            Some(dec.dimensions())
        }
        "png" => {
            let dec = kimg::codecs::png::PngDecoder::new(r).ok()?;
            Some(dec.dimensions())
        }
        "bmp" => {
            let dec = kimg::codecs::bmp::BmpDecoder::new(r).ok()?;
            Some(dec.dimensions())
        }
        _ => None,
    }
}

#[test]
#[ignore]
fn bench_corpus_dims_all() {
    let base = corpus_dir();
    let mut any = false;
    for d in ["JPEG", "PNG", "BMP"] {
        let dir = base.join(d);
        if !dir.is_dir() {
            println!("SKIP: missing {dir:?}");
            continue;
        }
        let mut rows: Vec<(String, u64, u32, u32, u64)> = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let mdlen = e.metadata().map(|m| m.len()).unwrap_or(0);
            if mdlen == 0 || !p.is_file() {
                continue;
            }
            if let Some((w, h)) = dims(&p) {
                rows.push((
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    mdlen,
                    w,
                    h,
                    u64::from(w) * u64::from(h),
                ));
            } else {
                println!("[DIMS] {d}: UNPARSEABLE {} ({} B)", p.display(), mdlen);
            }
        }
        rows.sort_by_key(|r| r.1);
        any = true;
        println!("\n[DIMS] === {d}: n={} ===", rows.len());
        for (name, len, w, h, px) in &rows {
            let rgba = *px as u64 * 4;
            let budget_ratio = rgba as f64 / CACHE_BUDGET as f64;
            println!(
                "[DIMS]   {:>9.2}MB | {:>10}px | {:>5}x{:>5} | {:>6.2}MP | RGBA {:>10.1}MB | {:.1} 缓存名额 | {}",
                *len as f64 / (1024.0 * 1024.0),
                px,
                w,
                h,
                *px as f64 / 1e6,
                rgba as f64 / (1024.0 * 1024.0),
                budget_ratio,
                name
            );
        }
        // top-6 大图（按字节）
        println!("[DIMS]   --- {d} top-6 大图（按文件字节）---");
        for row in rows.iter().rev().take(6) {
            let (name, len, w, h, px) = row;
            println!(
                "[DIMS]   BIG {:.2}MB {:>6.2}MP {}x{} {}",
                *len as f64 / (1024.0 * 1024.0),
                *px as f64 / 1e6,
                w,
                h,
                name
            );
        }
    }
    assert!(any, "无可扫语料目录");
}
