//! BMP 战役前置数据：corpus 属性扫描 + kill 探针（headroom 实证）。
//!
//! 只加 `#[ignore]` bench（不动生产逻辑）：
//! - `bench_bmp_corpus_attributes`：扫描 corpus 全部 BMP 的 宽x高 / bpp / 压缩 / 像素数，
//!   给「降档解码 vs 全解」与「颜色转换工作量」的可计数前提（K118）。
//! - `probe_bmp_vs_memcpy`：对最大 BMP，kimg 解码中位墙钟 vs 纯 memcpy / 标量 BGR→RGB
//!   重排的倍数——直接证 BMP 解码相对「搬运+换序下限」的 headroom。
//!
//! 目录 env `IMG_BMP_CORPUS_DIR`（缺省 `Benchmarks/corpus/BMP`）；`--ignored --nocapture`。

use std::path::PathBuf;
use std::time::Instant;

use kimg_core::ImageDecoder;

/// 解析 BITMAPFILEHEADER(14B) + BITMAPINFOHEADER：宽[18] 高[22] bpp[28] 压缩[30]
/// （小端）。压缩映射与 `codecs/bmp.rs:336-383` 一致：0=RGB, 1=RLE8, 2=RLE4,
/// 3=Bitfields, 4=JPEG(不支持), 5=PNG(不支持)。
fn header_info(bytes: &[u8]) -> Option<(u32, u32, u16, u32)> {
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return None;
    }
    let le_u32 = |o: usize| -> Option<u32> {
        let s: [u8; 4] = bytes.get(o..o + 4)?.try_into().ok()?;
        Some(u32::from_le_bytes(s))
    };
    let le_u16 = |o: usize| -> Option<u16> {
        let s: [u8; 2] = bytes.get(o..o + 2)?.try_into().ok()?;
        Some(u16::from_le_bytes(s))
    };
    Some((le_u32(18)?, le_u32(22)?, le_u16(28)?, le_u32(30)?))
}

fn comp_name(c: u32) -> &'static str {
    match c {
        0 => "RGB",
        1 => "RLE8",
        2 => "RLE4",
        3 => "Bitfields",
        4 => "JPEG",
        5 => "PNG",
        _ => "other",
    }
}

fn corpus_bmps() -> Vec<PathBuf> {
    let dir = std::env::var("IMG_BMP_CORPUS_DIR")
        .unwrap_or_else(|_| r"E:\Rust\gpui-editor\Benchmarks\corpus\BMP".to_string());
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读取语料目录失败 {dir}: {e}"))
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("bmp"))
        })
        .collect();
    v.sort();
    assert!(!v.is_empty(), "语料目录无 BMP：{dir}");
    v
}

