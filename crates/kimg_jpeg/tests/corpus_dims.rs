//! JPEG corpus 尺寸分布样本（A 表机制前提：降档候选按像素数定靶）。
//! 只用 `JpegDecoder::dimensions()` 头解析（不整图解码，秒级），扫 `JPEG` 与 `JPEG_FULL`
//! 各列 top-5 最大像素 + min/median/max 分布。`--ignored --nocapture` 运行。

use std::path::{Path, PathBuf};

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder;

fn corpus_dir() -> PathBuf {
    std::env::var("IMG_JPEG_DIMS_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("Benchmarks")
                .join("corpus")
        })
}

#[test]
#[ignore]
fn bench_jpeg_corpus_dims() {
    let base = corpus_dir();
    let mut any = false;
    for d in ["JPEG", "JPEG_FULL"] {
        let dir = base.join(d);
        if !dir.is_dir() {
            println!("SKIP: missing {dir:?}");
            continue;
        }
        let mut rows: Vec<(String, u32, u32, u64)> = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.extension()
                .and_then(|x| x.to_str())
                .is_none_or(|x| !x.eq_ignore_ascii_case("jpg") && !x.eq_ignore_ascii_case("jpeg"))
            {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&p)
                && let Ok(dec) = JpegDecoder::new(std::io::Cursor::new(&bytes))
            {
                let (w, h) = dec.dimensions();
                rows.push((
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    w,
                    h,
                    u64::from(w) * u64::from(h),
                ));
            }
        }
        rows.sort_by_key(|r| r.3);
        any = true;
        println!("[IMG-DIMS] {d}: n={}", rows.len());
        for (name, w, h, px) in rows.iter().rev().take(5) {
            println!("[IMG-DIMS]   top {px:>9}px  {name}  ({w}×{h})");
        }
        if !rows.is_empty() {
            let min = rows.first().unwrap().3;
            let max = rows.last().unwrap().3;
            let med = rows[rows.len() / 2].3;
            println!(
                "[IMG-DIMS]   dist min={min} median={med} max={max} (n={})",
                rows.len()
            );
        }
    }
    assert!(any, "无可扫语料目录");
}