/// 中位墙钟（5 轮取中间值，抗 GC/IO 抖动）。
fn median_5<F: FnMut()>(mut f: F) -> f64 {
    let mut v = Vec::with_capacity(5);
    for _ in 0..5 {
        let t = Instant::now();
        f();
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[2]
}

/// A 表补充：corpus BMP 属性（bpp/压缩/像素数）——解码工作量可计数前提。
#[test]
#[ignore]
fn bench_bmp_corpus_attributes() {
    let v = corpus_bmps();
    let dir = std::env::var("IMG_BMP_CORPUS_DIR")
        .unwrap_or_else(|_| r"E:\Rust\gpui-editor\Benchmarks\corpus\BMP".to_string());
    eprintln!("\n[IMG-BENCH] BMP corpus 属性（{} 文件, {dir}）:", v.len());
    let mut by_bpp: std::collections::BTreeMap<u16, usize> = Default::default();
    let mut by_comp: std::collections::BTreeMap<u32, usize> = Default::default();
    let mut max_px: Option<(u64, String)> = None;
    for p in &v {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(p).unwrap_or_default();
        let Some((w, h, bpp, comp)) = header_info(&bytes) else {
            eprintln!("  {name:<24} 头解析失败/SKIP");
            continue;
        };
        let px = u64::from(w) * u64::from(h);
        let (dw, dh, ct) = match kimg_bmp::BmpDecoder::new(std::io::Cursor::new(&bytes)) {
            Ok(d) => {
                let (dw, dh) = d.dimensions();
                (dw, dh, format!("{:?}", d.color_type()))
            }
            Err(e) => (0, 0, format!("ERR {e}")),
        };
        eprintln!(
            "  {name:<24} {:>6}×{:<6} bpp={bpp:>2} comp={:<9} px={px:>10} | kimg {dw}×{dh} {ct}",
            w,
            h,
            comp_name(comp)
        );
        *by_bpp.entry(bpp).or_default() += 1;
        *by_comp.entry(comp).or_default() += 1;
        if max_px.is_none() || px > max_px.as_ref().unwrap().0 {
            max_px = Some((px, name));
        }
    }
    eprintln!("  --- bpp  分布: {:?}", by_bpp);
    eprintln!("  --- 压缩 分布: {:?}", by_comp);
    eprintln!("  --- 最大像素: {:?}", max_px);
}

/// kill 探针：最大 BMP 的 kimg 解码中位墙钟 vs 纯 memcpy / 标量 BGR→RGB 重排的下限倍数。
/// 进程级 `KIMG_BMP_NO_SIMD=1` 时解码走标量——同一探针跑两遍即可量化 SIMD 有无的差距。
#[test]
#[ignore]
fn probe_bmp_vs_memcpy() {
    let v = corpus_bmps();
    let (path, w, h, bpp, comp) = {
        let mut best: Option<(PathBuf, u32, u32, u16, u32)> = None;
        for p in &v {
            let bytes = std::fs::read(p).unwrap_or_default();
            if let Some((w, h, bpp, comp)) = header_info(&bytes) {
                let better = match &best {
                    None => true,
                    Some((_, bw, bh, ..)) => u64::from(w) * u64::from(h) > u64::from(*bw) * u64::from(*bh),
                };
                if better {
                    best = Some((p.clone(), w, h, bpp, comp));
                }
            }
        }
        best.expect("语料无 BMP")
    };
    let bytes = std::fs::read(&path).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let dec = kimg_bmp::BmpDecoder::new(std::io::Cursor::new(&bytes)).expect("BMP 解码失败");
    let ctype = dec.color_type();
    let nbytes = dec.total_bytes() as usize;
    let out_bpp = ctype.bytes_per_pixel();

    // 解码中位（BmpDecoder 全量到原生输出）
    let dec_ms = median_5(|| {
        let dec = kimg_bmp::BmpDecoder::new(std::io::Cursor::new(&bytes)).expect("BMP 解码失败");
        let mut raw = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut raw).expect("BMP 解码失败");
        std::hint::black_box(raw);
    });

    // 下限 1：纯 memcpy 同字节数
    let src = vec![0x5Au8; nbytes];
    let mut dst = vec![0u8; nbytes];
    let cp_ms = median_5(|| {
        dst.copy_from_slice(&src);
        std::hint::black_box(&dst);
    });

    // 下限 2：标量 BGR→RGB 重排（颜色转换的最小逐像素工作，已加载 raw 上做）
    let mut sw_ms = f64::NAN;
    if out_bpp == 3 {
        let mut raw = vec![0u8; nbytes];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        sw_ms = median_5(|| {
            for px in raw.chunks_exact_mut(3) {
                px.swap(0, 2);
            }
            std::hint::black_box(&raw);
        });
    }

    let nosimd = std::env::var("KIMG_BMP_NO_SIMD").unwrap_or_default();
    eprintln!(
        "\n[IMG-PROBE-BMP] {} {w}×{h} bpp={bpp} comp={:>9} out={}B/px 解码 {dec_ms:8.2}ms vs memcpy {cp_ms:7.3}ms -> {:6.1}× | vs 标量重排 {sw_ms:7.3}ms -> {:6.1}× | KIMG_BMP_NO_SIMD={}",
        name,
        comp_name(comp),
        out_bpp,
        dec_ms / cp_ms.max(1e-6),
        if sw_ms.is_nan() { f64::NAN } else { dec_ms / sw_ms.max(1e-6) },
        if nosimd.is_empty() { "0" } else { &nosimd }
    );
}
